"""Emit JSON Schema for the six section 4 payloads.

This is the artifact a third party builds against without reading Python, and the artifact a
Rust implementer checks their serde output against. "Methodology is reproducible by a third
party from the repo alone" is an M0a acceptance criterion; a schema they can validate
against is most of what that means in practice.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from .answer import AnswerRequest, AnswerResponse
from .ingest import IngestRequest, IngestResponse
from .retrieval import RetrievalRequest, RetrievalResponse

SCHEMAS = {
    "retrieval_request": RetrievalRequest,
    "retrieval_response": RetrievalResponse,
    "ingest_request": IngestRequest,
    "ingest_response": IngestResponse,
    "answer_request": AnswerRequest,
    "answer_response": AnswerResponse,
}


def build() -> dict[str, Any]:
    return {name: cls.model_json_schema() for name, cls in SCHEMAS.items()}


def write(out_dir: Path) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for name, schema in build().items():
        path = out_dir / f"{name}.schema.json"
        path.write_text(
            json.dumps(schema, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        written.append(path)
    return written
