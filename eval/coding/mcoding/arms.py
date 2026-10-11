"""The arms of the head-to-head and the ablations (design section 6), and how each is built.

| Arm | What it is | Why |
|---|---|---|
| ``marlowe-full`` | ``marlowe -p --standalone --no-memory`` with every tool | the product under test (A1, A3 numerator) |
| ``marlowe-shell`` | the same binary with only the shell group (``MARLOWE_TOOL_GROUPS=shell``) | A3 baseline: no mini-SWE-agent, Marlowe's own ablation |
| ``marlowe-daemon`` | ``marlowe -p`` through a private daemon | A1: the daemon path |
| ``marlowe-memory`` | ``marlowe-full`` with memory on | memory on/off (the edge, not gated) |
| ``cc-local`` | the bundled Claude Code, isolated, on the same local model | the other side of A1 |
| ``noop`` | changes nothing | harness sanity: every task must fail |

Ablations whose switch another track has not shipped are listed in :data:`KNOBS` as not wired;
the harness will not pretend to run them.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from . import cc as ccmod
from .adapters import Adapter, CallableAdapter, ClaudeCodeAdapter, MarloweAdapter
from .util import run_cmd

PROVIDERS = {"ollama", "openrouter", "anthropic", "openai", "local_gguf"}


@dataclass(frozen=True)
class ArmSpec:
    name: str
    kind: str  # marlowe | cc | noop
    note: str
    shell_only: bool = False
    memory: bool = False
    daemon: bool = False


ARMS = {
    a.name: a
    for a in (
        ArmSpec("marlowe-full", "marlowe", "every tool, standalone, memory off"),
        ArmSpec("marlowe-shell", "marlowe", "shell group only (A3 baseline)", shell_only=True),
        ArmSpec("marlowe-daemon", "marlowe", "through a private daemon (A1 daemon path)", daemon=True),
        ArmSpec("marlowe-memory", "marlowe", "memory on", memory=True),
        ArmSpec("cc-local", "cc", "bundled Claude Code, isolated, same local model"),
        ArmSpec("noop", "noop", "does nothing"),
    )
}

#: Ablations by number (design 6) and whether the switch exists.
KNOBS = [
    {"name": "tools: full vs bash-only", "arm": "marlowe-shell", "wired": True, "switch": "MARLOWE_TOOL_GROUPS=shell"},
    {"name": "memory on vs off", "arm": "marlowe-memory", "wired": True, "switch": "--no-memory"},
    {"name": "daemon path", "arm": "marlowe-daemon", "wired": True, "switch": "no --standalone"},
    {"name": "core vs full tool exposure", "arm": None, "wired": False, "switch": "needs a profile switch beyond MARLOWE_TOOL_GROUPS (marlowe-tools exposure)"},
    {"name": "repo map on/off", "arm": None, "wired": False, "switch": "needs a repo-map setting in marlowe-agent context assembly"},
    {"name": "LSP on/off", "arm": None, "wired": False, "switch": "needs an LSP setting in marlowe-exec specs/code"},
    {"name": "rolling vs append-only masking", "arm": None, "wired": False, "switch": "needs a context masking setting in marlowe-agent context"},
    {"name": "edit vs edit_lines", "arm": None, "wired": False, "switch": "decided per model by `marlowe bench --apply` (library edit_formats); no agent-side consumer yet"},
    {"name": "single agent vs build_v2", "arm": None, "wired": False, "switch": "needs a headless playbook flag"},
]


def model_ref(model: str) -> str:
    """``provider/model``; a bare id is an Ollama tag."""
    head = model.split("/", 1)[0]
    return model if "/" in model and head in PROVIDERS else f"ollama/{model}"


def bare_model(model: str) -> str:
    head = model.split("/", 1)[0]
    return model.split("/", 1)[1] if "/" in model and head in PROVIDERS else model


@dataclass
class ArmConfig:
    out_dir: Path
    marlowe: str = ""
    model: str = ccmod.DEFAULT_MODEL
    settings: Optional[Path] = None
    cc_bin: Optional[str] = None
    base_url: str = ccmod.DEFAULT_BASE_URL
    thinking: bool = False
    max_budget_usd: float = 5.0
    env_extra: dict = field(default_factory=dict)


def make_adapter(arm: str, cfg: ArmConfig) -> Adapter:
    if arm not in ARMS:
        raise KeyError(f"unknown arm {arm!r}; arms: {', '.join(sorted(ARMS))}")
    spec = ARMS[arm]
    if spec.kind == "noop":
        return CallableAdapter(None, name=arm)
    if spec.kind == "cc":
        return ClaudeCodeAdapter(
            ccmod.find_claude(cfg.cc_bin),
            config_dir=cfg.out_dir / "cc-config",
            model=bare_model(cfg.model),
            base_url=cfg.base_url,
            thinking=cfg.thinking,
            max_budget_usd=cfg.max_budget_usd,
            name=arm,
        )
    return MarloweAdapter(
        cfg.marlowe,
        name=arm,
        model=model_ref(cfg.model),
        home=cfg.out_dir / "homes" / arm,
        shell_only=spec.shell_only,
        memory=spec.memory,
        daemon=spec.daemon,
        settings=cfg.settings,
        max_budget_usd=cfg.max_budget_usd,
        env_extra=cfg.env_extra,
    )


def seed_marlowe_home(a: MarloweAdapter, *, thinking: bool) -> list:
    """Puts the model in the arm's private library and sets its effort, then stops the helper daemon.

    Returns warnings; none is fatal, a run that cannot see the model fails visibly on its own.
    """
    warnings = []
    a.home.mkdir(parents=True, exist_ok=True)
    env = a.env()
    provider, _, ident = a.model.partition("/")
    steps = [
        [a.exe, "models", "add", provider, ident],
        [a.exe, "models", "slot", "conductor", a.model, "medium" if thinking else "off"],
    ]
    for argv in steps:
        r = run_cmd(argv, env=env, timeout=120)
        if r.code != 0:
            warnings.append(f"{' '.join(argv[1:])}: exit {r.code}: {(r.err or r.out).strip()[:200]}")
    run_cmd([a.exe, "shutdown"], env=env, timeout=60)
    return warnings
