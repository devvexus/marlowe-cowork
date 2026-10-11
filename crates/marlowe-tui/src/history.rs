//! Prompt history (U10): Up and Down at the edge of the buffer walk it, Ctrl+R searches it.
//!
//! Entries are stored one JSON string per line in `<home>\tui-history.jsonl`, so a multi-line
//! prompt is one entry. The file is read once at start and appended to on send; a failed write
//! only costs the entry.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Entries kept in memory and on a fresh load.
pub const MAX_ENTRIES: usize = 1_000;

#[derive(Debug, Clone, Default)]
pub struct History {
    entries: Vec<String>,
    /// While walking: the index shown. `None` is the live buffer.
    pos: Option<usize>,
    /// The buffer as it was when the walk began; Down past the newest entry puts it back.
    draft: String,
    path: Option<PathBuf>,
}

impl History {
    /// Loads `path` (missing or unreadable is an empty history) and appends to it from now on.
    pub fn load(path: &Path) -> History {
        let entries = std::fs::read_to_string(path)
            .map(|s| {
                let mut v: Vec<String> = s.lines().filter_map(|l| serde_json::from_str::<String>(l).ok()).collect();
                if v.len() > MAX_ENTRIES {
                    v.drain(..v.len() - MAX_ENTRIES);
                }
                v
            })
            .unwrap_or_default();
        History { entries, pos: None, draft: String::new(), path: Some(path.to_path_buf()) }
    }

    /// An in-memory history (tests, and before the profile home is known).
    pub fn in_memory(entries: &[&str]) -> History {
        History { entries: entries.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_walking(&self) -> bool {
        self.pos.is_some()
    }

    /// Records a sent prompt. Blank prompts and an immediate repeat are not kept.
    pub fn push(&mut self, text: &str) {
        self.pos = None;
        let t = text.trim_end();
        if t.trim().is_empty() || self.entries.last().is_some_and(|l| l == t) {
            return;
        }
        self.entries.push(t.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        if let Some(p) = &self.path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                if let Ok(line) = serde_json::to_string(t) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    /// Up: the next older entry, remembering `current` as the draft on the first step. `None`
    /// when there is nothing older.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let next = match self.pos {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.pos = Some(next);
        Some(&self.entries[next])
    }

    /// Down: the next newer entry, or the draft after the newest. `None` when not walking.
    pub fn newer(&mut self) -> Option<&str> {
        let i = self.pos?;
        if i + 1 < self.entries.len() {
            self.pos = Some(i + 1);
            Some(&self.entries[i + 1])
        } else {
            self.pos = None;
            Some(&self.draft)
        }
    }

    /// Stops walking; what is in the buffer is now the user's own text.
    pub fn stop_walking(&mut self) {
        self.pos = None;
    }

    /// Entries containing every word of `query` (case-insensitive), newest first, each once.
    pub fn search(&self, query: &str) -> Vec<&str> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut seen: Vec<&str> = Vec::new();
        for e in self.entries.iter().rev() {
            let low = e.to_lowercase();
            if words.iter().all(|w| low.contains(w.as_str())) && !seen.contains(&e.as_str()) {
                seen.push(e);
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walking_goes_back_through_entries_and_returns_to_the_draft() {
        let mut h = History::in_memory(&["one", "two", "three"]);
        assert_eq!(h.older("half typed"), Some("three"));
        assert_eq!(h.older("ignored"), Some("two"));
        assert_eq!(h.newer(), Some("three"));
        assert_eq!(h.newer(), Some("half typed"));
        assert!(!h.is_walking());
        assert_eq!(h.newer(), None);
    }

    #[test]
    fn the_oldest_entry_is_the_end_of_the_walk() {
        let mut h = History::in_memory(&["only"]);
        assert_eq!(h.older(""), Some("only"));
        assert_eq!(h.older(""), None);
        assert_eq!(History::in_memory(&[]).older("x"), None);
    }

    #[test]
    fn blank_and_repeated_prompts_are_not_kept() {
        let mut h = History::in_memory(&[]);
        h.push("  ");
        h.push("a\n");
        h.push("a");
        h.push("b");
        assert_eq!(h.entries(), ["a", "b"]);
    }

    #[test]
    fn search_is_all_words_newest_first_without_repeats() {
        let h = History::in_memory(&["fix the gate", "run tests", "fix the build", "fix the gate"]);
        assert_eq!(h.search("fix"), ["fix the gate", "fix the build"]);
        assert_eq!(h.search("the BUILD"), ["fix the build"]);
        assert!(h.search("zzz").is_empty());
        assert_eq!(h.search("").len(), 3);
    }

    #[test]
    fn entries_survive_a_restart_one_json_line_each() {
        let dir = std::env::temp_dir().join(format!("marlowe-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tui-history.jsonl");
        let _ = std::fs::remove_file(&p);
        let mut h = History::load(&p);
        h.push("first\nsecond line");
        h.push("third");
        let again = History::load(&p);
        assert_eq!(again.entries(), ["first\nsecond line", "third"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
