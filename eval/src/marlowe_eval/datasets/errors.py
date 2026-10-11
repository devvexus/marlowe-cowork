"""Corpus loading failures.

Separate from `ProtocolError` because the blame is different: a protocol error means the
implementation broke section 4, a corpus error means the data on disk is not the data the
adapter was written against. Conflating them would send someone debugging a retriever when a
dataset release changed a field name.
"""

from __future__ import annotations


class CorpusFormatError(Exception):
    """The corpus does not match the schema this adapter was written against.

    Always names the field and the instance, because the whole point of `verify-corpus` is
    that a drift is diagnosable in one read rather than one bisect.
    """

    def __init__(self, dataset: str, detail: str, *, where: str | None = None) -> None:
        self.dataset = dataset
        self.detail = detail
        self.where = where
        location = f" at {where}" if where else ""
        super().__init__(f"{dataset}{location}: {detail}")
