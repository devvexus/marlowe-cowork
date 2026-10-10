//! Read-before-edit, stale checks and the external-change watcher (design 2.1 T7, B1). Track A.
//!
//! [`FileStates`] lives in the conversation's extensions (one table per agent): `read` records
//! what it saw (`FileState{digest, mtime, size}` plus a bounded snapshot for diffs), and the edit
//! tools record what they wrote. Before an edit of an existing file, [`FileStates::check`] says
//! whether the agent has read it and whether it changed since. After every step,
//! [`FileStates::poll`] notices files changed by anything else (a shell command, the user, a
//! formatter) and describes each for a reminder, at most once per turn per file. Only a change
//! shown whole (a complete short diff) counts as seen; after an append, a cut diff or a change too
//! large to diff, the agent's view stays the old one, so an edit is refused until it reads again.

pub mod diff;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use marlowe_os::workspace::{Meta, Workspace};

pub use diff::short_diff;

/// A blake3 content digest.
pub type Digest = [u8; 32];

/// Files tracked per agent; the least recently touched are dropped beyond this.
pub const MAX_TRACKED: usize = 500;

/// Largest text kept for diffs; bigger files are tracked by digest only.
pub const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;

/// Files a poll re-checks per step (most recently touched first).
pub const POLL_MAX_FILES: usize = 200;

/// Lines of diff one change shows in a reminder.
pub const REMINDER_DIFF_LINES: usize = 40;

/// Above this size a poll does not read a changed file: it reports the sizes (stat only).
pub const POLL_READ_MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> Digest {
    *blake3::hash(bytes).as_bytes()
}

/// Lower-case hex of a digest.
pub fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the agent last saw of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub digest: Digest,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    /// The text, when it is text and at most [`SNAPSHOT_MAX_BYTES`].
    pub snapshot: Option<Arc<str>>,
    /// Touch order (higher is more recent).
    seq: u64,
    /// Size and mtime of a change a poll already reported without showing it whole (the agent's
    /// view above stays the old one).
    noticed: Option<(u64, Option<SystemTime>)>,
    /// The turn a poll last reported this file in.
    reported: Option<u64>,
}

impl FileState {
    /// Whether `meta` still matches this size and mtime (no mtime: never).
    fn matches(size: u64, mtime: Option<SystemTime>, meta: &Meta) -> bool {
        size == meta.len && mtime.is_some() && mtime == meta.modified
    }
}

/// Whether an edit may proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// The file does not exist (creating it needs no read).
    Absent,
    /// The agent has not read it in this session.
    Unread,
    Fresh,
    /// Changed since the agent last saw it; `diff` is what changed (may be empty when no
    /// snapshot was kept).
    Stale { diff: String },
}

/// One file changed outside the agent's own tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub what: Changed,
}

/// How a file changed, as far as the reminder shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    Deleted,
    /// The whole change; the new content now counts as seen.
    Diff(String),
    /// The first [`REMINDER_DIFF_LINES`] changed lines only; the agent must re-read.
    Cut(String),
    /// Only grew at the end (a log); not shown, the agent must re-read.
    Appended { lines: usize, bytes: u64 },
    /// Too large to read or diff, or not text; the agent must re-read.
    Unshown { before: u64, now: u64 },
}

/// The per-agent table. Cloned with the conversation (an `Extensions` value).
#[derive(Debug, Clone, Default)]
pub struct FileStates {
    files: HashMap<String, (PathBuf, FileState)>,
    seq: u64,
    /// Bumped by [`FileStates::begin_turn`]; a poll reports a file at most once per turn.
    turn: u64,
}

/// The table key: the path as given (absolute, normalised), case-folded on Windows.
fn key(path: &Path) -> String {
    if cfg!(windows) {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn snapshot(bytes: &[u8]) -> Option<Arc<str>> {
    if bytes.len() > SNAPSHOT_MAX_BYTES {
        return None;
    }
    crate::edit::TextDoc::decode(bytes).map(|d| Arc::from(d.text.as_str()))
}

impl FileStates {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn get(&self, path: &Path) -> Option<&FileState> {
        self.files.get(&key(path)).map(|(_, s)| s)
    }

    /// Records that the agent saw (read) or produced (wrote) `bytes` at `path`.
    pub fn record(&mut self, path: &Path, bytes: &[u8], meta: Option<&Meta>) {
        self.seq += 1;
        let state = FileState {
            digest: digest(bytes),
            size: bytes.len() as u64,
            mtime: meta.and_then(|m| m.modified),
            snapshot: snapshot(bytes),
            seq: self.seq,
            noticed: None,
            reported: None,
        };
        self.files.insert(key(path), (path.to_path_buf(), state));
        if self.files.len() > MAX_TRACKED {
            if let Some(oldest) = self.files.iter().min_by_key(|(_, (_, s))| s.seq).map(|(k, _)| k.clone()) {
                self.files.remove(&oldest);
            }
        }
    }

    /// Records a file known only by metadata (too large to hash cheaply): any change in size or
    /// mtime then counts as stale.
    pub fn record_meta(&mut self, path: &Path, meta: &Meta) {
        self.seq += 1;
        let state = FileState {
            digest: [0; 32],
            size: meta.len,
            mtime: meta.modified,
            snapshot: None,
            seq: self.seq,
            noticed: None,
            reported: None,
        };
        self.files.insert(key(path), (path.to_path_buf(), state));
    }

    pub fn forget(&mut self, path: &Path) {
        self.files.remove(&key(path));
    }

    /// Whether an edit of `path` may proceed: the agent read it and it has not changed since.
    pub fn check(&self, ws: &dyn Workspace, path: &Path) -> Freshness {
        let meta = match ws.metadata(path) {
            Ok(m) if m.is_file => m,
            Ok(_) => return Freshness::Fresh, // a directory: the tool reports it
            Err(_) => return Freshness::Absent,
        };
        let Some(state) = self.get(path) else { return Freshness::Unread };
        if FileState::matches(state.size, state.mtime, &meta) {
            return Freshness::Fresh;
        }
        if state.digest == [0; 32] {
            return Freshness::Stale { diff: String::new() };
        }
        match ws.read(path) {
            Ok(bytes) if digest(&bytes) == state.digest => Freshness::Fresh,
            Ok(bytes) => Freshness::Stale { diff: change_diff(state, &bytes).0 },
            Err(_) => Freshness::Absent,
        }
    }

    /// A new user turn: each file may be reported again.
    pub fn begin_turn(&mut self) {
        self.turn += 1;
    }

    /// Files changed (or deleted) since the agent last saw them, most recently touched first, at
    /// most [`POLL_MAX_FILES`] checked, each reported at most once per turn. A change shown whole
    /// is recorded as seen; any other keeps the agent's old view (an edit then needs a re-read)
    /// and is not reported again until the file changes again. A deleted file is forgotten. Files
    /// above [`POLL_READ_MAX_BYTES`] are judged by size and mtime only.
    pub fn poll(&mut self, ws: &dyn Workspace) -> Vec<Change> {
        let mut order: Vec<(String, u64)> = self.files.iter().map(|(k, (_, s))| (k.clone(), s.seq)).collect();
        order.sort_by(|a, b| b.1.cmp(&a.1));
        order.truncate(POLL_MAX_FILES);
        let mut changes = Vec::new();
        for (k, _) in order {
            let Some((path, state)) = self.files.get(&k).cloned() else { continue };
            let meta = match ws.metadata(&path) {
                Ok(m) if m.is_file => m,
                Ok(_) => continue,
                Err(_) => {
                    self.files.remove(&k);
                    changes.push(Change { path, what: Changed::Deleted });
                    continue;
                }
            };
            if FileState::matches(state.size, state.mtime, &meta)
                || state.noticed.is_some_and(|(size, mtime)| FileState::matches(size, mtime, &meta))
                || state.reported == Some(self.turn)
            {
                continue;
            }
            let unshown = Changed::Unshown { before: state.size, now: meta.len };
            let what = if state.digest == [0; 32] || meta.len > POLL_READ_MAX_BYTES {
                unshown
            } else {
                let Ok(bytes) = ws.read(&path) else { continue };
                if digest(&bytes) == state.digest {
                    // Touched, not changed: remember the new mtime.
                    if let Some((_, s)) = self.files.get_mut(&k) {
                        s.mtime = meta.modified;
                        s.noticed = None;
                    }
                    continue;
                }
                let old = state.size as usize;
                if bytes.len() > old && digest(&bytes[..old]) == state.digest {
                    let tail = &bytes[old..];
                    let lines = tail.iter().filter(|&&b| b == b'\n').count() + usize::from(!tail.ends_with(b"\n"));
                    Changed::Appended { lines, bytes: tail.len() as u64 }
                } else {
                    match change_diff(&state, &bytes) {
                        (d, Shown::Whole) => {
                            self.record(&path, &bytes, Some(&meta));
                            Changed::Diff(d)
                        }
                        (d, Shown::Cut) => Changed::Cut(d),
                        (_, Shown::Not) => unshown,
                    }
                }
            };
            let seen = matches!(what, Changed::Diff(_));
            let turn = self.turn;
            if let Some((_, s)) = self.files.get_mut(&k) {
                s.seq = state.seq; // a change seen by polling does not count as the agent's touch
                s.reported = Some(turn);
                if !seen {
                    s.noticed = Some((meta.len, meta.modified));
                }
            }
            changes.push(Change { path, what });
        }
        changes
    }
}

/// How much of a change a diff shows.
enum Shown {
    Whole,
    Cut,
    Not,
}

/// A short diff from what the agent saw to `now`, or a size note without a snapshot.
fn change_diff(state: &FileState, now: &[u8]) -> (String, Shown) {
    let new_text = if now.len() <= SNAPSHOT_MAX_BYTES * 4 {
        crate::edit::TextDoc::decode(now).map(|d| d.text)
    } else {
        None
    };
    match (&state.snapshot, new_text) {
        (Some(old), Some(new)) => match diff::short_diff_cut(old, &new, REMINDER_DIFF_LINES) {
            (d, false) => (d, Shown::Whole),
            (d, true) => (d, Shown::Cut),
        },
        _ => (format!("(not shown: {} bytes before, {} bytes now)", state.size, now.len()), Shown::Not),
    }
}

/// The reminder for `changes`, with paths shown by `display`. Empty when nothing changed.
pub fn reminder(changes: &[Change], display: &dyn Fn(&Path) -> String) -> String {
    if changes.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<system-reminder>\nThese files changed since you last read or wrote them, by something other \
         than your file tools (a command, the user, a formatter). Their current content is what \
         counts; re-read before relying on anything else.\n",
    );
    const REREAD: &str = "read it again before editing it";
    for c in changes {
        let shown = display(&c.path);
        let line = match &c.what {
            Changed::Deleted => format!("{shown}: deleted."),
            Changed::Diff(d) => format!("{shown}: modified.\n{}", d.trim_end()),
            Changed::Cut(d) => format!("{shown}: modified; only part of the change is shown, {REREAD}.\n{}", d.trim_end()),
            Changed::Appended { lines, bytes } => format!(
                "{shown}: {lines} line{} ({bytes} bytes) appended, earlier content unchanged; not shown, {REREAD}.",
                if *lines == 1 { "" } else { "s" }
            ),
            Changed::Unshown { before, now } => {
                format!("{shown}: modified ({before} bytes before, {now} now; too large to diff), {REREAD}.")
            }
        };
        out.push_str(&format!("\n{line}\n"));
    }
    out.push_str("</system-reminder>");
    out
}

#[cfg(test)]
mod tests;
