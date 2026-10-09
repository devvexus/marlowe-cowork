//! Path resolution and the workspace boundary.

use std::path::{Component, Path, PathBuf};

/// Absolute, lexically normalised form of `raw` (relative paths join `base`, else the process cwd).
pub fn resolve(base: Option<&Path>, raw: &str) -> PathBuf {
    let raw = Path::new(raw.trim());
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let base = base
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        base.join(raw)
    };
    normalise(&joined)
}

fn normalise(p: &Path) -> PathBuf {
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

/// The real location of `p`: the deepest existing ancestor is canonicalised (so links and `..`
/// cannot hide an escape) and the not-yet-existing tail is appended.
fn real_path(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
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

/// Whether `abs` is `root` or lies beneath it, after resolving links.
pub fn inside(root: &Path, abs: &Path) -> bool {
    real_path(abs).starts_with(real_path(root))
}

/// `abs` relative to `root` with forward slashes, or the absolute path when outside.
pub fn display(root: Option<&Path>, abs: &Path) -> String {
    let shown = match root.and_then(|r| abs.strip_prefix(r).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel,
        _ => abs,
    };
    shown.to_string_lossy().replace('\\', "/")
}

/// Whether `root` is too broad to free writes: a drive or filesystem root, the home directory or
/// any ancestor of it, or a system, program or app-data directory (or anything inside one). The
/// temp directory's subdirectories are allowed.
pub fn is_broad_root(root: &Path) -> bool {
    let r = real_path(root);
    if r.parent().is_none() {
        return true;
    }
    let env_dir = |key: &str| {
        std::env::var_os(key).filter(|v| !v.is_empty()).map(|v| real_path(Path::new(&v)))
    };
    let temp = real_path(&std::env::temp_dir());
    if r.starts_with(&temp) && r != temp {
        return false;
    }
    if let Some(home) = env_dir("USERPROFILE").or_else(|| env_dir("HOME")) {
        if home.starts_with(&r) {
            return true;
        }
    }
    const SYSTEM: [&str; 7] =
        ["WINDIR", "SystemRoot", "APPDATA", "LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"];
    SYSTEM.iter().filter_map(|k| env_dir(k)).any(|d| r.starts_with(&d) || d.starts_with(&r))
}
