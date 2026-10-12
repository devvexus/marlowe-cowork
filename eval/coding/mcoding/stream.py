"""Reading ``--output-format stream-json`` from either harness into one :class:`AgentResult`.

Marlowe's headless mode speaks Claude Code's SDK shapes (design 2.7), so one reader serves both:
``assistant`` lines carry ``tool_use`` blocks, ``user`` lines carry ``tool_result`` blocks (with
``is_error``), and the closing ``result`` line has the turn count, usage and cost.
"""

from __future__ import annotations

import json
from typing import Iterable, Optional

from .model import AgentResult


def parse_lines(text: str) -> list:
    """The JSON objects in ``text``, one per line; lines that are not JSON are skipped."""
    out = []
    for raw in text.splitlines():
        raw = raw.strip()
        if not raw.startswith("{"):
            continue
        try:
            v = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if isinstance(v, dict):
            out.append(v)
    return out


def tool_uses(events: Iterable[dict]) -> list:
    """``(name, input)`` for every tool call, in order."""
    out = []
    for e in events:
        if e.get("type") == "assistant":
            for b in (e.get("message") or {}).get("content") or []:
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    out.append((b.get("name", ""), b.get("input") or {}))
    return out


def tool_errors(events: Iterable[dict]) -> int:
    n = 0
    for e in events:
        if e.get("type") == "user":
            content = (e.get("message") or {}).get("content")
            if isinstance(content, list):
                n += sum(1 for b in content if isinstance(b, dict) and b.get("type") == "tool_result" and b.get("is_error"))
    return n


def _num(v, cast=int) -> Optional[float]:
    try:
        return cast(v) if v is not None else None
    except (TypeError, ValueError):
        return None


def summarize(events: list, *, allowed_tools: Optional[set] = None) -> AgentResult:
    """An :class:`AgentResult` from the parsed lines of one run.

    ``allowed_tools`` (lower-case names without underscores) turns any other tool the model managed to call into a
    ``violation``: the shell-only arm must not have had anything else to call.
    """
    r = AgentResult()
    uses = tool_uses(events)
    r.tool_calls = len(uses)
    r.tool_errors = tool_errors(events)
    if allowed_tools is not None:
        r.violations = sorted({n for n, _ in uses if n.lower().replace("_", "") not in allowed_tools})
    result = next((e for e in reversed(events) if e.get("type") == "result"), None)
    if result is None:
        return r
    usage = result.get("usage") or {}
    r.steps = _num(result.get("num_turns"))
    r.input_tokens = _num(usage.get("input_tokens"))
    r.output_tokens = _num(usage.get("output_tokens"))
    r.cached_tokens = _num(usage.get("cache_read_input_tokens"))
    r.cost_usd = _num(result.get("total_cost_usd"), float)
    r.api_ms = _num(result.get("duration_api_ms"))
    r.is_error = bool(result.get("is_error"))
    r.subtype = str(result.get("subtype") or "")
    r.final_text = str(result.get("result") or "")
    if "exit_code" in result:
        r.exit_code = int(result["exit_code"])
    return r
