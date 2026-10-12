//! Path arithmetic for the shadow repository: lexical cleaning, containment and the repository's
//! directory name. Pure functions; nothing here touches the disk except [`canonical`].

use std::path::{Component, Path, PathBuf};

/// `\\?\C:\x` as `C:\x`; every other path unchanged.
pub(crate) fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// The workspace as the OS names it (symlinks and `..` resolved), without the verbatim prefix.
pub(crate) fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).map(|c| strip_verbatim(&c)).unwrap_or_else(|_| lexical_clean(p))
}

/// `.` and `..` resolved by text alone (the path need not exist).
pub(crate) fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn fold(s: &str) -> String {
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

fn parts(p: &Path) -> Vec<String> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| fold(&c.as_os_str().to_string_lossy()))
        .collect()
}

/// `path` relative to `root` with `/` separators, when it lies inside (or is) `root`. Case is
/// ignored on Windows. `None` for a path outside.
pub(crate) fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let root_parts = parts(&lexical_clean(root));
    let full = lexical_clean(&if path.is_absolute() { path.to_path_buf() } else { root.join(path) });
    let path_parts = parts(&full);
    if path_parts.len() < root_parts.len() || path_parts[..root_parts.len()] != root_parts[..] {
        return None;
    }
    let rest: Vec<String> = full
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .skip(root_parts.len())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(rest.join("/"))
}

/// FNV-1a 64: a stable, dependency-free hash for directory names (not for security).
pub(crate) fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The shadow repository's directory stem for a workspace: a readable name plus a hash of the
/// full path, so two projects both called `app` never share one.
pub(crate) fn repo_stem(workspace: &Path) -> String {
    let name: String = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    let key = fold(&workspace.to_string_lossy().replace('\\', "/"));
    format!("{name}-{:016x}", fnv64(key.trim_end_matches('/')))
}

/// A ref path component for a session id: anything git would refuse becomes `_`.
pub(crate) fn ref_part(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

/// A `.gitignore` line that matches exactly `rel` (anchored, glob characters escaped).
pub(crate) fn ignore_line(rel: &str) -> String {
    let mut out = String::from("/");
    for (i, c) in rel.chars().enumerate() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') || (i == 0 && matches!(c, '#' | '!')) {
            out.push('\\');
        }
        out.push(c);
    }
    if out.ends_with(' ') {
        out.pop();
        out.push_str("\\ ");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_is_lexical_and_ignores_case_on_windows() {
        let root = Path::new(if cfg!(windows) { r"C:\w\proj" } else { "/w/proj" });
        let inside = root.join("src").join("a.rs");
        assert_eq!(relative_to(root, &inside).as_deref(), Some("src/a.rs"));
        assert_eq!(relative_to(root, root).as_deref(), Some(""));
        assert_eq!(relative_to(root, &root.join("src").join("..").join("b.rs")).as_deref(), Some("b.rs"));
        assert_eq!(relative_to(root, &root.join("..").join("other").join("c")), None);
        assert_eq!(relative_to(root, Path::new("rel/x.rs")).as_deref(), Some("rel/x.rs"));
        if cfg!(windows) {
            assert_eq!(relative_to(root, Path::new(r"c:\W\PROJ\Src\a.rs")).as_deref(), Some("Src/a.rs"));
        }
    }

    #[test]
    fn stems_differ_by_full_path_and_are_stable() {
        let a = repo_stem(Path::new(if cfg!(windows) { r"C:\a\app" } else { "/a/app" }));
        let b = repo_stem(Path::new(if cfg!(windows) { r"C:\b\app" } else { "/b/app" }));
        assert_ne!(a, b);
        assert!(a.starts_with("app-"));
        assert_eq!(a, repo_stem(Path::new(if cfg!(windows) { r"C:\a\app\" } else { "/a/app/" })));
        // FNV-1a 64 reference vectors.
        assert_eq!(fnv64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn ignore_lines_escape_globs() {
        assert_eq!(ignore_line("data/big[1].bin"), r"/data/big\[1\].bin");
        assert_eq!(ignore_line("#x"), r"/\#x");
        assert_eq!(ignore_line("a b "), r"/a b\ ");
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x\y")), PathBuf::from(r"C:\x\y"));
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\srv\share")), PathBuf::from(r"\\?\UNC\srv\share"));
    }
}
