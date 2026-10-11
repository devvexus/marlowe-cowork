"""Adapters between the harness and a system under test.

`base` holds the ABC and the validating client. The NDJSON subprocess transport is not here
yet on purpose: it is the only piece that depends on CONTRACTS.md section 4.0, which is
drafted in docs/design/proposed-4.0-transport.md and awaiting a pin.
"""

from .base import Client, Exchange, ImplementationFailure, MemorySystem

__all__ = ["Client", "Exchange", "ImplementationFailure", "MemorySystem"]
