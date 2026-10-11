//! `SKILL.md` loading and **progressive disclosure**. CONTRACTS §7.1, ADR-051.
//!
//! # The one property this module exists for
//!
//! §7.1: *"Only `description` + `trigger_phrases` are embedded for semantic discovery. Embedding
//! full instruction prose pollutes the vector space."*
//!
//! So a [`Skill`] holds the manifest and **not the body**. The body is a [`BodyRef`] — a path and
//! a byte count — and reading it is a separate, explicit call made when the model actually asks
//! for the skill. That is not an optimisation. Two hundred installed skills whose instructions all
//! sat in memory and all embedded into one index would make the discovery index worse at the exact
//! job it exists for, and the model's window would carry prose for skills it is not using.
//!
//! **The type is what enforces it.** There is no `body: String` field to accidentally read, and
//! `registry.rs`'s budget note has nowhere to leak into. A `Skill` that had loaded its own body
//! eagerly would still satisfy every assertion about *what gets embedded* while defeating the
//! reason for the rule.
//!
//! # Skills are registered, never exposed
//!
//! §7.2 splits registration from exposure and ARCHITECTURE §5 budgets exposure at twelve. A skill
//! **consumes no slot**: the model reaches every installed skill through the single `use` tool,
//! which is one of ADR-006's eleven and has been registered-and-unrunnable since M2 Session A.
//! `ExposureError::TooMany` already says so in its own message — *"expose fewer and let the model
//! reach the rest through `use`"*.
//!
//! # Provenance: an installed skill is `UserReviewed`, not `ThirdParty`
//!
//! **This resolves a contradiction inside the pinned contract, and it is worth stating plainly
//! because the alternative was to change a pinned schema.**
//!
//! §7.1's own example declares `consequence: reversible`. §7.3's `load` refuses any `ThirdParty`
//! manifest below `Consequential` — *"third-party code cannot self-declare its way down"* — and
//! `Transport::manifest_provenance` maps everything that is not `Builtin` to `ThirdParty`. Read
//! together, **the pinned example cannot load.**
//!
//! `ManifestProvenance::UserReviewed { at }` is the third variant, pinned in §7.3 since it was
//! written and **constructed nowhere in the workspace** — a declared shape with no producer. It is
//! exactly the right one: installing a skill into the profile IS the review, which is the same
//! reasoning ADR-052 gives for MCP servers being trusted. The user chose it, placed it, and can
//! read it; the agent cannot install one on its own initiative.
//!
//! So: a skill loaded from disk is `UserReviewed { at }`, and §7.1's example loads as written.
//! Nothing in the contract changed. `at` is supplied by the caller from a fenced clock and means
//! *when Marlowe observed this installed* — see [`scan`] for why it is not the file's mtime.
//!
//! # The signature field is parsed and is NOT verified
//!
//! §7.1 pins `signature: "ed25519:..."`. There is no key infrastructure in this project, no
//! trusted publisher set, and nothing to check a signature against. The field is therefore parsed
//! into [`DeclaredSignature`] — a name chosen so that no reader can mistake it for a checked one —
//! and [`Skill::signature`] is the only accessor.
//!
//! **There is deliberately no `verify` function, not even one returning `false`.** A declared
//! control that nothing reads is this repository's sixteenth logged defect (`web`'s
//! `inline_threshold_bytes: 0`, asserted by a green test, read by no code). A `verify` that always
//! refused would be worse: it would make every signed skill fail to load, so it would be deleted,
//! and its absence would then read as "signatures are fine".
//!
//! `signature_is_declared_but_never_verified` asserts the absence.

use std::path::{Path, PathBuf};

use crate::frontmatter::{self, ParseError, Value};
use crate::manifest::{
    load, CapabilityManifest, ConsequenceLevel, LoadError, ManifestProvenance, RawManifest, ToolId,
};
use crate::registry::{Description, MAX_DESCRIPTION_CHARS};

/// The filename the standard specifies. Not configurable.
pub const SKILL_FILE: &str = "SKILL.md";

/// A skill's body is loaded on use, and it is bounded when it is.
///
/// Large enough for a substantial instruction document, small enough that a hostile or accidental
/// multi-megabyte file cannot be pulled into a context window whole. A body over the cap is a
/// **load-time refusal**, not a truncation: a skill whose instructions were silently cut in half
/// would run with half a procedure, which is worse than not running.
pub const MAX_BODY_BYTES: u64 = 64 * 1024;

/// §7.1's discovery budget. Phrases are embedded, so an unbounded list is an unbounded index.
pub const MAX_TRIGGER_PHRASES: usize = 16;

/// A skill's identity. Distinct from [`ToolId`] on purpose — a skill is not a tool, it does not
/// occupy an exposure slot, and the model reaches it through `use` rather than by name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct SkillId(String);

impl SkillId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A signature as **declared** by the manifest. Nothing verifies it. See the module header.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeclaredSignature {
    /// The part before the `:` — `ed25519` in §7.1's example.
    pub algorithm: String,
    /// The part after. Opaque bytes as text; this project has nothing to check it against.
    pub value: String,
}

/// Where a skill's instructions live. **Not the instructions.** See the module header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyRef {
    path: PathBuf,
    bytes: u64,
}

impl BodyRef {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Read the instructions. **The disclosure step** — called when the model asks for this
    /// skill, never at discovery time.
    ///
    /// Re-reads the front matter and returns only what follows it, so the header never reaches
    /// the model's window: it is metadata about the skill, not instruction to it, and a
    /// `capability` block in the context reads as something the model may negotiate.
    pub fn read(&self) -> Result<String, SkillError> {
        let raw = std::fs::read_to_string(&self.path)
            .map_err(|e| SkillError::Unreadable { path: self.path.clone(), detail: e.to_string() })?;
        let (_, body) = frontmatter::split(&raw)
            .map_err(|e| SkillError::Frontmatter { path: self.path.clone(), source: e })?;
        Ok(body.trim_start_matches(['\n', '\r']).to_string())
    }
}

/// One installed skill: its manifest, and a reference to its body.
#[derive(Debug, Clone)]
pub struct Skill {
    id: SkillId,
    description: Description,
    trigger_phrases: Vec<String>,
    manifest: CapabilityManifest,
    signature: Option<DeclaredSignature>,
    body: BodyRef,
}

impl Skill {
    pub fn id(&self) -> &SkillId {
        &self.id
    }

    pub fn description(&self) -> &Description {
        &self.description
    }

    pub fn trigger_phrases(&self) -> &[String] {
        &self.trigger_phrases
    }

    pub fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    /// **Declared, never verified.** See the module header.
    pub fn signature(&self) -> Option<&DeclaredSignature> {
        self.signature.as_ref()
    }

    pub fn body(&self) -> &BodyRef {
        &self.body
    }

    /// Exactly what §7.1 permits into the discovery index: the description and the trigger
    /// phrases, and **not one character of the body**.
    ///
    /// One function, so the embedding site and any test of the embedding site cannot come to
    /// disagree about what is in it — the same reasoning as `blocks_composed_targets`.
    pub fn discovery_text(&self) -> String {
        let mut s = String::with_capacity(MAX_DESCRIPTION_CHARS);
        s.push_str(self.description.text());
        for p in &self.trigger_phrases {
            s.push_str(" \u{b7} ");
            s.push_str(p);
        }
        s
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill at `{path}`: {source}")]
    Frontmatter { path: PathBuf, source: ParseError },

    #[error("skill at `{path}` cannot be read: {detail}")]
    Unreadable { path: PathBuf, detail: String },

    #[error(
        "skill at `{path}` has no `{field}`. The Agent Skills standard requires it, and a \
         defaulted value would be a declaration nobody wrote"
    )]
    MissingField { path: PathBuf, field: &'static str },

    #[error("skill at `{path}`: `{field}` is {found}, expected {expected}")]
    WrongShape { path: PathBuf, field: String, found: &'static str, expected: &'static str },

    #[error(
        "skill `{id}` at `{path}` declares the unknown key `x-marlowe.{key}`. The Marlowe block \
         is a closed vocabulary: an unrecognised key is a declaration this build does not \
         enforce, and silently ignoring it would let a skill ship a capability nobody applies"
    )]
    UnknownExtensionKey { id: SkillId, path: PathBuf, key: String },

    #[error(
        "skill `{id}` at `{path}` declares consequence `{declared}`, which is not one of \
         inert, reversible, consequential, irreversible"
    )]
    UnknownConsequence { id: SkillId, path: PathBuf, declared: String },

    #[error(
        "skill `{id}` at `{path}` declares a signature `{raw}` that is not `<algorithm>:<value>`. \
         NOTE: Marlowe does not verify skill signatures at all -- see `skill.rs`. This refusal is \
         about the field being well-formed, and must not be read as a signature check"
    )]
    MalformedSignature { id: SkillId, path: PathBuf, raw: String },

    #[error(
        "skill `{id}` at `{path}` has a {bytes}-byte body; the cap is {MAX_BODY_BYTES}. It is \
         refused rather than truncated: half a procedure is worse than none"
    )]
    BodyTooLarge { id: SkillId, path: PathBuf, bytes: u64 },

    #[error(
        "skill `{id}` at `{path}` declares {got} trigger phrases; the cap is \
         {MAX_TRIGGER_PHRASES}. Phrases are embedded for discovery (7.1), so an unbounded list \
         is an unbounded index"
    )]
    TooManyTriggerPhrases { id: SkillId, path: PathBuf, got: usize },

    #[error("skill `{id}` at `{path}`: {source}")]
    Manifest { id: SkillId, path: PathBuf, source: LoadError },

    #[error(
        "skill `{id}` at `{path}` has the same name as the skill already loaded from `{first}`. \
         Two skills answering to one name make `use` ambiguous"
    )]
    Duplicate { id: SkillId, path: PathBuf, first: PathBuf },
}

fn consequence_from(s: &str) -> Option<ConsequenceLevel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "inert" => Some(ConsequenceLevel::Inert),
        "reversible" => Some(ConsequenceLevel::Reversible),
        "consequential" => Some(ConsequenceLevel::Consequential),
        "irreversible" => Some(ConsequenceLevel::Irreversible),
        _ => None,
    }
}

/// Load one `SKILL.md`.
///
/// `reviewed_at` is when the user installed it — the timestamp that goes into
/// [`ManifestProvenance::UserReviewed`]. It is a parameter rather than a call to a clock because
/// this crate has no clock and §2 forbids one outside the fences.
pub fn load_skill(path: &Path, source: &str, reviewed_at: i64) -> Result<Skill, SkillError> {
    let (front, body) = frontmatter::split(source)
        .map_err(|e| SkillError::Frontmatter { path: path.to_path_buf(), source: e })?;
    let front = frontmatter::parse(front)
        .map_err(|e| SkillError::Frontmatter { path: path.to_path_buf(), source: e })?;

    let scalar = |key: &'static str| -> Result<String, SkillError> {
        let v = front
            .get(key)
            .ok_or(SkillError::MissingField { path: path.to_path_buf(), field: key })?;
        v.as_scalar().map(str::to_string).ok_or_else(|| SkillError::WrongShape {
            path: path.to_path_buf(),
            field: key.to_string(),
            found: v.kind(),
            expected: "a scalar",
        })
    };

    let id = SkillId(scalar("name")?);
    if id.0.trim().is_empty() {
        return Err(SkillError::MissingField { path: path.to_path_buf(), field: "name" });
    }
    let description = Description::new(&scalar("description")?);

    // ── the Marlowe block ──────────────────────────────────────────────────────────────────
    //
    // **Absent is not an error.** §7.1 is "the standard, unmodified where the standard specifies
    // it", and `x-marlowe` is ours. A skill authored for another harness must still load — it
    // simply gets the strictest capability, exactly as §7.3's absent `consequence` does.
    let empty = std::collections::BTreeMap::new();
    let x = match front.get("x-marlowe") {
        Some(v) => v.as_map().ok_or_else(|| SkillError::WrongShape {
            path: path.to_path_buf(),
            field: "x-marlowe".to_string(),
            found: v.kind(),
            expected: "a block",
        })?,
        None => &empty,
    };

    // A closed vocabulary. See `UnknownExtensionKey` for why this refuses rather than ignores.
    for key in x.keys() {
        if !matches!(
            key.as_str(),
            "capability" | "consequence" | "trigger_phrases" | "signature"
        ) {
            return Err(SkillError::UnknownExtensionKey {
                id,
                path: path.to_path_buf(),
                key: key.clone(),
            });
        }
    }

    let cap = match x.get("capability") {
        Some(v) => v.as_map().ok_or_else(|| SkillError::WrongShape {
            path: path.to_path_buf(),
            field: "x-marlowe.capability".to_string(),
            found: v.kind(),
            expected: "a block",
        })?,
        None => &empty,
    };

    let list = |m: &std::collections::BTreeMap<String, Value>,
                key: &str|
     -> Result<Vec<String>, SkillError> {
        match m.get(key) {
            None => Ok(Vec::new()),
            Some(v) => v.as_seq().ok_or_else(|| SkillError::WrongShape {
                path: path.to_path_buf(),
                field: key.to_string(),
                found: v.kind(),
                expected: "a list",
            }),
        }
    };

    let paths = list(cap, "paths")?;
    let hosts = list(cap, "hosts")?;
    let creds = list(cap, "creds")?;

    // §7.1: "REQUIRED. Absent => Irreversible." An absent field is the maximum, and `load` does
    // that itself when handed `None` — so the absence is passed through rather than re-decided
    // here, and there is one definition of what absent means.
    let consequence = match x.get("consequence") {
        None => None,
        Some(v) => {
            let raw = v.as_scalar().ok_or_else(|| SkillError::WrongShape {
                path: path.to_path_buf(),
                field: "x-marlowe.consequence".to_string(),
                found: v.kind(),
                expected: "a scalar",
            })?;
            Some(consequence_from(raw).ok_or_else(|| SkillError::UnknownConsequence {
                id: id.clone(),
                path: path.to_path_buf(),
                declared: raw.to_string(),
            })?)
        }
    };

    let trigger_phrases: Vec<String> = list(x, "trigger_phrases")?
        .into_iter()
        // Sanitised for the same reason a description is: these are user-visible in `/skills` and
        // model-visible through discovery. `marlowe_contract::text` is the one definition.
        .map(|p| marlowe_contract::text::sanitize_line(&p).into_owned())
        .filter(|p| !p.trim().is_empty())
        .collect();
    if trigger_phrases.len() > MAX_TRIGGER_PHRASES {
        return Err(SkillError::TooManyTriggerPhrases {
            id,
            path: path.to_path_buf(),
            got: trigger_phrases.len(),
        });
    }

    let signature = match x.get("signature") {
        None => None,
        Some(v) => {
            let raw = v.as_scalar().unwrap_or_default();
            match raw.split_once(':') {
                Some((alg, val)) if !alg.trim().is_empty() && !val.trim().is_empty() => {
                    Some(DeclaredSignature {
                        algorithm: alg.trim().to_string(),
                        value: val.trim().to_string(),
                    })
                }
                _ => {
                    return Err(SkillError::MalformedSignature {
                        id,
                        path: path.to_path_buf(),
                        raw: raw.to_string(),
                    })
                }
            }
        }
    };

    let bytes = body.len() as u64;
    if bytes > MAX_BODY_BYTES {
        return Err(SkillError::BodyTooLarge { id, path: path.to_path_buf(), bytes });
    }

    // **`UserReviewed`, not `ThirdParty`.** See the module header — this is what makes §7.1's own
    // example loadable, and it is the same reasoning ADR-052 applies to MCP servers.
    let manifest = load(
        RawManifest {
            tool: ToolId::new(id.as_str()),
            paths,
            hosts,
            creds,
            consequence,
            params: vec![],
        },
        ManifestProvenance::UserReviewed { at: reviewed_at },
    )
    .map_err(|e| SkillError::Manifest { id: id.clone(), path: path.to_path_buf(), source: e })?;

    Ok(Skill {
        id,
        description,
        trigger_phrases,
        manifest,
        signature,
        body: BodyRef { path: path.to_path_buf(), bytes },
    })
}

/// Every installed skill, by id.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: std::collections::BTreeMap<SkillId, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, skill: Skill) -> Result<(), SkillError> {
        if let Some(first) = self.skills.get(&skill.id) {
            return Err(SkillError::Duplicate {
                id: skill.id.clone(),
                path: skill.body.path.clone(),
                first: first.body.path.clone(),
            });
        }
        self.skills.insert(skill.id.clone(), skill);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.get(&SkillId(id.to_string()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Scan `<root>/<name>/SKILL.md` for every immediate subdirectory of `root`.
///
/// **A malformed skill does not stop the scan, and it is not silent either.** Returns the
/// registry alongside the refusals, so the surface can say *"three skills loaded, one refused,
/// here is why"*. Failing the whole scan would let one bad file take away every other skill; a
/// silent skip would leave the user wondering where their skill went. Both are worse than a list.
///
/// A missing `root` is an empty registry with no errors — a profile with no skills directory has
/// no skills, which is not a fault.
///
/// # `reviewed_at` is a parameter, and the determinism guard is why
///
/// The obvious implementation reads each file's modification time, which is closer to "when the
/// user installed it". It is also **a real clock read outside §4.5's fences**, and
/// `determinism_guard::the_only_real_clock_read_is_the_latency_fence` refused it — correctly, on
/// the first workspace run after it was written. A stray clock read makes staleness half-life
/// unmeasurable and every decay-dependent result irreproducible, and `UserReviewed { at }` is a
/// timestamp that reaches a manifest.
///
/// So the caller supplies one timestamp for the whole scan, from the daemon's fenced clock, and
/// `at` means **when Marlowe observed these skills installed** rather than when the file was
/// written. That is a slightly weaker fact, stated rather than approximated.
pub fn scan(root: &Path, reviewed_at: i64) -> (SkillRegistry, Vec<SkillError>) {
    let mut registry = SkillRegistry::new();
    let mut errors = Vec::new();

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return (registry, errors),
    };

    // `read_dir` order is filesystem order, which differs between machines. Sorted, so that
    // which of two colliding names is reported as the duplicate is the same everywhere.
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let file = dir.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        match std::fs::read_to_string(&file) {
            Err(e) => errors.push(SkillError::Unreadable {
                path: file.clone(),
                detail: e.to_string(),
            }),
            Ok(source) => match load_skill(&file, &source, reviewed_at) {
                Ok(skill) => {
                    if let Err(e) = registry.insert(skill) {
                        errors.push(e);
                    }
                }
                Err(e) => errors.push(e),
            },
        }
    }

    (registry, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = "---\n\
name: pdf-report\n\
description: Generate a cited PDF report from a findings set.\n\
x-marlowe:\n\
\x20 capability:\n\
\x20   paths:  [\"./out/**\"]\n\
\x20   hosts:  []\n\
\x20   creds:  []\n\
\x20 consequence: reversible\n\
\x20 trigger_phrases: [\"write a report\", \"make a pdf\"]\n\
\x20 signature: \"ed25519:abc\"\n\
---\n\
Step one. Step two.\n";

    fn p() -> PathBuf {
        PathBuf::from("skills/pdf-report/SKILL.md")
    }

    /// **The contract's own example, loading as written.** Until `UserReviewed` was used here it
    /// could not: §7.3 refuses a `ThirdParty` manifest below `Consequential`, and `reversible` is
    /// below it. See the module header.
    #[test]
    fn the_pinned_example_loads_including_its_reversible_consequence() {
        let s = load_skill(&p(), PINNED, 1_700_000_000_000).expect("§7.1's example must load");
        assert_eq!(s.id().as_str(), "pdf-report");
        assert_eq!(s.manifest().consequence(), ConsequenceLevel::Reversible);
        assert!(matches!(
            s.manifest().provenance(),
            ManifestProvenance::UserReviewed { at: 1_700_000_000_000 }
        ));
        assert_eq!(s.trigger_phrases(), ["write a report", "make a pdf"]);
        assert_eq!(
            s.signature().map(|g| g.algorithm.as_str()),
            Some("ed25519"),
            "the signature is PARSED"
        );
    }

    /// Progressive disclosure, asserted on the type rather than on a promise.
    #[test]
    fn the_discovery_text_contains_no_word_of_the_body() {
        let s = load_skill(&p(), PINNED, 0).unwrap();
        let d = s.discovery_text();
        assert!(d.contains("Generate a cited PDF report"));
        assert!(d.contains("write a report"), "trigger phrases ARE embedded (§7.1)");
        for word in ["Step", "one.", "two."] {
            assert!(
                !d.contains(word),
                "`{word}` came from the body and reached the discovery text. §7.1: embedding \
                 full instruction prose pollutes the vector space"
            );
        }
        // The control: the body really does contain those words, so the assertion above is
        // about the split and not about a body that was empty.
        assert!(PINNED.contains("Step one. Step two."));
    }

    /// The other half of progressive disclosure: the body is reachable, on purpose, later.
    #[test]
    fn the_body_is_a_reference_and_reading_it_returns_the_instructions_without_the_header() {
        let dir = tempdir("body-read");
        let file = dir.join("SKILL.md");
        std::fs::write(&file, PINNED).unwrap();
        let s = load_skill(&file, PINNED, 0).unwrap();

        assert_eq!(s.body().bytes(), "Step one. Step two.\n".len() as u64);
        let body = s.body().read().expect("the body reads");
        assert_eq!(body.trim(), "Step one. Step two.");
        assert!(
            !body.contains("capability") && !body.contains("consequence"),
            "the front matter reached the model's window. A capability block in the context \
             reads as something the model may negotiate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **No verifier exists, deliberately.** See the module header on why a `verify` returning
    /// `false` would be worse than nothing.
    #[test]
    fn signature_is_declared_but_never_verified() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/skill.rs"))
            .expect("this module reads itself");

        // **The needles are assembled at runtime rather than written as literals.** Spelled out
        // in the source they would appear in the very file being scanned, and this test would
        // fail against itself — which it did, on its first run. A source-scanning guard has to
        // stay out of its own haystack.
        let fun = "fn ";
        for suffix in ["verify", "verify_signature", "is_signed", "check_signature"] {
            let forbidden = format!("{fun}{suffix}");
            assert!(
                !src.contains(&forbidden),
                "`{forbidden}` appeared in skill.rs. This project has no key infrastructure and \
                 nothing to check a signature against; a verifier here would be a declared \
                 control with nothing behind it, and one that always returned false would be \
                 deleted for breaking every signed skill — after which its absence would read \
                 as `signatures are fine`. If signing is being built, delete this assertion \
                 deliberately and say so in an ADR"
            );
        }

        // **The control.** Without it this test passes when `read_to_string` returns something
        // that is not this file, or when the scan silently looks at an empty string.
        assert!(
            src.contains("pub struct DeclaredSignature"),
            "the scan is not reading skill.rs"
        );
        assert!(
            src.contains(&format!("{fun}load_skill")),
            "the needle construction does not match real code in this file, so the assertions \
             above would pass against any content at all"
        );
    }

    #[test]
    fn a_skill_from_another_harness_loads_with_the_strictest_capability() {
        // No `x-marlowe` at all — §7.1 is the standard "unmodified where the standard specifies
        // it", so a foreign skill must load rather than refuse.
        let src = "---\nname: foreign\ndescription: Does something.\nlicense: MIT\n---\nBody.\n";
        let s = load_skill(&p(), src, 0).expect("a standard skill with no Marlowe block loads");
        assert_eq!(
            s.manifest().consequence(),
            ConsequenceLevel::Irreversible,
            "absent => maximum, per §7.3"
        );
        assert!(s.trigger_phrases().is_empty());
        assert!(s.signature().is_none());
    }

    #[test]
    fn an_unknown_key_inside_the_marlowe_block_refuses() {
        let src = "---\nname: a\ndescription: d\nx-marlowe:\n  autonomy: full\n---\nb\n";
        assert!(matches!(
            load_skill(&p(), src, 0),
            Err(SkillError::UnknownExtensionKey { .. })
        ));
        // The control: the same file without the unknown key loads, so the refusal is about the
        // key rather than about the block.
        let ok = "---\nname: a\ndescription: d\nx-marlowe:\n  consequence: inert\n---\nb\n";
        assert!(load_skill(&p(), ok, 0).is_ok());
    }

    #[test]
    fn the_required_standard_fields_are_required() {
        for (missing, src) in [
            ("name", "---\ndescription: d\n---\nb\n"),
            ("description", "---\nname: a\n---\nb\n"),
        ] {
            match load_skill(&p(), src, 0) {
                Err(SkillError::MissingField { field, .. }) => assert_eq!(field, missing),
                other => panic!("expected a missing `{missing}`, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_oversized_body_refuses_rather_than_truncating() {
        let big = format!(
            "---\nname: a\ndescription: d\n---\n{}\n",
            "x".repeat(MAX_BODY_BYTES as usize + 1)
        );
        assert!(matches!(load_skill(&p(), &big, 0), Err(SkillError::BodyTooLarge { .. })));
    }

    #[test]
    fn a_description_from_a_skill_is_sanitised_like_any_other() {
        let src = "---\nname: a\ndescription: \"safe\\u202Eevil\"\n---\nb\n";
        let s = load_skill(&p(), src, 0).unwrap();
        assert!(!s.description().text().contains('\u{202e}'));
        assert!(s.description().text().contains("<U+202E>"));
    }

    #[test]
    fn a_trigger_phrase_is_sanitised_too() {
        let src = "---\nname: a\ndescription: d\nx-marlowe:\n  trigger_phrases: [\"a\\u202Eb\"]\n---\nb\n";
        let s = load_skill(&p(), src, 0).unwrap();
        assert!(!s.trigger_phrases()[0].contains('\u{202e}'));
    }

    #[test]
    fn two_skills_with_one_name_collide_by_name() {
        let mut r = SkillRegistry::new();
        r.insert(load_skill(&PathBuf::from("a/SKILL.md"), PINNED, 0).unwrap()).unwrap();
        assert!(matches!(
            r.insert(load_skill(&PathBuf::from("b/SKILL.md"), PINNED, 0).unwrap()),
            Err(SkillError::Duplicate { .. })
        ));
    }

    #[test]
    fn a_scan_reports_the_bad_skill_and_keeps_the_good_ones() {
        let root = tempdir("scan");
        for (name, body) in [
            ("good", PINNED),
            ("bad", "not a skill file at all\n"),
            ("also-good", "---\nname: two\ndescription: d\n---\nb\n"),
        ] {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(SKILL_FILE), body).unwrap();
        }
        let (reg, errors) = scan(&root, 0);
        assert_eq!(reg.len(), 2, "one bad file must not take the others with it");
        assert_eq!(errors.len(), 1, "and it must not vanish silently either");
        assert!(reg.get("pdf-report").is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_with_no_skills_directory_is_not_a_fault() {
        let (reg, errors) = scan(&PathBuf::from("definitely/not/a/real/directory"), 0);
        assert!(reg.is_empty() && errors.is_empty());
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-skill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
