"""Shared vocabulary for the section 4 boundary.

Everything here appears in a pinned section 4 example, or is required to construct one.
Where section 4 pins a field but not its vocabulary, that is marked UNPINNED and listed in
docs/design/proposed-4.0-transport.md Part D.
"""

from __future__ import annotations

import enum

from pydantic import BaseModel, ConfigDict


class ContractModel(BaseModel):
    """Base for every section 4 payload.

    `extra="forbid"` is a deliberate strictness beyond the letter of section 4. An
    undeclared field is either a typo of a real one -- in which case silently ignoring it
    would misscore a run -- or an undeclared channel between implementation and harness,
    which the M0a/M0b split exists to prevent. Both are worth failing on.
    """

    model_config = ConfigDict(extra="forbid", frozen=True)


class Fidelity(str, enum.Enum):
    """CONTRACTS section 3.1 `FidelityTier`, as it appears on the wire in section 4.2."""

    RECORD = "record"
    SUMMARY = "summary"
    GIST = "gist"
    TOMBSTONE = "tombstone"


class TrustClass(str, enum.Enum):
    """CONTRACTS section 3.3. Ordered; the ordering IS the propagation rule."""

    UNTRUSTED_CONTENT = "untrusted_content"
    AGENT_INFERRED = "agent_inferred"
    AGENT_OBSERVED = "agent_observed"
    USER_ASSERTED = "user_asserted"

    @property
    def rank(self) -> int:
        return _TRUST_RANK[self]


_TRUST_RANK = {
    TrustClass.UNTRUSTED_CONTENT: 0,
    TrustClass.AGENT_INFERRED: 1,
    TrustClass.AGENT_OBSERVED: 2,
    TrustClass.USER_ASSERTED: 3,
}


class PayloadKind(str, enum.Enum):
    """CONTRACTS section 3.2 `Payload` variants, snake_cased for the wire."""

    EPISODE = "episode"
    FACT = "fact"
    ENTITY = "entity"
    EDGE = "edge"
    PROCEDURE = "procedure"
    COMMITMENT = "commitment"
    PERSON = "person"
    RELATIONSHIP = "relationship"
    VOICE_PARAMS = "voice_params"
    NOTICING = "noticing"


class AbstentionReason(str, enum.Enum):
    """CONTRACTS section 4.2: a closed set, given explicitly."""

    NO_CANDIDATE_ABOVE_THRESHOLD = "no_candidate_above_threshold"
    NO_CANDIDATES = "no_candidates"
    BUDGET_EXHAUSTED = "budget_exhausted"
    DEGRADED_PATH = "degraded_path"


class Channel(str, enum.Enum):
    """UNPINNED vocabulary. See proposed-4.0-transport.md Part D item 4.

    Section 4.6 pins the `origin.channel` field and shows two values ("terminal", "web").
    It does not pin the set. These are the values the harness emits; the laundering suite
    depends on `web` in particular being understood as untrusted at the origin.
    """

    TERMINAL = "terminal"
    VOICE = "voice"
    MESSAGING = "messaging"
    EMAIL = "email"
    WEB = "web"
    MCP = "mcp"
    TOOL_OUTPUT = "tool_output"
    FILE = "file"


class Speaker(str, enum.Enum):
    """UNPINNED vocabulary. Section 4.6 shows "user" and "tool"; benchmark histories also
    carry assistant turns. See proposed-4.0-transport.md Part D item 4."""

    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"


class Clock(ContractModel):
    """CONTRACTS section 4.5.

    Normative and binding: on any path reachable from sections 4.1, 4.6 or 4.7 the
    implementation MUST NOT read a system clock. Every timestamp derives from this value.

    Note the shape asymmetry, which is faithful to the pinned examples and not a
    normalization by the harness: the retrieval request carries `now_ms` at top level
    (section 4.1, and `pub now: Timestamp` in the 4.2b binding), while ingest and answer
    carry this object. See proposed-4.0-transport.md Part D item 5.
    """

    now_ms: int
