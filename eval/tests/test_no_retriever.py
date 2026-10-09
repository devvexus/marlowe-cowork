"""M0a's non-goal, enforced: no retriever, and nothing that could become one.

ROADMAP.md: *"No retrieval implementation. No storage. No gate. No embedding model. If M0a
contains a retriever, it has failed."* These checks are crude on purpose -- HP10's rule is
that a crude enforced mechanism beats an elegant unenforced one.
"""

from __future__ import annotations

import ast
import inspect
from pathlib import Path

import marlowe_eval
import marlowe_eval_stubs
from marlowe_eval_stubs.oracle import OracleStub

BANNED_IMPORTS = {
    "numpy", "scipy", "torch", "tensorflow", "jax", "sklearn", "faiss", "annoy",
    "hnswlib", "sentence_transformers", "transformers", "onnxruntime", "gensim",
    "rank_bm25", "whoosh", "chromadb", "qdrant_client", "lancedb", "usearch",
}

ROOTS = [Path(marlowe_eval.__file__).parent, Path(marlowe_eval_stubs.__file__).parent]


def _python_files():
    for root in ROOTS:
        yield from root.rglob("*.py")


def _imported_top_level(tree: ast.AST) -> set[str]:
    names: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            names.update(a.name.split(".")[0] for a in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            names.add(node.module.split(".")[0])
    return names


def test_no_search_or_embedding_dependencies():
    """An embedding model or vector index in this tree means M0a grew the thing it exists
    to measure independently of."""
    offenders = []
    for path in _python_files():
        tree = ast.parse(path.read_text(encoding="utf-8"))
        hits = _imported_top_level(tree) & BANNED_IMPORTS
        if hits:
            offenders.append((path.name, sorted(hits)))
    assert not offenders, f"retrieval-shaped dependencies appeared: {offenders}"


def test_stub_selection_never_sees_the_query_text():
    """The structural guarantee that the reference stub is an oracle, not a retriever.

    `_select` resolves relevance from the answer key. If `query_text` were ever passed in,
    a similarity search would have somewhere to live -- so the signature is the invariant,
    and this test is what keeps it from decaying into a comment.
    """
    params = set(inspect.signature(OracleStub._select).parameters)
    assert params == {"self", "query_id", "rng"}, (
        f"OracleStub._select takes {sorted(params)}; it must resolve relevance from the "
        "answer key alone. Receiving query_text would make it a retriever."
    )


def test_stub_does_not_compare_memory_text():
    """Supersession in the stub keys off a turn-id convention, never off content.

    Comparing two memories' text is the first move of a retriever, so the oracle is not
    permitted to do it even for a scripted behaviour.
    """
    source = inspect.getsource(OracleStub._superseder)
    assert ".text" not in source, (
        "OracleStub._superseder inspects memory text; supersession must key off the "
        "`base#vN` turn-id convention instead"
    )
