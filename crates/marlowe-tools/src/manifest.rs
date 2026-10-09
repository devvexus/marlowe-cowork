//! CONTRACTS.md §7.3 — the capability manifest and the consequence declaration.
//!
//! Two rules from that section are structural here rather than checked at call time:
//!
//! 1. **Default-deny on consequence.** An absent `consequence` loads as
//!    [`ConsequenceLevel::Irreversible`], the maximum. It is not an error — the pinned
//!    pseudocode says `None => Irreversible` in as many words — because the failure it
//!    guards against is a first-party tool that forgot the field, and the safe reading of
//!    a forgotten field is "assume the worst", not "refuse to boot".
//! 2. **A malformed or undeclared manifest is a load-time error.** A registration with no
//!    manifest at all cannot be constructed: [`crate::registry::ToolRegistration`] holds a
//!    `CapabilityManifest`, not an `Option<CapabilityManifest>`, and the only way to obtain
//!    one is [`load`].
//!
//! # `provenance` is assigned by the loader and is not a field in the file
//!
//! This is the non-obvious one, and getting it wrong would delete the third-party check
//! silently. §7.3 refuses a `ThirdParty` manifest that self-declares below `Consequential`.
//! If the manifest file could *state* its own provenance, a third-party skill would write
//! `provenance: first_party` and walk past the check — and every test would still pass,
//! because the check would still be there and would simply never fire.
//!
//! So [`RawManifest`] has no `provenance` field, [`load`] takes it as a separate argument
//! derived from where the manifest came from, and `deny_unknown_fields` turns an attempt to
//! declare it into a **named parse failure** rather than a silently ignored key.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A registered tool's identity. Registration is unlimited; see [`crate::exposure`] for the
/// budgeted half.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ToolId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// CONTRACTS.md §7.3. **Ordered**, and the ordering is load-bearing: §9's `check_targets`
/// early-returns at `<= Inert`, and §7.3 refuses third-party self-declaration
/// `< Consequential`. Reordering these variants silently changes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsequenceLevel {
    /// Pure reads, no side effects.
    Inert = 0,
    /// Undoable writes inside the workspace.
    Reversible = 1,
    /// External writes, spend, messages to third parties.
    Consequential = 2,
    /// Money movement, send-as-user, deletion, anything legally binding.
    Irreversible = 3,
}

/// CONTRACTS.md §7.3 — the `(action, target)` split.
///
/// > Untrusted content may shape `Payload` fields **freely**. It may never shape a `Target`.
///
/// There is no third variant and there must not be one. A middle role — "usually safe",
/// "checked only when suspicious" — is a role whose meaning is decided at the call site,
/// which is precisely where the model's output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentRole {
    /// Tool selection, recipient, path, host, amount, identifier.
    Target,
    /// Inert body content: a draft, a summary, a message body.
    Payload,
}

/// What a parameter carries, so the adjudicator knows which scoped check applies.
///
/// This is not a serialization type — it selects an enforcement path. `Path` routes to path
/// scoping, `Url` to egress. A parameter whose type says nothing about enforcement is
/// [`ParamType::Text`], and `Text` in a `Target` role is still provenance-checked; it simply
/// has no second, type-specific wall behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Text,
    /// Routed to path scoping, **read-only, and the target must already exist**.
    /// `read`, `find`, and `bash`'s `cwd`.
    Path,
    /// Routed to path scoping, **read-write, and the target may not exist yet** — it is created
    /// inside the verified parent directory if absent. `edit`.
    ///
    /// Declared per parameter rather than inferred from the tool's consequence level, and that
    /// distinction is load-bearing: `bash`'s `cwd` is `Irreversible` and must exist, `edit`'s
    /// `path` is `Reversible` and may not. Deriving access from consequence would make two
    /// different requirements take their behaviour from the same number.
    WritePath,
    /// Routed to egress allowlisting. Carries a full URL, not a bare host.
    Url,
    /// A money amount, in micros of the profile's currency.
    Amount,
    /// An opaque identifier — a memory id, a run id, a connection id.
    Identifier,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub role: ArgumentRole,
    pub ty: ParamType,
    /// **What this parameter means, in the tool's own words.**
    ///
    /// `None` falls back to a sentence generated from `ty` and `required` — which says a `Text`
    /// parameter takes "text", and that is all a model learned about `run`'s `task` until
    /// 2026-08-26. A spawn then delegated a question the child had no way to answer, spent 12,332
    /// tokens reasoning, and returned nothing.
    ///
    /// Set it wherever the name is not self-evident. A parameter whose meaning the model has to
    /// infer is a parameter the model will infer wrongly.
    pub description: Option<String>,
    /// **Whether the executor demands it. A separate question from [`ArgumentRole`].**
    ///
    /// `role` answers *what untrusted content may never shape*. `required` answers *what the tool
    /// cannot run without*. They are correlated — a path is usually both — and binding one to the
    /// other produced **eleven measured mismatches** across ten builtins:
    ///
    /// | tool | was sent as required | what the executor wants |
    /// |---|---|---|
    /// | `bash` | `command`, `cwd` | `command`; `cwd` defaults to the workspace |
    /// | `find` | `path` | `pattern` — it fails without it |
    /// | `edit` | `path` | `path` **and** `content` |
    /// | `remember` | `derived_from`, `payload_kind` | the claim itself |
    /// | `ask` | *nothing* | the question |
    ///
    /// Observed consequence: the model was told the search string was optional and the path
    /// mandatory, invented a `cwd` on every shell call, and — after a refusal — was handed the
    /// same wrong parameter list again by `Engine::expected_params`.
    pub required: bool,
}

impl ParamSpec {
    pub fn target(name: &str, ty: ParamType) -> Self {
        Self { name: name.to_string(), role: ArgumentRole::Target, ty, required: true, description: None }
    }

    pub fn payload(name: &str, ty: ParamType) -> Self {
        Self { name: name.to_string(), role: ArgumentRole::Payload, ty, required: false, description: None }
    }
}

/// Where a manifest came from. **Assigned by the loader, never parsed from the manifest.**
/// See this module's header for why that distinction is the whole of the third-party check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestProvenance {
    /// Compiled into this binary. The eleven tools of ADR-006.
    FirstParty,
    /// A human read the install-time diff and accepted it.
    UserReviewed { at: i64 },
    /// Everything else: an MCP server, a downloaded skill, a connector.
    ThirdParty,
}

/// A declared filesystem glob. **Matching lives in `marlowe_permission::scope`, not here** —
/// a glob type that could answer "does this path match" would be a path check in the crate
/// with no handle discipline, which is the shape ADR-002 names as worse than no check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PathGlob(String);

impl PathGlob {
    pub fn new(g: impl Into<String>) -> Self {
        Self(g.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A declared egress destination. Exact host, or a `*.` suffix pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostPattern(String);

impl HostPattern {
    pub fn new(p: impl Into<String>) -> Self {
        Self(p.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialId(String);

impl CredentialId {
    pub fn new(c: impl Into<String>) -> Self {
        Self(c.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// CONTRACTS.md §7.3.
///
/// **Every field is private and there is no public constructor other than [`load`].** That is
/// what makes the load-time validation a boundary rather than a convention: a caller cannot
/// assemble a manifest that skipped the checks, and `serde` cannot either — deserialization
/// goes through `try_from = "RawManifest"`, which routes to the same function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityManifest {
    tool: ToolId,
    paths: Vec<PathGlob>,
    hosts: Vec<HostPattern>,
    creds: Vec<CredentialId>,
    consequence: ConsequenceLevel,
    params: Vec<ParamSpec>,
    provenance: ManifestProvenance,
}

impl CapabilityManifest {
    pub fn tool(&self) -> &ToolId {
        &self.tool
    }
    pub fn paths(&self) -> &[PathGlob] {
        &self.paths
    }
    pub fn hosts(&self) -> &[HostPattern] {
        &self.hosts
    }
    pub fn creds(&self) -> &[CredentialId] {
        &self.creds
    }
    pub fn consequence(&self) -> ConsequenceLevel {
        self.consequence
    }
    pub fn params(&self) -> &[ParamSpec] {
        &self.params
    }
    pub fn provenance(&self) -> ManifestProvenance {
        self.provenance
    }

    /// The declared role of a named argument.
    ///
    /// **`None` means the manifest never declared this parameter**, which the adjudicator
    /// treats as a `Target` — an argument nobody declared is not an argument nobody checks.
    /// The alternative (defaulting an undeclared argument to `Payload`) would let a tool
    /// accept a target it never listed and skip provenance checking on it.
    pub fn role_of(&self, param: &str) -> Option<ArgumentRole> {
        self.params.iter().find(|p| p.name == param).map(|p| p.role)
    }

    pub fn spec_of(&self, param: &str) -> Option<&ParamSpec> {
        self.params.iter().find(|p| p.name == param)
    }
}

/// What a manifest file or descriptor deserializes into, before validation.
///
/// `deny_unknown_fields` is not tidiness. A third-party manifest that writes
/// `provenance: "first_party"` must fail **by name**, not be silently ignored — an ignored
/// key is an attempt that left no trace.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawManifest {
    pub tool: ToolId,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub creds: Vec<String>,
    /// Absent means maximum. See the module header.
    #[serde(default)]
    pub consequence: Option<ConsequenceLevel>,
    #[serde(default)]
    pub params: Vec<RawParamSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawParamSpec {
    pub name: String,
    /// See [`ParamSpec::description`]. `#[serde(default)]` so a manifest arriving as bytes -- an
    /// MCP descriptor -- simply has none, rather than failing to deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// **`None` is an error, never a default.** §7.3: *"no silent Payload default"*. A
    /// parameter whose role nobody stated is a parameter whose provenance nobody checks.
    #[serde(default)]
    pub role: Option<ArgumentRole>,
    pub ty: ParamType,
    /// See [`ParamSpec::required`]. Defaults to **optional**, which is the safe direction: a
    /// parameter wrongly called optional produces a tool error the model can read, while one
    /// wrongly called required makes it invent a value it should not have supplied.
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// A registration arrived with no manifest at all. The system does not start.
    #[error(
        "tool `{tool}` has no capability manifest. An unannotated tool cannot be registered: \
         declare paths, hosts, creds, consequence and a role for every parameter"
    )]
    MissingManifest { tool: ToolId },

    #[error(
        "tool `{tool}` is third-party and declares consequence `{declared:?}`, below \
         `consequential`. Third-party code cannot self-declare its way down"
    )]
    ThirdPartySelfDeclaredLow { tool: ToolId, declared: ConsequenceLevel },

    #[error(
        "tool `{tool}` parameter `{param}` has no declared role. Every parameter is `target` \
         or `payload`; there is no default, because the default would decide which arguments \
         untrusted content may shape"
    )]
    UnroledParameter { tool: ToolId, param: String },

    #[error("tool `{tool}` declares parameter `{param}` twice")]
    DuplicateParameter { tool: ToolId, param: String },

    #[error("tool `{tool}` declares a parameter with an empty name")]
    EmptyParameterName { tool: ToolId },

    #[error("manifest for `{tool}` is malformed: {detail}")]
    Malformed { tool: ToolId, detail: String },
}

/// CONTRACTS.md §7.3's `load`, with `provenance` supplied by the caller.
///
/// The order of the checks is the order in the pinned pseudocode, and the early default is
/// the pinned default. Nothing here is a judgment call; where this function differs from the
/// contract it is a defect.
pub fn load(
    raw: RawManifest,
    provenance: ManifestProvenance,
) -> Result<CapabilityManifest, LoadError> {
    let tool = raw.tool.clone();

    // Absent => maximum. Not an error: a first-party tool that forgot the field gets the
    // strictest treatment, which is loud at approval time rather than fatal at boot.
    let consequence = raw.consequence.unwrap_or(ConsequenceLevel::Irreversible);

    if provenance == ManifestProvenance::ThirdParty && consequence < ConsequenceLevel::Consequential
    {
        return Err(LoadError::ThirdPartySelfDeclaredLow { tool, declared: consequence });
    }

    let mut params: Vec<ParamSpec> = Vec::with_capacity(raw.params.len());
    for p in raw.params {
        if p.name.trim().is_empty() {
            return Err(LoadError::EmptyParameterName { tool });
        }
        if params.iter().any(|seen| seen.name == p.name) {
            return Err(LoadError::DuplicateParameter { tool, param: p.name });
        }
        let Some(role) = p.role else {
            return Err(LoadError::UnroledParameter { tool, param: p.name });
        };
        params.push(ParamSpec { name: p.name, role, ty: p.ty, required: p.required, description: p.description });
    }
    // `tool` survives the loop because every branch above returns; the moves are terminal.

    Ok(CapabilityManifest {
        tool,
        paths: raw.paths.into_iter().map(PathGlob::new).collect(),
        hosts: raw.hosts.into_iter().map(HostPattern::new).collect(),
        creds: raw.creds.into_iter().map(CredentialId::new).collect(),
        consequence,
        params,
        provenance,
    })
}

/// Deserializing a manifest routes through [`load`] with `ThirdParty` provenance.
///
/// **That default is deliberately the strictest one.** Anything arriving as serialized bytes
/// came from outside this binary; a deserialization path that assumed `FirstParty` would be a
/// second door into the check the module header exists to protect. First-party manifests are
/// built in code (see [`crate::builtin`]) and never travel this path.
impl<'de> Deserialize<'de> for CapabilityManifest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawManifest::deserialize(d)?;
        load(raw, ManifestProvenance::ThirdParty).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(tool: &str) -> RawManifest {
        RawManifest {
            tool: ToolId::new(tool),
            paths: vec![],
            hosts: vec![],
            creds: vec![],
            consequence: None,
            params: vec![],
        }
    }

    #[test]
    fn an_absent_consequence_loads_as_the_maximum() {
        let m = load(raw("t"), ManifestProvenance::FirstParty).expect("first-party, no params");
        assert_eq!(m.consequence(), ConsequenceLevel::Irreversible);
    }

    #[test]
    fn third_party_cannot_self_declare_below_consequential() {
        for declared in [ConsequenceLevel::Inert, ConsequenceLevel::Reversible] {
            let mut r = raw("downloaded");
            r.consequence = Some(declared);
            assert_eq!(
                load(r, ManifestProvenance::ThirdParty),
                Err(LoadError::ThirdPartySelfDeclaredLow {
                    tool: ToolId::new("downloaded"),
                    declared
                })
            );
        }
    }

    #[test]
    fn a_parameter_without_a_role_is_a_load_error() {
        let mut r = raw("t");
        r.params = vec![RawParamSpec { name: "path".into(), role: None, ty: ParamType::Path, required: true, description: None }];
        assert_eq!(
            load(r, ManifestProvenance::FirstParty),
            Err(LoadError::UnroledParameter {
                tool: ToolId::new("t"),
                param: "path".into()
            })
        );
    }

    #[test]
    fn a_manifest_cannot_declare_its_own_provenance() {
        // The hole this closes: a third-party skill writing `provenance: first_party` would
        // walk past ThirdPartySelfDeclaredLow, and the check would still be present, still be
        // tested, and simply never fire. `deny_unknown_fields` makes the attempt a named
        // parse failure instead of a silently dropped key.
        let json = r#"{"tool":"evil","consequence":"inert","provenance":"first_party"}"#;
        let err = serde_json::from_str::<RawManifest>(json).unwrap_err().to_string();
        assert!(
            err.contains("provenance"),
            "the refusal must name the field that was rejected, got: {err}"
        );
    }

    #[test]
    fn deserializing_a_manifest_is_third_party() {
        // Bytes came from outside the binary. Anything else here would be a second door into
        // the self-declaration check.
        let json = r#"{"tool":"mcp:thing","consequence":"consequential"}"#;
        let m: CapabilityManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.provenance(), ManifestProvenance::ThirdParty);

        let low = r#"{"tool":"mcp:thing","consequence":"inert"}"#;
        assert!(
            serde_json::from_str::<CapabilityManifest>(low).is_err(),
            "the serde path must run the same validation as load()"
        );
    }

    #[test]
    fn an_undeclared_parameter_has_no_role() {
        // `role_of` returning None is what the adjudicator reads as "treat as Target". The
        // test pins the absence, because a future `unwrap_or(Payload)` here would be
        // invisible: every declared parameter would still be checked correctly.
        let m = load(raw("t"), ManifestProvenance::FirstParty).unwrap();
        assert_eq!(m.role_of("surprise"), None);
    }

    #[test]
    fn consequence_ordering_is_the_one_the_checks_read() {
        assert!(ConsequenceLevel::Inert < ConsequenceLevel::Reversible);
        assert!(ConsequenceLevel::Reversible < ConsequenceLevel::Consequential);
        assert!(ConsequenceLevel::Consequential < ConsequenceLevel::Irreversible);
    }
}
