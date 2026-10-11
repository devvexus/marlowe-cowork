//! `file:` URIs and LSP positions.
//!
//! Servers spell the same file differently (`file:///C:/x`, `file:///c%3A/x`), so paths are
//! compared through [`crate::index::norm_key`], never as URI strings. Positions are UTF-16 code
//! units unless the server agreed to UTF-8 at initialize ([`Encoding`]).

use std::path::{Path, PathBuf};

/// A path as a `file:` URI: `file:///C:/a%20b/c.rs`, `file://server/share/x` for UNC paths.
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\\{r}")).unwrap_or_else(|| {
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    });
    let s = s.replace('\\', "/");
    let (authority, rest) = match s.strip_prefix("//") {
        Some(unc) => match unc.split_once('/') {
            Some((host, rest)) => (host.to_string(), format!("/{rest}")),
            None => (unc.to_string(), "/".to_string()),
        },
        None if s.starts_with('/') => (String::new(), s),
        None => (String::new(), format!("/{s}")),
    };
    let mut out = format!("file://{authority}");
    for (i, b) in rest.bytes().enumerate() {
        let drive_colon = i == 2 && b == b':' && rest.as_bytes()[1].is_ascii_alphabetic();
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') || drive_colon {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path of a `file:` URI; `None` for any other scheme or a malformed escape.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://").or_else(|| uri.strip_prefix("FILE://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let decoded = percent_decode(path)?;
    let host = if authority.eq_ignore_ascii_case("localhost") { "" } else { authority };
    let b = decoded.as_bytes();
    let local = if !host.is_empty() {
        format!("//{host}{decoded}")
    } else if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        decoded[1..].to_string()
    } else {
        decoded
    };
    if cfg!(windows) {
        Some(PathBuf::from(local.replace('/', "\\")))
    } else {
        Some(PathBuf::from(local))
    }
}

/// `p` with `.` dropped and `..` applied lexically; `None` when a `..` would climb past the top.
pub fn normalize(p: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    return None;
                }
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// `p` with links resolved: the canonical form of its deepest existing ancestor, plus the rest.
fn real(p: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            out.extend(tail.iter().rev());
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Whether `p` lies strictly beneath `root` once `..` is applied and junctions and symlinks are
/// resolved (a lexical prefix test lets `root/../x` and a link out of the root through).
pub fn contained(root: &Path, p: &Path) -> bool {
    let (Some(p), Some(root)) = (normalize(p), normalize(root)) else { return false };
    let (p, root) = (real(&p), real(&root));
    p != root && p.starts_with(&root)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// How a server counts columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// UTF-16 code units: the LSP default.
    #[default]
    Utf16,
    Utf8,
}

impl Encoding {
    /// From the server's `capabilities.positionEncoding`.
    pub fn from_server(kind: Option<&str>) -> Self {
        match kind {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        }
    }
}

/// The byte offset where each line starts (a line ends at `\n`; `\r` stays in the line).
pub fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

/// The LSP (line, character) of byte `offset` in `text`.
pub fn position_of(text: &str, offset: usize, enc: Encoding) -> (u32, u32) {
    let offset = offset.min(text.len());
    let starts = line_starts(text);
    let line = starts.partition_point(|&s| s <= offset).saturating_sub(1);
    let mut start = starts[line];
    let mut end = offset;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    start = start.min(end);
    let seg = &text[start..end];
    let ch = match enc {
        Encoding::Utf8 => seg.len(),
        Encoding::Utf16 => seg.encode_utf16().count(),
    };
    (line as u32, ch as u32)
}

/// The byte offset of LSP position (line, character) in `text`. A character past the line's end
/// clamps to the end (servers send that for "end of line"); a line past the end is `None`, except
/// the line just after a final newline, which is the end of the text.
pub fn offset_of(text: &str, line: u32, character: u32, enc: Encoding) -> Option<usize> {
    let starts = line_starts(text);
    let start = *starts.get(line as usize)?;
    let end = starts.get(line as usize + 1).map_or(text.len(), |&n| n - 1);
    let line_text = &text[start..end];
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let mut units = 0u32;
    for (i, c) in line_text.char_indices() {
        if units >= character {
            return Some(start + i);
        }
        units += match enc {
            Encoding::Utf8 => c.len_utf8() as u32,
            Encoding::Utf16 => c.len_utf16() as u32,
        };
    }
    Some(start + line_text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_windows_and_unc_paths() {
        if cfg!(windows) {
            let p = Path::new(r"C:\Users\me\My Repo\src\lib.rs");
            let u = path_to_uri(p);
            assert_eq!(u, "file:///C:/Users/me/My%20Repo/src/lib.rs");
            assert_eq!(uri_to_path(&u).unwrap(), p);
            assert_eq!(uri_to_path("file:///c%3A/Users/me/x.rs").unwrap(), Path::new(r"c:\Users\me\x.rs"));
            assert_eq!(path_to_uri(Path::new(r"\\?\C:\a\b.rs")), "file:///C:/a/b.rs");
            let unc = Path::new(r"\\server\share\a.py");
            assert_eq!(path_to_uri(unc), "file://server/share/a.py");
            assert_eq!(uri_to_path("file://server/share/a.py").unwrap(), unc);
        } else {
            let p = Path::new("/home/me/a b/é.rs");
            assert_eq!(uri_to_path(&path_to_uri(p)).unwrap(), p);
        }
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
        assert_eq!(uri_to_path("file:///bad%zz"), None);
    }

    #[test]
    fn containment_applies_dot_dot_and_refuses_climbing_out() {
        let root = std::env::temp_dir().join(format!("marlowe-uri-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert!(contained(&root, &root.join("src").join("a.rs")));
        assert!(contained(&root, &root.join("src").join("..").join("b.rs")), "still inside");
        assert!(!contained(&root, &root.join("..").join("x.rs")));
        assert!(!contained(&root, &root.join("src").join("..").join("..").join("x.rs")));
        assert!(!contained(&root, &root), "the root itself is not a file in it");
        assert!(contained(&root, &root.join("new").join("deep").join("c.rs")), "files that do not exist yet");
        assert_eq!(normalize(Path::new("a/../../b")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn positions_count_utf16_units_or_bytes() {
        let t = "fn a() {}\nlet é = \"😀x\";\r\nend";
        let x = t.find('x').unwrap();
        assert_eq!(position_of(t, x, Encoding::Utf8), (1, 14));
        assert_eq!(position_of(t, x, Encoding::Utf16), (1, 11), "é is one unit, 😀 is two");
        assert_eq!(offset_of(t, 1, 11, Encoding::Utf16), Some(x));
        assert_eq!(offset_of(t, 1, 14, Encoding::Utf8), Some(x));
        // Past the end of a CRLF line clamps before the `\r`.
        assert_eq!(offset_of(t, 1, 99, Encoding::Utf16), Some(t.find("\r\n").unwrap()));
        assert_eq!(offset_of(t, 2, 3, Encoding::Utf16), Some(t.len()));
        assert_eq!(offset_of(t, 3, 0, Encoding::Utf16), None);
        assert_eq!(offset_of("a\n", 1, 0, Encoding::Utf16), Some(2));
        for off in [0, 3, 10, x, t.len()] {
            let (l, c) = position_of(t, off, Encoding::Utf16);
            assert_eq!(offset_of(t, l, c, Encoding::Utf16), Some(off));
        }
    }
}
