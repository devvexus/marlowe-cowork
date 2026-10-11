"""Running one agent on one workspace: Marlowe, Claude Code, or a stand-in for tests.

Every adapter takes the prompt on stdin, runs ``-p`` with ``--output-format stream-json`` in the
workspace under a deadline, keeps the transcript, and returns an :class:`AgentResult` read from the
stream. The arms differ in flags and environment only; nothing about how a run is measured does.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path
from typing import Callable, Optional

from . import cc as ccmod
from . import stream
from .model import AgentResult
from .util import kill_tree, popen, read_json, scrubbed_env, tail

#: The shell-only arm's tools (Marlowe's ``shell`` group), normalised: lower case, no underscores.
SHELL_TOOLS = {"bash", "powershell", "python", "joboutput", "jobwatch", "jobkill"}


def norm(name: str) -> str:
    return name.lower().replace("_", "")


class Adapter:
    """What the runner needs from an agent harness."""

    name = "adapter"
    #: Marlowe-only features (rewind) are skipped on harnesses that lack them
    supports_rewind = False

    def invocation(self, prompt: str, ws: Path, *, run_dir: Path, tag: str, continue_session: bool = False) -> ccmod.Invocation:
        raise NotImplementedError

    def allowed_tools(self) -> Optional[set]:
        return None

    def session_invocation(self, ws: Path, *, run_dir: Path, tag: str, continue_session: bool = False) -> ccmod.Invocation:
        """A process that takes stream-json on stdin (the interactive suite); no prompt argument."""
        raise NotImplementedError(f"{self.name} has no interactive mode")

    def preflight(self) -> list:
        """Reasons this arm cannot run here (empty when it can)."""
        return []

    def after_run(self, inv: ccmod.Invocation, res: AgentResult, run_dir: Path, tag: str) -> None:
        return None

    def run(self, prompt: str, ws: Path, *, timeout: float, run_dir: Path, tag: str = "a1", continue_session: bool = False) -> AgentResult:
        run_dir.mkdir(parents=True, exist_ok=True)
        inv = self.invocation(prompt, ws, run_dir=run_dir, tag=tag, continue_session=continue_session)
        started = time.monotonic()
        res = AgentResult()
        try:
            p = popen(inv.argv, cwd=inv.cwd, env=inv.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        except OSError as e:
            res.error = f"cannot start {inv.argv[0]!r}: {e}"
            res.exit_code = 127
            return res
        with p:
            try:
                out, err = p.communicate(inv.stdin or "", timeout=timeout)
            except subprocess.TimeoutExpired:
                kill_tree(p.pid)
                try:
                    out, err = p.communicate(timeout=20)
                except subprocess.TimeoutExpired:
                    out, err = "", ""
                res.timed_out = True
            code = p.returncode
        elapsed = time.monotonic() - started
        transcript = run_dir / f"transcript-{tag}.jsonl"
        transcript.write_text(out or "", encoding="utf-8")
        (run_dir / f"stderr-{tag}.txt").write_text(err or "", encoding="utf-8")
        events = stream.parse_lines(out or "")
        parsed = stream.summarize(events, allowed_tools=self.allowed_tools())
        parsed.timed_out = res.timed_out
        parsed.seconds = elapsed
        parsed.exit_code = code if code is not None else -9
        parsed.transcript = str(transcript)
        parsed.stderr_tail = tail(err or "", 800)
        if not events and not parsed.timed_out:
            parsed.error = "no stream-json output: " + tail((err or out or "").strip(), 300)
        self.after_run(inv, parsed, run_dir, tag)
        return parsed


class MarloweAdapter(Adapter):
    """``marlowe -p`` in one of its arms (see ``arms.py``)."""

    supports_rewind = True

    def __init__(
        self,
        exe: str,
        *,
        name: str,
        model: str,
        home: Path,
        shell_only: bool = False,
        memory: bool = False,
        daemon: bool = False,
        settings: Optional[Path] = None,
        max_budget_usd: float = 5.0,
        env_extra: Optional[dict] = None,
        extra_args: Optional[list] = None,
        prefix: Optional[list] = None,
    ) -> None:
        self.exe = str(exe)
        #: arguments between the binary and its flags (a test runs a script as ``python script.py``)
        self.prefix = list(prefix or [])
        self.name = name
        self.model = model
        self.home = Path(home)
        self.shell_only = shell_only
        self.memory = memory
        self.daemon = daemon
        self.settings = settings
        self.max_budget_usd = max_budget_usd
        self.env_extra = dict(env_extra or {})
        self.extra_args = list(extra_args or [])

    def allowed_tools(self) -> Optional[set]:
        return set(SHELL_TOOLS) if self.shell_only else None

    def env(self, stats_file: Optional[Path] = None) -> dict:
        env = scrubbed_env()
        env["MARLOWE_HOME"] = str(self.home)
        if self.shell_only:
            env["MARLOWE_TOOL_GROUPS"] = "shell"
        if stats_file is not None:
            env["MARLOWE_EVAL_STATS"] = str(stats_file)
        env.update(self.env_extra)
        return env

    def flags(self, ws: Path, *, continue_session: bool = False, stream_input: bool = False) -> list:
        argv = [self.exe, *self.prefix, "-p"]
        if not self.daemon:
            argv.append("--standalone")
            if not self.memory:
                argv.append("--no-memory")
        argv += ["--output-format", "stream-json", "--permission-mode", "bypass", "--workspace", str(ws), "--model", self.model]
        argv += ["--max-budget-usd", f"{self.max_budget_usd:g}"]
        if self.settings:
            argv += ["--settings", str(self.settings)]
        if stream_input:
            argv += ["--input-format", "stream-json", "--permission-prompt-tool", "stdio"]
        if continue_session:
            argv.append("-c")
        return argv + self.extra_args

    def invocation(self, prompt, ws, *, run_dir, tag, continue_session=False):
        stats = run_dir / f"repairs-{tag}.json"
        return ccmod.Invocation(
            argv=self.flags(ws, continue_session=continue_session),
            env=self.env(stats),
            cwd=str(ws),
            stdin=prompt,
            label=f"marlowe -p ({self.name})",
        )

    def session_invocation(self, ws, *, run_dir, tag, continue_session=False):
        run_dir.mkdir(parents=True, exist_ok=True)
        return ccmod.Invocation(
            argv=self.flags(ws, continue_session=continue_session, stream_input=True),
            env=self.env(run_dir / f"repairs-{tag}.json"),
            cwd=str(ws),
            stdin=None,
            label=f"marlowe -p stream-json ({self.name})",
        )

    def after_run(self, inv, res, run_dir, tag):
        stats = run_dir / f"repairs-{tag}.json"
        if stats.is_file():
            try:
                d = read_json(stats)
                res.repairs = int(d.get("repaired", 0)) + int(d.get("from_text", 0))
            except (ValueError, OSError):
                pass

    def preflight(self) -> list:
        probs = []
        if not Path(self.exe).is_file():
            probs.append(f"marlowe binary not found: {self.exe} (set MARLOWE_BIN or pass --marlowe)")
            return probs
        if self.shell_only:
            # The arm is only an ablation if the tool list really is just the shell.
            from .util import run_cmd

            r = run_cmd([self.exe, "eval", "tools"], env=self.env(), timeout=60)
            names = {norm(n) for n in r.out.split() if n.strip()}
            if r.code != 0 or not names:
                probs.append(f"`marlowe eval tools` failed under MARLOWE_TOOL_GROUPS=shell: {tail(r.err, 200)}")
            elif not names <= SHELL_TOOLS:
                probs.append(f"the shell-only arm still exposes {sorted(names - SHELL_TOOLS)}")
        return probs


class ClaudeCodeAdapter(Adapter):
    """The Claude Code bundled in the VS Code extension, isolated, on the same local model."""

    name = "cc-local"

    def __init__(
        self,
        claude: Optional[Path],
        *,
        config_dir: Path,
        model: str = ccmod.DEFAULT_MODEL,
        base_url: str = ccmod.DEFAULT_BASE_URL,
        thinking: bool = False,
        effort: Optional[str] = None,
        max_budget_usd: float = 5.0,
        name: str = "cc-local",
        prefix: Optional[list] = None,
    ) -> None:
        ccmod.assert_loopback(base_url)
        self.prefix = list(prefix or [])
        self.claude = claude
        self.config_dir = Path(config_dir)
        self.model = model
        self.base_url = base_url
        self.thinking = thinking
        self.effort = effort
        self.max_budget_usd = max_budget_usd
        self.name = name

    def invocation(self, prompt, ws, *, run_dir, tag, continue_session=False):
        self.config_dir.mkdir(parents=True, exist_ok=True)
        inv = ccmod.invocation(
            self.claude or Path("claude"),
            ws,
            prompt,
            self.config_dir,
            base_url=self.base_url,
            model=self.model,
            thinking=self.thinking,
            max_budget_usd=self.max_budget_usd,
            effort=self.effort,
            continue_session=continue_session,
        )
        inv.argv[1:1] = self.prefix
        return inv

    def session_invocation(self, ws, *, run_dir, tag, continue_session=False):
        self.config_dir.mkdir(parents=True, exist_ok=True)
        inv = ccmod.invocation(
            self.claude or Path("claude"),
            ws,
            None,
            self.config_dir,
            base_url=self.base_url,
            model=self.model,
            thinking=self.thinking,
            max_budget_usd=self.max_budget_usd,
            effort=self.effort,
            continue_session=continue_session,
            stream_input=True,
            permission_prompt_stdio=True,
        )
        inv.argv[1:1] = self.prefix
        return inv

    def preflight(self) -> list:
        if self.claude is None or not Path(self.claude).is_file():
            return ["no claude binary found (looked for the newest %USERPROFILE%\\.vscode\\extensions\\anthropic.claude-code-*; set MARLOWE_CC_BIN)"]
        inv = self.invocation("x", Path("."), run_dir=Path("."), tag="pf")
        return ccmod.isolation_problems(inv)


class CallableAdapter(Adapter):
    """A stand-in agent for tests and for ``--arm noop``: ``fn(ws, prompt)`` edits files directly."""

    def __init__(self, fn: Optional[Callable[[Path, str], None]] = None, *, name: str = "noop") -> None:
        self.fn = fn
        self.name = name
        self.calls = []

    def run(self, prompt, ws, *, timeout, run_dir, tag="a1", continue_session=False):
        started = time.monotonic()
        self.calls.append((prompt, continue_session))
        res = AgentResult()
        try:
            if self.fn:
                self.fn(ws, prompt)
        except Exception as e:  # a stand-in that fails is a failed run, not a crashed harness
            res.error = f"{type(e).__name__}: {e}"
            res.is_error = True
        res.seconds = time.monotonic() - started
        res.steps = 1
        return res


def stream_events(path) -> list:
    return stream.parse_lines(Path(path).read_text(encoding="utf-8", errors="replace"))

