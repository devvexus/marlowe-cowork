//! **The providers a build can be switched between — the single definition of that set.**
//!
//! # Why this lives in the view crate and not in the daemon
//!
//! It used to live in `marlowe_daemon::project::PROVIDERS`, and there was a second copy in
//! `marlowe_stub::script` — `["ollama", "openrouter"]`, two entries, out of date the moment a
//! third arrived. `marlowe-stub` **structurally cannot reach `marlowe-daemon`** (that is the C2d
//! acceptance and it is enforced by the dependency graph), so the two lists could never have been
//! made to agree from either end. The fix is not a rule about keeping them in step; it is putting
//! the list in the one crate both can see.
//!
//! `marlowe-view` depends on nothing, and the stub, the daemon and the surface all depend on it.
//! So: here.
//!
//! # `ollama/llama.cpp` is ONE entry naming TWO halves, and that is the decision
//!
//! ADR-060, accepted 2026-08-27. Ollama stores, downloads and lists the models; `llama-server`
//! serves them off the blob Ollama already holds. It is a single choice because the user makes a
//! single choice — *"simple on the user"* — and it names both halves because the user must be able
//! to see **which engine is answering**. A silent swap under the existing `ollama` entry would have
//! served 52 ms from a different runtime while the screen said `ollama`, with no way to find out.
//!
//! Plain `ollama` stays selectable, unchanged, for compatibility. `openrouter` is untouched.

/// The provider names, in the order a picker offers them.
///
/// **Every consumer reads this array**: the daemon's picker, the daemon's `set_provider`
/// validation, the stub's scripted control strip, and `marlowe-surface`'s `/provider` help text.
/// A name a user can see is therefore a name the daemon accepts, by construction rather than by
/// review.
// **`HYBRID` IS SHELVED AS OF 2026-08-27 AND IS DELIBERATELY ABSENT FROM THIS ARRAY.**
//
// Matthew's decision, taken after using it: *"LLAMA.cpp gets shelved. It has so many issues.
// OLLAMA stays the default. Shelve the hybrid path. Don't remove it. But make it unavailable."*
//
// **Why, and the reason is tool calling rather than speed.** The engine is genuinely faster — the
// measurements stand — but tool calling through `llama-server` failed in real use at a rate the
// probes never saw. Observed live: the model emitting raw `<tool_call><function=read>` XML into
// the **reasoning** channel, looping, never producing a call the harness could act on; and a
// single-call turn where the parser **ate the opening `<tool_call>` and emitted the remainder as
// visible text**. That is a parser-level failure, not a prompt one.
//
// **The probes missed it twice, and both misses are this project's standing family.** The leak
// check asserted no markup in **`content`** — 0/168, clean, and the wrong channel. And the
// batching check recorded *"spurious batches (n_calls > 1): 0"*, which was read as "the model
// never over-calls" when it meant **the parser never returned more than one** — the probe never
// sent a prompt that would provoke a parallel batch. A single `glob` worked; six chained calls
// did not.
//
// **Nothing is deleted.** `marlowe-provider::{llamacpp, hybrid, ollama_store}`, the supervisor, the
// offload check, the `PATH` fix and every test remain and stay green. Only the door is closed:
// this array is what the picker offers, what `set_provider` validates against, and what the stub
// scripts — so removing the entry here removes it everywhere, by construction. Put it back and the
// path is live again.
//
// **What has to be true before it returns:** parallel tool calls parse, single tool calls parse
// without eating their own delimiter, and both are measured on a prompt that provokes a BATCH —
// not on the single-shot probe set that scored it 84/84 and told us nothing about this.
pub const PROVIDERS: &[&str] = &[OLLAMA, OPENROUTER];

/// ADR-028's default. Ollama stores the models **and** runs them.
pub const OLLAMA: &str = "ollama";

/// ADR-060's hybrid. Ollama stores and lists; a supervised `llama-server` runs.
///
/// **The spelling is load-bearing in three places at once** — the picker's option, the string
/// `ModelProviderChoice::name()` returns, and the value `set_provider` accepts — so it is a
/// constant rather than three literals. `project.rs`'s picker locates the active provider with
/// `position()`, and a mismatch there does not error: it selects index 0 and renders a hybrid
/// daemon as plain `ollama`, which is a silent wrong answer in the one place a user reads which
/// engine is live.
pub const HYBRID: &str = "ollama/llama.cpp";

/// ADR-046. Hosted, opt-in, needs a key.
pub const OPENROUTER: &str = "openrouter";
