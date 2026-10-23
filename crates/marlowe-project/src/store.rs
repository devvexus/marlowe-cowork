//! The project store: files under the Marlowe home and an index in `cowork.db`.
//!
//! ```text
//! <home>\projects\<id>\
//!   project.json                state, written atomically (temp file, then rename)
//!   plan.json  plans\NN.json    the current plan and each plan's history
//!   forms.json                  the forms in force (also embedded in project.json)
//!   phases\<phase>\<step>.jsonl append-only task results (the journal)
//!   sources\<blake3>.md         stored page text, content addressed
//!   archive\NN-<label>.json     snapshot at each phase boundary and at Finish
//!   report.md ...               what the project made
//!   work\                       scratch, disposed per the plan
//! ```
//!
//! The files are the truth. The `projects` table in `cowork.db` is a cache of what each
//! `project.json` says and is rebuilt by scanning ([`ProjectStore::rebuild_index`]). A write that
//! stops between the temp file and the rename leaves the old `project.json` whole.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ids::is_project_id;
use crate::plan::Workspace;
use crate::state::{ProjectState, ProjectStatus};

/// The `projects` table. `CREATE ... IF NOT EXISTS`, so the crew store's own migration list can
/// run it too.
pub const INDEX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id         TEXT PRIMARY KEY,
    session    TEXT,
    run        TEXT,
    title      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    status     TEXT NOT NULL,
    phase      TEXT NOT NULL,
    rev        INTEGER NOT NULL,
    plan_no    INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    dir        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS projects_updated ON projects(updated_ms DESC);
";

/// Why a store call failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A file operation failed.
    #[error("project store: {0}")]
    Io(#[from] io::Error),
    /// A file held JSON that does not parse as it should.
    #[error("project store: bad JSON in {file}: {source}")]
    Json {
        /// The file.
        file: String,
        /// The parse error.
        source: serde_json::Error,
    },
    /// The index failed.
    #[error("project index: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A string that is not a project id was used to build a path.
    #[error("`{0}` is not a project id")]
    BadId(String),
    /// A phase, step or label that cannot be a file name.
    #[error("`{0}` is not a safe name")]
    BadName(String),
    /// No such project.
    #[error("no project `{0}`")]
    NotFound(String),
    /// The project directory already exists.
    #[error("project `{0}` already exists")]
    Exists(String),
    /// No snapshot with this label.
    #[error("project `{id}` has no snapshot `{label}`")]
    NoSnapshot {
        /// The project.
        id: String,
        /// The label asked for.
        label: String,
    },
}

/// One row of the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    /// `p_<8hex>`.
    pub id: String,
    /// The conversation.
    pub session: Option<String>,
    /// The run.
    pub run: Option<String>,
    /// The title.
    pub title: String,
    /// The kind.
    pub kind: String,
    /// The status word.
    pub status: String,
    /// The current phase type.
    pub phase: String,
    /// The revision saved.
    pub rev: u64,
    /// The plan number.
    pub plan_no: u32,
    /// When it last changed.
    pub updated_ms: u64,
    /// The project directory.
    pub dir: String,
}

/// A snapshot in `archive\`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// 1, 2, ... in the order taken.
    pub no: u32,
    /// What it was taken for (`ph_dv` for "before DataValidation", `final`).
    pub label: String,
    /// The state's `updated_ms` when taken.
    pub taken_ms: u64,
    /// The state's revision.
    pub rev: u64,
    /// Journal files (relative to the project directory) and their lengths in bytes at that moment.
    pub journal: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    snapshot: Snapshot,
    state: Value,
}

/// What a disposal removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisposeReport {
    /// Names (relative to the project directory) that were removed.
    pub removed: Vec<String>,
}

/// The store: a projects directory and the index connection.
pub struct ProjectStore {
    root: PathBuf,
    db: Connection,
}

fn json_err(file: &Path) -> impl FnOnce(serde_json::Error) -> StoreError + '_ {
    move |source| StoreError::Json { file: file.display().to_string(), source }
}

fn tmp_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Writes `bytes` to the temp file next to `path` and flushes it to disk, without renaming. Half
/// of [`write_atomic`]; kept separate so a test can stop between the two.
pub fn stage(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let tmp = tmp_of(path);
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(tmp)
}

/// Replaces `path` with `bytes` all at once: a reader sees the old file or the new one, never half.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = stage(path, bytes)?;
    let mut last = None;
    for _ in 0..5 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            // An indexer or a virus scanner can hold the target for a moment on Windows.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

fn safe_name(s: &str) -> Result<&str, StoreError> {
    if !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Ok(s)
    } else {
        Err(StoreError::BadName(s.to_string()))
    }
}

fn journal_files(dir: &Path) -> io::Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    let phases = dir.join("phases");
    if !phases.is_dir() {
        return Ok(out);
    }
    for ph in fs::read_dir(&phases)? {
        let ph = ph?;
        if !ph.path().is_dir() {
            continue;
        }
        for f in fs::read_dir(ph.path())? {
            let f = f?;
            if f.path().extension().is_some_and(|e| e == "jsonl") {
                out.insert(format!("phases/{}/{}", ph.file_name().to_string_lossy(), f.file_name().to_string_lossy()), f.metadata()?.len());
            }
        }
    }
    Ok(out)
}

fn copy_into(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(p) = to.parent() {
        fs::create_dir_all(p)?;
    }
    fs::copy(from, to).map(|_| ())
}

impl ProjectStore {
    /// Opens the store: `root` is `<home>\projects`, `db_path` is `cowork.db` (WAL).
    pub fn open(root: &Path, db_path: &Path) -> Result<ProjectStore, StoreError> {
        fs::create_dir_all(root)?;
        if let Some(p) = db_path.parent() {
            fs::create_dir_all(p)?;
        }
        let db = Connection::open(db_path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(INDEX_SCHEMA)?;
        Ok(ProjectStore { root: root.to_path_buf(), db })
    }

    /// A store whose index lives in memory (tests, and callers that only need the files).
    pub fn open_in_memory(root: &Path) -> Result<ProjectStore, StoreError> {
        fs::create_dir_all(root)?;
        let db = Connection::open_in_memory()?;
        db.execute_batch(INDEX_SCHEMA)?;
        Ok(ProjectStore { root: root.to_path_buf(), db })
    }

    /// The projects directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of project `id`.
    pub fn dir(&self, id: &str) -> Result<PathBuf, StoreError> {
        if is_project_id(id) {
            Ok(self.root.join(id))
        } else {
            Err(StoreError::BadId(id.to_string()))
        }
    }

    /// Whether the project's directory has a `project.json`.
    pub fn exists(&self, id: &str) -> bool {
        self.dir(id).map(|d| d.join("project.json").is_file()).unwrap_or(false)
    }

    /// Creates the project's directory and writes its first files and index row.
    pub fn create(&self, st: &ProjectState) -> Result<PathBuf, StoreError> {
        let dir = self.dir(&st.id)?;
        if dir.exists() {
            return Err(StoreError::Exists(st.id.clone()));
        }
        for sub in ["phases", "sources", "archive", "plans", "work"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        self.save(st)?;
        Ok(dir)
    }

    /// Writes `project.json` (atomically), the plan files and `forms.json`, and updates the index.
    pub fn save(&self, st: &ProjectState) -> Result<(), StoreError> {
        let dir = self.dir(&st.id)?;
        fs::create_dir_all(dir.join("plans"))?;
        let plan = serde_json::to_vec_pretty(&st.plan).map_err(json_err(&dir.join("plan.json")))?;
        write_atomic(&dir.join("plan.json"), &plan)?;
        write_atomic(&dir.join("plans").join(format!("{:02}.json", st.plan.plan_no)), &plan)?;
        let forms = serde_json::to_vec_pretty(&st.forms).map_err(json_err(&dir.join("forms.json")))?;
        write_atomic(&dir.join("forms.json"), &forms)?;
        let body = serde_json::to_vec_pretty(st).map_err(json_err(&dir.join("project.json")))?;
        write_atomic(&dir.join("project.json"), &body)?;
        self.upsert_row(st, &dir)?;
        Ok(())
    }

    /// Reads a project's state. A temp file left beside it by an interrupted write is ignored (it
    /// is removed by [`ProjectStore::rebuild_index`], which runs at boot before any writer).
    pub fn load(&self, id: &str) -> Result<ProjectState, StoreError> {
        let dir = self.dir(id)?;
        let path = dir.join("project.json");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(StoreError::NotFound(id.to_string())),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes).map_err(json_err(&path))
    }

    fn upsert_row(&self, st: &ProjectState, dir: &Path) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO projects (id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET session = excluded.session, run = excluded.run, title = excluded.title,
                 kind = excluded.kind, status = excluded.status, phase = excluded.phase, rev = excluded.rev,
                 plan_no = excluded.plan_no, updated_ms = excluded.updated_ms, dir = excluded.dir",
            params![
                st.id,
                st.session,
                st.run,
                st.title,
                st.kind,
                st.status.as_str(),
                st.current_phase().map(|p| p.ty.clone()).unwrap_or_default(),
                st.rev,
                st.plan.plan_no,
                st.updated_ms,
                dir.display().to_string(),
            ],
        )?;
        Ok(())
    }

    fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRow> {
        Ok(IndexRow {
            id: r.get(0)?,
            session: r.get(1)?,
            run: r.get(2)?,
            title: r.get(3)?,
            kind: r.get(4)?,
            status: r.get(5)?,
            phase: r.get(6)?,
            rev: r.get(7)?,
            plan_no: r.get(8)?,
            updated_ms: r.get(9)?,
            dir: r.get(10)?,
        })
    }

    /// Index rows, most recently changed first.
    pub fn list(&self, limit: u32) -> Result<Vec<IndexRow>, StoreError> {
        let mut q = self.db.prepare(
            "SELECT id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir
             FROM projects ORDER BY updated_ms DESC, id LIMIT ?1",
        )?;
        let rows = q.query_map([limit.max(1)], Self::row_of)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One index row.
    pub fn row(&self, id: &str) -> Result<Option<IndexRow>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir FROM projects WHERE id = ?1",
                [id],
                Self::row_of,
            )
            .optional()?)
    }

    /// Projects a restart should pick up: those whose status is running (they resume from the
    /// journal) or parked on a gate (the open gate is rebuilt from `project.json`).
    pub fn resumable(&self) -> Result<Vec<IndexRow>, StoreError> {
        let mut q = self.db.prepare(
            "SELECT id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir
             FROM projects WHERE status IN ('running', 'awaiting_plan', 'awaiting_checkin') ORDER BY updated_ms, id",
        )?;
        let rows = q.query_map([], Self::row_of)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rebuilds the index from the files: every readable `project.json` gets its row, and rows
    /// whose directory is gone are dropped. Returns the number of projects indexed. A directory
    /// whose `project.json` does not parse is skipped, not fatal.
    pub fn rebuild_index(&self) -> Result<usize, StoreError> {
        self.db.execute("DELETE FROM projects", [])?;
        let mut n = 0;
        let sweep = |dir: &Path| {
            for f in ["project.json", "plan.json", "forms.json"] {
                let _ = fs::remove_file(tmp_of(&dir.join(f)));
            }
        };
        for e in fs::read_dir(&self.root)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if !is_project_id(&name) || !e.path().is_dir() {
                continue;
            }
            sweep(&e.path());
            if let Ok(st) = self.load(&name) {
                self.upsert_row(&st, &e.path())?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Appends one JSON line to `phases/<phase>/<step>.jsonl` (flushed to disk before returning).
    pub fn journal_append(&self, id: &str, phase: &str, step: &str, line: &Value) -> Result<(), StoreError> {
        let dir = self.dir(id)?.join("phases").join(safe_name(phase)?);
        fs::create_dir_all(&dir)?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{}.jsonl", safe_name(step)?)))?;
        let mut text = serde_json::to_string(line).map_err(|source| StoreError::Json { file: "journal line".into(), source })?;
        text.push('\n');
        f.write_all(text.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    /// The journal lines of a step. A last line cut short by a crash is ignored.
    pub fn journal_read(&self, id: &str, phase: &str, step: &str) -> Result<Vec<Value>, StoreError> {
        let path = self.dir(id)?.join("phases").join(safe_name(phase)?).join(format!("{}.jsonl", safe_name(step)?));
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let complete = match text.rfind('\n') {
            Some(i) => &text[..=i],
            None => "",
        };
        Ok(complete.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    }

    /// Stores page text under its blake3 hash (idempotent) and returns the hash.
    pub fn put_source(&self, id: &str, text: &str) -> Result<String, StoreError> {
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        let path = self.dir(id)?.join("sources").join(format!("{hash}.md"));
        if !path.exists() {
            fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
            write_atomic(&path, text.as_bytes())?;
        }
        Ok(hash)
    }

    /// Reads stored page text by hash.
    pub fn get_source(&self, id: &str, hash: &str) -> Result<Option<String>, StoreError> {
        let h = safe_name(hash)?;
        match fs::read_to_string(self.dir(id)?.join("sources").join(format!("{h}.md"))) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `archive\NN-<label>.json`: the state and where every journal file ended. Call it at
    /// each phase boundary (label the phase about to start) and at Finish (`final`).
    pub fn archive(&self, st: &ProjectState, label: &str) -> Result<Snapshot, StoreError> {
        let label = safe_name(label)?;
        let dir = self.dir(&st.id)?;
        let arch = dir.join("archive");
        fs::create_dir_all(&arch)?;
        let no = self.snapshots(&st.id)?.iter().map(|s| s.no).max().unwrap_or(0) + 1;
        let snap = Snapshot { no, label: label.to_string(), taken_ms: st.updated_ms, rev: st.rev, journal: journal_files(&dir)? };
        let file = SnapshotFile { snapshot: snap.clone(), state: st.to_value() };
        let path = arch.join(format!("{no:02}-{label}.json"));
        write_atomic(&path, &serde_json::to_vec_pretty(&file).map_err(json_err(&path))?)?;
        Ok(snap)
    }

    /// The snapshots of a project in the order taken.
    pub fn snapshots(&self, id: &str) -> Result<Vec<Snapshot>, StoreError> {
        let arch = self.dir(id)?.join("archive");
        let mut out = Vec::new();
        if !arch.is_dir() {
            return Ok(out);
        }
        for e in fs::read_dir(&arch)? {
            let p = e?.path();
            if !p.extension().is_some_and(|x| x == "json") {
                continue;
            }
            if let Ok(f) = fs::read(&p).map_err(StoreError::from).and_then(|b| serde_json::from_slice::<SnapshotFile>(&b).map_err(json_err(&p))) {
                out.push(f.snapshot);
            }
        }
        out.sort_by_key(|s| s.no);
        Ok(out)
    }

    /// Reopens a project from a snapshot (a label, or `NN`): the snapshot becomes the state
    /// (running again, `rev` past every revision seen so clients refetch), journal files are cut
    /// back to where they ended then, and everything removed is kept under `.reopened\<now>\`
    /// (the journals as they were, the snapshots taken later). `final` with no snapshot of that
    /// name reopens from `project.json` as it stands, which is how a project whose archive was
    /// disposed of reopens at Finish. The caller re-queues the run at the snapshot's phase.
    pub fn reopen(&self, id: &str, label: &str, now_ms: u64) -> Result<ProjectState, StoreError> {
        let dir = self.dir(id)?;
        let current = self.load(id)?;
        let snaps = self.snapshots(id)?;
        let chosen = match label.parse::<u32>() {
            Ok(n) => snaps.iter().find(|s| s.no == n),
            Err(_) => snaps.iter().rfind(|s| s.label == label),
        };
        let (mut state, snap) = match chosen {
            Some(s) => {
                let path = dir.join("archive").join(format!("{:02}-{}.json", s.no, s.label));
                let f: SnapshotFile = serde_json::from_slice(&fs::read(&path)?).map_err(json_err(&path))?;
                let st: ProjectState = serde_json::from_value(f.state).map_err(json_err(&path))?;
                (st, Some(f.snapshot))
            }
            None if label == "final" => (current.clone(), None),
            None => return Err(StoreError::NoSnapshot { id: id.to_string(), label: label.to_string() }),
        };
        if let Some(snap) = &snap {
            let kept = dir.join(".reopened").join(now_ms.to_string());
            // Journals: keep a full copy, then cut back.
            for (rel, len) in journal_files(&dir)? {
                let path = dir.join(&rel);
                match snap.journal.get(&rel) {
                    Some(mark) if *mark >= len => continue,
                    Some(mark) => {
                        copy_into(&path, &kept.join(&rel))?;
                        fs::OpenOptions::new().write(true).open(&path)?.set_len(*mark)?;
                    }
                    None => {
                        copy_into(&path, &kept.join(&rel))?;
                        fs::remove_file(&path)?;
                    }
                }
            }
            // Snapshots taken after this one describe a future that no longer happened.
            for later in snaps.iter().filter(|s| s.no > snap.no) {
                let name = format!("{:02}-{}.json", later.no, later.label);
                let from = dir.join("archive").join(&name);
                copy_into(&from, &kept.join("archive").join(&name))?;
                fs::remove_file(&from)?;
            }
        }
        // Running again, unless the snapshot was taken parked on a card the user still owes.
        if state.open_gate().is_none() {
            state.status = ProjectStatus::Running;
        }
        state.finished_ms = None;
        state.rev = state.rev.max(current.rev) + 1;
        state.updated_ms = now_ms;
        self.save(&state)?;
        Ok(state)
    }

    /// Writes the pointer a workspace keeps to the project: `<workspace>\.marlowe\projects\<id>.json`.
    pub fn link_workspace(&self, id: &str, workspace: &Path) -> Result<PathBuf, StoreError> {
        let dir = self.dir(id)?;
        let ptr_dir = workspace.join(".marlowe").join("projects");
        fs::create_dir_all(&ptr_dir)?;
        let ptr = ptr_dir.join(format!("{id}.json"));
        let body = serde_json::json!({"project": dir.display().to_string()});
        write_atomic(&ptr, &serde_json::to_vec_pretty(&body).map_err(json_err(&ptr))?)?;
        Ok(ptr)
    }

    /// Disposes of scratch per the plan, inside the project directory only. `Keep` removes
    /// nothing; `Compact` removes `work\` and raw fetch bytes (`sources\*.raw`); `Delete` removes
    /// everything except `project.json` and `report.md`.
    pub fn dispose(&self, id: &str, how: Workspace) -> Result<DisposeReport, StoreError> {
        let dir = self.dir(id)?;
        let mut rep = DisposeReport::default();
        let remove = |p: PathBuf, rep: &mut DisposeReport| -> io::Result<()> {
            let rel = p.strip_prefix(&dir).unwrap_or(&p).display().to_string().replace('\\', "/");
            if p.is_dir() {
                fs::remove_dir_all(&p)?;
            } else if p.exists() {
                fs::remove_file(&p)?;
            } else {
                return Ok(());
            }
            rep.removed.push(rel);
            Ok(())
        };
        match how {
            Workspace::Keep => {}
            Workspace::Compact => {
                remove(dir.join("work"), &mut rep)?;
                let src = dir.join("sources");
                if src.is_dir() {
                    for e in fs::read_dir(&src)? {
                        let p = e?.path();
                        if p.extension().is_some_and(|x| x == "raw") {
                            remove(p, &mut rep)?;
                        }
                    }
                }
            }
            Workspace::Delete => {
                for e in fs::read_dir(&dir)? {
                    let p = e?.path();
                    let keep = p.file_name().is_some_and(|n| n == "project.json" || n == "report.md");
                    if !keep {
                        remove(p, &mut rep)?;
                    }
                }
            }
        }
        rep.removed.sort();
        Ok(rep)
    }
}

#[cfg(test)]
pub(crate) struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("marlowe-project-{tag}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mutation::{ItemUpdate, Mutation};
    use crate::state::tests::sample;
    use crate::state::{Item, ItemStatus};

    fn store(t: &TempDir) -> ProjectStore {
        ProjectStore::open(&t.0.join("projects"), &t.0.join("cowork.db")).unwrap()
    }

    #[test]
    fn create_writes_the_layout_and_load_reads_the_same_state() {
        let t = TempDir::new("create");
        let s = store(&t);
        let st = sample();
        let dir = s.create(&st).unwrap();
        for f in ["project.json", "plan.json", "forms.json", "plans/01.json"] {
            assert!(dir.join(f).is_file(), "{f}");
        }
        for d in ["phases", "sources", "archive", "work"] {
            assert!(dir.join(d).is_dir(), "{d}");
        }
        assert_eq!(s.load(&st.id).unwrap(), st);
        assert!(matches!(s.create(&st), Err(StoreError::Exists(_))));
        assert!(matches!(s.load("p_00000000"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn ids_that_could_escape_the_directory_are_refused() {
        let t = TempDir::new("badid");
        let s = store(&t);
        for bad in ["..", "../x", "p_7f3a91c2/..", "", "C:\\x"] {
            assert!(matches!(s.dir(bad), Err(StoreError::BadId(_))), "{bad}");
            assert!(s.load(bad).is_err());
        }
        assert!(s.journal_append("p_7f3a91c2", "../x", "s", &json!(1)).is_err());
        assert!(s.journal_append("p_7f3a91c2", "ph", "a/b", &json!(1)).is_err());
    }

    #[test]
    fn the_index_follows_saves_and_lists_newest_first() {
        let t = TempDir::new("index");
        let s = store(&t);
        let mut a = sample();
        s.create(&a).unwrap();
        let mut b = sample();
        b.id = "p_00000002".into();
        b.updated_ms = 5_000;
        s.create(&b).unwrap();
        let rows = s.list(10).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["p_00000002", "p_7f3a91c2"]);
        assert_eq!(rows[0].phase, "Plan");
        assert_eq!(rows[0].status, "awaiting_plan");
        a.apply(Mutation::SetStatus(ProjectStatus::Running), 9_000).unwrap();
        s.save(&a).unwrap();
        let row = s.row(&a.id).unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.rev, row.updated_ms), ("running", a.rev, 9_000));
        assert_eq!(s.list(10).unwrap()[0].id, a.id);
        assert_eq!(s.resumable().unwrap().len(), 2, "running and parked projects are picked up at boot");
        a.apply(Mutation::SetStatus(ProjectStatus::Finished), 9_500).unwrap();
        s.save(&a).unwrap();
        assert_eq!(s.resumable().unwrap().len(), 1);
    }

    #[test]
    fn the_index_can_be_rebuilt_by_scanning() {
        let t = TempDir::new("rebuild");
        let s = store(&t);
        let st = sample();
        s.create(&st).unwrap();
        let mut b = sample();
        b.id = "p_00000002".into();
        s.create(&b).unwrap();
        // Lose the cache, and add a corrupt directory and a stray one.
        s.db.execute("DELETE FROM projects", []).unwrap();
        fs::create_dir_all(s.root().join("p_00000003")).unwrap();
        fs::write(s.root().join("p_00000003").join("project.json"), b"{ not json").unwrap();
        fs::create_dir_all(s.root().join("stray")).unwrap();
        assert_eq!(s.rebuild_index().unwrap(), 2);
        assert_eq!(s.list(10).unwrap().len(), 2);
        assert_eq!(s.row(&st.id).unwrap().unwrap().title, st.title);
        // A project removed from disk leaves the index on the next rebuild.
        fs::remove_dir_all(s.dir("p_00000002").unwrap()).unwrap();
        assert_eq!(s.rebuild_index().unwrap(), 1);
    }

    #[test]
    fn a_write_that_stops_before_the_rename_leaves_the_old_state_whole() {
        let t = TempDir::new("crash");
        let s = store(&t);
        let mut st = sample();
        s.create(&st).unwrap();
        let old = s.load(&st.id).unwrap();
        st.apply(Mutation::SetStatus(ProjectStatus::Running), 77).unwrap();
        // The process dies after the temp file is written and before the rename.
        let path = s.dir(&st.id).unwrap().join("project.json");
        let tmp = stage(&path, &serde_json::to_vec(&st).unwrap()).unwrap();
        assert!(tmp.is_file());
        assert_eq!(s.load(&st.id).unwrap_or_else(|e| panic!("{e}")).status, old.status, "the state on disk is the old one");
        // A torn temp file (half the bytes) is no better a reason to fail.
        let bytes = serde_json::to_vec(&st).unwrap();
        fs::write(&tmp, &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(s.load(&st.id).unwrap().rev, old.rev);
        assert!(tmp.exists(), "a reader never touches a writer's temp file");
        // The index is a cache: it rebuilds to the old state, and boot sweeps the stale temp file.
        assert_eq!(s.rebuild_index().unwrap(), 1);
        assert!(!tmp.exists());
        assert_eq!(s.row(&st.id).unwrap().unwrap().rev, old.rev);
    }

    #[test]
    fn write_atomic_replaces_a_file_and_leaves_no_temp() {
        let t = TempDir::new("atomic");
        let p = t.0.join("f.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(!tmp_of(&p).exists());
    }

    #[test]
    fn the_journal_appends_lines_and_ignores_a_torn_last_line() {
        let t = TempDir::new("journal");
        let s = store(&t);
        let st = sample();
        s.create(&st).unwrap();
        s.journal_append(&st.id, "ph_dc", "analyze", &json!({"key": "a", "ok": true})).unwrap();
        s.journal_append(&st.id, "ph_dc", "analyze", &json!({"key": "b"})).unwrap();
        assert_eq!(s.journal_read(&st.id, "ph_dc", "analyze").unwrap(), [json!({"key": "a", "ok": true}), json!({"key": "b"})]);
        assert!(s.journal_read(&st.id, "ph_dc", "other").unwrap().is_empty());
        let path = s.dir(&st.id).unwrap().join("phases/ph_dc/analyze.jsonl");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"key\": \"c\", \"half").unwrap();
        assert_eq!(s.journal_read(&st.id, "ph_dc", "analyze").unwrap().len(), 2);
    }

    #[test]
    fn page_text_is_stored_once_by_hash() {
        let t = TempDir::new("sources");
        let s = store(&t);
        let st = sample();
        s.create(&st).unwrap();
        let h = s.put_source(&st.id, "# Page\n\nBody [L1]").unwrap();
        assert_eq!(h, s.put_source(&st.id, "# Page\n\nBody [L1]").unwrap());
        assert_eq!(s.get_source(&st.id, &h).unwrap().as_deref(), Some("# Page\n\nBody [L1]"));
        assert_eq!(s.get_source(&st.id, "deadbeef").unwrap(), None);
        assert!(s.get_source(&st.id, "../project").is_err());
    }

    fn add_source(st: &mut ProjectState, n: u32, at: u64) {
        let mut it = Item::new(crate::ids::item_id(&st.id, "source", &n.to_string()), "source", "analyze@1");
        it.status = ItemStatus::Queued;
        st.apply(Mutation::PutItem(Box::new(it)), at).unwrap();
    }

    #[test]
    fn archive_then_reopen_restores_the_snapshot_and_truncates_the_journal() {
        let t = TempDir::new("reopen");
        let s = store(&t);
        let mut st = sample();
        s.create(&st).unwrap();
        // Phase one: one source, two journal lines, snapshot "before ph_dv".
        add_source(&mut st, 1, 10);
        s.journal_append(&st.id, "ph_dc", "analyze", &json!({"k": 1})).unwrap();
        s.journal_append(&st.id, "ph_dc", "analyze", &json!({"k": 2})).unwrap();
        s.save(&st).unwrap();
        let snap = s.archive(&st, "ph_dv").unwrap();
        assert_eq!((snap.no, snap.label.as_str()), (1, "ph_dv"));
        assert_eq!(snap.journal["phases/ph_dc/analyze.jsonl"], 16);
        // Phase two goes on: more items, journal lines, a new journal file, a later snapshot.
        add_source(&mut st, 2, 20);
        st.apply(Mutation::SetStatus(ProjectStatus::Finished), 30).unwrap();
        s.journal_append(&st.id, "ph_dc", "analyze", &json!({"k": 3})).unwrap();
        s.journal_append(&st.id, "ph_dv", "verify", &json!({"k": 4})).unwrap();
        s.save(&st).unwrap();
        s.archive(&st, "final").unwrap();
        let live_rev = st.rev;

        let re = s.reopen(&st.id, "ph_dv", 99).unwrap();
        assert_eq!(re.items_of_kind("source").len(), 1, "the snapshot's items");
        assert_eq!(re.status, ProjectStatus::Running);
        assert!(re.rev > live_rev, "clients see a newer revision and refetch");
        assert_eq!(re.updated_ms, 99);
        assert_eq!(re.finished_ms, None);
        assert_eq!(s.load(&st.id).unwrap(), re, "saved");
        assert_eq!(s.journal_read(&st.id, "ph_dc", "analyze").unwrap(), [json!({"k": 1}), json!({"k": 2})]);
        assert!(s.journal_read(&st.id, "ph_dv", "verify").unwrap().is_empty(), "a journal born after the snapshot is gone");
        let kept = s.dir(&st.id).unwrap().join(".reopened/99");
        assert!(fs::read_to_string(kept.join("phases/ph_dc/analyze.jsonl")).unwrap().contains("\"k\":3"), "the cut lines are kept");
        assert!(kept.join("phases/ph_dv/verify.jsonl").is_file());
        assert_eq!(s.snapshots(&st.id).unwrap().len(), 1, "the later snapshot was moved aside");
        assert!(kept.join("archive/02-final.json").is_file());
        assert!(matches!(s.reopen(&st.id, "nope", 100), Err(StoreError::NoSnapshot { .. })));
        // By number.
        assert_eq!(s.reopen(&st.id, "1", 101).unwrap().items_of_kind("source").len(), 1);
    }

    #[test]
    fn a_finished_project_reopens_from_final_even_when_its_archive_was_deleted() {
        let t = TempDir::new("final");
        let s = store(&t);
        let mut st = sample();
        s.create(&st).unwrap();
        st.apply(Mutation::SetStatus(ProjectStatus::Finished), 5).unwrap();
        s.save(&st).unwrap();
        fs::write(s.dir(&st.id).unwrap().join("report.md"), "# Report").unwrap();
        s.dispose(&st.id, Workspace::Delete).unwrap();
        let re = s.reopen(&st.id, "final", 50).unwrap();
        assert_eq!(re.status, ProjectStatus::Running);
        assert!(re.rev > st.rev);
    }

    #[test]
    fn disposal_stays_inside_the_project_and_follows_the_plan() {
        let t = TempDir::new("dispose");
        let s = store(&t);
        let st = sample();
        let dir = s.create(&st).unwrap();
        fs::write(dir.join("work/scratch.txt"), "x").unwrap();
        fs::write(dir.join("sources/aa.raw"), "raw").unwrap();
        s.put_source(&st.id, "page text").unwrap();
        fs::write(dir.join("report.md"), "# R").unwrap();
        s.journal_append(&st.id, "ph_dc", "analyze", &json!(1)).unwrap();
        s.archive(&st, "final").unwrap();
        // The outside world next to the project must survive everything.
        let neighbour = s.root().join("p_99999999");
        fs::create_dir_all(&neighbour).unwrap();
        fs::write(neighbour.join("keep.txt"), "k").unwrap();

        assert!(s.dispose(&st.id, Workspace::Keep).unwrap().removed.is_empty());
        let compact = s.dispose(&st.id, Workspace::Compact).unwrap();
        assert_eq!(compact.removed, ["sources/aa.raw", "work"]);
        assert!(dir.join("sources").read_dir().unwrap().count() == 1 && dir.join("phases").is_dir() && dir.join("archive").is_dir());
        let del = s.dispose(&st.id, Workspace::Delete).unwrap();
        assert!(del.removed.contains(&"archive".to_string()) && del.removed.contains(&"phases".to_string()));
        let left: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.contains(&"project.json".to_string()) && left.contains(&"report.md".to_string()));
        assert!(neighbour.join("keep.txt").is_file());
        assert!(s.dispose("../x", Workspace::Delete).is_err());
    }

    #[test]
    fn the_workspace_keeps_a_pointer_to_the_project() {
        let t = TempDir::new("link");
        let s = store(&t);
        let st = sample();
        s.create(&st).unwrap();
        let ws = t.0.join("workspace");
        let ptr = s.link_workspace(&st.id, &ws).unwrap();
        assert_eq!(ptr, ws.join(".marlowe").join("projects").join(format!("{}.json", st.id)));
        let v: Value = serde_json::from_slice(&fs::read(&ptr).unwrap()).unwrap();
        assert_eq!(v["project"], s.dir(&st.id).unwrap().display().to_string());
    }

    #[test]
    fn updating_an_item_through_the_store_round_trip_keeps_every_field() {
        // The state a reader loads is the state the writer had, live slots and all.
        let t = TempDir::new("roundtrip");
        let s = store(&t);
        let mut st = sample();
        s.create(&st).unwrap();
        add_source(&mut st, 1, 10);
        let id = st.items_of_kind("source")[0].id.clone();
        st.apply(
            Mutation::UpdateItem {
                id,
                update: ItemUpdate {
                    status: Some(ItemStatus::Running),
                    live: Some(crate::mutation::LiveUpdate { started_ms: Some(11), extra: vec![("vram_mb".into(), json!(1024))], ..Default::default() }),
                    ..ItemUpdate::default()
                },
            },
            11,
        )
        .unwrap();
        s.save(&st).unwrap();
        assert_eq!(s.load(&st.id).unwrap(), st);
    }
}
