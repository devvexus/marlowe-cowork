//! Matching a validated request against a manifest's declared path globs.
//!
//! Deliberately small. Two wildcards, both segment-aware:
//!
//! | Pattern | Matches |
//! |---|---|
//! | `**` | any number of segments, including zero |
//! | `*` | any run of characters **within one segment** — never a separator |
//!
//! No `?`, no character classes, no brace expansion, no negation. Every one of those is a
//! feature whose interaction with the others has to be reasoned about, on a comparison that
//! decides whether a path is inside a security boundary. A manifest that needs a richer pattern
//! should declare more globs.
//!
//! **Matching happens on the NFC-normalized relative form** produced by
//! [`super::request::validate`], with the pattern normalized the same way — see that module for
//! why normalization is the one thing here that is not a refusal.

use marlowe_tools::PathGlob;

use super::request::{normalize_for_match, RequestedPath};

/// Whether any declared glob admits this request.
///
/// **An empty declaration matches nothing.** A tool that declared no paths gets no filesystem,
/// which is the default-deny reading; the alternative — treating "no declaration" as "no
/// restriction" — is the permissive default that CLAUDE.md names as this project's most
/// productive source of bugs.
pub fn admits(declared: &[PathGlob], request: &RequestedPath) -> bool {
    let path = request.as_relative();
    declared.iter().any(|g| matches_one(g.as_str(), &path))
}

fn matches_one(pattern: &str, path: &str) -> bool {
    let pattern = normalize_for_match(pattern);
    // A manifest writes workspace-relative globs. `./x` and `x` are the same declaration.
    let pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat, &seg)
}

fn match_segments(pat: &[&str], seg: &[&str]) -> bool {
    match pat.first() {
        None => seg.is_empty(),
        Some(&"**") => {
            // Zero or more segments. Try every split; the pattern is short and bounded by the
            // manifest, so the exponential worst case needs a manifest written to be slow.
            (0..=seg.len()).any(|skip| match_segments(&pat[1..], &seg[skip..]))
        }
        Some(p) => match seg.first() {
            None => false,
            Some(s) => match_within_segment(p, s) && match_segments(&pat[1..], &seg[1..]),
        },
    }
}

/// `*` inside one segment. Never crosses a separator, because the caller already split on them.
fn match_within_segment(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    // The first literal must anchor at the start.
    if !parts[0].is_empty() {
        match rest.strip_prefix(parts[0]) {
            Some(r) => rest = r,
            None => return false,
        }
    }
    // The last must anchor at the end.
    let last = parts[parts.len() - 1];
    for middle in &parts[1..parts.len() - 1] {
        if middle.is_empty() {
            continue;
        }
        match rest.find(middle) {
            Some(i) => rest = &rest[i + middle.len()..],
            None => return false,
        }
    }
    if last.is_empty() {
        true
    } else {
        rest.len() >= last.len() && rest.ends_with(last)
    }
}

#[cfg(test)]
mod tests {
    use super::super::request::validate;
    use super::*;

    fn admitted(globs: &[&str], path: &str) -> bool {
        let declared: Vec<PathGlob> = globs.iter().map(|g| PathGlob::new(*g)).collect();
        admits(&declared, &validate(path).expect("the request is well formed"))
    }

    #[test]
    fn the_workspace_glob_admits_everything_under_it() {
        assert!(admitted(&["./**"], "src/main.rs"));
        assert!(admitted(&["./**"], "a/b/c/d/e.txt"));
        assert!(admitted(&["./**"], "notes.md"));
    }

    #[test]
    fn a_narrower_glob_admits_only_its_subtree() {
        assert!(admitted(&["./out/**"], "out/report.pdf"));
        assert!(admitted(&["./out/**"], "out/a/b.pdf"));
        assert!(!admitted(&["./out/**"], "src/main.rs"));
        // The trap: a textual prefix match would admit this.
        assert!(!admitted(&["./out/**"], "outside/secret.txt"));
    }

    #[test]
    fn a_single_star_never_crosses_a_separator() {
        assert!(admitted(&["*.rs"], "main.rs"));
        assert!(!admitted(&["*.rs"], "src/main.rs"), "`*` is not `**`");
        assert!(admitted(&["src/*.rs"], "src/main.rs"));
        assert!(!admitted(&["src/*.rs"], "src/deep/main.rs"));
    }

    #[test]
    fn double_star_matches_zero_segments() {
        assert!(admitted(&["out/**"], "out"), "the directory itself is in scope");
        assert!(admitted(&["**/target/**"], "target/debug/x"));
        assert!(admitted(&["**/target/**"], "a/b/target/debug/x"));
    }

    #[test]
    fn an_empty_declaration_admits_nothing() {
        // Default-deny. "No declaration" is not "no restriction".
        assert!(!admitted(&[], "anything.txt"));
        assert!(!admitted(&[], "a/b/c"));
    }

    #[test]
    fn normalization_applies_to_both_sides() {
        let nfd_glob = "caf\u{65}\u{301}/**";
        let nfc_path = "caf\u{e9}/notes.md";
        assert!(
            admitted(&[nfd_glob], nfc_path),
            "a glob and a request naming one directory must match whichever form each is in"
        );
    }

    #[test]
    fn a_star_in_the_middle_anchors_at_both_ends() {
        assert!(admitted(&["report-*.pdf"], "report-2026.pdf"));
        assert!(!admitted(&["report-*.pdf"], "xreport-2026.pdf"));
        assert!(!admitted(&["report-*.pdf"], "report-2026.pdf.exe"));
    }
}
