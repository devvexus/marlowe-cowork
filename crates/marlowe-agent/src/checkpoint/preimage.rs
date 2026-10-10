//! Pre-images of files the shadow tree cannot hold: ignored files and files outside the
//! workspace that a tool is about to write. One directory per (session, turn) holds an index and
//! the saved bytes; the first save of a path in a turn is the state the turn started with.
//!
//! A conversation rewind reuses turn numbers while the files stay as they are, so the directories
//! of the dropped turns are retired (`<turn>-<stamp>`) rather than deleted: a later rewind to an
//! older turn still undoes what they did, but a new turn with the same number saves its own
//! starting state, and a rewind to that turn ignores everything saved before it began.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CheckpointError;

/// A file bigger than this is not saved (the rewind says so).
pub const MAX_PREIMAGE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Index {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The file's absolute path.
    path: String,
    /// False when the tool is about to create it: a rewind deletes it.
    existed: bool,
    /// The saved bytes, a file name inside the turn's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    saved: Option<String>,
    /// Too large to save: a rewind cannot restore it.
    #[serde(default)]
    skipped: bool,
    /// When it was saved, Unix epoch ms: a turn number can be reused after a conversation
    /// rewind, so "earliest" is by time, not by turn.
    #[serde(default)]
    at_ms: i64,
}

/// What a rewind did with the pre-images of the turns it undid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreimageReport {
    pub restored: usize,
    pub deleted: usize,
    /// Paths a pre-image could not be saved for (too large); they were not restored.
    pub skipped: Vec<String>,
}

pub(crate) struct Preimages {
    root: PathBuf,
}

impl Preimages {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn turn_dir(&self, session: &str, turn: u32) -> PathBuf {
        self.root.join(session).join(format!("{turn:08}"))
    }

    /// Every directory of the session: `(turn, directory)`, live and retired.
    fn all_dirs(&self, session: &str) -> Vec<(u32, PathBuf)> {
        fs::read_dir(self.root.join(session))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let turn = name.split('-').next()?.parse().ok()?;
                e.path().is_dir().then(|| (turn, e.path()))
            })
            .collect()
    }

    fn read(dir: &Path) -> Index {
        fs::read(dir.join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn write(dir: &Path, index: &Index) -> Result<(), CheckpointError> {
        fs::create_dir_all(dir)?;
        let tmp = dir.join("index.json.tmp");
        fs::write(&tmp, serde_json::to_vec(index)?)?;
        fs::rename(&tmp, dir.join("index.json"))?;
        Ok(())
    }

    /// Saves `path` as it is now, unless this turn already saved it. Returns true when it saved
    /// (or recorded) something.
    pub fn save(&self, session: &str, turn: u32, path: &Path) -> Result<bool, CheckpointError> {
        let dir = self.turn_dir(session, turn);
        let mut index = Self::read(&dir);
        let key = path.to_string_lossy().into_owned();
        if index.entries.iter().any(|e| same_path(&e.path, &key)) {
            return Ok(false);
        }
        let entry = match fs::metadata(path) {
            Ok(m) if m.is_file() && m.len() > MAX_PREIMAGE_BYTES => Entry { path: key, existed: true, saved: None, skipped: true, at_ms: super::shadow::now_ms() },
            Ok(m) if m.is_file() => {
                fs::create_dir_all(&dir)?;
                let name = format!("{}.bin", index.entries.len());
                fs::copy(path, dir.join(&name))?;
                Entry { path: key, existed: true, saved: Some(name), skipped: false, at_ms: super::shadow::now_ms() }
            }
            // A directory in the way, or nothing: the tool creates the file.
            Ok(_) | Err(_) => Entry { path: key, existed: false, saved: None, skipped: false, at_ms: super::shadow::now_ms() },
        };
        index.entries.push(entry);
        Self::write(&dir, &index)?;
        Ok(true)
    }

    /// The turns that have saved pre-images, ascending (retired ones are not turns any more).
    pub fn turns(&self, session: &str) -> Vec<u32> {
        let mut out: Vec<u32> = fs::read_dir(self.root.join(session))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
            .collect();
        out.sort_unstable();
        out
    }

    /// Puts back every file the turns `from..` saved, the earliest save of a path winning (the
    /// state before the first of those turns touched it), and deletes the files they created.
    /// Retired turns count too, but only what was saved at or after `since_ms` (when the turn
    /// being rewound to began): older saves belong to a branch the files had already passed.
    pub fn restore_from(&self, session: &str, from: u32, since_ms: Option<i64>) -> PreimageReport {
        let mut report = PreimageReport::default();
        // (saved at, turn, position in its turn, entry): a reused turn number sorts by time.
        let mut all: Vec<(i64, u32, usize, PathBuf, Entry)> = Vec::new();
        for (turn, dir) in self.all_dirs(session).into_iter().filter(|(t, _)| *t >= from) {
            for (i, e) in Self::read(&dir).entries.into_iter().enumerate() {
                if since_ms.is_none_or(|s| e.at_ms >= s) {
                    all.push((e.at_ms, turn, i, dir.clone(), e));
                }
            }
        }
        all.sort_by_key(|a| (a.0, a.1, a.2));
        let mut done: Vec<String> = Vec::new();
        for (_, _, _, dir, e) in all {
            if done.iter().any(|d| same_path(d, &e.path)) {
                continue;
            }
            done.push(e.path.clone());
            let target = Path::new(&e.path);
            if e.skipped {
                report.skipped.push(e.path.clone());
            } else if e.existed {
                if let Some(saved) = &e.saved {
                    if let Some(parent) = target.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    clear_readonly(target);
                    if fs::copy(dir.join(saved), target).is_ok() {
                        report.restored += 1;
                    }
                }
            } else if remove_file(target) {
                report.deleted += 1;
            }
        }
        report
    }

    /// When turn `turn` first saved something, if it did.
    pub fn first_save_ms(&self, session: &str, turn: u32) -> Option<i64> {
        Self::read(&self.turn_dir(session, turn)).entries.iter().map(|e| e.at_ms).min()
    }

    /// The conversation moved back to `from` with the files left alone: the live directories of
    /// `from..` are retired under `stamp`, so the turns that replace them start clean.
    pub fn retire_from(&self, session: &str, from: u32, stamp: i64) {
        for turn in self.turns(session).into_iter().filter(|t| *t >= from) {
            let dir = self.turn_dir(session, turn);
            let _ = fs::rename(&dir, self.root.join(session).join(format!("{turn:08}-{stamp}")));
        }
    }

    /// Forgets the turns `from..` (their effects were undone, or the conversation moved on).
    pub fn discard_from(&self, session: &str, from: u32) {
        for turn in self.turns(session).into_iter().filter(|t| *t >= from) {
            let _ = fs::remove_dir_all(self.turn_dir(session, turn));
        }
    }

    /// Copies the turns before `before` into another session (a fork).
    pub fn copy_before(&self, from: &str, to: &str, before: u32) -> Result<(), CheckpointError> {
        for turn in self.turns(from).into_iter().filter(|t| *t < before) {
            copy_dir(&self.turn_dir(from, turn), &self.turn_dir(to, turn))?;
        }
        Ok(())
    }

    /// Deletes every pre-image of a session.
    pub fn discard_session(&self, session: &str) {
        let _ = fs::remove_dir_all(self.root.join(session));
    }

    /// Deletes every session's pre-images (the whole root).
    pub fn remove_all(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }

    /// Sessions that have pre-images.
    pub fn sessions(&self) -> Vec<String> {
        fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// When a session's pre-images last changed, Unix epoch ms.
    pub fn newest_ms(&self, session: &str) -> Option<i64> {
        let mut newest = None;
        for (_, dir) in self.all_dirs(session) {
            let m = fs::metadata(dir.join("index.json")).and_then(|m| m.modified()).ok()?;
            let ms = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
            newest = newest.max(Some(ms));
        }
        newest
    }
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn clear_readonly(p: &Path) {
    if let Ok(m) = fs::metadata(p) {
        let mut perms = m.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(p, perms);
        }
    }
}

/// Removes a file, clearing a read-only attribute that blocks it. True when it was removed.
pub(crate) fn remove_file(p: &Path) -> bool {
    match fs::remove_file(p) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            clear_readonly(p);
            fs::remove_file(p).is_ok()
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), CheckpointError> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()))?;
        } else {
            fs::copy(&p, to.join(e.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-preimg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_first_save_in_a_turn_wins_and_the_earliest_turn_wins_a_rewind() {
        let d = tmp("restore");
        let store = Preimages::new(d.join("pre"));
        let file = d.join("ignored.log");
        let created = d.join("made-by-tool.txt");
        fs::write(&file, "v0").unwrap();
        // Turn 3 saves v0, then writes v1; a second write in the same turn saves nothing more.
        assert!(store.save("s", 3, &file).unwrap());
        fs::write(&file, "v1").unwrap();
        assert!(!store.save("s", 3, &file).unwrap());
        assert!(store.save("s", 3, &created).unwrap());
        fs::write(&created, "new").unwrap();
        // Turn 5 saves v1, then writes v2.
        assert!(store.save("s", 5, &file).unwrap());
        fs::write(&file, "v2").unwrap();
        assert_eq!(store.turns("s"), vec![3, 5]);

        // Rewinding to turn 5 brings back v1; to turn 3, v0 and the created file is gone.
        let r = store.restore_from("s", 5, None);
        assert_eq!((r.restored, r.deleted), (1, 0));
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
        let r = store.restore_from("s", 3, None);
        assert_eq!((r.restored, r.deleted), (1, 1));
        assert_eq!(fs::read_to_string(&file).unwrap(), "v0");
        assert!(!created.exists());

        store.discard_from("s", 5);
        assert_eq!(store.turns("s"), vec![3]);
        store.copy_before("s", "fork", 4).unwrap();
        assert_eq!(store.turns("fork"), vec![3]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reused_turn_number_starts_clean_and_a_rewind_to_it_ignores_what_came_before_it() {
        let d = tmp("reuse");
        let store = Preimages::new(d.join("pre"));
        let file = d.join("x.log");
        fs::write(&file, "v0").unwrap();
        // Old turn 0 takes v0 to v1, old turn 2 takes v1 to v2.
        store.save("s", 0, &file).unwrap();
        fs::write(&file, "v1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        store.save("s", 2, &file).unwrap();
        fs::write(&file, "v2").unwrap();
        // A conversation rewind to 1 leaves the file at v2 and retires the dropped turns.
        store.retire_from("s", 1, 99);
        assert_eq!(store.turns("s"), vec![0], "a retired turn is not a turn");
        std::thread::sleep(std::time::Duration::from_millis(3));
        let new_turn_began = crate::checkpoint::shadow::now_ms();
        std::thread::sleep(std::time::Duration::from_millis(3));
        // The new turn 1 starts from v2 and records that, though old turn 2 saved the same path.
        assert!(store.save("s", 1, &file).unwrap());
        assert!(store.first_save_ms("s", 1).is_some_and(|ms| ms >= new_turn_began));
        fs::write(&file, "v3").unwrap();

        store.restore_from("s", 1, Some(new_turn_began));
        assert_eq!(fs::read_to_string(&file).unwrap(), "v2", "the state the new turn 1 started with");
        fs::write(&file, "v3").unwrap();
        store.restore_from("s", 0, Some(0));
        assert_eq!(fs::read_to_string(&file).unwrap(), "v0", "older turns still reach back through the retired ones");
        let _ = fs::remove_dir_all(&d);
    }
}
