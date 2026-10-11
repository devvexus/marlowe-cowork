//! Runs and their step checkpoints in `cowork.db` (WAL). The supervisor owns one connection.

use std::path::Path;

use marlowe_proto::RunId;
use rusqlite::{params, Connection, OptionalExtension};

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
";

/// Columns added after a table first shipped: `ALTER TABLE` on an older `cowork.db`.
const MIGRATIONS: [&str; 1] = ["ALTER TABLE crew_run_steps ADD COLUMN truncated INTEGER NOT NULL DEFAULT 0"];

/// Applies [`SCHEMA`], then each migration whose column is missing.
fn apply_schema(conn: &Connection) -> Result<(), CrewError> {
    conn.execute_batch(SCHEMA)?;
    let has_truncated = conn.prepare("SELECT truncated FROM crew_run_steps LIMIT 0").is_ok();
    if !has_truncated {
        conn.execute_batch(MIGRATIONS[0])?;
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
}
