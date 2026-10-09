//! The OpenRouter provider adapter. **ADR-046.**
//!
//! # What this is for
//!
//! Marlowe's default is local: `127.0.0.1:11434`, no account, no key, no network. That is what
//! makes K6's *"install → first useful output, zero config"* reachable, and **nothing here
//! changes it.** Absent `--provider openrouter`, no code in this crate runs, no environment
//! variable is read, and no TLS handshake happens.
//!
//! What this path is *for*, primarily, is **benchmark runs**: K2 (LongMemEval-S, a kill criterion
//! that has never been measured) and M2's four unmet acceptance rows all need a model stronger or
//! faster than a local 9B, and all of them are blocked partly on a credential. This adapter is
//! therefore built as a **measurement instrument first** and a user-facing option second, and the
//! difference shows in three places:
//!
//! * [`attribution`] — **which upstream actually served each call**, recorded per call. OpenRouter
//!   routes one model name to several upstreams at different quantizations and may change that
//!   routing between two requests. A benchmark whose backing provider was not recorded is not
//!   reproducible, and this project treats an unreproducible number as not existing.
//! * [`retry`] — bounded, with the bound named. A benchmark is thousands of calls and some will be
//!   rate-limited; an unbounded retry loop is audit finding E8 in a new place.
//! * Cost and tokens come from **OpenRouter's own response fields**, never from a price table in
//!   this repo, and reach `Usage::micros_usd` — which is how `Event::Done`'s `spend_micros_usd`
//!   stops being the zero it has always been.
//!
//! # This is an adapter, not a broker
//!
//! It normalizes what a model speaks (§12). It does **not** store, refresh or transport a
//! credential: the key comes from one environment variable, is read once, and lives in
//! [`secret::ApiKey`], a type with no `Display`, no `Serialize`, and a hand-written `Debug` that
//! cannot print it. The credential **broker** — storage, rotation, per-tool scoping, an audit
//! trail — is M5, and M5's acceptance row is *"credential exposure to model context: zero,
//! structurally enforced and tested"*. See ADR-046 §5 for exactly what M5 replaces.
//!
//! # Determinism, stated honestly
//!
//! `marlowe_eval repro` reproduces bit-identically at a fixed seed and clock. **That guarantee is
//! not available on this path and this crate does not pretend otherwise.** `temperature: 0` is
//! sent, `seed` is passed through where a caller supplies one, and the upstream can be pinned with
//! `provider.order` + `allow_fallbacks: false` — three controls, none of which is a guarantee.
//! What replaces the guarantee is the record: every call's resolved model, serving upstream,
//! generation id, token counts and cost. See ADR-046 §6.
//!
//! # Egress
//!
//! ADR-046 §4 rules that a model call is the harness's own infrastructure and not tool-initiated
//! egress, and the ruling holds **only** because the destination cannot be chosen from inside a
//! run: [`transport::OPENROUTER_HOST`] is a constant, there is no base-URL setting, and
//! `marlowe-net` refuses plaintext and does not follow redirects. Read §4 before adding a setting
//! that would make the host configurable.

#![forbid(unsafe_code)]

pub mod attribution;
pub mod catalogue;
pub mod availability;
pub mod driver;
pub mod retry;
pub mod secret;
/// The SSE decoder, which **lives in `marlowe-provider`** as of ADR-060 §6.
///
/// Re-exported rather than duplicated: `marlowe-provider` cannot depend on this crate (TLS would
/// enter its graph and `no_tls_in_the_default_path.rs` fails the build), and a local `llama-server`
/// driver needs the same decoder. A crate-root `pub use` participates in path resolution, so
/// `crate::sse::SseStream` in `driver.rs` still resolves and this crate's public API is unchanged.
pub use marlowe_provider::sse;
pub mod transport;

pub use attribution::{CallAttribution, RunAttribution, NOT_REPORTED};
pub use availability::{explain_status, Availability};
pub use driver::{OpenRouterDriver, DEFAULT_CONTEXT_TOKENS};
pub use secret::{ApiKey, KeyMissing, KEY_VAR, REDACTED};
pub use transport::{
    Reply, Response, ScriptedTransport, Seen, TlsTransport, Transport, TransportError,
    API_ROOT, OPENROUTER_HOST,
};

/// Build the production driver, or say why not.
///
/// **One entry point, so the load-time refusals cannot be skipped.** A caller that constructed
/// `OpenRouterDriver::new` directly would bypass the model check and the key check, and the
/// failure would arrive a network round trip later as a 404 or a 401 — a configuration mistake
/// reported as a provider fault.
pub fn build(
    model: &str,
    registry: marlowe_tools::ToolRegistry,
) -> Result<OpenRouterDriver, Availability> {
    let key = ApiKey::from_environment();
    let availability = Availability::check(model, &key);
    match (availability, key) {
        (Availability::Ready { .. }, Ok(key)) => Ok(OpenRouterDriver::new(
            Box::new(TlsTransport::new()),
            key,
            model,
            registry,
        )),
        (other, _) => Err(other),
    }
}
