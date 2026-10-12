"""Records the `system/init` event of the isolated Claude Code binary for `parity_inventory`.

Runs `claude -p --output-format stream-json --verbose` exactly as the coding eval's Claude Code arm
does (eval/coding/mcoding/cc.py: scratch config dir, no settings, no MCP, scrubbed environment,
every outbound call through a dead proxy), but against the loopback fake Anthropic server from
eval/coding/tests, so no model runs and nothing leaves the machine. Keeps names only (tools, slash
commands, agents, skills); no prompt or Claude Code text is stored.

    python -I tools/record_cc_init.py crates/marlowe/tests/fixtures/cc_init.json

Re-record when the pinned Claude Code version changes, then map every new name in
crates/marlowe/tests/parity_inventory.rs (design 2.14).
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval" / "coding"))
sys.path.insert(0, str(REPO / "eval" / "coding" / "tests"))
from mcoding import cc  # noqa: E402
from fake_anthropic import FakeAnthropic  # noqa: E402

CREATE_NO_WINDOW = 0x08000000


def main() -> int:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / "crates" / "marlowe" / "tests" / "fixtures" / "cc_init.json"
    claude = cc.find_claude()
    if claude is None:
        print("no claude.exe found (MARLOWE_CC_BIN or the VS Code extension)", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory() as tmp, FakeAnthropic("OK") as srv:
        tmp = Path(tmp)
        ws = tmp / "ws"
        ws.mkdir()
        inv = cc.invocation(claude, ws, "say hi", tmp / "cfg", base_url=srv.url, model="marlowe-dusk:27b-super")
        flags = CREATE_NO_WINDOW if sys.platform == "win32" else 0
        p = subprocess.run(
            inv.argv, input=inv.stdin, env=inv.env, cwd=inv.cwd, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=180, creationflags=flags,
        )
    init = None
    for line in p.stdout.splitlines():
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            init = ev
            break
    if init is None:
        print("no init event; stderr tail:\n" + p.stderr[-2000:], file=sys.stderr)
        return 1
    version = init.get("claude_code_version") or cc.version_of(claude)
    keep = {
        "_about": f"The system/init event of `claude -p --output-format stream-json --verbose` from Claude Code {version} "
        "(VS Code extension binary), run isolated against a loopback fake API: no account, settings, plugins or MCP "
        "servers. Names only, for parity_inventory (design 2.14). Re-record with tools/record_cc_init.py.",
        "claude_code_version": version,
        "tools": init.get("tools", []),
        "slash_commands": init.get("slash_commands", []),
        "terminal_slash_commands": init.get("terminal_slash_commands", []),
        "agents": init.get("agents", []),
        "skills": init.get("skills", []),
    }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(keep, indent=1) + "\n", encoding="utf-8")
    print(f"{out}: Claude Code {version}, {len(keep['tools'])} tools, {len(keep['slash_commands'])} slash commands")
    return 0


if __name__ == "__main__":
    sys.exit(main())
