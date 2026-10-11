"""M0a -- the Marlowe memory eval harness.

A benchmark harness that can score any memory implementation behind the pinned interface,
built before and without the retriever it will score. The measurement cannot be authored by
the thing being measured.

Contains no retriever, no storage, no gate, and no embedding model. If it ever does, M0a has
failed and the split with M0b was never real.
"""

__version__ = "0.1.0"
