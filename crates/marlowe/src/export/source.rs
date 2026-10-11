//! Reads trajectories out of `cowork.db` without writing to it: conductor sessions and their
//! turns, `/good` marks (`daemon_kv`), crew runs and their step messages.

use std::collections::HashMap;
use std::path::Path;

use marlowe_crew::Run;
use marlowe_model::{Message, Part, Role};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

/// `daemon_kv` key prefix of a `/good` mark: `export.good.<session id>`. The conductor's `/good`
/// command and `marlowe export good` both write it; the value is a [`GoodMark`].
pub const GOOD_PREFIX: &str = "export.good.";

/// The user said this conductor session (up to turn `upto`, exclusive) is worth keeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoodMark {
    /// Turns (stored messages) kept; `None` keeps the whole session as it is at export time.
    #[serde(default)]
    pub upto: Option<u32>,
    #[serde(default)]
    pub at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub workspace: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub tainted: bool,
}

/// One crew step's conversation (all rows of one playbook step key, in order).
#[derive(Debug, Clone)]
pub struct CrewStep {
    pub key: String,
    pub role: String,
    pub messages: Vec<Message>,
    pub tainted: bool,
}

/// Opens `cowork.db` read-only. A WAL database whose `-shm` cannot be created read-only is
/// opened read-write instead; nothing here writes either way.
pub fn open(path: &Path) -> Result<Connection, String> {
    if !path.is_file() {
        return Err(format!("{} does not exist: nothing has been recorded yet", path.display()));
    }
    let ro = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .and_then(|c| c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).map(|_| c));
    let conn = match ro {
        Ok(c) => c,
        Err(_) => Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
    };
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Opens `cowork.db` for the one write export makes: a `/good` mark.
pub fn open_rw(path: &Path) -> Result<Connection, String> {
    if !path.is_file() {
        return Err(format!("{} does not exist: nothing has been recorded yet", path.display()));
    }
    let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool, String> {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
        .optional()
        .map(|r| r.is_some())
        .map_err(|e| e.to_string())
}

fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool, String> {
    conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))
        .and_then(|mut st| st.exists([col]))
        .map_err(|e| e.to_string())
}

pub fn sessions(conn: &Connection) -> Result<Vec<SessionRow>, String> {
    if !has_table(conn, "sessions")? {
        return Ok(Vec::new());
    }
    let tainted = if has_column(conn, "sessions", "tainted")? { "tainted" } else { "0" };
    let sql = format!("SELECT id, workspace, created, updated, {tainted} FROM sessions ORDER BY created, id");
    let mut st = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                workspace: r.get(1)?,
                created_ms: r.get(2)?,
                updated_ms: r.get(3)?,
                tainted: r.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

fn parse_role(s: &str) -> Option<Role> {
    Some(match s {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => return None,
    })
}

/// The session's stored messages in order, at most `upto` of them.
pub fn turns(conn: &Connection, session: &str, upto: Option<u32>) -> Result<Vec<Message>, String> {
    let mut st = conn
        .prepare("SELECT idx, role, content FROM turns WHERE session = ?1 ORDER BY idx")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([session], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (idx, role, content) = row.map_err(|e| e.to_string())?;
        if upto.is_some_and(|u| idx >= u as i64) {
            break;
        }
        let role = parse_role(&role).ok_or_else(|| format!("session {session} turn {idx}: bad role {role:?}"))?;
        let content: Vec<Part> =
            serde_json::from_str(&content).map_err(|e| format!("session {session} turn {idx}: {e}"))?;
        out.push(Message { role, content });
    }
    Ok(out)
}

/// Number of stored messages in a session.
pub fn turn_count(conn: &Connection, session: &str) -> Result<u32, String> {
    conn.query_row("SELECT COUNT(*) FROM turns WHERE session = ?1", [session], |r| r.get::<_, i64>(0))
        .map(|n| n as u32)
        .map_err(|e| e.to_string())
}

/// Every `/good` mark, by session id.
pub fn good_marks(conn: &Connection) -> Result<HashMap<String, GoodMark>, String> {
    if !has_table(conn, "daemon_kv")? {
        return Ok(HashMap::new());
    }
    let mut st = conn
        .prepare("SELECT key, value FROM daemon_kv WHERE substr(key, 1, ?1) = ?2")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(rusqlite::params![GOOD_PREFIX.len() as i64, GOOD_PREFIX], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut out = HashMap::new();
    for row in rows {
        let (k, v) = row.map_err(|e| e.to_string())?;
        // A mark that does not parse keeps nothing: the keep rule is an explicit yes.
        if let Ok(mark) = serde_json::from_str::<GoodMark>(&v) {
            out.insert(k[GOOD_PREFIX.len()..].to_string(), mark);
        }
    }
    Ok(out)
}

/// Every crew run, oldest first.
pub fn crew_runs(conn: &Connection) -> Result<Vec<Run>, String> {
    if !has_table(conn, "crew_runs")? {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare("SELECT id, record FROM crew_runs ORDER BY created_ms, id").map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (id, record) = row.map_err(|e| e.to_string())?;
        let run: Run = serde_json::from_str(&record).map_err(|e| format!("crew run {id}: {e}"))?;
        out.push(run);
    }
    Ok(out)
}

/// A crew run's step conversations, one per playbook step key, in first-step order.
pub fn crew_steps(conn: &Connection, run: &str) -> Result<Vec<CrewStep>, String> {
    if !has_table(conn, "crew_run_steps")? {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare("SELECT key, role, messages, tainted FROM crew_run_steps WHERE run = ?1 ORDER BY step")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([run], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)? != 0))
        })
        .map_err(|e| e.to_string())?;
    let mut out: Vec<CrewStep> = Vec::new();
    for row in rows {
        let (key, role, messages, tainted) = row.map_err(|e| e.to_string())?;
        let msgs: Vec<Message> = serde_json::from_str(&messages).map_err(|e| format!("crew run {run} step {key}: {e}"))?;
        match out.iter_mut().find(|s| s.key == key) {
            Some(s) => {
                s.messages.extend(msgs);
                s.tainted |= tainted;
            }
            None => out.push(CrewStep { key, role, messages: msgs, tainted }),
        }
    }
    Ok(out)
}

/// Writes or clears a `/good` mark.
pub fn set_good(conn: &Connection, session: &str, mark: Option<&GoodMark>) -> Result<(), String> {
    let key = format!("{GOOD_PREFIX}{session}");
    match mark {
        Some(m) => {
            conn.execute_batch("CREATE TABLE IF NOT EXISTS daemon_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
                .map_err(|e| e.to_string())?;
            let v = serde_json::to_string(m).map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT INTO daemon_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, v],
            )
            .map_err(|e| e.to_string())?;
        }
        None => {
            if has_table(conn, "daemon_kv")? {
                conn.execute("DELETE FROM daemon_kv WHERE key = ?1", [key]).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// The session whose id is `id` or starts with it (unique), if any.
pub fn resolve_session(conn: &Connection, id: &str) -> Result<String, String> {
    let all = sessions(conn)?;
    if all.iter().any(|s| s.id == id) {
        return Ok(id.to_string());
    }
    let hits: Vec<&SessionRow> = all.iter().filter(|s| !id.is_empty() && s.id.starts_with(id)).collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(format!("no session {id:?} (see `marlowe sessions`)")),
        _ => Err(format!("{id:?} matches {} sessions; give more of the id", hits.len())),
    }
}
