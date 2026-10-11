"""The own-repo tier: bugs this repository fixed in the past, put back and handed to an agent.

Matthew has not picked the 40 tasks (design 9.2-h), so this module drafts a candidate set from the
repository's own history and everything below the draft is his to change:

1. :func:`mine` walks the history for fix commits and, per crate a fix touched, keeps the ones that
   changed source *and* added tests in separate test files (inline ``#[cfg(test)]`` tests cannot be
   hidden from the agent without hiding the code beside them, so those commits are not candidates).
2. A candidate is a *group*: one commit, one crate. The task starts from the fix commit with that
   crate's source files put back as they were in the commit's parent (so the bug is back), history
   dropped (so ``git log`` cannot show the fix), and the crate's test files also put back; the
   fix commit's version of those test files is the hidden test, installed after the agent is done.
3. :func:`validate` proves a task is sound before it is ever used: with the bug back the hidden tests
   must fail, and at the fix commit they must pass.

The statement an agent sees is built from the commit message and the *names* of the tests the fix
added. It is a draft (``needs_review``): Matthew edits the wording, drops tasks, and approves the set.
"""

from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path
from typing import Optional

from .model import Task
from .util import read_json, run_cmd, write_json
from .workspace import Context, git_bytes, grade, materialize

FIX_WORDS = re.compile(r"review fix|\bfix(es|ed)?\b|\bbug\b|repair", re.I)
# Feature commits that mention "fix" in passing.
FEATURE_START = re.compile(r"^M4\.6 \w+: (background work|trajectory export|wave-2|TUI parity|proton|the conductor at)", re.I)
TEST_PATH = re.compile(r"(^|/)(tests?|fixtures)/|(^|/)tests\.rs$|(^|/)tests_[^/]*\.rs$|_tests\.rs$|(^|/)test_[^/]*\.py$|_test\.py$")
FIXTURE_ONLY = re.compile(r"(^|/)fixtures?/")
CARGO_IDLE = ["powershell", "-NoProfile", "-File", "{cargo_idle}"]


def _git(repo, *args) -> str:
    r = run_cmd(["git", "-C", str(repo), *args], timeout=300)
    if r.code != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.err.strip()}")
    return r.out


def is_test_path(p: str) -> bool:
    return bool(TEST_PATH.search(p))


def module_path(rel_in_crate: str) -> str:
    """``src/headless/tests.rs`` -> ``headless::tests``."""
    p = rel_in_crate.replace("\\", "/")
    p = re.sub(r"^src/", "", p)
    p = re.sub(r"\.rs$", "", p)
    p = re.sub(r"/mod$", "", p)
    return p.replace("/", "::")


def _exists(repo, commit: str, path: str) -> bool:
    r = run_cmd(["git", "-C", str(repo), "cat-file", "-e", f"{commit}:{path}"], timeout=60)
    return r.code == 0


def rust_test_commands(repo, commit: str, crate: str, test_files: list) -> list:
    """The ``cargo test`` commands that run exactly these test files of ``crate`` (via the cargo-idle guard)."""
    prefix = f"crates/{crate}/"
    lib = _exists(repo, commit, f"crates/{crate}/src/lib.rs")
    cmds = []
    for f in test_files:
        rel = f[len(prefix):] if f.startswith(prefix) else f
        if rel.startswith("tests/") and rel.endswith(".rs"):
            parts = rel[len("tests/"):-3].split("/")
            # tests/foo.rs is the integration test `foo`; tests/foo/bar.rs is a module of `foo`.
            if len(parts) == 1:
                cmds.append(CARGO_IDLE + ["test", "-p", crate, "--test", parts[0]])
            continue
        if rel.startswith("src/") and rel.endswith(".rs"):
            filt = module_path(rel)
            target = ["--lib"] if lib else ["--bin", crate]
            cmds.append(CARGO_IDLE + ["test", "-p", crate, *target, filt])
    out, seen = [], set()
    for c in cmds:
        k = tuple(c)
        if k not in seen:
            seen.add(k)
            out.append(c)
    return out


def _numstat(repo, parent: str, commit: str, files: list) -> int:
    if not files:
        return 0
    total = 0
    for line in _git(repo, "diff", "--numstat", parent, commit, "--", *files).splitlines():
        a, b, *_ = line.split("\t")
        total += (int(a) if a.isdigit() else 0) + (int(b) if b.isdigit() else 0)
    return total


def added_test_names(repo, parent: str, commit: str, test_files: list) -> list:
    """Names of test functions the commit added to the test files (``#[test]`` / ``def test_``)."""
    if not test_files:
        return []
    diff = _git(repo, "diff", "-U0", parent, commit, "--", *test_files)
    added = [l[1:] for l in diff.splitlines() if l.startswith("+") and not l.startswith("+++")]
    names = []
    for i, line in enumerate(added):
        m = re.match(r"\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)", line) or re.match(r"\s*def\s+(test_\w+)", line)
        if not m:
            continue
        window = "\n".join(added[max(0, i - 3): i + 1])
        if "#[test]" in window or "#[tokio::test]" in window or m.group(1).startswith("test_"):
            names.append(m.group(1))
    return names


def statement(subject: str, body: str, crate: str, tests: list) -> str:
    s = re.sub(r"^M4\.6 \w+(\s+review fixes)?:?\s*", "", subject).strip()
    s = re.sub(r"^review fixes[:,]?\s*", "", s, flags=re.I).strip()
    if s.startswith("(") and s.endswith(")"):
        s = s[1:-1].strip()
    head = f"There is a bug in the `{crate}` crate of this repository: {s.rstrip('.')}." if s else f"A code review of the `{crate}` crate found bugs."
    parts = [head]
    body = " ".join(body.split())
    if body:
        parts.append(body[:600])
    if tests:
        shown = "\n".join(f"- {t.replace('_', ' ')}" for t in tests[:8])
        parts.append("The behaviour a regression test will check (the tests are not in the repository):\n" + shown)
    parts.append("Find and fix the bug in the source. Do not edit existing tests.")
    return "\n\n".join(parts)


def _commit_groups(repo: str, sha: str, parent: str, subject: str, date: str, body: str) -> list:
    """The candidates one fix commit yields: one per crate that changed source and added tests.

    The task's workspace is the commit's *parent* (every file as it was before the fix, tests
    included); the hidden tests are this crate's test files at the fix. So the agent faces the
    whole pre-fix tree and is graded on this crate's behaviour only.
    """
    status = _git(repo, "diff-tree", "--no-commit-id", "--name-status", "-r", "-M", sha).splitlines()
    by_crate = defaultdict(lambda: {"src": [], "tests": [], "added": []})
    for st in status:
        code, *paths = st.split("\t")
        path = paths[-1]
        m = re.match(r"crates/([^/]+)/(.+)$", path)
        if not m:
            continue
        is_t = is_test_path(path)
        if not is_t and not path.endswith(".rs"):
            continue
        g = by_crate[m.group(1)]
        kind = "tests" if is_t else "src"
        g[kind].append(path)
        if code.startswith("A"):
            g["added"].append(path)
        if code.startswith("R"):
            g[kind].append(paths[0])
    out = []
    for crate, g in sorted(by_crate.items()):
        src, tests = sorted(set(g["src"])), sorted(set(g["tests"]))
        rust_tests = [t for t in tests if t.endswith(".rs") and not FIXTURE_ONLY.search(t)]
        if not src or not rust_tests:
            continue
        names = added_test_names(repo, parent, sha, rust_tests)
        churn = _numstat(repo, parent, sha, src)
        cmds = rust_test_commands(repo, sha, crate, tests)
        # A fix that changed the assertions of existing tests (no new test function) still has a test to hide.
        if not cmds or churn < 2 or churn > 2500 or (not names and _numstat(repo, parent, sha, rust_tests) < 3):
            continue
        out.append({"sha": sha, "parent": parent, "date": date, "subject": subject, "body": body, "crate": crate, "src": src, "tests": tests,
                    "added": sorted(set(g["added"])), "names": names, "churn": churn, "cmds": cmds})
    return out


def mine(repo, *, limit: int = 40, since: str = "2026-06-01") -> list:
    """Candidate tasks from ``repo``'s history (all branches): fix commits that changed source and
    added tests in separate files. Newest commits first, crates spread out, at most ``limit``."""
    repo = str(repo)
    log = _git(repo, "log", "--all", "--no-merges", f"--since={since}", "--format=%H%x09%P%x09%ad%x09%s", "--date=short").splitlines()
    seen, groups = set(), []
    for line in log:
        sha, parents, date, subject = (line.split("	", 3) + ["", "", "", ""])[:4]
        if sha in seen or not parents.split():
            continue
        seen.add(sha)
        if subject.startswith(("index on", "wip:", "Baseline")) or not FIX_WORDS.search(subject) or FEATURE_START.search(subject):
            continue
        body = _git(repo, "show", "-s", "--format=%b", sha).strip()
        groups += _commit_groups(repo, sha, parents.split()[0], subject, date, body)
    groups.sort(key=lambda g: (g["date"], g["sha"]), reverse=True)
    used, per_commit = defaultdict(int), defaultdict(int)
    picked = []
    pool = list(groups)
    while pool and len(picked) < limit:
        # Prefer the crate and the commit used least so far, then the newest.
        g = min(pool, key=lambda g: (per_commit[g["sha"]] >= 3, used[g["crate"]], per_commit[g["sha"]], g["churn"]))
        pool.remove(g)
        used[g["crate"]] += 1
        per_commit[g["sha"]] += 1
        picked.append(g)
    picked.sort(key=lambda g: (g["date"], g["sha"]))
    # A commit message that covers several crates would put other crates' bugs in this task's statement.
    groups_per_commit = defaultdict(int)
    for g in groups:
        groups_per_commit[g["sha"]] += 1
    out = []
    for i, g in enumerate(picked, 1):
        crate = g["crate"]
        out.append(
            {
                "id": f"own-{i:03d}-{crate}-{g['sha'][:7]}",
                "category": "bugfix",
                "source": "git-history",
                "repo": repo,
                "fix_commit": g["sha"],
                "parent": g["parent"],
                "date": g["date"],
                "subject": g["subject"],
                "crate": crate,
                "src_files": g["src"],
                "test_files": g["tests"],
                "added_files": g["added"],
                "tests": g["cmds"],
                "added_tests": g["names"],
                "churn_lines": g["churn"],
                "statement": statement(g["subject"], g["body"] if groups_per_commit[g["sha"]] == 1 else "", crate, g["names"]),
                "statement_weak": not g["names"] and groups_per_commit[g["sha"]] > 1,
                "status": "draft",
                "needs_review": True,
                "validated": None,
            }
        )
    return out


def save_candidates(cands: list, path) -> None:
    write_json(path, {"about": ABOUT, "candidates": cands})


ABOUT = (
    "Draft of 40 own-repo tasks mined from this repository's git history by `marlowe eval own-mine` (M4.6 track L). "
    "Each is a past bug fix put back: the task starts from the fix commit's parent (history dropped, so the fix cannot be read "
    "from `git log`); the fix commit's tests for one crate are hidden and run after the agent finishes. "
    "Matthew has not approved them (design 9.2-h): `needs_review` stays true and `status` stays draft until he edits "
    "statements, drops tasks and sets status to approved. All are bug fixes; the design's 15/15/5/5 split (bugfix, feature, "
    "refactor, ML) cannot come from the history, which is mostly one milestone's work."
)


def load_candidates(path) -> list:
    d = read_json(path)
    return d["candidates"] if isinstance(d, dict) else d


def to_task(c: dict, root: Path, *, timeout: int = 1800) -> Task:
    """The runnable task for a candidate dict: the parent's tree, the fix's tests hidden."""
    return Task(
        id=c["id"],
        tier="own",
        prompt=c["statement"],
        root=root,
        language="rust",
        setup={"kind": "git", "repo": c["repo"], "commit": c["parent"], "parent": c["parent"], "revert": [], "reuse": True},
        hidden_git={"repo": c["repo"], "commit": c["fix_commit"], "paths": list(c["test_files"])},
        tests=c["tests"],
        timeout=timeout,
        test_timeout=1800,
        meta={"category": c.get("category", "bugfix"), "subject": c.get("subject", ""), "status": c.get("status", "draft"),
              "fix_commit": c["fix_commit"], "src_files": list(c["src_files"]), "crate": c.get("crate", "")},
    )


def load_tasks(path, root: Optional[Path] = None, *, approved_only: bool = False, ids: Optional[list] = None) -> list:
    root = root or Path(path).parent
    cands = load_candidates(path)
    if approved_only:
        cands = [c for c in cands if c.get("status") == "approved"]
    if ids:
        cands = [c for c in cands if c["id"] in ids]
    return [to_task(c, root) for c in cands]


def apply_fix(task: Task, ws: Path) -> None:
    """Puts the fix commit's version of the crate's source files into ``ws`` (deleting the ones it removed)."""
    repo, fix = task.hidden_git["repo"], task.meta["fix_commit"]
    for rel in task.meta["src_files"]:
        data = git_bytes(repo, "show", f"{fix}:{rel}")
        target = ws / rel
        if data is None:
            if target.exists():
                target.unlink()
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)


def validate(task: Task, work: Path, ctx: Optional[Context] = None) -> dict:
    """Proof a task is sound and fair.

    * with the bug in place (the parent's tree) the hidden tests must fail;
    * with only this crate's source files taken from the fix they must pass, which shows the fix
      is self-contained: an agent can reach it without the commit's changes to other crates.
    """
    ctx = ctx or Context()
    broken_ws = work / "broken"
    materialize(task, broken_ws, ctx)
    broken = grade(task, broken_ws, ctx)
    fixed_ws = work / "fixed"
    materialize(task, fixed_ws, ctx)
    apply_fix(task, fixed_ws)
    fixed = grade(task, fixed_ws, ctx)
    return {
        "id": task.id,
        "broken_fails": not broken.passed,
        "fixed_passes": fixed.passed,
        "ok": (not broken.passed) and fixed.passed,
        "broken_reason": broken.reason,
        "fixed_reason": fixed.reason,
        "fixed_tail": fixed.output_tail[-600:] if not fixed.passed else "",
    }


def dump_task_json(task: Task, path) -> None:
    """For inspection: the task as the harness will run it."""
    d = task.to_json()
    Path(path).write_text(json.dumps(d, indent=2, sort_keys=True), encoding="utf-8")
