//! Research sources for deep research v2.
//!
//! - [`registry`] — the keyless channel registry (no credential field exists) and routing by
//!   question type. Each channel declares its endpoint, rate limits, result shape, the identifier
//!   namespaces it is authoritative for, and what it indexes.
//! - [`source`] — [`SourceRef`] (one channel's record of one work) and [`Identity`].
//! - [`dedup`] — the identity-key cascade (DOI authoritative); routes merge into sources, an
//!   identifier asserted by fetched content is a claim and never merges.
//! - [`corroboration`] — claims with verbatim quotes and locators, counted over independent roots
//!   of the derivation graph.
//! - [`links`] — link tables for navigation by selection; the model never composes a URL.
//! - [`cache`] — `<home>\cache\research`.
//! - [`politeness`] — robots.txt and per-host spacing for page fetches.
//! - [`net`] — the real network behind [`HttpGet`]: marlowe-net for https, the loopback client
//!   for a local SearXNG.
//!
//! Everything a channel returns is untrusted content. Memory recalled through the Mind is a
//! source too, in its own trust bucket.

#![forbid(unsafe_code)]

pub mod cache;
pub mod channel;
pub mod channels;
pub mod corroboration;
pub mod dedup;
pub mod links;
pub mod net;
pub mod politeness;
pub mod registry;
pub mod source;
pub mod xml;

pub use cache::{CacheKey, ResearchCache};
pub use channel::{
    Assertion, Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpGet, HttpRequest, HttpResponse,
    Limiter, RateLimit, ResultShape, USER_AGENT,
};
pub use channels::arxiv::{self, ArxivEntry, ArxivFeed};
pub use channels::{
    builtin_spec, HostedSearch, KeylessChannel, KeylessConfig, WebBackend, WebChannel, WebConfig, DEFAULT_FEEDS,
    DEFAULT_SEARXNG,
};
pub use corroboration::{
    corroborate, Claim, ClaimGroup, ClaimId, Corroboration, DerivationGraph, Locator, Quote, Stance, Standing,
};
pub use dedup::{
    BuiltinAuthority, Conflict, ConflictField, Deduper, IdentityAuthority, MatchTier, Placement, Resolution, Source,
    SourceId,
};
pub use links::{DocRef, LinkRow, LinkTable, Navigator, SelectError};
pub use marlowe_proto::ChannelId;
pub use net::NetHttp;
pub use politeness::{HostGate, Refusal, Robots};
pub use registry::{classify, route, ChannelRegistry, QuestionType, RegistryError, DEFAULT_BACKOFF};
pub use source::{
    canonical_url, normalize_arxiv, normalize_doi, normalize_title, Identity, Namespace, RepoAt, SourceRef, TrustBucket,
};
