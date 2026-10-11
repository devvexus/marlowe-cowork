"""The reference stub. It reads the answer key. It does not search.

M0a's non-goals are explicit: no retrieval implementation, no storage, no gate, no embedding
model. "If M0a contains a retriever, it has failed." So the stub that makes the harness
testable before M0b exists must be structurally incapable of being one.

The guarantee is in a function signature. `_select` takes `(query_id, rng)` and never
receives `query_text`. There is nowhere for a similarity search to hide, and a test asserts
the signature, so the guarantee cannot rot into a comment.

What the stub is FOR, beyond smoke-testing the plumbing: it is a calibration source for the
instrument. Set `precision=0.7` and the harness must report evidence precision of 0.70. That
is the one thing M0a can verify about its own arithmetic before there is anything real to
measure -- an instrument that cannot recover a known input is not measuring.
"""

from __future__ import annotations

import random
from dataclasses import dataclass
from typing import Any

from marlowe_eval.adapter.base import MemorySystem
from marlowe_eval.contract import (
    AnswerRequest,
    Channel,
    IngestRequest,
    Interface,
    RetrievalRequest,
    TrustClass,
)
from marlowe_eval.datasets.model import Corpus
from marlowe_eval.determinism import stream

# Origin -> trust, derived by the implementation from what the harness declared. The harness
# never sends a trust class (section 4.6); this table is the stub playing the part M0b will
# play for real. CONTRACTS section 3.3 names web, inbound mail, MCP output and third-party
# skill output as untrusted.
#
# TOTAL over the closed set, and looked up WITHOUT a default. Section 4.6 pins `channel` as
# a closed set precisely so an unrecognized value is a loud error on both sides: a silent
# fallback would make the laundering suite pass while measuring nothing, and if the fallback
# were a trusted class it would hide the very failure the suite exists to catch.
_TRUST_BY_CHANNEL = {
    Channel.TERMINAL: TrustClass.USER_ASSERTED,
    Channel.VOICE: TrustClass.USER_ASSERTED,
    Channel.MESSAGING: TrustClass.UNTRUSTED_CONTENT,
    Channel.EMAIL: TrustClass.UNTRUSTED_CONTENT,
    Channel.WEB: TrustClass.UNTRUSTED_CONTENT,
    Channel.MCP: TrustClass.UNTRUSTED_CONTENT,
    Channel.TOOL_OUTPUT: TrustClass.AGENT_OBSERVED,
    Channel.FILE: TrustClass.AGENT_INFERRED,
}

# Actors a turn may not claim. CONTRACTS section 1: the model is not in the Actor enum, and
# TierGranted/TierDemoted are rejected unless actor == Permission. A history that arrives
# claiming one of these is attempting an unsigned privileged write.
_RESERVED_ACTORS = frozenset({"permission", "harness", "consolidation"})


@dataclass
class _Memory:
    memory_id: str
    turn_id: str
    session_id: str
    text: str
    trust: TrustClass
    written_at_ms: int
    mature_at_ms: int
    payload_kind: str


class OracleStub(MemorySystem):
    """A section 4 implementation backed by the benchmark's own answer key.

    Knobs, all of which the harness must be able to recover from the report:

      precision       fraction of injected memories drawn from gold evidence
      abstention_rate fraction of abstention cases the stub correctly declines
      answer_accuracy fraction of answerable cases it answers with the gold answer
      maturation_ms   section 4.3 exclusion (3): how long a new belief stays silent
      k               injected set size

    `maturation_ms` defaults to an hour rather than zero. Section 4.3 calls maturation a
    security property and *the cheapest available defence against single-exposure
    poisoning*, so a reference implementation with it switched off would be a bad reference
    -- and the clock probe treats a total absence of time-dependence as a failure, which an
    implementation without maturation would deserve.
    """

    name = "oracle-stub"

    def __init__(
        self,
        corpus: Corpus,
        *,
        seed: int = 7,
        precision: float = 0.7,
        abstention_rate: float = 0.9,
        answer_accuracy: float = 0.85,
        maturation_ms: int = 3_600_000,
        supersession_half_life_ms: int = 7 * 86_400_000,
        k: int = 3,
        gate_threshold: float = 0.71,
        read_system_clock: bool = False,
    ) -> None:
        self.corpus = corpus
        self.seed = seed
        self.precision = precision
        self.abstention_rate = abstention_rate
        self.answer_accuracy = answer_accuracy
        self.maturation_ms = maturation_ms
        self.supersession_half_life_ms = supersession_half_life_ms
        self.k = k
        self.gate_threshold = gate_threshold
        # Only the `clock_reader` conformance fixture sets this. It exists so the clock
        # probe can be shown to have teeth: a probe no implementation can fail is not a
        # probe. See marlowe_eval_stubs.broken.
        self._read_system_clock = read_system_clock

        self._memories: dict[str, _Memory] = {}
        self._by_turn: dict[str, str] = {}
        self._gold = corpus.gold_map()
        self._case_by_query = {c.query_id: c for c in corpus.cases}

    # -- MemorySystem ---------------------------------------------------------------

    def call(self, op: Interface, body: dict[str, Any]) -> dict[str, Any]:
        if op is Interface.INGEST:
            return self._ingest(IngestRequest.model_validate(body))
        if op is Interface.RETRIEVE:
            return self._retrieve(RetrievalRequest.model_validate(body))
        return self._answer(AnswerRequest.model_validate(body))

    # -- section 4.6 ----------------------------------------------------------------

    def _ingest(self, req: IngestRequest) -> dict[str, Any]:
        now = self._now(req.clock.now_ms)
        written: list[dict[str, Any]] = []
        rejected: list[dict[str, str]] = []

        for turn in req.turns:
            actor_root = turn.origin.actor.split(":", 1)[0].strip().lower()
            if actor_root in _RESERVED_ACTORS:
                # An unsigned privileged write. Refused, and visibly so: a suite that plants
                # one must be able to see it refused rather than infer refusal from absence.
                rejected.append({"turn_id": turn.turn_id, "reason": "unauthorized_actor"})
                continue

            memory_id = f"m-{req.session_id}-{turn.turn_id}"
            try:
                trust = _TRUST_BY_CHANNEL[turn.origin.channel]
            except KeyError:  # pragma: no cover - unreachable while the enum is total
                raise ValueError(
                    f"no trust derivation for channel {turn.origin.channel!r}. Section 4.6 "
                    "pins the channel vocabulary as a closed set; an unrecognized value is "
                    "a load-time error, never a default."
                ) from None
            self._memories[memory_id] = _Memory(
                memory_id=memory_id,
                turn_id=turn.turn_id,
                session_id=req.session_id,
                text=turn.text,
                trust=trust,
                written_at_ms=turn.occurred_at_ms,
                # Section 4.3 exclusion (3), engram maturation. Read off the SUPPLIED clock,
                # never a system one -- which is the whole reason section 4.5 exists.
                mature_at_ms=now + self.maturation_ms,
                payload_kind="fact",
            )
            self._by_turn[turn.turn_id] = memory_id
            written.append(
                {
                    "turn_id": turn.turn_id,
                    "memory_ids": [memory_id],
                    "effective_trust": trust.value,
                }
            )

        return {
            "contract_version": "1.0",
            "session_id": req.session_id,
            "written": written,
            "rejected": rejected,
            "cost": {
                "ingest_tokens": sum(len(t.text) // 4 for t in req.turns),
                # Synthetic and deterministic. The stub has no clock to time itself with,
                # and inventing a wall-clock reading would be the one thing section 4.5
                # forbids. The report marks stub latencies as synthetic.
                "latency_ms": {"total": 1 + len(req.turns) // 4},
            },
        }

    # -- sections 4.1-4.4 -----------------------------------------------------------

    def _select(self, query_id: str, rng: random.Random) -> list[tuple[str, bool]]:
        """Choose what to inject.

        NOTE THE SIGNATURE. No `query_text`, no embedding, no corpus scan by content. The
        stub resolves relevance by looking up the answer key, which is the definition of not
        being a retriever. A test asserts these parameter names.
        """
        gold_turns = self._gold.get(query_id, frozenset())
        gold = [self._by_turn[t] for t in sorted(gold_turns) if t in self._by_turn]
        case = self._case_by_query.get(query_id)
        session = case.session_id if case else None
        pool = [
            m.memory_id
            for m in self._memories.values()
            if (session is None or m.session_id == session) and m.turn_id not in gold_turns
        ]
        pool.sort()

        picked: list[tuple[str, bool]] = []
        gold_left = list(gold)
        for _ in range(self.k):
            take_gold = rng.random() < self.precision and gold_left
            if take_gold:
                picked.append((gold_left.pop(0), True))
            elif pool:
                picked.append((pool.pop(rng.randrange(len(pool))), False))
        return picked

    def _superseder(self, memory: _Memory) -> _Memory | None:
        """Scripted supersession, keyed on a turn-id convention: `base#v1` is superseded by
        `base#v2`.

        A convention rather than content inspection, on purpose. The stub is an oracle -- it
        is allowed to read the key -- but it is not allowed to compare texts, because
        comparing texts is the first move of a retriever.
        """
        if "#v" not in memory.turn_id:
            return None
        base, _, version = memory.turn_id.rpartition("#v")
        if not version.isdigit():
            return None
        best: _Memory | None = None
        for other in self._memories.values():
            if other.session_id != memory.session_id or "#v" not in other.turn_id:
                continue
            o_base, _, o_version = other.turn_id.rpartition("#v")
            if o_base != base or not o_version.isdigit():
                continue
            if int(o_version) > int(version):
                if best is None or int(o_version) > int(best.turn_id.rpartition("#v")[2]):
                    best = other
        return best

    def _survives_supersession(self, memory: _Memory, now: int, rng: random.Random) -> bool:
        superseder = self._superseder(memory)
        if superseder is None:
            return True
        age = max(0, now - superseder.written_at_ms)
        if self.supersession_half_life_ms <= 0:
            return False
        survival = 0.5 ** (age / self.supersession_half_life_ms)
        return rng.random() < survival

    def _retrieve(self, req: RetrievalRequest) -> dict[str, Any]:
        now = self._now(req.clock.now_ms)
        rng = stream(self.seed, f"retrieve:{req.query_id}")

        picked = [
            (mid, is_gold)
            for mid, is_gold in self._select(req.query_id, rng)
            # Section 4.3: unmatured entries are excluded from auto-injection. They remain
            # reachable by explicit recall -- the bar is on unprompted influence.
            if self._memories[mid].mature_at_ms <= now
            and self._survives_supersession(self._memories[mid], now, rng)
        ]

        considered = sum(
            1 for m in self._memories.values() if m.mature_at_ms <= now
        )

        if not picked:
            reason = "no_candidates" if considered == 0 else "no_candidate_above_threshold"
            return self._retrieval_body(req.query_id, [], considered, abstain=reason)

        injected = []
        for mid, is_gold in picked:
            mem = self._memories[mid]
            # Gold and distractor scores overlap on purpose. A gate whose score perfectly
            # separated relevance would make the blinded sampler's deciles meaningless, and
            # the sampler is the thing being exercised.
            score = rng.uniform(0.50, 1.0) if is_gold else rng.uniform(0.0, 0.70)
            injected.append(
                {
                    "memory_id": mid,
                    "content": mem.text,
                    "score": round(score, 6),
                    "calibrated_precision": round(min(0.99, score * 0.95 + 0.02), 6),
                    "fidelity": "summary",
                    "effective_trust": mem.trust.value,
                    "payload_kind": mem.payload_kind,
                }
            )

        return self._retrieval_body(req.query_id, injected, considered)

    def _retrieval_body(
        self,
        query_id: str,
        injected: list[dict[str, Any]],
        considered: int,
        abstain: str | None = None,
    ) -> dict[str, Any]:
        tokens = sum(len(i["content"]) // 4 for i in injected)
        return {
            "contract_version": "1.0",
            "query_id": query_id,
            "abstained": abstain is not None,
            "abstention_reason": abstain,
            "injected": injected,
            "considered": considered,
            "gate": {
                "version": "stub-frozen-v1",
                "threshold": self.gate_threshold,
                "adaptive": False,
            },
            "cost": {
                "retrieval_tokens": tokens,
                "latency_ms": {
                    "total": 5 + considered // 8,
                    "embed": 1,
                    "cues": 2 + considered // 16,
                    "fuse": 1,
                    "gate": 1,
                },
            },
        }

    # -- section 4.7 ----------------------------------------------------------------

    def _answer(self, req: AnswerRequest) -> dict[str, Any]:
        case = self._case_by_query.get(req.query_id)
        retrieval = self._retrieve(
            RetrievalRequest(
                query_id=req.query_id,
                session_id=req.session_id,
                turn_index=0,
                query_text=req.question,
                clock={"now_ms": req.clock.now_ms},
                budget={"max_tokens": 7000, "max_latency_ms": 300},
            )
        )
        rng = stream(self.seed, f"answer:{req.query_id}")

        abstain = False
        answer: str | None = None
        answered = True

        if case is not None and case.is_abstention:
            # The honest "no". Section 4.7 makes a populated answer here a protocol error,
            # so the stub must produce a clean refusal, not a hedge.
            if rng.random() < self.abstention_rate:
                abstain, answered, answer = True, False, None
            else:
                answer = "Yes -- you mentioned that a while back."
        elif case is not None:
            if rng.random() < self.answer_accuracy and case.gold_answer:
                answer = case.gold_answer
            else:
                answer = "I believe it was the other one."
        else:
            answered, abstain = False, True

        if retrieval["abstained"] and not abstain:
            abstain, answered, answer = True, False, None

        # Section 4.7, pinned 2026-08-02: an abstention cites no grounding, and an
        # unanswered question is always an abstention. Enforced here rather than left to
        # the validator so the reference implementation demonstrates the coherent shape.
        grounded_in = (
            [] if abstain else [i["memory_id"] for i in retrieval["injected"]]
        )
        if not answered:
            abstain = True

        return {
            "contract_version": "1.0",
            "query_id": req.query_id,
            "answered": answered,
            "answer": answer,
            "abstained": abstain,
            "abstention_reason": "no_candidate_above_threshold" if abstain else None,
            "grounded_in": grounded_in,
            "retrieval": retrieval,
            "cost": {
                "prompt_tokens": 400 + retrieval["cost"]["retrieval_tokens"],
                "completion_tokens": len(answer or "") // 4,
                "retrieval_tokens": retrieval["cost"]["retrieval_tokens"],
                "latency_ms": {
                    "total": 40 + retrieval["cost"]["latency_ms"]["total"],
                    "retrieval": retrieval["cost"]["latency_ms"]["total"],
                    "generation": 40,
                },
            },
        }

    # -- the clock ------------------------------------------------------------------

    def _now(self, supplied_ms: int) -> int:
        """Every timestamp derives from the caller's clock. Section 4.5, binding.

        The `read_system_clock` branch is the deliberate violation used by the conformance
        fixture, and it is the only place in this repository that touches a real clock.
        """
        if self._read_system_clock:  # pragma: no cover - exercised via the broken fixture
            import time

            return int(time.time() * 1000)
        return supplied_ms
