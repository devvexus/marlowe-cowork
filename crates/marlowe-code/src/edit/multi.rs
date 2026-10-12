//! Edits planned in memory, then committed. [`plan`] reads every file once, applies its edits in
//! order and stops at the first failure without touching disk; [`commit`] writes each file
//! atomically (the [`Workspace`] writes a temp file and renames it over) and, if a write fails,
//! puts back every file it already wrote. `edit` is a plan of one; `multi_edit` is the same plan
//! across files (design 2.1 T8).

use std::path::{Path, PathBuf};

use marlowe_os::workspace::Workspace;

use super::doc::{conform, TextDoc};
use super::engine::{apply, EditError, Tier};

/// One replacement in a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    /// Absolute path.
    pub path: PathBuf,
    /// Empty only to create a file that does not exist (or fill an empty one).
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// What one applied edit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReport {
    /// Index into the plan's edits.
    pub index: usize,
    pub tier: Tier,
    pub count: usize,
    /// 1-based first line of each site.
    pub lines: Vec<usize>,
    pub added: usize,
    pub removed: usize,
    /// The edit created the file.
    pub created: bool,
}

/// A file's new bytes, with what was there before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub path: PathBuf,
    /// `None`: the file does not exist yet.
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
    /// The new text (for FileState snapshots and diffs).
    pub text: String,
    pub edits: Vec<EditReport>,
}

/// Why a plan stopped. Nothing was written.
#[derive(Debug)]
pub enum MultiError {
    Edit { index: usize, path: PathBuf, error: EditError },
    Missing { index: usize, path: PathBuf },
    NotText { path: PathBuf },
    Read { path: PathBuf, error: std::io::Error },
}

impl MultiError {
    /// The failing edit's index, when one edit failed.
    pub fn index(&self) -> Option<usize> {
        match self {
            MultiError::Edit { index, .. } | MultiError::Missing { index, .. } => Some(*index),
            _ => None,
        }
    }
}

/// Why a commit failed, and what it put back.
#[derive(Debug)]
pub struct CommitError {
    pub path: PathBuf,
    pub error: std::io::Error,
    /// Files already written and then restored.
    pub restored: Vec<PathBuf>,
    /// Files that could not be put back (named so the model can say so).
    pub unrestored: Vec<PathBuf>,
}

fn load(ws: &dyn Workspace, path: &Path) -> Result<Option<(Vec<u8>, TextDoc)>, MultiError> {
    match ws.read(path) {
        Ok(bytes) => match TextDoc::decode(&bytes) {
            Some(doc) => Ok(Some((bytes, doc))),
            None => Err(MultiError::NotText { path: path.to_path_buf() }),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MultiError::Read { path: path.to_path_buf(), error: e }),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Applies `edits` in order (edits to the same file see the earlier ones' result) and returns
/// one [`Planned`] per file, in first-mention order. Nothing is written.
pub fn plan(ws: &dyn Workspace, edits: &[FileEdit]) -> Result<Vec<Planned>, MultiError> {
    let mut out: Vec<(Planned, TextDoc)> = Vec::new();
    for (index, e) in edits.iter().enumerate() {
        let slot = match out.iter().position(|(p, _)| same_file(&p.path, &e.path)) {
            Some(i) => i,
            None => {
                let (before, doc) = match load(ws, &e.path)? {
                    Some((bytes, doc)) => (Some(bytes), doc),
                    None => (None, TextDoc::utf8(String::new())),
                };
                let text = doc.text.clone();
                out.push((Planned { path: e.path.clone(), before, after: Vec::new(), text, edits: Vec::new() }, doc));
                out.len() - 1
            }
        };
        let (planned, doc) = &mut out[slot];
        let exists = planned.before.is_some() || !planned.edits.is_empty();
        if e.old.is_empty() && (!exists || planned.text.is_empty()) {
            let created = !exists;
            let next = conform(if exists { Some(&*doc) } else { None }, &e.new);
            *doc = next;
            planned.text = doc.text.clone();
            let added = super::numbering::count_lines(&planned.text);
            planned.edits.push(EditReport { index, tier: Tier::Exact, count: 1, lines: vec![1], added, removed: 0, created });
            continue;
        }
        if !exists {
            return Err(MultiError::Missing { index, path: e.path.clone() });
        }
        let applied = apply(&planned.text, &e.old, &e.new, e.replace_all)
            .map_err(|error| MultiError::Edit { index, path: e.path.clone(), error })?;
        planned.edits.push(EditReport {
            index,
            tier: applied.tier,
            count: applied.count,
            lines: applied.lines,
            added: applied.added,
            removed: applied.removed,
            created: false,
        });
        planned.text = applied.text;
    }
    Ok(out
        .into_iter()
        .map(|(mut p, doc)| {
            p.after = doc.encode_text(&p.text);
            p
        })
        .collect())
}

/// `content` replacing `path` whole, keeping an existing file's encoding (BOM) and CRLF endings.
pub fn plan_write(ws: &dyn Workspace, path: &Path, content: &str) -> Result<Planned, MultiError> {
    let (before, doc) = match load(ws, path) {
        Ok(Some((bytes, doc))) => (Some(bytes), Some(doc)),
        Ok(None) => (None, None),
        // A binary file is replaced whole; there is no encoding to keep.
        Err(MultiError::NotText { .. }) => (ws.read(path).ok(), None),
        Err(e) => return Err(e),
    };
    let new = conform(doc.as_ref(), content);
    let removed = doc.as_ref().map(|d| super::numbering::count_lines(&d.text)).unwrap_or(0);
    let report = EditReport {
        index: 0,
        tier: Tier::Exact,
        count: 1,
        lines: vec![1],
        added: super::numbering::count_lines(&new.text),
        removed,
        created: before.is_none(),
    };
    Ok(Planned { path: path.to_path_buf(), after: new.encode(), text: new.text, before, edits: vec![report] })
}

/// Writes every planned file. On a failed write, files already written are restored (or removed,
/// if the plan created them) and the error says what could not be put back.
pub fn commit(ws: &dyn Workspace, planned: &[Planned]) -> Result<(), CommitError> {
    for (k, p) in planned.iter().enumerate() {
        if let Err(error) = ws.write(&p.path, &p.after) {
            let mut restored = Vec::new();
            let mut unrestored = Vec::new();
            for done in planned[..k].iter().rev() {
                let ok = match &done.before {
                    Some(bytes) => ws.write(&done.path, bytes).is_ok(),
                    None => remove_created(ws, &done.path),
                };
                if ok {
                    restored.push(done.path.clone());
                } else {
                    unrestored.push(done.path.clone());
                }
            }
            return Err(CommitError { path: p.path.clone(), error, restored, unrestored });
        }
    }
    Ok(())
}

/// Removes a file the plan created. The port has no delete, so only this machine's files can be
/// removed; elsewhere the file is emptied and reported as not restored.
fn remove_created(ws: &dyn Workspace, path: &Path) -> bool {
    if ws.kind() == "local" {
        std::fs::remove_file(path).is_ok()
    } else {
        let _ = ws.write(path, b"");
        false
    }
}
