//! Runs and their step checkpoints in `cowork.db` (WAL). The supervisor owns one connection.

use std::path::Path;

use marlowe_proto::RunId;
use rusqlite::{params, Connection, OptionalExtension};

use crate::playbook::{CallCheckpoint, PartialStep};
use crate::run::{Run, StepRecord};
use crate::CrewError;

/// Tables. `crew_runs.record` is the run as JSON (small, rewritten on every update);
/// `crew_run_steps.messages` is the JSON array appended by one agent step.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS crew_runs (
    id          TEXT PRIMARY KEY,
    session     TEXT,
    status      TEXT NOT NULL,
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL,
    record      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS crew_runs_updated ON crew_runs(updated_ms DESC);
CREATE TABLE IF NOT EXISTS crew_run_steps (
    run       TEXT NOT NULL REFERENCES crew_runs(id) ON DELETE CASCADE,
    step      INTEGER NOT NULL,
    key       TEXT NOT NULL,
    role      TEXT NOT NULL,
    action    TEXT NOT NULL,
    messages  TEXT NOT NULL,
    output    TEXT,
    usage     TEXT NOT NULL,
    tainted   INTEGER NOT NULL,
    at_ms     INTEGER NOT NULL,
    truncated INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run, step)
);
CREATE TABLE IF NOT EXISTS crew_run_calls (
    run       TEXT NOT NULL REFERENCES crew_runs(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    full      INTEGER NOT NULL,
    messages  TEXT NOT NULL,
    at_ms     INTEGER NOT NULL,
    summary   TEXT,
    pinned    TEXT,
    tainted   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run, key, seq)
);
";

/// Columns added after a table first shipped: `ALTER TABLE` on an older `cowork.db`, each with
/// the probe that says whether it is there.
const MIGRATIONS: [(&str, &str); 2] = [
    ("SELECT truncated FROM crew_run_steps LIMIT 0", "ALTER TABLE crew_run_steps ADD COLUMN truncated INTEGER NOT NULL DEFAULT 0"),
    (
        "SELECT summary, pinned, tainted FROM crew_run_calls LIMIT 0",
        "ALTER TABLE crew_run_calls ADD COLUMN summary TEXT;
         ALTER TABLE crew_run_calls ADD COLUMN pinned TEXT;
         ALTER TABLE crew_run_calls ADD COLUMN tainted INTEGER NOT NULL DEFAULT 0;",
    ),
];

/// Applies [`SCHEMA`], then each migration whose column is missing.
fn apply_schema(conn: &Connection) -> Result<(), CrewError> {
    conn.execute_batch(SCHEMA)?;
    for (probe, alter) in MIGRATIONS {
        if conn.prepare(probe).is_err() {
            conn.execute_batch(alter)?;
        }
    }
    Ok(())
}

/// The crew's view of `cowork.db`.
pub struct RunStore {
    conn: Connection,
}

impl RunStore {
    /// Opens `cowork.db` (WAL, busy timeout) and applies [`SCHEMA`].
    pub fn open(path: &Path) -> Result<RunStore, CrewError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        apply_schema(&conn)?;
        Ok(RunStore { conn })
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<RunStore, CrewError> {
        let conn = Connection::open_in_memory()?;
        apply_schema(&conn)?;
        Ok(RunStore { conn })
    }

    /// Inserts or replaces the run row.
    pub fn save(&self, run: &Run) -> Result<(), CrewError> {
        self.conn.execute(
            "INSERT INTO crew_runs (id, session, status, created_ms, updated_ms, record)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET session = excluded.session, status = excluded.status,
                 updated_ms = excluded.updated_ms, record = excluded.record",
            params![
                run.id.as_str(),
                run.session.as_ref().map(|s| s.as_str().to_string()),
                run.status.as_str(),
                run.created_ms,
                run.updated_ms,
                to_json(run)?,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &RunId) -> Result<Option<Run>, CrewError> {
        let rec: Option<String> = self
            .conn
            .query_row("SELECT record FROM crew_runs WHERE id = ?1", [id.as_str()], |r| r.get(0))
            .optional()?;
        rec.map(|r| from_json(&r)).transpose()
    }

    /// Newest first.
    pub fn list(&self, limit: u32) -> Result<Vec<Run>, CrewError> {
        self.select("SELECT record FROM crew_runs ORDER BY updated_ms DESC, rowid DESC LIMIT ?1", params![limit.max(1)])
    }

    /// Runs to pick up at boot: `queued` and `running` (a `running` run was interrupted by a
    /// restart and resumes from its last completed step), oldest first.
    pub fn resumable(&self) -> Result<Vec<Run>, CrewError> {
        self.select(
            "SELECT record FROM crew_runs WHERE status IN ('queued', 'running') ORDER BY created_ms, rowid",
            params![],
        )
    }

    /// Appends one step checkpoint.
    pub fn append_step(&self, rec: &StepRecord) -> Result<(), CrewError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO crew_run_steps
                 (run, step, key, role, action, messages, output, usage, tainted, at_ms, truncated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                rec.run.as_str(),
                rec.step,
                rec.key,
                rec.role,
                rec.action,
                to_json(&rec.messages)?,
                rec.output,
                to_json(&rec.usage)?,
                rec.tainted,
                rec.at_ms,
                rec.truncated,
            ],
        )?;
        Ok(())
    }

    /// Every checkpoint of a run, in step order.
    pub fn steps(&self, id: &RunId) -> Result<Vec<StepRecord>, CrewError> {
        let mut st = self.conn.prepare(
            "SELECT step, key, role, action, messages, output, usage, tainted, at_ms, truncated
             FROM crew_run_steps WHERE run = ?1 ORDER BY step",
        )?;
        let rows = st.query_map([id.as_str()], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, bool>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, bool>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (step, key, role, action, messages, output, usage, tainted, at_ms, truncated) = row?;
            out.push(StepRecord {
                run: id.clone(),
                step,
                key,
                role,
                action,
                messages: from_json(&messages)?,
                output,
                usage: from_json(&usage)?,
                tainted,
                truncated,
                at_ms,
            });
        }
        Ok(out)
    }

    /// Appends a per-call checkpoint of the unfinished step `key` (design 3.7): the messages a
    /// model call or a tool batch added, or (`full`) the whole conversation and its compaction
    /// state after it was compacted.
    pub fn append_call(&self, run: &RunId, key: &str, c: &CallCheckpoint<'_>) -> Result<(), CrewError> {
        self.conn.execute(
            "INSERT INTO crew_run_calls (run, key, seq, full, messages, at_ms, summary, pinned, tainted)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM crew_run_calls WHERE run = ?1 AND key = ?2),
                     ?3, ?4, ?5, ?6, ?7, ?8)",
            params![run.as_str(), key, c.full, to_json(&c.messages)?, crate::now_ms(), c.summary, c.pinned, c.tainted],
        )?;
        Ok(())
    }

    /// The unfinished step `key`, rebuilt from its per-call checkpoints: the conversation and
    /// compaction state from the last full snapshot on, tainted if any checkpoint was. Empty
    /// when the step has none.
    pub fn calls(&self, run: &RunId, key: &str) -> Result<PartialStep, CrewError> {
        let mut st = self.conn.prepare(
            "SELECT full, messages, summary, pinned, tainted FROM crew_run_calls WHERE run = ?1 AND key = ?2 ORDER BY seq",
        )?;
        let rows = st.query_map(params![run.as_str(), key], |r| {
            Ok((
                r.get::<_, bool>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = PartialStep::default();
        for row in rows {
            let (full, messages, summary, pinned, tainted) = row?;
            let messages: Vec<marlowe_model::Message> = from_json(&messages)?;
            if full {
                out.messages = messages;
                out.summary = summary;
                out.pinned = pinned;
            } else {
                out.messages.extend(messages);
            }
            out.tainted |= tainted;
        }
        Ok(out)
    }

    /// Drops the per-call checkpoints of `key` once its step row holds the whole conversation.
    pub fn clear_calls(&self, run: &RunId, key: &str) -> Result<(), CrewError> {
        self.conn.execute("DELETE FROM crew_run_calls WHERE run = ?1 AND key = ?2", params![run.as_str(), key])?;
        Ok(())
    }

    fn select(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Run>, CrewError> {
        let mut st = self.conn.prepare(sql)?;
        let recs = st.query_map(args, |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        recs.iter().map(|r| from_json(r)).collect()
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, CrewError> {
    serde_json::to_string(v).map_err(|e| CrewError::Io(std::io::Error::other(e)))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, CrewError> {
    serde_json::from_str(s).map_err(|e| CrewError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::Usage;

    fn rec(run: &RunId, step: u32, truncated: bool) -> StepRecord {
        StepRecord {
            run: run.clone(),
            step,
            key: format!("k{step}"),
            role: "researcher".into(),
            action: "a".into(),
            messages: Vec::new(),
            output: Some("cut off mid-sent".into()),
            usage: Usage::default(),
            tainted: false,
            truncated,
            at_ms: 1,
        }
    }

    /// A cut-off output is stored as cut off, so a resumed run retries it.
    #[test]
    fn truncation_survives_the_store_and_old_databases_migrate() {
        let dir = std::env::temp_dir().join(format!("marlowe-crew-store-{}", RunId::new_random().short()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cowork.db");
        {
            // A database from before the column existed.
            let old = Connection::open(&path).unwrap();
            old.execute_batch(&SCHEMA.replace("    truncated INTEGER NOT NULL DEFAULT 0,
", "")).unwrap();
            assert!(old.prepare("SELECT truncated FROM crew_run_steps LIMIT 0").is_err());
        }
        let store = RunStore::open(&path).unwrap();
        let roles = crate::role::Roles::builtin();
        let r = crate::run::Run::new(None, roles.get("scout").unwrap(), "g", None, None, Default::default());
        let run = r.id.clone();
        store.save(&r).unwrap();
        store.append_step(&rec(&run, 1, true)).unwrap();
        store.append_step(&rec(&run, 2, false)).unwrap();
        let steps = store.steps(&run).unwrap();
        assert_eq!(steps.iter().map(|s| s.truncated).collect::<Vec<_>>(), [true, false]);
        drop(store);
        RunStore::open(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resumed step gets back what compaction kept outside its messages, and its taint.
    #[test]
    fn call_checkpoints_keep_compaction_state_and_taint() {
        use marlowe_model::Message;
        let dir = std::env::temp_dir().join(format!("marlowe-crew-calls-{}", RunId::new_random().short()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cowork.db");
        {
            // A database from before the compaction and taint columns.
            let old = Connection::open(&path).unwrap();
            let schema = SCHEMA
                .replace("    summary   TEXT,\n", "")
                .replace("    pinned    TEXT,\n", "")
                .replace("    tainted   INTEGER NOT NULL DEFAULT 0,\n    PRIMARY KEY (run, key, seq)", "    PRIMARY KEY (run, key, seq)");
            old.execute_batch(&schema).unwrap();
            assert!(old.prepare("SELECT summary FROM crew_run_calls LIMIT 0").is_err());
        }
        let store = RunStore::open(&path).unwrap();
        let roles = crate::role::Roles::builtin();
        let r = crate::run::Run::new(None, roles.get("scout").unwrap(), "g", None, None, Default::default());
        let run = r.id.clone();
        store.save(&r).unwrap();
        let cp = |messages: &'static [Message], full: bool, summary: Option<&'static str>, tainted: bool| CallCheckpoint {
            messages,
            full,
            summary,
            pinned: summary.map(|_| "PINNED"),
            tainted,
        };
        let early: &'static [Message] = Box::leak(Box::new([Message::user("one"), Message::assistant("two")]));
        let tail: &'static [Message] = Box::leak(Box::new([Message::user("kept tail")]));
        let last: &'static [Message] = Box::leak(Box::new([Message::assistant("four")]));
        store.append_call(&run, "k", &cp(early, false, None, false)).unwrap();
        store.append_call(&run, "k", &cp(&[], false, None, true)).unwrap();
        store.append_call(&run, "k", &cp(tail, true, Some("SUMMARY"), false)).unwrap();
        store.append_call(&run, "k", &cp(last, false, None, false)).unwrap();
        let p = store.calls(&run, "k").unwrap();
        assert_eq!(p.messages, [tail[0].clone(), last[0].clone()], "from the last full snapshot on");
        assert_eq!((p.summary.as_deref(), p.pinned.as_deref()), (Some("SUMMARY"), Some("PINNED")));
        assert!(p.tainted, "a tainted checkpoint before the snapshot still counts");
        assert_eq!(store.calls(&run, "other").unwrap(), PartialStep::default());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
