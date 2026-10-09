//! CONTRACTS.md §7 and §8 — the tool/skill registry.
//!
//! ARCHITECTURE §2.11: this component **owns** manifests, signature verification, install-time
//! diff review, and exposure selection. It **never executes anything and never holds a
//! credential** — execution is the loop's tool host, credentials are the broker's.
//!
//! Three properties are structural here rather than checked at call time, and each is a place
//! where a plausible default would delete a requirement while every test stayed green:
//!
//! | Where | The default that would hide a failure |
//! |---|---|
//! | [`manifest::RawManifest`] has no `provenance` field | a self-declared `first_party` walks past the third-party consequence floor, and the check still passes because it never fires |
//! | [`exposure::ExposedSet`] deserializes through its constructor | a thirteenth tool from a config file, with the constructor's assertion still green |
//! | [`registry::Description`] carries a trust class | a `String` would make "never let a tool description alter system-prompt-level behavior" unenforceable one layer up, while looking complete here |
//!
//! **Not built here, and named so it is not assumed:** signature verification and install-time
//! diff review (§7.1) are M2 Session C, with skill loading. Nothing in this crate should be
//! read as verifying that a third-party manifest is authentic — only that it is *declared*.

#![forbid(unsafe_code)]

pub mod builtin;
pub mod exposure;
/// The strict YAML subset `SKILL.md` front matter is parsed with. ADR-051.
pub mod frontmatter;
pub mod manifest;
/// Installed-tool description pinning. ADR-052 §4.
pub mod pin;
pub mod registry;
/// `SKILL.md` loading and progressive disclosure. ADR-051.
pub mod skill;
pub mod summary;

pub use builtin::{builtin_registry, BUILTIN_TOOLS};
pub use exposure::{ExposedSet, ExposureError, MAX_EXPOSED_TOOLS};
pub use manifest::{
    load, ArgumentRole, CapabilityManifest, ConsequenceLevel, CredentialId, HostPattern, LoadError,
    ManifestProvenance, ParamSpec, ParamType, PathGlob, RawManifest, RawParamSpec, ToolId,
};
pub use registry::{
    Description, RegistryError, ToolRegistration, ToolRegistry, Transport, MAX_DESCRIPTION_CHARS,
};
pub use summary::{Metric, ResultSummary, SummarySpec};
