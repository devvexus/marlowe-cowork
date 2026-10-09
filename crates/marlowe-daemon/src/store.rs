//! The store actor: sessions and their messages in `cowork.db`.
//!
//! One thread owns the connection; callers send a command and wait for its reply.

use std::path::Path;
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, unbounded, Sender};
use marlowe_model::{Message, Part, Role};
use marlowe_proto::{SessionId, SessionSummary, StoredTurn};
use rusqlite::{params, Connection, OptionalExtension};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id        TEXT PRIMARY KEY,
    title     TEXT NOT NULL,
    workspace TEXT,
    created   INTEGER NOT NULL,
    updated   INTEGER NOT NULL,
    next_turn INTEGER NOT NULL DEFAULT 0,
    tainted   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_updated ON sessions(updated DESC);
CREATE TABLE IF NOT EXISTS turns (
    session TEXT NOT NULL,
    idx     INTEGER NOT NULL,
    role    TEXT NOT NULL,
    content TEXT NOT NULL,
    at      INTEGER NOT NULL,
    PRIMARY KEY (session, idx)
);
";

pub(crate) type StoreResult<T> = Result<T, String>;

enum Cmd {
    Create(SessionSummary, Sender<StoreResult<()>>),
    SetTitle(SessionId, String, Sender<StoreResult<()>>),
    Append { session: SessionId, messages: Vec<Message>, next_turn: u32, tainted: bool, at: i64, reply: Sender<StoreResult<()>> },
    Get(SessionId, Sender<StoreResult<Option<SessionSummary>>>),
    Tainted(SessionId, Sender<StoreResult<bool>>),
    List(u32, Sender<StoreResult<Vec<SessionSummary>>>),
    Load(SessionId, Sender<StoreResult<Vec<StoredTurn>>>),
    Close,
}

/// A cheap, cloneable handle on the store thread.
#[derive(Clone)]
pub(crate) struct Store {
    tx: Sender<Cmd>,
}

/// Opens `cowork.db` (WAL), applies the schema, and starts the store thread.
pub(crate) fn open(path: &Path) -> Result<(Store, JoinHandle<()>), rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    migrate(&conn)?;
    let (tx, rx) = unbounded::<Cmd>();
    let handle = std::thread::Builder::new()
        .name("marlowe-store".into())
        .spawn(move || {
            let mut conn = conn;
            for cmd in rx.iter() {
                if !serve(&mut conn, cmd) {
                    break;
                }
            }
        })
        .expect("spawn store thread");
    Ok((Store { tx }, handle))
}

fn serve(conn: &mut Connection, cmd: Cmd) -> bool {
    let e = |e: rusqlite::Error| e.to_string();
    match cmd {
        Cmd::Create(s, reply) => {
            let r = conn
                .execute(
                    "INSERT INTO sessions (id, title, workspace, created, updated, next_turn) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![s.id.as_str(), s.title, s.workspace, s.created_ms, s.updated_ms, s.turns],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::SetTitle(id, title, reply) => {
            let r = conn
                .execute("UPDATE sessions SET title = ?2 WHERE id = ?1", params![id.as_str(), title])
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::Append { session, messages, next_turn, tainted, at, reply } => {
            let _ = reply.send(append(conn, &session, &messages, next_turn, tainted, at).map_err(e));
        }
        Cmd::Tainted(id, reply) => {
            let r = conn
                .query_row("SELECT tainted FROM sessions WHERE id = ?1", [id.as_str()], |r| r.get::<_, i64>(0))
                .optional()
                .map(|t| t.unwrap_or(0) != 0)
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::Get(id, reply) => {
            let r = conn
                .query_row(&format!("{SELECT_SUMMARY} WHERE id = ?1"), [id.as_str()], summary_row)
                .optional()
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::List(limit, reply) => {
            let r = (|| {
                let mut st = conn.prepare(&format!("{SELECT_SUMMARY} ORDER BY updated DESC, created DESC LIMIT ?1"))?;
                let rows = st.query_map([limit.max(1)], summary_row)?;
                rows.collect::<Result<Vec<_>, _>>()
            })()
            .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::Load(id, reply) => {
            let _ = reply.send(load(conn, &id));
        }
        Cmd::Close => return false,
    }
    true
}

const SELECT_SUMMARY: &str = "SELECT id, title, workspace, created, updated, next_turn FROM sessions";

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: SessionId(r.get(0)?),
        title: r.get(1)?,
        workspace: r.get(2)?,
        created_ms: r.get(3)?,
        updated_ms: r.get(4)?,
        turns: r.get(5)?,
    })
}

/// Adds `sessions.tainted` to a database created before it existed.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let has: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'tainted'")?
        .exists([])?;
    if !has {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN tainted INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

/// Appends a turn's messages. `tainted` latches: once a session read untrusted content it stays
/// tainted across reloads, including taint that left no trace in its messages (an injected memory).
fn append(conn: &mut Connection, session: &SessionId, messages: &[Message], next_turn: u32, tainted: bool, at: i64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let mut idx: i64 = tx.query_row(
        "SELECT COALESCE(MAX(idx) + 1, 0) FROM turns WHERE session = ?1",
        [session.as_str()],
        |r| r.get(0),
    )?;
    {
        let mut ins = tx.prepare("INSERT INTO turns (session, idx, role, content, at) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for m in messages {
            let content = serde_json::to_string(&m.content).unwrap_or_else(|_| "[]".into());
            ins.execute(params![session.as_str(), idx, role_str(m.role), content, at])?;
            idx += 1;
        }
    }
    tx.execute(
        "UPDATE sessions SET updated = ?2, next_turn = MAX(next_turn, ?3), tainted = MAX(tainted, ?4) WHERE id = ?1",
        params![session.as_str(), at, next_turn, tainted as i64],
    )?;
    tx.commit()
}

fn load(conn: &Connection, id: &SessionId) -> StoreResult<Vec<StoredTurn>> {
    let mut st = conn
        .prepare("SELECT idx, role, content, at FROM turns WHERE session = ?1 ORDER BY idx")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([id.as_str()], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (idx, role, content, at_ms) = row.map_err(|e| e.to_string())?;
        let role = parse_role(&role).ok_or_else(|| format!("bad role '{role}' in turn {idx}"))?;
        let content: Vec<Part> = serde_json::from_str(&content).map_err(|e| format!("turn {idx}: {e}"))?;
        out.push(StoredTurn { idx, role, content, at_ms });
    }
    Ok(out)
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
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

impl Store {
    fn call<T>(&self, make: impl FnOnce(Sender<StoreResult<T>>) -> Cmd) -> StoreResult<T> {
        let (tx, rx) = bounded(1);
        self.tx.send(make(tx)).map_err(|_| "store is closed".to_string())?;
        rx.recv().map_err(|_| "store is closed".to_string())?
    }

    pub fn create(&self, s: SessionSummary) -> StoreResult<()> {
        self.call(|r| Cmd::Create(s, r))
    }

    pub fn set_title(&self, id: &SessionId, title: &str) -> StoreResult<()> {
        self.call(|r| Cmd::SetTitle(id.clone(), title.to_string(), r))
    }

    pub fn append(&self, session: &SessionId, messages: Vec<Message>, next_turn: u32, tainted: bool, at: i64) -> StoreResult<()> {
        self.call(|reply| Cmd::Append { session: session.clone(), messages, next_turn, tainted, at, reply })
    }

    /// Whether the session has read untrusted content.
    pub fn tainted(&self, id: &SessionId) -> StoreResult<bool> {
        self.call(|r| Cmd::Tainted(id.clone(), r))
    }

    pub fn get(&self, id: &SessionId) -> StoreResult<Option<SessionSummary>> {
        self.call(|r| Cmd::Get(id.clone(), r))
    }

    pub fn list(&self, limit: u32) -> StoreResult<Vec<SessionSummary>> {
        self.call(|r| Cmd::List(limit, r))
    }

    pub fn load(&self, id: &SessionId) -> StoreResult<Vec<StoredTurn>> {
        self.call(|r| Cmd::Load(id.clone(), r))
    }

    /// Stops the store thread after pending commands.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_turns_round_trip() {
        let dir = std::env::temp_dir().join(format!("marlowe-daemon-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("s1".into());
        store
            .create(SessionSummary { id: id.clone(), title: String::new(), workspace: Some("C:/w".into()), created_ms: 1, updated_ms: 1, turns: 0 })
            .unwrap();
        store.set_title(&id, "hello").unwrap();
        store.append(&id, vec![Message::user("hi"), Message::assistant("hey")], 1, false, 5).unwrap();
        assert!(!store.tainted(&id).unwrap());
        store.append(&id, vec![Message::user("again")], 2, true, 9).unwrap();
        store.append(&id, vec![Message::user("later")], 3, false, 10).unwrap();
        assert!(store.tainted(&id).unwrap(), "taint latches");
        let s = store.get(&id).unwrap().unwrap();
        assert_eq!((s.title.as_str(), s.turns, s.updated_ms), ("hello", 3, 10));
        let turns = store.load(&id).unwrap();
        assert_eq!(turns.iter().map(|t| t.idx).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(store.list(10).unwrap().len(), 1);
        assert!(store.get(&SessionId("nope".into())).unwrap().is_none());
        store.close();
        handle.join().unwrap();
        assert!(store.list(1).is_err(), "a closed store refuses");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
