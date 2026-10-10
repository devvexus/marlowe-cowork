//! Lexical path handling for shell arguments: no filesystem access, no symlink resolution.

use std::path::{Component, Path, PathBuf};

use super::Dialect;

/// `path` joined onto `base` with `.` and `..` folded away (never above the root or drive).
pub fn lexical_join(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() || path.has_root() { path.to_path_buf() } else { base.join(path) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is `root` or below it. Case-insensitive and separator-agnostic on Windows.
pub fn within(root: &Path, path: &Path) -> bool {
    let (r, p) = (key(root), key(path));
    !r.is_empty() && p.len() >= r.len() && p[..r.len()] == r[..]
}

/// Whether `path` is exactly `root`.
pub(crate) fn same(root: &Path, path: &Path) -> bool {
    key(root) == key(path)
}

fn key(p: &Path) -> Vec<String> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir | Component::RootDir))
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().replace('\\', "/");
            if cfg!(windows) {
                s.to_lowercase()
            } else {
                s
            }
        })
        .collect()
}

/// A shell argument as an absolute path, or `None` when it expands at run time (`~`, `$X`,
/// `%X%`) or names no place on this machine (`/etc` from Git Bash).
pub(crate) fn resolve(dialect: Dialect, cwd: &Path, arg: &str) -> Option<PathBuf> {
    let arg = arg.trim();
    if arg.is_empty() || arg.starts_with('~') || arg.contains('$') || arg.contains('%') {
        return None;
    }
    // PowerShell provider paths (`HKLM:\...`, `Env:`) are not files.
    if dialect == Dialect::PowerShell {
        if let Some((drive, _)) = arg.split_once(':') {
            if drive.len() > 1 && !drive.contains(['/', '\\']) {
                return None;
            }
        }
    }
    let arg = arg.strip_prefix("Microsoft.PowerShell.Core\\FileSystem::").unwrap_or(arg);
    if cfg!(windows) {
        if dialect == Dialect::Bash {
            // Git Bash: `/c/Users` is `C:\Users`; other absolute paths are its own tree.
            let b = arg.as_bytes();
            if b.first() == Some(&b'/') && b.len() >= 2 && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
                let rest = if b.len() > 2 { &arg[3..] } else { "" };
                return Some(lexical_join(Path::new(&format!("{}:\\", b[1] as char)), Path::new(rest)));
            }
            if arg.starts_with('/') && !arg.starts_with("//") {
                return None;
            }
        }
        // `\x` on Windows is relative to the current drive: treat as unknown.
        if (arg.starts_with('\\') && !arg.starts_with("\\\\")) || (arg.starts_with('/') && !arg.starts_with("//")) {
            return None;
        }
        // `C:x` (drive-relative) is unknown too.
        let b = arg.as_bytes();
        if b.len() >= 2 && b[1] == b':' && (b.len() == 2 || (b[2] != b'\\' && b[2] != b'/')) {
            return None;
        }
    }
    Some(lexical_join(cwd, Path::new(arg)))
}

/// Null devices and standard streams, never a file write.
pub(crate) fn is_null_device(arg: &str) -> bool {
    matches!(
        arg.to_ascii_lowercase().as_str(),
        "nul" | "nul:" | "/dev/null" | "$null" | "/dev/stdout" | "/dev/stderr" | "con" | "con:" | "/dev/tty"
    )
}

/// A word as a program name: lower-case file stem (`C:\x\git.exe` -> `git`).
pub(crate) fn program(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    for ext in [".exe", ".com"] {
        if let Some(stem) = base.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    base
}

/// Whether a program word names a script file (`./build.sh`, `tools\x.ps1`) rather than a command.
pub(crate) fn script_kind(word: &str) -> Option<Dialect> {
    let lower = word.to_ascii_lowercase();
    if lower.ends_with(".sh") || lower.ends_with(".bash") {
        Some(Dialect::Bash)
    } else if lower.ends_with(".ps1") || lower.ends_with(".psm1") {
        Some(Dialect::PowerShell)
    } else if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        Some(Dialect::Cmd)
    } else {
        None
    }
}
