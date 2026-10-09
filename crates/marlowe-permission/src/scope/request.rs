//! String-level validation of a requested path, **before any syscall**.
//!
//! This is the first of two walls and it is the weaker one. It exists to reject forms that are
//! ambiguous *as strings* — where two different spellings name one file, or where the spelling
//! means something different to the checker than to the kernel. The second wall is the handle
//! walk in [`super::walk`], which is what actually enforces containment.
//!
//! **Every rule here is a refusal, never a normalization**, with one deliberate exception noted
//! below. ADR-002: *"canonicalize before the check, never after"* — and the strongest version of
//! that is not to canonicalize a hostile string at all, but to refuse the spellings that make
//! canonicalization load-bearing. A normalizer has to be right about every encoding; a refusal
//! has to be right about one thing.
//!
//! # The one normalization, and why it is not a refusal
//!
//! Unicode normalization form. macOS stores NFD, Linux stores what it was given, and a glob
//! written in NFC will not match an NFD request that names the same file. Refusing non-NFC
//! would make legitimate macOS filenames unreachable, which is a real cost with no attacker
//! behind it. So **both the request and the declared glob are normalized to NFC for matching**,
//! and the normalization is applied to both sides of one comparison rather than to a value that
//! is then used for something else.
//!
//! Homoglyph separators are a different problem and *are* refused: U+2215 DIVISION SLASH and
//! U+FF0F FULLWIDTH SOLIDUS look like `/` and are not, so a component containing one is a
//! component pretending to be two.

use unicode_normalization::{is_nfc, UnicodeNormalization};

/// A requested path that has passed string-level validation: relative, no `..`, no ambiguous
/// component. **Not yet resolved and not yet known to be in scope** — that is the walk's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedPath {
    /// NFC-normalized components, in order. `.` is dropped; `..` never survives validation.
    components: Vec<String>,
}

impl RequestedPath {
    pub fn components(&self) -> &[String] {
        &self.components
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// The normalized relative form, `/`-separated. Used for glob matching and for display.
    pub fn as_relative(&self) -> String {
        self.components.join("/")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error(
        "`{requested}` is absolute or drive-qualified. A tool's paths are declared relative to \
         the workspace, and an absolute request is a request to leave it"
    )]
    NotRelative { requested: String },

    #[error(
        "`{requested}` contains a `..` component. It is refused rather than collapsed: a \
         relative request inside a workspace never needs one, and collapsing is where a check \
         and a kernel disagree"
    )]
    ParentComponent { requested: String },

    #[error("`{requested}` contains an empty path component")]
    EmptyComponent { requested: String },

    #[error(
        "component `{component}` contains a colon. On Windows that is an alternate data stream \
         or a drive specifier — `allowed.txt:hidden` is a different file from `allowed.txt`"
    )]
    Colon { component: String },

    #[error("component `{component}` contains a NUL byte")]
    Nul { component: String },

    #[error(
        "component `{component}` ends with a dot or a space. Win32 strips both, so two \
         different strings would name one file and only one of them would be checked"
    )]
    TrailingDotOrSpace { component: String },

    #[error(
        "component `{component}` is a reserved device name. `CON`, `NUL`, `COM1` and their \
         kin resolve to devices rather than files, whatever directory they appear in"
    )]
    ReservedDeviceName { component: String },

    #[error(
        "component `{component}` looks like an 8.3 short name. A short name is a second \
         spelling of a file that a declared glob written against the long name would not \
         match. Rename the request to the long form"
    )]
    ShortName { component: String },

    #[error(
        "component `{component}` contains a character that looks like a path separator and is \
         not (U+2215 or U+FF0F). One component pretending to be two"
    )]
    HomoglyphSeparator { component: String },

    #[error("component `{component}` contains a control character")]
    ControlCharacter { component: String },
}

/// Windows reserved device names. Reserved with or without an extension, case-insensitively.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters that render like a separator and are not one.
const SEPARATOR_HOMOGLYPHS: [char; 4] = ['\u{2215}', '\u{FF0F}', '\u{29F8}', '\u{FE68}'];

/// Whether a component matches the 8.3 short-name shape: up to six characters, `~`, digits,
/// optionally an extension of up to three characters.
///
/// **This has a false-positive cost and it is accepted**: a legitimate `backup~1.txt` is
/// refused and has to be renamed. The alternative is a second spelling of a path that a
/// declared glob written against the long name does not match, on the one wall with no kernel
/// behind it. The handle walk verifies identity as well, so this is the cheap half of a pair.
fn looks_like_short_name(component: &str) -> bool {
    let (stem, ext) = match component.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (component, None),
    };
    if let Some(e) = ext {
        if e.len() > 3 || e.is_empty() {
            return false;
        }
    }
    let Some((before, after)) = stem.rsplit_once('~') else { return false };
    !before.is_empty()
        && before.len() <= 6
        && !after.is_empty()
        && after.len() <= 2
        && after.chars().all(|c| c.is_ascii_digit())
}

/// Whether a raw string is absolute, drive-qualified, UNC, extended-length or device-namespaced.
///
/// Checked on the **raw string** on every platform, not via `Path::is_absolute`, because
/// `Path::is_absolute` answers for the *host* platform: `C:\Windows` is relative on Linux and
/// `/etc/passwd` is relative on Windows. A checker that changes its answer with the build host
/// is a checker that passes its own tests on one machine.
fn is_rooted(raw: &str) -> bool {
    let b = raw.as_bytes();
    if b.is_empty() {
        return false;
    }
    // POSIX absolute, and the Windows root-relative form `\foo`.
    if b[0] == b'/' || b[0] == b'\\' {
        return true;
    }
    // `C:` anything — both `C:\abs` and the drive-relative `C:foo`, which resolves against a
    // per-drive current directory nobody has audited.
    if b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic() {
        return true;
    }
    false
}

/// Validate a requested path. See the module header for what is refused and what is normalized.
pub fn validate(requested: &str) -> Result<RequestedPath, RequestError> {
    let raw = requested;

    if is_rooted(raw) {
        return Err(RequestError::NotRelative { requested: raw.to_string() });
    }

    let mut components = Vec::new();
    // Both separators on every platform. A `\` in a request is a separator on Windows and a
    // legal filename character on POSIX; treating it as a separator everywhere is the
    // conservative reading, because it can only split a component that a POSIX kernel would
    // have kept whole — which fails closed.
    for part in raw.split(['/', '\\']) {
        if part == "." {
            continue;
        }
        if part.is_empty() {
            // A trailing separator produces one empty part and is harmless; anything else is a
            // doubled separator, which is a spelling difference the kernel collapses.
            if raw.ends_with('/') || raw.ends_with('\\') {
                continue;
            }
            return Err(RequestError::EmptyComponent { requested: raw.to_string() });
        }
        if part == ".." {
            return Err(RequestError::ParentComponent { requested: raw.to_string() });
        }
        if part.contains('\0') {
            return Err(RequestError::Nul { component: part.to_string() });
        }
        if part.chars().any(|c| c.is_control()) {
            return Err(RequestError::ControlCharacter { component: part.to_string() });
        }
        if part.contains(':') {
            return Err(RequestError::Colon { component: part.to_string() });
        }
        if part.chars().any(|c| SEPARATOR_HOMOGLYPHS.contains(&c)) {
            return Err(RequestError::HomoglyphSeparator { component: part.to_string() });
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err(RequestError::TrailingDotOrSpace { component: part.to_string() });
        }
        let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(RequestError::ReservedDeviceName { component: part.to_string() });
        }
        if looks_like_short_name(part) {
            return Err(RequestError::ShortName { component: part.to_string() });
        }
        // The one normalization. Applied to both sides of the glob comparison, never to a value
        // that is then used for something else.
        let normalized: String = if is_nfc(part) { part.to_string() } else { part.nfc().collect() };
        components.push(normalized);
    }

    Ok(RequestedPath { components })
}

/// NFC-normalize a declared glob so it is compared against a request on equal terms.
pub fn normalize_for_match(s: &str) -> String {
    if is_nfc(s) {
        s.to_string()
    } else {
        s.nfc().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn refused(p: &str) -> RequestError {
        validate(p).expect_err(&format!("`{p}` must be refused"))
    }

    #[test]
    fn ordinary_relative_paths_are_accepted() {
        assert_eq!(validate("src/main.rs").unwrap().as_relative(), "src/main.rs");
        assert_eq!(validate("./src/./main.rs").unwrap().as_relative(), "src/main.rs");
        assert_eq!(validate("a/b/c/").unwrap().as_relative(), "a/b/c");
        assert_eq!(validate("notes.md").unwrap().as_relative(), "notes.md");
    }

    #[test]
    fn relative_traversal_is_refused_not_collapsed() {
        for p in [
            "../etc/passwd",
            "..\\windows\\system32",
            "a/../../b",
            "a/b/../../../../../../etc/passwd",
            "..",
            "a/..",
            // Interleaved and doubled separators.
            "a/./../../b",
        ] {
            assert!(
                matches!(refused(p), RequestError::ParentComponent { .. }),
                "`{p}` should be refused as a parent component"
            );
        }
    }

    #[test]
    fn every_rooted_form_is_refused_on_every_platform() {
        // Checked on the raw string rather than via Path::is_absolute, which answers for the
        // host: `C:\x` is "relative" on Linux and `/etc/passwd` is "relative" on Windows.
        for p in [
            "/etc/passwd",
            "\\windows\\system32",
            "C:\\Windows",
            "c:/windows",
            "C:foo",             // drive-relative: a per-drive cwd nobody audited
            "\\\\server\\share", // UNC
            "\\\\?\\C:\\Windows",
            "\\\\.\\PhysicalDrive0", // device namespace
            "\\\\?\\UNC\\server\\share",
        ] {
            assert!(
                matches!(refused(p), RequestError::NotRelative { .. }),
                "`{p}` should be refused as rooted"
            );
        }
    }

    #[test]
    fn alternate_data_streams_are_refused() {
        assert!(matches!(refused("allowed.txt:hidden"), RequestError::Colon { .. }));
        assert!(matches!(refused("dir::$INDEX_ALLOCATION"), RequestError::Colon { .. }));
        assert!(matches!(refused("a/b.txt:$DATA"), RequestError::Colon { .. }));
    }

    #[test]
    fn win32_name_munging_is_refused() {
        // Win32 silently strips trailing dots and spaces, so `secret.txt.` and `secret.txt`
        // are one file and two strings. Only one of them would be checked.
        assert!(matches!(refused("secret.txt."), RequestError::TrailingDotOrSpace { .. }));
        assert!(matches!(refused("secret.txt "), RequestError::TrailingDotOrSpace { .. }));
        assert!(matches!(refused("dir./file"), RequestError::TrailingDotOrSpace { .. }));
    }

    #[test]
    fn reserved_device_names_are_refused_anywhere_in_the_path() {
        for p in ["CON", "nul", "a/COM1", "b/LPT9.txt", "AUX.log", "deep/dir/PRN"] {
            assert!(
                matches!(refused(p), RequestError::ReservedDeviceName { .. }),
                "`{p}` should be refused as a device name"
            );
        }
        // ...and a name that merely starts with one is fine.
        assert!(validate("console.log").is_ok());
        assert!(validate("nullable.rs").is_ok());
    }

    #[test]
    fn short_names_are_refused_and_the_false_positive_is_accepted() {
        for p in ["PROGRA~1", "PROGRA~1/x", "MYDOCU~1.TXT"] {
            assert!(
                matches!(refused(p), RequestError::ShortName { .. }),
                "`{p}` should be refused as a short name"
            );
        }
        // The accepted false positive, asserted so it is a known cost rather than a surprise.
        assert!(matches!(refused("backup~1.txt"), RequestError::ShortName { .. }));
        // A tilde that is not a short name is fine.
        assert!(validate("~/notes.md").is_err() || validate("notes~.md").is_ok());
        assert!(validate("a~bcdefgh1.txt").is_ok(), "stem too long to be 8.3");
    }

    #[test]
    fn separator_homoglyphs_are_refused() {
        assert!(matches!(refused("a\u{2215}b"), RequestError::HomoglyphSeparator { .. }));
        assert!(matches!(refused("a\u{FF0F}b"), RequestError::HomoglyphSeparator { .. }));
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(matches!(refused("a\u{1b}[2Kb"), RequestError::ControlCharacter { .. }));
        assert!(matches!(refused("a\nb"), RequestError::ControlCharacter { .. }));
    }

    #[test]
    fn unicode_is_normalized_to_nfc_on_both_sides_rather_than_refused() {
        // e + combining acute (NFD) and precomposed é (NFC) name the same file on macOS.
        let nfd = "caf\u{65}\u{301}.md";
        let nfc = "caf\u{e9}.md";
        assert_ne!(nfd, nfc, "the two spellings differ as bytes");
        assert_eq!(
            validate(nfd).unwrap().as_relative(),
            validate(nfc).unwrap().as_relative(),
            "both requests must reach one canonical form"
        );
        assert_eq!(normalize_for_match(nfd), validate(nfc).unwrap().as_relative());
    }

    #[test]
    fn overlong_utf8_cannot_reach_this_function_at_all() {
        // The classic overlong encoding of `/` is 0xC0 0xAF. Rust's `str` cannot hold it, so
        // the class is closed by the type rather than by a check — asserted here because "we
        // do not check for it" and "it cannot happen" look identical in a review.
        //
        // Built at runtime rather than as a literal: the compiler proves a literal invalid and
        // warns, which would make this assertion a statement about constant folding.
        let overlong_solidus: Vec<u8> = vec![0xC0, 0xAF];
        let overlong_three_byte: Vec<u8> = vec![0xE0, 0x80, 0xAF];
        assert!(std::str::from_utf8(&overlong_solidus).is_err());
        assert!(String::from_utf8(overlong_three_byte).is_err());
    }

    #[test]
    fn a_backslash_is_a_separator_on_every_platform() {
        // On POSIX a backslash is a legal filename character, so treating it as a separator
        // splits a component the kernel would have kept whole. That fails closed: the request
        // reaches a deeper, narrower path or is refused, never a wider one.
        assert_eq!(validate("a\\b").unwrap().as_relative(), "a/b");
        assert!(matches!(refused("a\\..\\b"), RequestError::ParentComponent { .. }));
    }
}
