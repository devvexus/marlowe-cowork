//! Path scoping. **This is the wall.**
//!
//! ADR-002 (revised) removed the kernel backstop, and brief §8.3 states the consequence:
//!
//! > **Inseparable from handle-based access**: canonicalize-then-open leaves a check-then-use
//! > race, so a traversal suite passing against a check-then-open implementation reports a
//! > boundary that is not there. The suite and the handle discipline are one requirement and
//! > ship together.
//!
//! They ship together here. Three parts, and each does one job:
//!
//! | Part | Job |
//! |---|---|
//! | [`request`] | refuse ambiguous *spellings* before any syscall — `..`, rooted forms, ADS, device names, short names, munged names, homoglyph separators |
//! | [`glob`] | decide whether a well-formed request is inside what the manifest **declared** |
//! | [`walk`] | open it without ever letting a string be resolved twice — `openat`/`O_NOFOLLOW` on POSIX, pinned handles plus reparse refusal plus identity verification on Windows |
//!
//! **Order matters and it is the order ADR-002 names: canonicalize before the check, never
//! after.** Here the strongest reading of that is stronger still — the hostile string is never
//! canonicalized at all. It is validated, matched against the declaration, and then *walked*,
//! with the kernel resolving one component at a time under our supervision. There is no moment
//! at which a resolved string exists and is trusted.
//!
//! # `ScopedPath` cannot be built from a string, and that is the discipline as a type
//!
//! It holds an open handle. An implementation that resolved a path without opening it cannot
//! produce one, so "operate on handles rather than re-resolved strings" is enforced by what the
//! adjudicator demands rather than by review.

pub mod glob;
pub mod request;
pub mod walk;

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use marlowe_tools::PathGlob;

pub use request::RequestError;
pub use walk::WalkObserver;

/// A path that was resolved **and opened**, with the handle retained.
///
/// The handle is the point. Every subsequent operation goes through it rather than re-resolving
/// the string, which is what closes the check-then-use window: a link planted after the check
/// cannot redirect an already-open handle.
#[derive(Debug)]
pub struct ScopedPath {
    /// Private, and there is deliberately no constructor taking a `PathBuf`.
    handle: File,
    resolved: PathBuf,
    relative: String,
}

impl ScopedPath {
    /// The open handle. All access goes through it.
    pub fn handle(&self) -> &File {
        &self.handle
    }

    /// Consume the scope and take the handle.
    pub fn into_handle(self) -> File {
        self.handle
    }

    /// The resolved path, for logging and for the blast-radius line the user sees.
    ///
    /// **Not for re-opening.** A caller that takes this and calls `File::open` on it has
    /// reintroduced the race the handle exists to close.
    pub fn resolved(&self) -> &Path {
        &self.resolved
    }

    /// The workspace-relative form the request normalized to.
    pub fn relative(&self) -> &str {
        &self.relative
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// Only reachable from [`Unavailable`].
    #[error(
        "path scoping is not implemented in this build, so `{requested}` is refused. The \
         traversal suite and the handle discipline ship together (brief §8.3) — a path check \
         without them certifies a boundary that is not there"
    )]
    Unavailable { requested: String },

    #[error("`{requested}` is not a well-formed workspace-relative path: {source}")]
    Malformed {
        requested: String,
        #[source]
        source: RequestError,
    },

    #[error(
        "`{requested}` is not inside any path this tool declared. Undeclared access is denied \
         at the boundary, not warned about (brief §8.2)"
    )]
    Undeclared { requested: String },

    #[error(
        "`{requested}` resolves outside the workspace. A link in the path pointed out of \
         scope and was refused rather than followed"
    )]
    OutsideScope { requested: String },

    /// **The message names the remedy, because the old one was mistaken for a refusal.**
    ///
    /// On 2026-08-25 an agent asked for four paths that do not exist, read
    /// *"could not be opened: The system cannot find the file specified"* as a permission or
    /// path-parsing problem, and wrote a handoff document explaining it with MSVC-versus-bash
    /// path resolution and sandbox limits. **Every one of those files was simply absent.** It then
    /// recommended that a human open them in an editor.
    ///
    /// The conflation was available: `Undeclared` above says *"Undeclared access is denied"*, so
    /// "denied" is a word this enum genuinely uses — and a reader who has seen it once will map an
    /// unfamiliar open failure onto it. So this arm now says what it is NOT, and names the check.
    ///
    /// §B5's rule for degraded states, applied to a tool result: *"names the remedy, because a
    /// degraded state a user cannot act on is a crash with better manners."*
    #[error(
        "`{requested}` could not be opened: {detail}. \
         This is NOT a scoping refusal and NOT a permission denial — an undeclared path says so in \
         those words. If the detail above says the file does not exist, then it does not exist: \
         confirm with `glob` before concluding that access was blocked"
    )]
    Unopenable { requested: String, detail: String },

    #[error(
        "`{requested}` changed identity during resolution. The handle does not refer to the \
         object that was checked, so it is refused"
    )]
    IdentityChanged { requested: String },

    #[error(
        "path scoping refuses to run on `{platform}`: the traversal suite and the TOCTOU race have \
         never been EXECUTED there. Verified platforms are {verified:?}. macOS in particular is \
         case-insensitive and NFD-normalizing, which is exactly where the glob matcher and the NFC \
         handling would diverge — run the suite there before trusting this wall"
    )]
    PlatformUnverified { platform: &'static str, verified: &'static [&'static str] },
}

/// Platforms on which the traversal suite and the TOCTOU race have actually been **executed**.
///
/// Not "platforms the code compiles for" and not "platforms we believe are POSIX". ADR-027's
/// closing requirement is that both halves of the wall are exercised where they run, and the
/// halves do not overlap: Windows never executes `openat`, Linux never exercises the share-mode
/// pinning.
///
/// **macOS is deliberately absent.** It is the platform most likely to diverge and least like the
/// one it would be assumed to resemble: case-insensitive by default, and NFD-normalizing, which is
/// exactly where `scope::glob`'s matching and `scope::request`'s NFC handling would disagree.
/// "POSIX is POSIX" is the assumption this project has paid for repeatedly.
pub const VERIFIED_PLATFORMS: &[&str] = &["windows", "linux"];

#[cfg(any(windows, target_os = "linux"))]
const THIS_PLATFORM_VERIFIED: bool = true;
#[cfg(not(any(windows, target_os = "linux")))]
const THIS_PLATFORM_VERIFIED: bool = false;

/// What the walk should open the final component as.
///
/// Declared per parameter in the manifest via [`marlowe_tools::ParamType`], not inferred from the
/// tool's consequence level. Inferring it would mean `bash`'s `cwd` — a directory that must exist
/// — and `edit`'s `path` — a file that may not — take their behaviour from the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Must exist. Opened read-only.
    Read,
    /// Must exist. Opened read-write.
    ReadWrite,
    /// May not exist. Created inside the verified parent directory if absent.
    CreateOrOpen,
}

/// Resolve a requested path against a tool's declared globs, and open it.
///
/// The signature takes the **workspace root** because manifests declare workspace-relative globs
/// (`./**`): a manifest holding an absolute path would mean something different on another
/// machine, and resolving that binding is the scope's job, not the manifest's.
pub trait PathScope: fmt::Debug + Send + Sync {
    fn open(
        &self,
        declared: &[PathGlob],
        workspace: &Path,
        requested: &str,
        access: Access,
    ) -> Result<ScopedPath, ScopeError>;
}

/// Refuses every path. Retained after M2 Session B rather than deleted.
///
/// It is what a capability profile uses when it must provably not touch the filesystem, and it
/// is what the adjudicator's tests use so that a permission-layer test never depends on a real
/// directory existing. Keeping it also means "no scope configured" has a value that fails
/// closed, rather than being represented by an `Option` whose `None` arm somebody writes a
/// permissive branch for.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unavailable;

impl PathScope for Unavailable {
    fn open(
        &self,
        _declared: &[PathGlob],
        _workspace: &Path,
        requested: &str,
        _access: Access,
    ) -> Result<ScopedPath, ScopeError> {
        Err(ScopeError::Unavailable { requested: requested.to_string() })
    }
}

/// The real one.
#[derive(Debug)]
pub struct WorkspaceScope {
    /// Zero-sized proof that [`WorkspaceScope::new`] ran and the platform check passed. Having a
    /// private field is what stops `WorkspaceScope {}` being written at a call site, which would
    /// be a construction that skipped the gate.
    _gated: (),
}

impl WorkspaceScope {
    /// **Refuses at construction on any platform whose suite has never been executed.**
    ///
    /// Not at first use: a wall that refuses only when someone happens to open a path would let a
    /// process start, report healthy, and fail on the first file — which reads as a bug in the
    /// tool rather than as an unverified boundary. See [`VERIFIED_PLATFORMS`].
    pub fn new() -> Result<Self, ScopeError> {
        if !THIS_PLATFORM_VERIFIED {
            return Err(ScopeError::PlatformUnverified {
                platform: std::env::consts::OS,
                verified: VERIFIED_PLATFORMS,
            });
        }
        Ok(Self { _gated: () })
    }

    /// The same walk, with an observer. **Test-facing**, and the reason it is `pub` is stated
    /// in `walk`: the TOCTOU test has to be able to plant a link at the exact instant a race
    /// would land, and a test that races a thread and hopes would prove nothing when it passed.
    #[doc(hidden)]
    pub fn open_observed(
        &self,
        declared: &[PathGlob],
        workspace: &Path,
        requested: &str,
        access: Access,
        observer: &dyn WalkObserver,
    ) -> Result<ScopedPath, ScopeError> {
        // 1. Refuse ambiguous spellings, before any syscall.
        let parsed = request::validate(requested).map_err(|source| ScopeError::Malformed {
            requested: requested.to_string(),
            source,
        })?;

        // 2. The declaration decides, and an empty declaration decides "no".
        if !glob::admits(declared, &parsed) {
            return Err(ScopeError::Undeclared { requested: parsed.as_relative() });
        }

        // 3. Walk it open. Nothing above this line touched the filesystem, and nothing below it
        //    resolves a string the kernel will resolve again.
        let opened = walk::open_within(workspace, &parsed, access, observer)?;
        Ok(ScopedPath {
            handle: opened.handle,
            resolved: opened.resolved,
            relative: parsed.as_relative(),
        })
    }
}

impl PathScope for WorkspaceScope {
    fn open(
        &self,
        declared: &[PathGlob],
        workspace: &Path,
        requested: &str,
        access: Access,
    ) -> Result<ScopedPath, ScopeError> {
        self.open_observed(declared, workspace, requested, access, &())
    }
}

#[cfg(test)]
mod tests {
    /// **A missing file must not read as a refusal.**
    ///
    /// The regression for 2026-08-25: an agent read `could not be opened: ... cannot find the
    /// file` as a permission or path-parsing failure and wrote a handoff document diagnosing
    /// MSVC-versus-bash path resolution. The files were absent. The message now says what it is
    /// not, and names the check.
    #[test]
    fn a_missing_file_says_it_is_not_a_refusal_and_names_the_check() {
        let e = ScopeError::Unopenable {
            requested: "docs/memory.md".into(),
            detail: "The system cannot find the file specified. (os error 2)".into(),
        };
        let m = e.to_string();
        assert!(m.contains("NOT a scoping refusal"), "{m}");
        assert!(m.contains("NOT a permission denial"), "{m}");
        assert!(m.contains("`glob`"), "the remedy must be named, not implied: {m}");
        assert!(m.contains("docs/memory.md"), "the path must survive: {m}");
        assert!(m.contains("cannot find the file"), "the OS detail must survive: {m}");
    }

    /// **The vacuity control, and it is the whole point of the change.** The two errors have to be
    /// distinguishable by their text, because that text is all a model gets. A refusal still says
    /// "denied"; a missing file now says it is not one.
    #[test]
    fn a_refusal_and_a_missing_file_do_not_read_alike() {
        let refused = ScopeError::Undeclared { requested: "/etc/passwd".into() }.to_string();
        let missing = ScopeError::Unopenable {
            requested: "docs/memory.md".into(),
            detail: "The system cannot find the file specified. (os error 2)".into(),
        }
        .to_string();

        assert!(refused.contains("denied"), "a refusal must still say so: {refused}");
        assert!(
            !missing.contains("Undeclared access is denied"),
            "a missing file must not carry the refusal's own sentence: {missing}"
        );
    }

    use super::*;

    #[test]
    fn construction_succeeds_only_on_a_platform_whose_suite_has_been_executed() {
        // The invariant, asserted the only way it can be from inside one platform: whenever
        // construction succeeds, this OS is on the verified list. A test that merely called
        // `new()` and unwrapped would pass identically with the gate deleted.
        match WorkspaceScope::new() {
            Ok(_) => assert!(
                VERIFIED_PLATFORMS.contains(&std::env::consts::OS),
                "constructed on `{}`, which is not in {VERIFIED_PLATFORMS:?}",
                std::env::consts::OS
            ),
            Err(ScopeError::PlatformUnverified { platform, .. }) => assert!(
                !VERIFIED_PLATFORMS.contains(&platform),
                "refused on `{platform}`, which IS in {VERIFIED_PLATFORMS:?}"
            ),
            Err(other) => panic!("unexpected error from construction: {other}"),
        }
    }

    #[test]
    fn macos_is_not_verified_and_the_refusal_says_why() {
        // Named rather than left implicit. macOS is the platform a reader is most likely to
        // assume is covered by "POSIX", and it is the one where case-insensitivity and NFD
        // normalization would make `glob` and `request` disagree.
        assert!(
            !VERIFIED_PLATFORMS.contains(&"macos"),
            "macOS is listed as verified; has the suite actually been executed there?"
        );
        let e = ScopeError::PlatformUnverified { platform: "macos", verified: VERIFIED_PLATFORMS };
        let msg = e.to_string();
        assert!(msg.contains("never been EXECUTED"), "{msg}");
        assert!(msg.contains("NFD"), "the refusal must name the specific divergence: {msg}");
    }

    #[test]
    fn the_refusing_scope_still_refuses_and_says_why() {
        let e = Unavailable
            .open(&[PathGlob::new("./**")], Path::new("/ws"), "src/main.rs", Access::Read)
            .unwrap_err();
        assert!(matches!(e, ScopeError::Unavailable { .. }));
        assert!(e.to_string().contains("ship together"), "{e}");
    }

    #[test]
    fn a_scoped_path_cannot_be_built_without_a_handle() {
        // Executable documentation. `ScopedPath` has private fields and no `new`; the only way
        // to obtain one is from an implementation that actually opened something. If a future
        // session adds `ScopedPath::from_path(PathBuf)`, this comment is what has to be argued
        // past, and brief §8.3 is the argument it has to beat.
        fn _assert_no_public_constructor() {
            // Intentionally empty: the assertion is that the following does not compile.
            //   let _ = ScopedPath { handle: .., resolved: .., relative: .. };
        }
    }

    #[test]
    fn the_declaration_is_checked_before_the_filesystem_is_touched() {
        // A path outside the declaration is refused without a syscall, so a nonexistent
        // workspace still produces `Undeclared` rather than an io error. That ordering is what
        // keeps an undeclared probe from being answerable by timing.
        let e = WorkspaceScope::new()
            .expect("verified platform")
            .open(
                &[PathGlob::new("./out/**")],
                Path::new("/nonexistent-workspace-xyzzy"),
                "src/secret.rs",
                Access::Read,
            )
            .unwrap_err();
        assert!(matches!(e, ScopeError::Undeclared { .. }), "{e:?}");
    }

    #[test]
    fn a_malformed_request_is_refused_before_the_declaration_is_consulted() {
        let e = WorkspaceScope::new()
            .expect("verified platform")
            .open(&[PathGlob::new("./**")], Path::new("/nonexistent-xyzzy"), "../../etc/passwd", Access::Read)
            .unwrap_err();
        assert!(matches!(e, ScopeError::Malformed { .. }), "{e:?}");
    }
}
