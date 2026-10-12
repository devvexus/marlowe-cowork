"""The polyglot tier: Aider's polyglot benchmark exercises, two attempts each (design section 6).

Point ``--polyglot-dir`` at a checkout of ``Aider-AI/polyglot-benchmark`` (a folder with
``<language>/exercises/practice/<exercise>/``); this machine fetches nothing by itself. Each
exercise has ``.docs/instructions.md`` and ``.meta/config.json`` naming the solution and test
files. The agent gets the instructions and the stub files (never ``.meta``, where the reference
solutions live). Its tests are put back from the pristine copy before they run, so editing a test
cannot pass a task. Only languages whose toolchain is installed are scored.
"""

from __future__ import annotations

import json
import random
from pathlib import Path
from typing import Iterable, Optional

from . import toolchains
from .model import Task
from .util import read_text

LANGUAGES = ("python", "rust", "javascript", "go", "cpp", "java")

PROMPT = """# Instructions

{instructions}

####

Use the above instructions to modify the supplied files: {files}
Don't change the names of existing functions or classes, as they may be referenced from other code like unit tests, etc.
Only use standard libraries, don't suggest installing any packages.
The tests for this exercise are in {tests}; do not edit them. You can run them to check your work.
"""

RETRY = """```
{errors}
```

See the testing errors above.
The tests are correct, don't try and change them.
Fix the code in {files} to resolve the errors.
"""


def load_exercise(path: Path, language: str) -> Optional[Task]:
    cfg_path = path / ".meta" / "config.json"
    if not cfg_path.is_file():
        return None
    cfg = json.loads(read_text(cfg_path))
    files = cfg.get("files", {})
    solution, tests = list(files.get("solution", [])), list(files.get("test", []))
    docs = path / ".docs"
    instructions = read_text(docs / "instructions.md") if (docs / "instructions.md").is_file() else ""
    append = docs / "instructions.append.md"
    if append.is_file():
        instructions += "\n\n" + read_text(append)
    cmds = toolchains.COMMANDS.get(language, {"tests": [], "prepare": []})
    meta = {"retry_template": RETRY, "language": language}
    tc = toolchains.detect(language)
    if not tc.available:
        meta["skip"] = f"{language} toolchain unavailable: {tc.detail}"
    return Task(
        id=f"polyglot-{language}-{path.name}",
        tier="polyglot",
        prompt=PROMPT.format(instructions=instructions.strip(), files=", ".join(solution), tests=", ".join(tests)),
        root=path,
        language=language,
        setup={"kind": "dir", "path": ".", "exclude": [".meta", ".docs", "task.json"]},
        hidden={t: t for t in tests},
        tests=cmds["tests"],
        prepare=cmds["prepare"],
        timeout=1200,
        test_timeout=600,
        attempts=2,
        files=solution,
        meta=meta,
    )


def discover(root, languages: Optional[Iterable[str]] = None) -> list:
    """Every exercise under ``root`` for ``languages``, sorted by id (an unreadable one is skipped)."""
    root = Path(root)
    wanted = list(languages) if languages else list(LANGUAGES)
    out = []
    for lang in wanted:
        base = root / lang / "exercises" / "practice"
        if not base.is_dir():
            continue
        for ex in sorted(p for p in base.iterdir() if p.is_dir()):
            try:
                t = load_exercise(ex, lang)
            except (ValueError, OSError):
                t = None
            if t is not None:
                out.append(t)
    return sorted(out, key=lambda t: t.id)


def sample(tasks: list, n: int, seed: int = 0) -> list:
    """``n`` tasks, spread over the languages that can run, the same for the same seed.

    Tasks whose toolchain is missing are kept at the end of the list (they are reported as not
    scored) only when asked for all of them; a sample draws from what can actually run.
    """
    runnable = [t for t in tasks if not t.meta.get("skip")]
    if n <= 0 or n >= len(runnable):
        return sorted(tasks, key=lambda t: t.id) if n <= 0 else sorted(runnable, key=lambda t: t.id)
    rng = random.Random(seed)
    by_lang: dict = {}
    for t in sorted(runnable, key=lambda t: t.id):
        by_lang.setdefault(t.language, []).append(t)
    for ts in by_lang.values():
        rng.shuffle(ts)
    out = []
    langs = sorted(by_lang)
    i = 0
    while len(out) < n and any(by_lang.values()):
        lang = langs[i % len(langs)]
        i += 1
        if by_lang[lang]:
            out.append(by_lang[lang].pop())
    return sorted(out, key=lambda t: t.id)
