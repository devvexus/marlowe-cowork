//! Retrieval cues.
//!
//! Brief §5 requires five: dense semantic, lexical/BM25, entity-graph traversal, temporal
//! proximity, and causal linkage. **Session B ships one — the lexical cue — and the module is
//! shaped so that is visible rather than implied.** A single cue is not the system K1 measures,
//! and a number produced by one cue is a statement about an incomplete cue set.
//!
//! What the missing four cost, named so a later session does not have to re-derive it:
//!
//! | Cue | What it would find that lexical cannot |
//! |---|---|
//! | dense | the memory that answers the question in different vocabulary |
//! | entity-graph | the memory two hops away, reachable only through a shared entity |
//! | temporal | "what did I say *before* the migration" — proximity, not similarity |
//! | causal | the memory that explains the one that matched |
//!
//! The gate's `cue_agreement` feature exists now, constant, for the same reason: cues 2–5 land
//! in a slot that already exists instead of reshaping the frozen artifact.

pub mod dense;
pub mod lexical;
