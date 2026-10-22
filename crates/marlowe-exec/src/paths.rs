//! Path resolution and the workspace boundary.

use std::path::{Component, Path, PathBuf};

/// The user's home folder (`USERPROFILE`, else `HOME`).
fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// `~`, `~/x` and `~\x` name the user's home folder, as in a shell (the permission layer reads
/// them the same way, so a write there is judged where it lands).
pub fn expand_home(raw: &str) -> Option<PathBuf> {
    let rest = raw.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
        return None;
    }
    Some(user_home()?.join(rest.trim_start_matches(['/', '\\'])))
}

/// What a tool says when `dir` is not a folder: if the session's workspace `root` is itself gone
/// and `dir` is it (or under it), that, plainly, so the model does not hunt for the folder.
pub fn not_a_directory(root: Option<&Path>, dir: &Path) -> String {
    match root.filter(|r| !r.is_dir() && inside(r, dir)) {
        Some(r) => marlowe_proto::workspace::missing_message(r),
        None => format!("cwd {} is not a directory", dir.display()),
    }
}

/// Absolute, lexically normalised form of `raw` (relative paths join `base`, else the process
/// cwd; a leading `~` is the user's home).
pub fn resolve(base: Option<&Path>, raw: &str) -> PathBuf {
    if let Some(home) = expand_home(raw.trim()) {
        return normalise(&home);
    }
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

/// Most directory entries [`nearest_existing`] looks at for one path.
const NEAREST_SCAN: usize = 4_000;
/// Deepest level below a folder that a misspelt name is looked for.
const NEAREST_DEPTH: usize = 3;

/// For a path that does not exist, the one existing path it was most likely meant to be: walk to
/// the deepest existing ancestor, then for each missing component take the unique entry (up to
/// three levels down, skipping VCS and build folders) whose name is within two edits of it, or
/// is a suffix of it (`ARCHITECTURE.md` for `docrARCHITECTURE.md`). `None` when the path exists,
/// or when no entry, or more than one, fits.
pub fn nearest_existing(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return None;
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    while !cur.exists() {
        missing.push(cur.file_name()?.to_os_string());
        cur = cur.parent()?.to_path_buf();
    }
    let mut budget = NEAREST_SCAN;
    while let Some(want) = missing.pop() {
        let want = want.to_string_lossy().to_lowercase();
        let mut found: Vec<PathBuf> = Vec::new();
        let mut level = vec![cur.clone()];
        for depth in 0..NEAREST_DEPTH {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else { continue };
                for e in entries.flatten() {
                    budget = budget.checked_sub(1)?;
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                    if is_dir && crate::search::walk::is_skipped(&name) {
                        continue;
                    }
                    if name_fits(&want, &name.to_lowercase()) {
                        found.push(e.path());
                    }
                    if is_dir && depth + 1 < NEAREST_DEPTH {
                        next.push(e.path());
                    }
                }
            }
            level = next;
        }
        if found.len() != 1 {
            return None;
        }
        cur = found.pop()?;
    }
    cur.exists().then_some(cur)
}

/// `name` (lower case) is what `want` (lower case) was meant to be: within two edits (for names
/// of four or more characters), or a suffix of it of at least six.
fn name_fits(want: &str, name: &str) -> bool {
    if want == name {
        return false;
    }
    if name.chars().count() >= 6 && want.ends_with(name) {
        return true;
    }
    want.chars().count() >= 4 && name.chars().count() >= 4 && edit_distance(want, name) <= 2
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            row.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

/// The error-message tail for a missing `abs`: " Did you mean <path>?" (shown against `root`),
/// or nothing.
pub fn did_you_mean(root: Option<&Path>, abs: &Path) -> String {
    nearest_existing(abs).map(|p| format!(" Did you mean {}?", display(root, &p))).unwrap_or_default()
}

#[cfg(test)]
mod tests {

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-nearest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_misspelt_path_names_the_one_file_it_meant() {
        let d = scratch("a");
        std::fs::create_dir_all(d.join("MarloweCowork").join("docs")).unwrap();
        std::fs::write(d.join("MarloweCowork").join("docs").join("ARCHITECTURE.md"), "x").unwrap();
        std::fs::write(d.join("MarloweCowork").join("README.md"), "x").unwrap();
        let hit = super::nearest_existing(&d.join("MarloweCowork").join("docrARCHITECTURE.md")).unwrap();
        assert!(hit.ends_with(std::path::Path::new("docs").join("ARCHITECTURE.md")), "{hit:?}");
        assert_eq!(super::nearest_existing(&d.join("Marl oweCowork")).unwrap(), d.join("MarloweCowork"));
        assert_eq!(super::nearest_existing(&d.join("MarloweCowork")), None, "an existing path needs no guess");
        assert!(super::did_you_mean(Some(&d), &d.join("Marl oweCowork")).contains("Did you mean MarloweCowork?"));
        assert_eq!(super::nearest_existing(&d.join("nothing_like_it.md")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_ambiguous_path_gets_no_guess_and_build_folders_are_not_searched() {
        let d = scratch("b");
        std::fs::create_dir_all(d.join("one")).unwrap();
        std::fs::create_dir_all(d.join("two")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("one").join("ARCHITECTURE.md"), "x").unwrap();
        std::fs::write(d.join("two").join("ARCHITECTURE.md"), "x").unwrap();
        std::fs::write(d.join("target").join("BUILDNOTES.md"), "x").unwrap();
        assert_eq!(super::nearest_existing(&d.join("docrARCHITECTURE.md")), None);
        assert_eq!(super::nearest_existing(&d.join("xBUILDNOTES.md")), None);
        let _ = std::fs::remove_dir_all(&d);
    }
    /// FlashAgent's case (track A): `~/` is the user's home folder, never a folder named `~` in
    /// the project.
    #[test]
    fn a_path_under_the_home_folder_is_read_from_there_not_from_the_project() {
        let Some(home) = super::user_home() else { return };
        let ws = std::env::temp_dir().join("marlowe-paths-ws");
        assert_eq!(super::resolve(Some(&ws), "~/notes.txt"), super::normalise(&home.join("notes.txt")));
        assert_eq!(super::resolve(Some(&ws), "~"), super::normalise(&home));
        assert_eq!(super::resolve(Some(&ws), "~x/y"), super::normalise(&ws.join("~x").join("y")));
        assert!(!super::inside(&ws, &super::resolve(Some(&ws), "~/notes.txt")));
        assert_eq!(super::expand_home("a/~/b"), None);
    }
}
