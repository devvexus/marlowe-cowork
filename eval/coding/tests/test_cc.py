"""The Claude Code adapter: finding the binary, the isolation flags and environment, a dry run,
and (when the binary is on this machine) a real run against a fake Anthropic server on loopback.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from mcoding import cc, cli
from mcoding.adapters import ClaudeCodeAdapter
from mcoding.util import scrubbed_env

from fake_anthropic import FakeAnthropic
from helpers import fake_cc


def extension(tmp: Path, versions: list) -> Path:
    for v in versions:
        d = tmp / ".vscode" / "extensions" / f"anthropic.claude-code-{v}-win32-x64" / "resources" / "native-binary"
        d.mkdir(parents=True)
        (d / "claude.exe").write_bytes(b"")
    return tmp


def test_the_newest_extension_wins_by_version_not_by_name(tmp_path):
    home = extension(tmp_path, ["2.1.9", "2.1.296", "2.0.1000", "1.9.9"])
    found = cc.find_claude(userprofile=str(home))
    assert "claude-code-2.1.296-" in str(found)
    assert cc.version_of(found) == "2.1.296"
    assert cc.find_claude(userprofile=str(tmp_path / "nowhere")) is None


def test_an_explicit_path_or_environment_override_beats_the_search(tmp_path, monkeypatch):
    home = extension(tmp_path / "h", ["2.1.296"])
    mine = tmp_path / "my-claude.exe"
    mine.write_bytes(b"")
    assert cc.find_claude(str(mine), userprofile=str(home)) == mine
    monkeypatch.setenv("MARLOWE_CC_BIN", str(mine))
    assert cc.find_claude(userprofile=str(home)) == mine
    assert cc.find_claude(str(tmp_path / "missing.exe"), userprofile=str(home)) is None


def test_only_a_loopback_endpoint_is_accepted():
    for ok in ("http://127.0.0.1:11434", "http://localhost:11434", "http://[::1]:11434"):
        cc.assert_loopback(ok)
    for bad in ("https://api.anthropic.com", "http://192.168.1.5:11434", "http://example.com", ""):
        with pytest.raises(ValueError):
            cc.assert_loopback(bad)
    with pytest.raises(ValueError):
        ClaudeCodeAdapter(Path("claude"), config_dir=Path("x"), base_url="https://api.anthropic.com")


def test_the_isolated_environment_has_no_credentials_and_points_everything_at_the_local_model(tmp_path):
    base = {"PATH": os.environ.get("PATH", ""), "ANTHROPIC_API_KEY": "sk-real", "OPENROUTER_API_KEY": "or-real", "HF_TOKEN": "hf", "GH_TOKEN": "gh", "CLAUDE_CODE_OAUTH_TOKEN": "oauth", "AWS_SECRET_ACCESS_KEY": "aws", "KEEP_ME": "1"}
    env = cc.isolated_env(tmp_path / "cfg", base=base, model="m:1")
    assert env["ANTHROPIC_API_KEY"] == "" and env["ANTHROPIC_AUTH_TOKEN"] == "ollama"
    for leaked in ("OPENROUTER_API_KEY", "HF_TOKEN", "GH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "AWS_SECRET_ACCESS_KEY"):
        assert leaked not in env
    assert env["KEEP_ME"] == "1" and env["PATH"]
    assert env["CLAUDE_CONFIG_DIR"] == str(tmp_path / "cfg")
    models = {env[k] for k in ("ANTHROPIC_MODEL", "ANTHROPIC_SMALL_FAST_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL")}
    assert models == {"m:1"}, "every alias, the small-fast model and subagents all use the one local model"
    assert env["HTTPS_PROXY"] == cc.DEAD_PROXY and "127.0.0.1" in env["NO_PROXY"]
    assert env["MAX_THINKING_TOKENS"] == "0"
    assert "MAX_THINKING_TOKENS" not in cc.isolated_env(tmp_path / "cfg", base=base, thinking=True)


def test_the_command_carries_every_isolation_flag(tmp_path):
    inv = cc.invocation(Path("claude.exe"), tmp_path, "do it", tmp_path / "cfg")
    a = inv.argv
    assert a[:2] == ["claude.exe", "-p"]
    assert a[a.index("--output-format") + 1] == "stream-json" and "--verbose" in a
    assert a[a.index("--permission-mode") + 1] == "bypassPermissions"
    assert a[a.index("--setting-sources") + 1] == "", "no settings files at all"
    assert "--strict-mcp-config" in a
    settings = json.loads(a[a.index("--settings") + 1])
    assert settings["autoMemoryEnabled"] is False and settings["alwaysThinkingEnabled"] is False
    assert a[a.index("--max-budget-usd") + 1] == "5" and a[a.index("--model") + 1] == cc.DEFAULT_MODEL
    assert inv.stdin == "do it", "the prompt goes on stdin, not the command line"
    assert cc.isolation_problems(inv) == []


def test_isolation_problems_name_each_way_the_setup_could_leak(tmp_path):
    inv = cc.invocation(Path("claude.exe"), tmp_path, "x", tmp_path / "cfg")
    real = Path(os.environ.get("USERPROFILE") or os.path.expanduser("~")) / ".claude"

    def problems(mutate) -> list:
        i = cc.Invocation(list(inv.argv), dict(inv.env), inv.cwd, inv.stdin)
        mutate(i)
        return cc.isolation_problems(i)

    assert any("real config folder" in p for p in problems(lambda i: i.env.update(CLAUDE_CONFIG_DIR=str(real))))
    assert any("not set" in p for p in problems(lambda i: i.env.pop("CLAUDE_CONFIG_DIR")))
    assert any("loopback" in p for p in problems(lambda i: i.env.update(ANTHROPIC_BASE_URL="https://api.anthropic.com")))
    assert any("ANTHROPIC_API_KEY" in p for p in problems(lambda i: i.env.update(ANTHROPIC_API_KEY="sk-x")))
    assert any("OPENAI_API_KEY" in p for p in problems(lambda i: i.env.update(OPENAI_API_KEY="x")))
    assert any("TELEMETRY" in p for p in problems(lambda i: i.env.update(DISABLE_TELEMETRY="0")))
    assert any("proxy" in p for p in problems(lambda i: i.env.pop("HTTPS_PROXY")))
    assert any("--strict-mcp-config" in p for p in problems(lambda i: i.argv.remove("--strict-mcp-config")))
    assert any("setting-sources" in p for p in problems(lambda i: (i.argv.pop(i.argv.index("--setting-sources") + 1), i.argv.insert(i.argv.index("--setting-sources") + 1, "user"))))
    assert any("ANTHROPIC_DEFAULT_HAIKU_MODEL" in p for p in problems(lambda i: i.env.update(ANTHROPIC_DEFAULT_HAIKU_MODEL="claude-haiku")))


def test_the_dry_run_prints_the_flags_and_exits_clean(capsys, tmp_path):
    rc = cli.main(["cc-dry-run", "--config-dir", str(tmp_path / "cfg"), "--workspace", str(tmp_path)])
    out = capsys.readouterr().out
    assert rc == 0, out
    for want in ("--setting-sources", "--strict-mcp-config", "bypassPermissions", "--settings", "CLAUDE_CONFIG_DIR=", "ANTHROPIC_BASE_URL=http://127.0.0.1:11434", "ANTHROPIC_AUTH_TOKEN=ollama", "isolation: all checks hold"):
        assert want in out, f"{want} missing from:\n{out}"
    assert "sk-" not in out


def test_the_dry_run_refuses_a_remote_endpoint(capsys):
    with pytest.raises(ValueError):
        cli.main(["cc-dry-run", "--base-url", "https://api.anthropic.com"])


def test_the_shared_scrub_drops_secrets_but_keeps_ordinary_variables():
    env = scrubbed_env({"PATH": "p", "HOME": "h", "MY_API_KEY": "k", "SOME_PASSWORD": "p", "GITHUB_TOKEN": "t", "CLAUDE_CONFIG_DIR": "c", "LANG": "en"})
    assert env == {"PATH": "p", "HOME": "h", "LANG": "en"}


def test_a_stand_in_binary_receives_the_isolated_command_and_environment(tmp_path):
    ws = tmp_path / "ws"
    ws.mkdir()
    a, record = fake_cc(tmp_path, [{"tools": ["Read", "Edit"], "writes": {"x.txt": "hi"}}])
    res = a.run("fix it", ws, timeout=60, run_dir=tmp_path / "run")
    assert res.tool_calls == 2 and res.steps == 3 and res.input_tokens == 100
    row = json.loads(record.read_text(encoding="utf-8").splitlines()[0])
    assert row["stdin"] == "fix it"
    assert row["env"]["ANTHROPIC_BASE_URL"] == "http://127.0.0.1:11434"
    assert row["env"]["CLAUDE_CONFIG_DIR"] == str(tmp_path / "cc-config")
    assert row["env"]["ANTHROPIC_API_KEY"] == "" and row["env"]["OPENROUTER_API_KEY"] is None
    assert (ws / "x.txt").read_text() == "hi"


def test_the_preflight_of_an_arm_without_a_binary_says_where_it_looked(tmp_path):
    a = ClaudeCodeAdapter(None, config_dir=tmp_path / "cfg")
    probs = a.preflight()
    assert probs and "anthropic.claude-code" in probs[0]


REAL = cc.find_claude()


@pytest.mark.skipif(REAL is None, reason="Claude Code is not installed in VS Code on this machine")
def test_the_real_binary_runs_isolated_against_a_fake_server_and_touches_nothing_else(tmp_path):
    """Claude Code itself, no model: its only API peer is the fake server on loopback, with the
    placeholder credential; its config lives in the scratch folder."""
    ws = tmp_path / "ws"
    ws.mkdir()
    with FakeAnthropic("FAKE-REPLY-123") as srv:
        a = ClaudeCodeAdapter(REAL, config_dir=tmp_path / "cfg", base_url=srv.url, model="marlowe-dusk:27b-super")
        assert a.preflight() == []
        res = a.run("say hello", ws, timeout=180, run_dir=tmp_path / "run")
        requests = list(srv.requests)
    assert res.final_text == "FAKE-REPLY-123", (res.error, res.stderr_tail, res.final_text)
    assert res.exit_code == 0 and not res.is_error
    model_calls = [r for r in requests if r["path"].split("?")[0].endswith("/v1/messages")]
    assert model_calls, requests
    assert {r["model"] for r in model_calls} <= {"marlowe-dusk:27b-super"}, "every call, including helpers, names the one local model"
    for r in requests:
        creds = {r.get("x-api-key"), (r.get("authorization") or "").replace("Bearer ", "")} - {None, ""}
        assert creds <= {"ollama"}, f"a credential other than the placeholder reached the server: {r}"
    assert any((tmp_path / "cfg").iterdir()), "its state (sessions, backups) went to the scratch config folder"
