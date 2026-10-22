//! Workspace paths: one normal form for every place a workspace is set or read.
//!
//! A workspace arrives as the user typed it (`~/Projects/MSFFN`, `/Projects/MSFFN`, a quoted path,
//! a relative one). [`normalise`] turns it into the absolute, canonical path that is stored and
//! handed to tools; [`display`] is the `~`-shortened form shown to the user, kept apart from it.

use std::path::{Path, PathBuf};

/// What a path is resolved against; [`Where::current`] reads the process, tests build their own.
#[derive(Clone, Debug, Default)]
pub struct Where {
    pub home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    /// Roots a rooted path without a drive (`/Projects/x`) is tried on, in order.
    pub roots: Vec<PathBuf>,
}

impl Where {
    pub fn current() -> Self {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        let cwd = std::env::current_dir().ok();
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut add = |p: &Path| {
            if let Some(r) = root_of(p) {
                if !roots.contains(&r) {
                    roots.push(r);
                }
            }
        };
        if let Some(h) = &home {
            add(h);
        }
        if let Some(c) = &cwd {
            add(c);
        }
        if cfg!(windows) {
            for d in ['C', 'D', 'E'] {
                add(Path::new(&format!("{d}:\\")));
            }
        } else {
            add(Path::new("/"));
        }
        Self { home, cwd, roots }
    }
}

/// The root (`C:\` or `/`) of an absolute path.
fn root_of(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => out.push(c.as_os_str()),
            _ => break,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// A rooted path with no drive: `/x` or `\x` (not `//` or `\\`, which are network paths).
fn is_driveless(s: &str) -> bool {
    let b = s.as_bytes();
    cfg!(windows) && matches!(b.first(), Some(b'/' | b'\\')) && !matches!(b.get(1), Some(b'/' | b'\\'))
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// The candidates `raw` could mean, most likely first.
fn candidates(raw: &str, at: &Where) -> Vec<PathBuf> {
    let home_rel = |rest: &str| at.home.as_ref().map(|h| h.join(rest.trim_start_matches(['/', '\\'])));
    if let Some(rest) = raw.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with(['/', '\\']) {
            return home_rel(rest).into_iter().collect();
        }
    }
    if is_driveless(raw) {
        let rest = raw.trim_start_matches(['/', '\\']);
        let mut out: Vec<PathBuf> = at.roots.iter().map(|r| r.join(rest)).collect();
        out.extend(home_rel(rest));
        return out;
    }
    let p = Path::new(raw);
    if p.is_absolute() || p.has_root() {
        return vec![p.to_path_buf()];
    }
    // A bare drive (`D:`) means that drive's root, not its working directory.
    if cfg!(windows) && raw.len() == 2 && raw.as_bytes()[1] == b':' {
        return vec![PathBuf::from(format!("{raw}\\"))];
    }
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(at.cwd.as_ref().map(|c| c.join(p)));
    out.extend(at.home.as_ref().map(|h| h.join(p)));
    out
}

/// The absolute, canonical folder `raw` names (no `\\?\` prefix), or why it cannot be one.
pub fn normalise_in(raw: &str, at: &Where) -> Result<PathBuf, String> {
    let raw = raw.trim().trim_matches('"').trim();
    if raw.is_empty() {
        return Err("no path given".into());
    }
    let cands = candidates(raw, at);
    let mut file: Option<&PathBuf> = None;
    for c in &cands {
        match std::fs::metadata(c) {
            Ok(m) if m.is_dir() => {
                let real = std::fs::canonicalize(c).map_err(|e| format!("{} cannot be opened: {e}", c.display()))?;
                return Ok(strip_verbatim(real));
            }
            Ok(_) => file = file.or(Some(c)),
            Err(_) => {}
        }
    }
    if let Some(f) = file {
        return Err(format!("{} is a file, not a folder", f.display()));
    }
    let shown = cands.first().map_or_else(|| raw.to_string(), |c| c.display().to_string());
    Err(format!("{shown} does not exist"))
}

/// [`normalise_in`] against the running process.
pub fn normalise(raw: &str) -> Result<PathBuf, String> {
    normalise_in(raw, &Where::current())
}

/// `path` with the home folder written `~` (the form shown; the stored path stays absolute).
pub fn display_in(path: &str, home: Option<&Path>) -> String {
    let Some(home) = home else { return path.to_string() };
    let key = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let (p, h) = (key(path), key(&home.to_string_lossy()));
    if p == h {
        return "~".into();
    }
    match p.strip_prefix(&format!("{h}/")) {
        // Same length as the lower-cased prefix, so the original text can be sliced.
        Some(_) if path.len() > h.len() => format!("~/{}", path[h.len() + 1..].replace('\\', "/")),
        _ => path.to_string(),
    }
}

pub fn display(path: &str) -> String {
    display_in(path, Where::current().home.as_deref())
}

/// The stored workspace `raw` as it should be: `Some(new)` when it names an existing folder in a
/// different spelling (`~/x`, `/x`, relative, a `\\?\` form), `None` when it is already normal or
/// cannot be resolved (the session keeps what it has and tools report it missing).
pub fn repair_in(raw: &str, at: &Where) -> Option<String> {
    let fixed = normalise_in(raw, at).ok()?.to_string_lossy().into_owned();
    let same = |a: &str, b: &str| a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"));
    (!same(raw.trim(), &fixed)).then_some(fixed)
}

/// The plain message a tool returns when the session's workspace is gone.
pub fn missing_message(path: &Path) -> String {
    format!("this session's workspace {} does not exist — set it in the workspace picker", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree removed on drop.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree() -> (Scratch, Where) {
        let dir = std::env::temp_dir().join(format!("marlowe-ws-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = Scratch(dir);
        let base = strip_verbatim(std::fs::canonicalize(&d.0).unwrap());
        std::fs::create_dir_all(base.join("home").join("Projects").join("MSFFN")).unwrap();
        std::fs::create_dir_all(base.join("drive").join("Data").join("set")).unwrap();
        std::fs::write(base.join("drive").join("note.txt"), "x").unwrap();
        let at = Where { home: Some(base.join("home")), cwd: Some(base.join("drive")), roots: vec![base.join("drive")] };
        (d, at)
    }

    #[test]
    fn tilde_expands_to_the_home_folder() {
        let (_d, at) = tree();
        let got = normalise_in("~/Projects/MSFFN", &at).unwrap();
        assert!(got.ends_with("home/Projects/MSFFN") || got.ends_with(r"home\Projects\MSFFN"), "{got:?}");
        assert_eq!(normalise_in("~", &at).unwrap(), strip_verbatim(std::fs::canonicalize(at.home.clone().unwrap()).unwrap()));
        assert_eq!(normalise_in(r#""~\Projects\MSFFN""#, &at).unwrap(), got);
    }

    #[cfg(windows)]
    #[test]
    fn a_rooted_path_gets_a_drive_and_falls_back_to_home() {
        let (_d, at) = tree();
        // On the first root.
        let got = normalise_in("/Data/set", &at).unwrap();
        assert!(got.to_string_lossy().ends_with(r"drive\Data\set"), "{got:?}");
        // Not on any root, but under home: the stored `/Projects/MSFFN` case.
        let got = normalise_in("/Projects/MSFFN", &at).unwrap();
        assert!(got.to_string_lossy().ends_with(r"home\Projects\MSFFN"), "{got:?}");
        assert!(normalise_in(r"\Projects\MSFFN", &at).is_ok());
    }

    #[test]
    fn a_relative_path_tries_the_cwd_then_home() {
        let (_d, at) = tree();
        assert!(normalise_in("Data/set", &at).unwrap().to_string_lossy().contains("drive"));
        assert!(normalise_in("Projects/MSFFN", &at).unwrap().to_string_lossy().contains("home"));
    }

    #[test]
    fn the_result_is_canonical_without_a_verbatim_prefix() {
        let (_d, at) = tree();
        let got = normalise_in("Data/../Data/./set", &at).unwrap().to_string_lossy().into_owned();
        assert!(!got.starts_with(r"\\?\"), "{got}");
        assert!(!got.contains(".."), "{got}");
    }

    #[test]
    fn a_missing_path_and_a_file_are_refused_with_a_reason() {
        let (_d, at) = tree();
        let e = normalise_in("~/Projects/nope", &at).unwrap_err();
        assert!(e.contains("does not exist") && e.contains("nope"), "{e}");
        let e = normalise_in("Data/set/../../note.txt", &at).unwrap_err();
        assert!(e.contains("is a file"), "{e}");
        assert_eq!(normalise_in("   ", &at).unwrap_err(), "no path given");
    }

    #[test]
    fn display_writes_home_as_a_tilde_and_leaves_the_rest() {
        let home = Path::new(r"C:\Users\matth");
        assert_eq!(display_in(r"C:\Users\matth\Projects\MSFFN", Some(home)), "~/Projects/MSFFN");
        assert_eq!(display_in(r"c:\users\MATTH", Some(home)), "~");
        assert_eq!(display_in(r"C:\Users\matthew\x", Some(home)), r"C:\Users\matthew\x");
        assert_eq!(display_in(r"D:\data", Some(home)), r"D:\data");
    }

    #[test]
    fn repair_rewrites_the_stored_spellings_and_leaves_normal_ones() {
        let (_d, at) = tree();
        let fixed = repair_in("~/Projects/MSFFN", &at).unwrap();
        assert_eq!(repair_in(&fixed, &at), None, "a repaired path is stable");
        #[cfg(windows)]
        assert_eq!(repair_in("/Projects/MSFFN", &at).as_deref(), Some(fixed.as_str()));
        assert_eq!(repair_in("~/Projects/gone", &at), None, "unresolvable stays as it is");
    }

    #[test]
    fn the_missing_message_names_the_picker() {
        let m = missing_message(Path::new("C:/x"));
        assert!(m.starts_with("this session's workspace ") && m.ends_with("does not exist — set it in the workspace picker"), "{m}");
    }
}
