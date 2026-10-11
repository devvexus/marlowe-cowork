"""Claude Code as the other side of the head-to-head, isolated and pointed at the local model.

Both harnesses use the same model (``marlowe-dusk:27b-super`` on Ollama) so the comparison is the
harness alone, and it costs nothing. Claude Code is the ``claude.exe`` bundled in Matthew's VS Code
extension. Everything that could make it behave as *his* Claude Code, or spend money, is cut:

* ``CLAUDE_CONFIG_DIR`` is a scratch folder (no settings, memory, plugins, credentials, history);
  ``--setting-sources ""`` and ``--strict-mcp-config`` load no settings files and no MCP servers;
* ``ANTHROPIC_BASE_URL`` is Ollama's Anthropic-compatible endpoint on loopback (anything else is
  refused), the token is the literal ``ollama``, and every model alias maps to the one local model;
* the environment is scrubbed of keys and tokens, and ``HTTP(S)_PROXY`` points at a dead port with
  loopback exempt, so even a stray call to the internet fails closed;
* telemetry, updates, error reporting, auto-memory and prompt suggestions are off.
"""

from __future__ import annotations

import json
import os
import re
import urllib.parse
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Optional

from .util import scrubbed_env

DEFAULT_BASE_URL = "http://127.0.0.1:11434"
DEFAULT_MODEL = "marlowe-dusk:27b-super"
LOOPBACK = {"127.0.0.1", "localhost", "::1", "[::1]"}
#: where a stray outbound call goes to die
DEAD_PROXY = "http://127.0.0.1:9"
PIN_FILE = "cc-version.txt"


@dataclass
class Invocation:
    """One process launch, fully described so a dry run can show it."""

    argv: list
    env: dict
    cwd: str
    stdin: Optional[str] = None
    label: str = ""


def _version_key(name: str) -> tuple:
    m = re.search(r"claude-code-(\d+(?:\.\d+)*)", name)
    return tuple(int(x) for x in m.group(1).split(".")) if m else (0,)


def find_claude(explicit: Optional[str] = None, userprofile: Optional[str] = None) -> Optional[Path]:
    """The ``claude`` binary: the override, else the newest in VS Code's extensions folder."""
    for cand in (explicit, os.environ.get("MARLOWE_CC_BIN")):
        if cand:
            p = Path(cand)
            return p if p.is_file() else None
    home = Path(userprofile or os.environ.get("USERPROFILE") or os.path.expanduser("~"))
    exts = home / ".vscode" / "extensions"
    if not exts.is_dir():
        return None
    best = None
    for d in sorted(exts.glob("anthropic.claude-code-*"), key=lambda d: _version_key(d.name), reverse=True):
        for exe in ("claude.exe", "claude"):
            p = d / "resources" / "native-binary" / exe
            if p.is_file():
                best = p
                break
        if best:
            break
    return best


def version_of(claude: Path) -> str:
    """The extension folder's version (``2.1.296``), read from the path, without running anything."""
    m = re.search(r"claude-code-(\d+(?:\.\d+)*)", str(claude))
    return m.group(1) if m else "unknown"


def assert_loopback(url: str) -> None:
    host = urllib.parse.urlparse(url).hostname or ""
    if host not in LOOPBACK:
        raise ValueError(f"refusing {url!r}: the head-to-head runs on the local model only (loopback), never an API")


def settings_json(thinking: bool) -> str:
    """Inline ``--settings``: the knobs that change behaviour between runs, set explicitly."""
    return json.dumps(
        {
            "autoMemoryEnabled": False,
            "promptSuggestionEnabled": False,
            "alwaysThinkingEnabled": bool(thinking),
            "attribution": {"commit": "", "pr": ""},
            "skipDangerousModePermissionPrompt": True,
            "cleanupPeriodDays": 1,
        },
        separators=(",", ":"),
    )


def git_bash() -> Optional[str]:
    """Claude Code on Windows wants to be told where Git Bash is."""
    for p in (os.environ.get("CLAUDE_CODE_GIT_BASH_PATH"), r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files (x86)\Git\bin\bash.exe"):
        if p and Path(p).is_file():
            return p
    return None


def isolated_env(
    config_dir: Path,
    *,
    base_url: str = DEFAULT_BASE_URL,
    model: str = DEFAULT_MODEL,
    thinking: bool = False,
    base: Optional[Mapping[str, str]] = None,
    timeout_ms: int = 3_000_000,
) -> dict:
    assert_loopback(base_url)
    env = scrubbed_env(base)
    env.update(
        {
            "CLAUDE_CONFIG_DIR": str(config_dir),
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_AUTH_TOKEN": "ollama",
            "ANTHROPIC_API_KEY": "",
            "ANTHROPIC_MODEL": model,
            "ANTHROPIC_SMALL_FAST_MODEL": model,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
            "CLAUDE_CODE_SUBAGENT_MODEL": model,
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
            "DISABLE_TELEMETRY": "1",
            "DISABLE_ERROR_REPORTING": "1",
            "DISABLE_AUTOUPDATER": "1",
            "DISABLE_BUG_COMMAND": "1",
            "API_TIMEOUT_MS": str(timeout_ms),
            "HTTP_PROXY": DEAD_PROXY,
            "HTTPS_PROXY": DEAD_PROXY,
            "NO_PROXY": "127.0.0.1,localhost,::1",
        }
    )
    if not thinking:
        env["MAX_THINKING_TOKENS"] = "0"
    gb = git_bash()
    if gb:
        env["CLAUDE_CODE_GIT_BASH_PATH"] = gb
    return env


def build_argv(
    claude: Path,
    *,
    model: str = DEFAULT_MODEL,
    max_budget_usd: float = 5.0,
    thinking: bool = False,
    effort: Optional[str] = None,
    continue_session: bool = False,
    stream_input: bool = False,
    permission_prompt_stdio: bool = False,
) -> list:
    """``claude -p`` flags. The prompt goes on stdin (long prompts, and nothing shows in a process list)."""
    argv = [
        str(claude),
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--settings",
        settings_json(thinking),
        "--model",
        model,
        "--max-budget-usd",
        f"{max_budget_usd:g}",
    ]
    if effort:
        argv += ["--effort", effort]
    if continue_session:
        argv.append("--continue")
    if stream_input:
        argv += ["--input-format", "stream-json"]
    if permission_prompt_stdio:
        argv += ["--permission-prompt-tool", "stdio"]
    return argv


def invocation(
    claude: Path,
    workspace: Path,
    prompt: Optional[str],
    config_dir: Path,
    *,
    base_url: str = DEFAULT_BASE_URL,
    model: str = DEFAULT_MODEL,
    thinking: bool = False,
    max_budget_usd: float = 5.0,
    effort: Optional[str] = None,
    continue_session: bool = False,
    base_env: Optional[Mapping[str, str]] = None,
    stream_input: bool = False,
    permission_prompt_stdio: bool = False,
) -> Invocation:
    return Invocation(
        argv=build_argv(
            claude,
            model=model,
            max_budget_usd=max_budget_usd,
            thinking=thinking,
            effort=effort,
            continue_session=continue_session,
            stream_input=stream_input,
            permission_prompt_stdio=permission_prompt_stdio,
        ),
        env=isolated_env(config_dir, base_url=base_url, model=model, thinking=thinking, base=base_env),
        cwd=str(workspace),
        stdin=prompt,
        label="claude -p (isolated, local model)",
    )


_REDACT = {"ANTHROPIC_API_KEY"}
_SHOWN_ENV = (
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "DISABLE_TELEMETRY",
    "DISABLE_ERROR_REPORTING",
    "DISABLE_AUTOUPDATER",
    "MAX_THINKING_TOKENS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "CLAUDE_CODE_GIT_BASH_PATH",
)


def describe(inv: Invocation) -> str:
    """What the dry run prints: the command, and the environment that matters (tokens shown as set/unset)."""
    lines = [f"# {inv.label}", f"cwd: {inv.cwd}", "argv:"]
    lines += [f"  {a!r}" for a in inv.argv]
    lines.append("env (isolation-relevant):")
    for k in _SHOWN_ENV:
        if k in inv.env:
            v = inv.env[k]
            shown = ("<set>" if v else "<empty>") if k in _REDACT else v
            lines.append(f"  {k}={shown}")
    lines.append(f"stdin: {'<prompt, ' + str(len(inv.stdin)) + ' chars>' if inv.stdin else '<none>'}")
    return "\n".join(lines)


def isolation_problems(inv: Invocation, real_claude_dir: Optional[Path] = None) -> list:
    """Everything wrong with the isolation of ``inv``; an empty list means the checks hold."""
    probs = []
    env = inv.env
    real = (real_claude_dir or (Path(os.environ.get("USERPROFILE") or os.path.expanduser("~")) / ".claude")).resolve()
    cfg = env.get("CLAUDE_CONFIG_DIR", "")
    if not cfg:
        probs.append("CLAUDE_CONFIG_DIR is not set, so Claude Code would read the real ~/.claude")
    else:
        try:
            p = Path(cfg).resolve()
            if p == real or real in p.parents:
                probs.append(f"CLAUDE_CONFIG_DIR {cfg} is inside the real config folder")
        except OSError:
            pass
    try:
        assert_loopback(env.get("ANTHROPIC_BASE_URL", ""))
    except ValueError as e:
        probs.append(str(e))
    if env.get("ANTHROPIC_API_KEY"):
        probs.append("ANTHROPIC_API_KEY is set")
    if env.get("ANTHROPIC_AUTH_TOKEN") != "ollama":
        probs.append("ANTHROPIC_AUTH_TOKEN is not the placeholder 'ollama'")
    for k in env:
        if k.startswith(("OPENAI_", "OPENROUTER_", "AWS_", "GOOGLE_", "AZURE_", "GEMINI_", "HF_", "GITHUB_", "GH_")):
            probs.append(f"{k} leaked into the environment")
    for must in ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "DISABLE_TELEMETRY", "DISABLE_AUTOUPDATER", "CLAUDE_CODE_DISABLE_AUTO_MEMORY"):
        if env.get(must) != "1":
            probs.append(f"{must} is not 1")
    if env.get("HTTPS_PROXY") != DEAD_PROXY or "127.0.0.1" not in env.get("NO_PROXY", ""):
        probs.append("outbound traffic is not forced through the dead proxy")
    a = inv.argv
    if "--setting-sources" not in a or a[a.index("--setting-sources") + 1] != "":
        probs.append('--setting-sources "" is missing')
    if "--strict-mcp-config" not in a:
        probs.append("--strict-mcp-config is missing")
    if "bypassPermissions" not in a:
        probs.append("--permission-mode bypassPermissions is missing")
    if "--settings" not in a:
        probs.append("--settings (explicit memory/thinking switches) is missing")
    for key in ("ANTHROPIC_MODEL", "ANTHROPIC_SMALL_FAST_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"):
        if env.get(key) != env.get("ANTHROPIC_MODEL"):
            probs.append(f"{key} differs from the one local model")
    try:
        cwd = Path(inv.cwd).resolve()
        if cwd == real or real in cwd.parents:
            probs.append("the working folder is inside the real config folder")
    except OSError:
        pass
    return probs
