//! The workspace picker's path field: what the typed text means as a folder, the folder listings
//! it needs (read on a worker, never while drawing), and Tab completion over them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A folder's children are read at most this many (a huge folder is cut, not walked).
pub const MAX_NAMES: usize = 400;

/// Resolves a chosen path to the stored form, or says why not. Swapped by tests that use paths that
/// do not exist.
pub type Check = fn(&str) -> Result<String, String>;

fn real_check(raw: &str) -> Result<String, String> {
    marlowe_proto::workspace::normalise(raw).map(|p| p.to_string_lossy().into_owned())
}

/// What the picker knows about folders on disk.
#[derive(Debug, Clone)]
pub struct WsPick {
    /// Listings by [`key`] of the folder read (`""` is the roots: `~` and the drives).
    done: HashMap<String, Vec<String>>,
    asked: HashSet<String>,
    /// Why the last Enter was refused; shown under the field until the text changes.
    pub note: Option<String>,
    pub check: Check,
}

impl Default for WsPick {
    fn default() -> Self {
        Self { done: HashMap::new(), asked: HashSet::new(), note: None, check: real_check }
    }
}

pub fn key(dir: &str) -> String {
    dir.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// The separator the typed text uses (`/` unless it has only backslashes).
pub fn sep_of(q: &str) -> char {
    if q.contains('\\') && !q.contains('/') {
        '\\'
    } else {
        '/'
    }
}

/// The typed folder and the name prefix being typed in it: `C:/Users/ma` is (`C:/Users/`, `ma`),
/// `~/` is (`~/`, ``). `None` for text with no folder in it yet.
pub fn split_query(q: &str) -> Option<(String, String)> {
    let b = q.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Some((format!("{q}\\"), String::new()));
    }
    let at = q.rfind(['/', '\\'])?;
    Some((q[..=at].to_string(), q[at + 1..].to_string()))
}

/// The real folder a typed `dir` names (`~` expanded), for reading it.
pub fn expand_dir(dir: &str) -> String {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    match (dir.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            h.join(rest.trim_start_matches(['/', '\\'])).to_string_lossy().into_owned()
        }
        _ => dir.to_string(),
    }
}

/// The sub-folders of `dir`, sorted, hidden and system ones left out, at most [`MAX_NAMES`] read.
/// `""` lists the roots instead. Blocking: runs on a worker.
pub fn list_dirs(dir: &str) -> Vec<String> {
    if dir.is_empty() {
        return roots();
    }
    let mut names: Vec<String> = match std::fs::read_dir(Path::new(dir)) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .take(MAX_NAMES)
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// `~` and every drive root that exists.
fn roots() -> Vec<String> {
    let mut out = vec!["~".to_string()];
    if cfg!(windows) {
        out.extend(('A'..='Z').map(|d| format!("{d}:\\")).filter(|r| Path::new(r).exists()));
    } else {
        out.push("/".into());
    }
    out
}

impl WsPick {
    /// Forgets listings (the picker was opened again: folders may have changed).
    pub fn reset(&mut self) {
        self.done.clear();
        self.asked.clear();
        self.note = None;
    }

    /// The folder to read for `typed` (already expanded), if it is not known or asked for yet.
    pub fn want(&mut self, dir: &str) -> Option<String> {
        self.asked.insert(key(dir)).then(|| dir.to_string())
    }

    /// The folder listing `typed` needs, if not yet known: what to send the worker.
    pub fn want_for(&mut self, q: &str) -> Option<String> {
        let (dir, _) = split_query(q)?;
        self.want(&expand_dir(&dir))
    }

    pub fn store(&mut self, dir: &str, names: Vec<String>) {
        self.done.insert(key(dir), names);
    }

    pub fn has(&self, dir: &str) -> bool {
        self.done.contains_key(&key(dir))
    }

    /// The roots, filtered by what is typed (`""` and anything without a folder in it).
    pub fn root_matches(&self, q: &str) -> Vec<String> {
        let ql = q.to_lowercase();
        self.done.get("").into_iter().flatten().filter(|r| r.to_lowercase().starts_with(&ql)).cloned().collect()
    }

    /// The folders under the typed one whose names start with what is typed, spelled as typed.
    pub fn children(&self, q: &str) -> Vec<String> {
        let Some((dir, prefix)) = split_query(q) else { return Vec::new() };
        let Some(names) = self.done.get(&key(&expand_dir(&dir))) else { return Vec::new() };
        let pl = prefix.to_lowercase();
        let show_hidden = prefix.starts_with(['.', '$']);
        names
            .iter()
            .filter(|n| show_hidden || !n.starts_with(['.', '$']))
            .filter(|n| n.to_lowercase().starts_with(&pl))
            .map(|n| format!("{dir}{n}"))
            .collect()
    }
}

/// The longest start all of `items` share (case ignored, the first's spelling kept).
pub fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else { return String::new() };
    let mut n = first.chars().count();
    for it in &items[1..] {
        n = n.min(first.chars().zip(it.chars()).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count());
    }
    first.chars().take(n).collect()
}

/// The text Tab leaves: the only match, else the common start of the matches when it adds
/// something, else the selected choice, each descended into with a separator.
pub fn complete(q: &str, children: &[String], selected: Option<&str>) -> Option<String> {
    let sep = sep_of(q);
    let descend = |p: &str| if p.ends_with(['/', '\\']) { p.to_string() } else { format!("{p}{sep}") };
    match children {
        [one] => return Some(descend(one)),
        [] => {}
        many => {
            let common = common_prefix(many);
            if common.chars().count() > q.chars().count() {
                return Some(common);
            }
        }
    }
    selected.map(descend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_path_splits_into_folder_and_prefix() {
        assert_eq!(split_query("C:/Users/ma"), Some(("C:/Users/".into(), "ma".into())));
        assert_eq!(split_query("~/"), Some(("~/".into(), String::new())));
        assert_eq!(split_query(r"D:\lab\Pr"), Some((r"D:\lab\".into(), "Pr".into())));
        assert_eq!(split_query("D:"), Some((r"D:\".into(), String::new())));
        assert_eq!(split_query("pruning"), None);
    }

    #[test]
    fn children_are_filtered_by_prefix_and_hide_dot_folders() {
        let mut w = WsPick::default();
        w.store("C:/work/", vec![".git".into(), "Alpha".into(), "alps".into(), "beta".into()]);
        assert_eq!(w.children("C:/work/"), ["C:/work/Alpha", "C:/work/alps", "C:/work/beta"]);
        assert_eq!(w.children("C:/work/AL"), ["C:/work/Alpha", "C:/work/alps"]);
        assert_eq!(w.children("C:/work/.g"), ["C:/work/.git"]);
        assert!(w.children("C:/elsewhere/").is_empty(), "an unread folder offers nothing");
    }

    #[test]
    fn a_folder_is_asked_for_once() {
        let mut w = WsPick::default();
        assert_eq!(w.want_for("C:/work/a").as_deref(), Some("C:/work/"));
        assert_eq!(w.want_for("C:/work/b"), None);
        w.reset();
        assert!(w.want_for("C:/work/b").is_some(), "opening the picker again re-reads");
    }

    #[test]
    fn tab_takes_the_only_match_then_the_common_start_then_the_selection() {
        let kids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(complete("C:/w/al", &kids(&["C:/w/alpha"]), None).as_deref(), Some("C:/w/alpha/"));
        assert_eq!(complete("C:/w/a", &kids(&["C:/w/alpha", "C:/w/alps"]), Some("C:/w/alpha")).as_deref(), Some("C:/w/alp"));
        // No common start beyond what is typed: the selected row.
        assert_eq!(complete("C:/w/alp", &kids(&["C:/w/alpha", "C:/w/alps"]), Some("C:/w/alps")).as_deref(), Some("C:/w/alps/"));
        assert_eq!(complete(r"D:\x\p", &kids(&[r"D:\x\proj"]), None).as_deref(), Some(r"D:\x\proj\"));
        assert_eq!(complete("zz", &[], None), None);
    }

    #[test]
    fn listing_reads_folders_only_sorted_and_capped() {
        let dir = std::env::temp_dir().join(format!("marlowe-wsl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for n in ["b", "A", "c"] {
            std::fs::create_dir_all(dir.join(n)).unwrap();
        }
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        assert_eq!(list_dirs(&dir.to_string_lossy()), ["A", "b", "c"]);
        assert!(list_dirs(&dir.join("nope").to_string_lossy()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
