"""CONTRACTS.md section 4 only.

This package is a binding of the pinned JSON wire format. The JSON is normative
(CONTRACTS.md section 4: "the scorer is Python, the implementation is Rust"); these types
are its Python expression, not the other way round.
"""

CONTRACT_VERSION = "1.0"
"""Matches CONTRACTS.md `CONTRACT_VERSION: (u16, u16) = (1, 0)`, rendered as the wire string."""
