"""Human relevance labels: the sampler, the packet format, and the label set.

Nothing here writes labels. ROADMAP.md: *"The judge protocol and harness are the agent's.
The human-judged relevance labels are the human's."* The harness draws the sample, blinds
it, and computes agreement -- it never supplies a judgment.
"""

from .sampler import SamplePlan, decoy_pool_from, draw
from .schema import HumanLabel, HumanLabelSet, LabelPacket, SampleDraw, write_packets

__all__ = [
    "HumanLabel",
    "HumanLabelSet",
    "LabelPacket",
    "SampleDraw",
    "SamplePlan",
    "decoy_pool_from",
    "draw",
    "write_packets",
]
