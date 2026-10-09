"""The Claude-backed offline judge. Unrunnable by default, not merely unrun.

Two independent gates, both required at CONSTRUCTION:

  1. An explicit opt-in: the `--judge anthropic` flag AND the environment variable
     `MARLOWE_EVAL_ALLOW_API_JUDGE=1`. Either one alone is not enough. A flag alone is too
     easy to leave in a script; an env var alone is too easy to leave in a shell.
  2. A loaded human label set. HP1 permits this judge only where its agreement against the
     human set is published alongside anything it produces, so with no labels to compare
     against there is no legitimate use for it -- and it refuses to exist.

Gate 2 is the one that matters. Without it the class would still be *runnable* and would
still emit numbers, and the requirement would be back to being a rule someone remembers.

Determinism, stated honestly: sampling parameters are rejected on this model, so a judge
call cannot be pinned to a fixed decode. Structured output and low effort narrow the
variance; they do not eliminate it. **The verdict cache is what makes a judged run
reproducible**, which is why it is required rather than optional.
"""

from __future__ import annotations

import json
import os

from ..labels.schema import HumanLabelSet, LabelPacket
from .cache import VerdictCache, key_for
from .protocol import JudgeUnavailable, Verdict

MODEL = "claude-opus-5"
PROMPT_VERSION = "judge-v1"
ENV_FLAG = "MARLOWE_EVAL_ALLOW_API_JUDGE"

SYSTEM = """\
You are judging whether a remembered item is relevant to a user's query.

Relevant means: a careful person, seeing this memory surfaced while answering this query, \
would say it belongs there -- it bears on the question, not merely on the same topic.

Judge only the pair you are shown. You are not told the system's confidence, its ranking, \
or whether the memory was actually used; that is deliberate, and there is no signal about \
it hidden in the ordering. Answer on the content alone."""

_SCHEMA = {
    "type": "object",
    "properties": {
        "relevant": {"type": "boolean"},
        "reason": {"type": "string"},
    },
    "required": ["relevant", "reason"],
    "additionalProperties": False,
}


class AnthropicJudge:
    """HP1 tier-2 offline judge, behind two gates and a cache."""

    judge_id = f"anthropic:{MODEL}:{PROMPT_VERSION}"

    def __init__(
        self,
        *,
        allow_api: bool,
        label_set: HumanLabelSet | None,
        cache: VerdictCache,
        model: str = MODEL,
    ) -> None:
        if not allow_api:
            raise JudgeUnavailable(
                "the API-backed judge requires --judge anthropic on the command line. "
                "It is opt-in because it spends money and because its output is only "
                "publishable alongside an agreement rate."
            )
        if os.environ.get(ENV_FLAG) != "1":
            raise JudgeUnavailable(
                f"the API-backed judge also requires {ENV_FLAG}=1 in the environment. "
                "Two independent opt-ins on purpose: a flag alone is easy to leave in a "
                "script, an environment variable alone is easy to leave in a shell."
            )
        if label_set is None or len(label_set) == 0:
            raise JudgeUnavailable(
                "the API-backed judge requires a human label set to compute agreement "
                "against. HP1 permits this judge only where its agreement rate is "
                "published alongside any number it produces, so with no labels there is "
                "no legitimate output. See ROADMAP.md -- the label set is the human's "
                "deliverable, and nothing in this repository writes one."
            )

        try:
            import anthropic  # noqa: PLC0415 - optional extra, imported on use
        except ModuleNotFoundError as exc:  # pragma: no cover - depends on install extras
            raise JudgeUnavailable(
                "the `judge` extra is not installed: pip install 'marlowe-eval[judge]'"
            ) from exc

        self.model = model
        self.judge_id = f"anthropic:{model}:{PROMPT_VERSION}"
        self.cache = cache
        self._client = anthropic.Anthropic()

    def judge(self, packet: LabelPacket) -> Verdict:
        key = key_for(self.judge_id, PROMPT_VERSION, packet)
        cached = self.cache.get(key, packet.packet_id)
        if cached is not None:
            return cached

        response = self._client.messages.create(
            model=self.model,
            max_tokens=2048,
            system=SYSTEM,
            output_config={
                "effort": "low",
                "format": {"type": "json_schema", "schema": _SCHEMA},
            },
            messages=[
                {
                    "role": "user",
                    "content": (
                        f"Query:\n{packet.query}\n\nRemembered item:\n{packet.memory}"
                    ),
                }
            ],
        )

        # Check the stop reason before touching content: a declined request returns HTTP
        # 200 with an empty or partial content list, and indexing it would raise something
        # unrelated to what actually happened.
        if response.stop_reason == "refusal":
            raise JudgeUnavailable(
                f"the judge model declined packet {packet.packet_id} "
                f"(category: {getattr(response.stop_details, 'category', None)}). "
                "A refused packet cannot be scored; drop it from the sample rather than "
                "recording a verdict."
            )

        text = next((b.text for b in response.content if b.type == "text"), "")
        parsed = json.loads(text)
        verdict = Verdict(
            packet_id=packet.packet_id,
            relevant=bool(parsed["relevant"]),
            judge_id=self.judge_id,
            rationale=str(parsed.get("reason", "")),
        )
        self.cache.put(key, verdict)
        return verdict
