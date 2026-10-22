//! The store actor: sessions and their messages in `cowork.db`.
//!
//! One thread owns the connection; callers send a command and wait for its reply.
//!
//! A session's transcript is one row per message, written as the loop produces it (track F2): an
//! assistant row per model call with its `provider`, `model`, `usage` and `latency_ms`, a row per
//! tool batch with its results, and the user's words when the turn begins. Every row carries the
//! `turn` it belongs to, and a turn's first row the memory journal's `mem_seq` at that moment, so
//! `/rewind` can drop a turn and `Retracted` can name what it wrote. A tool call that finished
//! while its batch was still running is kept in `call_results`, so a daemon killed mid-batch
//! loses no completed call. Rewound rows move to `rewound_turns` with their model, usage and
//! latency; nothing is deleted. A model call that left no row (an empty answer) keeps its usage
//! in `extra_calls`. Both still count in `/cost`: a rewind does not give the money back.

use std::path::Path;
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, unbounded, Sender};
use marlowe_model::{Message, Part, Role, Usage};
use marlowe_proto::{PhaseSpan, SessionId, SessionSummary, StoredTurn, TurnNumbers};
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
CREATE TABLE IF NOT EXISTS daemon_kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS call_results (
    session  TEXT NOT NULL,
    call_id  TEXT NOT NULL,
    content  TEXT NOT NULL,
    is_error INTEGER NOT NULL,
    at       INTEGER NOT NULL,
    PRIMARY KEY (session, call_id)
);
CREATE TABLE IF NOT EXISTS extra_calls (
    session    TEXT NOT NULL,
    turn       INTEGER NOT NULL,
    step       INTEGER,
    at         INTEGER NOT NULL,
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    usage      TEXT NOT NULL,
    latency_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS turn_timings (
    session TEXT NOT NULL,
    turn    INTEGER NOT NULL,
    spans   TEXT NOT NULL,
    numbers TEXT NOT NULL,
    PRIMARY KEY (session, turn)
);
CREATE TABLE IF NOT EXISTS turn_context (
    session TEXT NOT NULL,
    turn    INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    text    TEXT NOT NULL,
    -- the message the text rides on, counted from the turn's first row (hooks may add messages before the prompt)
    at_offset INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session, turn, kind)
);
CREATE TABLE IF NOT EXISTS rewound_turns (
    session     TEXT NOT NULL,
    idx         INTEGER NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    at          INTEGER NOT NULL,
    turn        INTEGER,
    rewound_at  INTEGER NOT NULL
);
";

/// Columns added after the first schema: `(table, column, declaration)`. `migrate` adds the ones
/// an older database lacks.
const ADDED: &[(&str, &str, &str)] = &[
    ("sessions", "tainted", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "parent", "TEXT"),
    ("turns", "turn", "INTEGER"),
    ("turns", "step", "INTEGER"),
    ("turns", "provider", "TEXT"),
    ("turns", "model", "TEXT"),
    ("turns", "usage", "TEXT"),
    ("turns", "latency_ms", "INTEGER"),
    ("turns", "mem_seq", "INTEGER"),
    ("rewound_turns", "step", "INTEGER"),
    ("rewound_turns", "provider", "TEXT"),
    ("rewound_turns", "model", "TEXT"),
    ("rewound_turns", "usage", "TEXT"),
    ("rewound_turns", "latency_ms", "INTEGER"),
    ("rewound_turns", "mem_seq", "INTEGER"),
];

pub(crate) type StoreResult<T> = Result<T, String>;

/// What a model call cost and how long it took, kept on its assistant row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallMeta {
    /// `ollama`, `openrouter`, ... (a [`marlowe_model::ProviderKind`] id).
    pub provider: String,
    /// The provider-native model id.
    pub model: String,
    pub usage: Usage,
    pub latency_ms: u64,
}

/// One message to persist, with where it came from.
#[derive(Debug, Clone)]
pub(crate) struct RowIn {
    pub message: Message,
    pub turn: u32,
    /// The model call (1-based) within the turn this row belongs to, when it has one.
    pub step: Option<u32>,
    /// When it happened, Unix epoch ms.
    pub at: i64,
    pub meta: Option<CallMeta>,
    /// The memory journal's last seq when the turn began (a turn's first row only).
    pub mem_seq: Option<u64>,
}

/// A stored message with its provenance.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowOut {
    pub idx: u32,
    pub role: Role,
    pub content: Vec<Part>,
    pub at_ms: i64,
    pub turn: u32,
    pub step: Option<u32>,
    pub meta: Option<CallMeta>,
    pub mem_seq: Option<u64>,
}

impl RowOut {
    pub fn message(&self) -> Message {
        Message { role: self.role, content: self.content.clone() }
    }
}

/// A tool call that finished before its batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallResultRow {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// What dropping a session's turns removed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Dropped {
    pub removed: u32,
    /// The earliest memory journal seq the dropped turns recorded.
    pub mem_seq: Option<u64>,
    /// When the last kept row happened (the dropped turns began after it), for turns stored
    /// without a `mem_seq`.
    pub kept_until_ms: Option<i64>,
    /// When the first dropped row happened.
    pub first_ms: Option<i64>,
}

/// The per-turn text a turn carried on its user message (`TAIL`: memory and environment, `NOTE`:
/// teams reminders) and the session's prompt sections, kept so a session that is loaded again
/// (a restart, an idle eviction, a rewind) sends the same bytes its earlier requests did.
pub(crate) const TAIL: &str = "tail";
pub(crate) const NOTE: &str = "note";
pub(crate) const SECTIONS: &str = "sections";

/// What [`Store::context`] returns: `(position of the turn's user message, kind, text)` and the
/// prompt sections as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SavedContext {
    pub turns: Vec<(usize, String, String)>,
    pub sections: Option<String>,
}

enum Cmd {
    Create(SessionSummary, Sender<StoreResult<()>>),
    SetTitle(SessionId, String, Sender<StoreResult<()>>),
    AppendRows { session: SessionId, rows: Vec<RowIn>, next_turn: u32, tainted: bool, at: i64, reply: Sender<StoreResult<()>> },
    PutCallResult { session: SessionId, row: CallResultRow, at: i64, reply: Sender<StoreResult<()>> },
    CallResults(SessionId, Sender<StoreResult<Vec<CallResultRow>>>),
    PutSpend { session: SessionId, turn: u32, step: Option<u32>, at: i64, meta: CallMeta, reply: Sender<StoreResult<()>> },
    HiddenSpend(SessionId, Sender<StoreResult<Vec<CallMeta>>>),
    Rows(SessionId, Sender<StoreResult<Vec<RowOut>>>),
    ReplaceLast(SessionId, Message, Sender<StoreResult<()>>),
    DropFrom { session: SessionId, turn: u32, at: i64, reply: Sender<StoreResult<Dropped>> },
    Fork { from: SessionId, to: SessionSummary, before_turn: Option<u32>, reply: Sender<StoreResult<SessionSummary>> },
    Get(SessionId, Sender<StoreResult<Option<SessionSummary>>>),
    Tainted(SessionId, Sender<StoreResult<bool>>),
    List(u32, Sender<StoreResult<Vec<SessionSummary>>>),
    Load(SessionId, Sender<StoreResult<Vec<StoredTurn>>>),
    GetKv(String, Sender<StoreResult<Option<String>>>),
    SetKv(String, String, Sender<StoreResult<()>>),
    LastActivity(Sender<StoreResult<Option<i64>>>),
    PutTimings { session: SessionId, turn: u32, spans: Vec<PhaseSpan>, numbers: TurnNumbers, reply: Sender<StoreResult<()>> },
    PutContext { session: SessionId, turn: u32, kind: String, text: String, offset: u32, reply: Sender<StoreResult<()>> },
    Context(SessionId, Sender<StoreResult<SavedContext>>),
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
        Cmd::AppendRows { session, rows, next_turn, tainted, at, reply } => {
            let _ = reply.send(append_rows(conn, &session, &rows, next_turn, tainted, at).map_err(e));
        }
        Cmd::PutCallResult { session, row, at, reply } => {
            let r = conn
                .execute(
                    "INSERT INTO call_results (session, call_id, content, is_error, at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(session, call_id) DO UPDATE SET content = excluded.content, is_error = excluded.is_error",
                    params![session.as_str(), row.call_id, row.content, row.is_error as i64, at],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::PutSpend { session, turn, step, at, meta, reply } => {
            let usage = serde_json::to_string(&meta.usage).unwrap_or_else(|_| "{}".into());
            let r = conn
                .execute(
                    "INSERT INTO extra_calls (session, turn, step, at, provider, model, usage, latency_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![session.as_str(), turn, step, at, meta.provider, meta.model, usage, meta.latency_ms as i64],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::HiddenSpend(session, reply) => {
            let _ = reply.send(hidden_spend(conn, &session).map_err(e));
        }
        Cmd::CallResults(session, reply) => {
            let r = (|| {
                let mut st = conn.prepare("SELECT call_id, content, is_error FROM call_results WHERE session = ?1 ORDER BY at, call_id")?;
                let rows = st.query_map([session.as_str()], |r| {
                    Ok(CallResultRow { call_id: r.get(0)?, content: r.get(1)?, is_error: r.get::<_, i64>(2)? != 0 })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })()
            .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::Rows(session, reply) => {
            let _ = reply.send(rows(conn, &session));
        }
        Cmd::ReplaceLast(session, message, reply) => {
            let content = serde_json::to_string(&message.content).unwrap_or_else(|_| "[]".into());
            let r = conn
                .execute(
                    "UPDATE turns SET content = ?2 WHERE session = ?1 AND idx = (SELECT MAX(idx) FROM turns WHERE session = ?1)",
                    params![session.as_str(), content],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::DropFrom { session, turn, at, reply } => {
            let _ = reply.send(drop_from(conn, &session, turn, at).map_err(e));
        }
        Cmd::PutContext { session, turn, kind, text, offset, reply } => {
            let r = conn
                .execute(
                    "INSERT INTO turn_context (session, turn, kind, text, at_offset) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(session, turn, kind) DO UPDATE SET text = excluded.text, at_offset = excluded.at_offset",
                    params![session.as_str(), turn, kind, text, offset],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::Context(session, reply) => {
            let _ = reply.send(saved_context(conn, &session).map_err(e));
        }
        Cmd::Fork { from, to, before_turn, reply } => {
            let _ = reply.send(fork(conn, &from, to, before_turn));
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
        Cmd::GetKv(key, reply) => {
            let r = conn
                .query_row("SELECT value FROM daemon_kv WHERE key = ?1", [key], |r| r.get::<_, String>(0))
                .optional()
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::SetKv(key, value, reply) => {
            let r = conn
                .execute(
                    "INSERT INTO daemon_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
        }
        Cmd::LastActivity(reply) => {
            let r = conn.query_row("SELECT MAX(updated) FROM sessions", [], |r| r.get::<_, Option<i64>>(0)).map_err(e);
            let _ = reply.send(r);
        }
        Cmd::PutTimings { session, turn, spans, numbers, reply } => {
            let spans = serde_json::to_string(&spans).unwrap_or_else(|_| "[]".into());
            let numbers = serde_json::to_string(&numbers).unwrap_or_else(|_| "{}".into());
            let r = conn
                .execute(
                    "INSERT INTO turn_timings (session, turn, spans, numbers) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(session, turn) DO UPDATE SET spans = excluded.spans, numbers = excluded.numbers",
                    params![session.as_str(), turn, spans, numbers],
                )
                .map(|_| ())
                .map_err(e);
            let _ = reply.send(r);
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

/// Adds the columns an older database lacks, and numbers the turns of rows saved before rows
/// carried one: a turn starts at each user-role message (a mid-turn steer counts as one there,
/// which only matters for rewinding sessions saved by an older build).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, decl) in ADDED {
        let has: bool = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"))?
            .exists([])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    conn.execute_batch(
        "UPDATE turns SET turn = MAX(0, (SELECT COUNT(*) FROM turns t2 WHERE t2.session = turns.session AND t2.idx <= turns.idx AND t2.role = 'user') - 1)
         WHERE turn IS NULL",
    )?;
    Ok(())
}

type MetaColumns<'a> = (Option<&'a str>, Option<&'a str>, Option<String>, Option<i64>);

fn meta_columns(m: &Option<CallMeta>) -> MetaColumns<'_> {
    match m {
        Some(m) => (Some(m.provider.as_str()), Some(m.model.as_str()), serde_json::to_string(&m.usage).ok(), Some(m.latency_ms as i64)),
        None => (None, None, None, None),
    }
}

/// Appends rows. `tainted` latches: once a session read untrusted content it stays tainted across
/// reloads, including taint that left no trace in its messages (an injected memory). A row of tool
/// results also settles the `call_results` kept for its calls.
fn append_rows(
    conn: &mut Connection,
    session: &SessionId,
    rows: &[RowIn],
    next_turn: u32,
    tainted: bool,
    at: i64,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let mut idx: i64 = tx.query_row(
        "SELECT COALESCE(MAX(idx) + 1, 0) FROM turns WHERE session = ?1",
        [session.as_str()],
        |r| r.get(0),
    )?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO turns (session, idx, role, content, at, turn, step, provider, model, usage, latency_ms, mem_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let mut settle = tx.prepare("DELETE FROM call_results WHERE session = ?1 AND call_id = ?2")?;
        for row in rows {
            let content = serde_json::to_string(&row.message.content).unwrap_or_else(|_| "[]".into());
            let (provider, model, usage, latency) = meta_columns(&row.meta);
            ins.execute(params![
                session.as_str(),
                idx,
                role_str(row.message.role),
                content,
                row.at,
                row.turn,
                row.step,
                provider,
                model,
                usage,
                latency,
                row.mem_seq.map(|s| s as i64),
            ])?;
            idx += 1;
            for r in row.message.tool_results_iter() {
                settle.execute(params![session.as_str(), r.call_id])?;
            }
        }
    }
    tx.execute(
        "UPDATE sessions SET updated = ?2, next_turn = MAX(next_turn, ?3), tainted = MAX(tainted, ?4) WHERE id = ?1",
        params![session.as_str(), at, next_turn, tainted as i64],
    )?;
    tx.commit()
}

type RowTuple = (u32, String, String, i64, u32, Option<u32>, Option<String>, Option<String>, Option<String>, Option<i64>, Option<i64>);

fn rows(conn: &Connection, id: &SessionId) -> StoreResult<Vec<RowOut>> {
    let mut st = conn
        .prepare(
            "SELECT idx, role, content, at, COALESCE(turn, 0), step, provider, model, usage, latency_ms, mem_seq
             FROM turns WHERE session = ?1 ORDER BY idx",
        )
        .map_err(|e| e.to_string())?;
    let found = st
        .query_map([id.as_str()], |r| -> rusqlite::Result<RowTuple> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in found {
        let (idx, role, content, at_ms, turn, step, provider, model, usage, latency, mem_seq) = row.map_err(|e| e.to_string())?;
        let role = parse_role(&role).ok_or_else(|| format!("bad role '{role}' in turn {idx}"))?;
        let content: Vec<Part> = serde_json::from_str(&content).map_err(|e| format!("turn {idx}: {e}"))?;
        let meta = match (provider, model) {
            (Some(provider), Some(model)) => Some(CallMeta {
                provider,
                model,
                usage: usage.and_then(|u| serde_json::from_str(&u).ok()).unwrap_or_default(),
                latency_ms: latency.unwrap_or(0).max(0) as u64,
            }),
            _ => None,
        };
        out.push(RowOut { idx, role, content, at_ms, turn, step, meta, mem_seq: mem_seq.map(|s| s.max(0) as u64) });
    }
    Ok(out)
}

/// The model calls of a session that no longer show in its transcript: rows a rewind moved aside
/// and calls that produced no row.
fn hidden_spend(conn: &Connection, session: &SessionId) -> rusqlite::Result<Vec<CallMeta>> {
    let mut st = conn.prepare(
        "SELECT provider, model, usage, latency_ms FROM rewound_turns WHERE session = ?1 AND provider IS NOT NULL AND model IS NOT NULL
         UNION ALL
         SELECT provider, model, usage, latency_ms FROM extra_calls WHERE session = ?1",
    )?;
    let found = st.query_map([session.as_str()], |r| {
        Ok(CallMeta {
            provider: r.get(0)?,
            model: r.get(1)?,
            usage: r.get::<_, Option<String>>(2)?.and_then(|u| serde_json::from_str(&u).ok()).unwrap_or_default(),
            latency_ms: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
        })
    })?;
    found.collect()
}

/// Moves every row of turns `turn..` to `rewound_turns` and sets the session's next turn back.
fn drop_from(conn: &mut Connection, session: &SessionId, turn: u32, at: i64) -> rusqlite::Result<Dropped> {
    let tx = conn.transaction()?;
    let first: Option<(i64, i64)> = tx
        .query_row(
            "SELECT idx, at FROM turns WHERE session = ?1 AND COALESCE(turn, 0) >= ?2 ORDER BY idx LIMIT 1",
            params![session.as_str(), turn],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut out = Dropped::default();
    if let Some((first_idx, first_ms)) = first {
        out.first_ms = Some(first_ms);
        out.kept_until_ms = tx.query_row(
            "SELECT MAX(at) FROM turns WHERE session = ?1 AND idx < ?2",
            params![session.as_str(), first_idx],
            |r| r.get(0),
        )?;
        out.mem_seq = tx
            .query_row(
                "SELECT MIN(mem_seq) FROM turns WHERE session = ?1 AND idx >= ?2 AND mem_seq IS NOT NULL",
                params![session.as_str(), first_idx],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .map(|s| s.max(0) as u64);
        out.removed = tx.execute(
            "INSERT INTO rewound_turns (session, idx, role, content, at, turn, rewound_at, step, provider, model, usage, latency_ms, mem_seq)
             SELECT session, idx, role, content, at, turn, ?3, step, provider, model, usage, latency_ms, mem_seq FROM turns WHERE session = ?1 AND idx >= ?2",
            params![session.as_str(), first_idx, at],
        )? as u32;
        tx.execute("DELETE FROM turns WHERE session = ?1 AND idx >= ?2", params![session.as_str(), first_idx])?;
    }
    tx.execute("DELETE FROM call_results WHERE session = ?1", [session.as_str()])?;
    tx.execute("DELETE FROM turn_timings WHERE session = ?1 AND turn >= ?2", params![session.as_str(), turn])?;
    tx.execute("DELETE FROM turn_context WHERE session = ?1 AND kind != ?3 AND turn >= ?2", params![session.as_str(), turn, SECTIONS])?;
    tx.execute(
        "UPDATE sessions SET next_turn = MIN(next_turn, ?2), updated = ?3 WHERE id = ?1",
        params![session.as_str(), turn, at],
    )?;
    tx.commit()?;
    Ok(out)
}

/// A copy of `from` under `to`'s id: its rows of turns before `before_turn` (all of them for
/// `None`), the workspace, the taint latch and a link to the original.
fn fork(conn: &mut Connection, from: &SessionId, to: SessionSummary, before_turn: Option<u32>) -> StoreResult<SessionSummary> {
    let source = rows(conn, from)?;
    let tainted: i64 = conn
        .query_row("SELECT tainted FROM sessions WHERE id = ?1", [from.as_str()], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let kept: Vec<&RowOut> = source.iter().filter(|r| before_turn.is_none_or(|t| r.turn < t)).collect();
    let next_turn = match before_turn {
        Some(t) => t,
        None => kept.iter().map(|r| r.turn + 1).max().unwrap_or(0),
    };
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO sessions (id, title, workspace, created, updated, next_turn, tainted, parent) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![to.id.as_str(), to.title, to.workspace, to.created_ms, to.updated_ms, next_turn, tainted, from.as_str()],
    )
    .map_err(|e| e.to_string())?;
    {
        let mut ins = tx
            .prepare(
                "INSERT INTO turns (session, idx, role, content, at, turn, step, provider, model, usage, latency_ms, mem_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(|e| e.to_string())?;
        for (i, row) in kept.iter().enumerate() {
            let content = serde_json::to_string(&row.content).map_err(|e| e.to_string())?;
            let (provider, model, usage, latency) = meta_columns(&row.meta);
            ins.execute(params![
                to.id.as_str(),
                i as i64,
                role_str(row.role),
                content,
                row.at_ms,
                row.turn,
                row.step,
                provider,
                model,
                usage,
                latency,
                row.mem_seq.map(|s| s as i64),
            ])
            .map_err(|e| e.to_string())?;
        }
    }
    tx.execute(
        "INSERT INTO turn_timings (session, turn, spans, numbers)
         SELECT ?1, turn, spans, numbers FROM turn_timings WHERE session = ?2 AND turn < ?3",
        params![to.id.as_str(), from.as_str(), next_turn],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO turn_context (session, turn, kind, text, at_offset)
         SELECT ?1, turn, kind, text, at_offset FROM turn_context WHERE session = ?2 AND (kind = ?4 OR turn < ?3)",
        params![to.id.as_str(), from.as_str(), next_turn, SECTIONS],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(SessionSummary { turns: next_turn, ..to })
}

/// Puts a session's saved per-turn text and prompt sections back on `convo` (loaded from the store
/// or rewound): each turn's request is then rebuilt with the bytes it first carried.
pub(crate) fn restore_context(store: &Store, convo: &mut marlowe_agent::Conversation) {
    use marlowe_agent::context::{TailReminders, TurnNotes};
    let Ok(saved) = store.context(&convo.session) else { return };
    for (pos, kind, text) in saved.turns {
        match kind.as_str() {
            TAIL => convo.ext.get_or_default::<TailReminders>().0.push((pos, text)),
            NOTE => convo.ext.get_or_default::<TurnNotes>().0.push((pos, text)),
            _ => {}
        }
    }
    if let Some(s) = saved.sections.and_then(|j| serde_json::from_str(&j).ok()) {
        convo.ext.insert(marlowe_agent::system::RestoredSections(s));
    }
}

/// A session's saved per-turn text, each placed at its turn's user message (counted among the rows
/// as they load), and its prompt sections.
fn saved_context(conn: &Connection, id: &SessionId) -> rusqlite::Result<SavedContext> {
    let mut out = SavedContext::default();
    let mut st = conn.prepare(
        "SELECT (SELECT COUNT(*) FROM turns t2 WHERE t2.session = c.session
                   AND t2.idx < (SELECT MIN(t.idx) FROM turns t WHERE t.session = c.session AND COALESCE(t.turn, 0) = c.turn)) + c.at_offset,
                c.kind, c.text
         FROM turn_context c
         WHERE c.session = ?1 AND c.kind != ?2
           AND EXISTS (SELECT 1 FROM turns t WHERE t.session = c.session AND COALESCE(t.turn, 0) = c.turn)
         ORDER BY c.turn, c.kind DESC",
    )?;
    let rows = st.query_map(params![id.as_str(), SECTIONS], |r| Ok((r.get::<_, i64>(0)? as usize, r.get(1)?, r.get(2)?)))?;
    out.turns = rows.collect::<Result<_, _>>()?;
    out.sections = conn
        .query_row("SELECT text FROM turn_context WHERE session = ?1 AND kind = ?2", params![id.as_str(), SECTIONS], |r| r.get(0))
        .optional()?;
    Ok(out)
}

fn load(conn: &Connection, id: &SessionId) -> StoreResult<Vec<StoredTurn>> {
    let mut st = conn
        .prepare("SELECT idx, role, content, at, COALESCE(turn, 0) FROM turns WHERE session = ?1 ORDER BY idx")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([id.as_str()], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, u32>(4)?))
        })
        .map_err(|e| e.to_string())?;
    let mut timings = timings(conn, id)?;
    let mut out = Vec::new();
    for row in rows {
        let (idx, role, content, at_ms, turn) = row.map_err(|e| e.to_string())?;
        let role = parse_role(&role).ok_or_else(|| format!("bad role '{role}' in turn {idx}"))?;
        let content: Vec<Part> = serde_json::from_str(&content).map_err(|e| format!("turn {idx}: {e}"))?;
        // A turn's measured phases ride on its first message.
        let (phases, numbers) = timings.remove(&turn).unwrap_or_default();
        out.push(StoredTurn { idx, role, content, at_ms, phases, numbers });
    }
    Ok(out)
}

/// The stored spans and totals of each turn of a session.
fn timings(conn: &Connection, id: &SessionId) -> StoreResult<std::collections::HashMap<u32, (Vec<PhaseSpan>, Option<TurnNumbers>)>> {
    let mut st = conn.prepare("SELECT turn, spans, numbers FROM turn_timings WHERE session = ?1").map_err(|e| e.to_string())?;
    let rows = st
        .query_map([id.as_str()], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (turn, spans, numbers) = row.map_err(|e| e.to_string())?;
        // A row an older or newer build wrote that does not parse is a turn without timings.
        let (Ok(spans), Ok(numbers)) = (serde_json::from_str(&spans), serde_json::from_str(&numbers)) else { continue };
        out.insert(turn, (spans, Some(numbers)));
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

    /// Appends whole messages with no per-call provenance, all of turn `next_turn - 1`.
    pub fn append(&self, session: &SessionId, messages: Vec<Message>, next_turn: u32, tainted: bool, at: i64) -> StoreResult<()> {
        let turn = next_turn.saturating_sub(1);
        let rows = messages.into_iter().map(|message| RowIn { message, turn, step: None, at, meta: None, mem_seq: None }).collect();
        self.append_rows(session, rows, next_turn, tainted, at)
    }

    /// Appends rows in one transaction and moves the session's `updated`, `next_turn` (never
    /// back) and taint latch.
    pub fn append_rows(&self, session: &SessionId, rows: Vec<RowIn>, next_turn: u32, tainted: bool, at: i64) -> StoreResult<()> {
        self.call(|reply| Cmd::AppendRows { session: session.clone(), rows, next_turn, tainted, at, reply })
    }

    /// Keeps a tool call's outcome the moment it finishes, before its batch is complete.
    pub fn put_call_result(&self, session: &SessionId, row: CallResultRow, at: i64) -> StoreResult<()> {
        self.call(|reply| Cmd::PutCallResult { session: session.clone(), row, at, reply })
    }

    /// Outcomes of calls that finished in a batch that never completed.
    pub fn call_results(&self, session: &SessionId) -> StoreResult<Vec<CallResultRow>> {
        self.call(|r| Cmd::CallResults(session.clone(), r))
    }

    /// Keeps the usage of a model call that left no row (it answered with nothing).
    pub fn put_spend(&self, session: &SessionId, turn: u32, step: Option<u32>, meta: CallMeta, at: i64) -> StoreResult<()> {
        self.call(|reply| Cmd::PutSpend { session: session.clone(), turn, step, at, meta, reply })
    }

    /// Model calls that cost something but are not in [`Store::rows`]: rewound rows and calls
    /// without one.
    pub fn hidden_spend(&self, session: &SessionId) -> StoreResult<Vec<CallMeta>> {
        self.call(|r| Cmd::HiddenSpend(session.clone(), r))
    }

    /// Every row of a session with its provenance.
    pub fn rows(&self, session: &SessionId) -> StoreResult<Vec<RowOut>> {
        self.call(|r| Cmd::Rows(session.clone(), r))
    }

    /// Replaces the content of the session's last row (the stored prompt turned out to differ
    /// from what the loop used).
    pub fn replace_last(&self, session: &SessionId, message: Message) -> StoreResult<()> {
        self.call(|reply| Cmd::ReplaceLast(session.clone(), message, reply))
    }

    /// `/rewind`: moves turns `turn..` out of the transcript (to `rewound_turns`) and sets the
    /// session's next turn back to `turn`.
    pub fn drop_from(&self, session: &SessionId, turn: u32, at: i64) -> StoreResult<Dropped> {
        self.call(|reply| Cmd::DropFrom { session: session.clone(), turn, at, reply })
    }

    /// `/fork`: registers `to` as a copy of `from` up to (not including) `before_turn`.
    pub fn fork(&self, from: &SessionId, to: SessionSummary, before_turn: Option<u32>) -> StoreResult<SessionSummary> {
        self.call(|reply| Cmd::Fork { from: from.clone(), to, before_turn, reply })
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

    /// Keeps `text` as what `turn` carried of `kind` ([`TAIL`], [`NOTE`]) on the message `offset` rows
    /// after the turn's first; the sections use turn 0.
    pub fn put_context(&self, session: &SessionId, turn: u32, kind: &str, text: &str, offset: u32) -> StoreResult<()> {
        self.call(|r| Cmd::PutContext { session: session.clone(), turn, kind: kind.into(), text: text.into(), offset, reply: r })
    }

    pub fn context(&self, id: &SessionId) -> StoreResult<SavedContext> {
        self.call(|r| Cmd::Context(id.clone(), r))
    }

    pub fn load(&self, id: &SessionId) -> StoreResult<Vec<StoredTurn>> {
        self.call(|r| Cmd::Load(id.clone(), r))
    }

    /// A daemon setting or marker (`presence.last_seen_ms`, the day's brief session).
    pub fn kv(&self, key: &str) -> StoreResult<Option<String>> {
        self.call(|r| Cmd::GetKv(key.to_string(), r))
    }

    pub fn set_kv(&self, key: &str, value: &str) -> StoreResult<()> {
        self.call(|r| Cmd::SetKv(key.to_string(), value.to_string(), r))
    }

    /// When any session last changed (a turn was saved), Unix epoch ms.
    pub fn last_activity(&self) -> StoreResult<Option<i64>> {
        self.call(Cmd::LastActivity)
    }

    /// Keeps a turn's measured phases and totals.
    pub fn put_timings(&self, session: &SessionId, turn: u32, spans: Vec<PhaseSpan>, numbers: TurnNumbers) -> StoreResult<()> {
        self.call(|reply| Cmd::PutTimings { session: session.clone(), turn, spans, numbers, reply })
    }

    /// Stops the store thread after pending commands.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("marlowe-daemon-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn summary(id: &SessionId, ws: &str) -> SessionSummary {
        SessionSummary { id: id.clone(), title: String::new(), workspace: Some(ws.into()), created_ms: 1, updated_ms: 1, turns: 0 }
    }

    #[test]
    fn sessions_and_turns_round_trip() {
        let dir = scratch("round");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("s1".into());
        store.create(summary(&id, "C:/w")).unwrap();
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
        assert_eq!(store.last_activity().unwrap(), Some(10));
        assert_eq!(store.kv("k").unwrap(), None);
        store.set_kv("k", "a").unwrap();
        store.set_kv("k", "b").unwrap();
        assert_eq!(store.kv("k").unwrap().as_deref(), Some("b"));
        store.close();
        handle.join().unwrap();
        assert!(store.list(1).is_err(), "a closed store refuses");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn row(m: Message, turn: u32, at: i64) -> RowIn {
        RowIn { message: m, turn, step: None, at, meta: None, mem_seq: None }
    }

    /// What a turn carried beyond its words comes back on the right message after a reload, follows
    /// a fork, and goes with a rewind; the sections outlive both.
    #[test]
    fn a_turns_tail_and_notes_come_back_where_they_rode() {
        let dir = scratch("context");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("c1".into());
        store.create(summary(&id, "C:/w")).unwrap();
        // Turn 0: the prompt. Turn 1: a reminder ahead of the prompt (offset 1), then the prompt.
        store.append_rows(&id, vec![row(Message::user("one"), 0, 1), row(Message::assistant("a"), 0, 2)], 1, false, 2).unwrap();
        store.append_rows(&id, vec![row(Message::user("<system-reminder>files</system-reminder>"), 1, 3), row(Message::user("two"), 1, 4)], 2, false, 4).unwrap();
        store.put_context(&id, 0, TAIL, "tail0", 0).unwrap();
        store.put_context(&id, 1, TAIL, "tail1", 1).unwrap();
        store.put_context(&id, 1, NOTE, "note1", 1).unwrap();
        store.put_context(&id, 0, SECTIONS, "{\"s\":1}", 0).unwrap();
        let saved = store.context(&id).unwrap();
        assert_eq!(
            saved.turns,
            vec![(0, TAIL.to_string(), "tail0".to_string()), (3, TAIL.to_string(), "tail1".to_string()), (3, NOTE.to_string(), "note1".to_string())]
        );
        assert_eq!(saved.sections.as_deref(), Some("{\"s\":1}"));
        store.put_context(&id, 1, TAIL, "tail1b", 1).unwrap();
        assert_eq!(store.context(&id).unwrap().turns[1].2, "tail1b", "a turn's tail is replaced, not repeated");

        let copy = store.fork(&id, summary(&SessionId("c2".into()), "C:/w"), Some(1)).unwrap();
        let forked = store.context(&copy.id).unwrap();
        assert_eq!(forked.turns, vec![(0, TAIL.to_string(), "tail0".to_string())], "a fork keeps the tails of the turns it keeps");
        assert!(forked.sections.is_some());

        store.drop_from(&id, 1, 9).unwrap();
        let after = store.context(&id).unwrap();
        assert_eq!(after.turns.len(), 1, "a rewind drops the dropped turns' tails");
        assert!(after.sections.is_some());
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_carry_their_turn_and_provenance_and_a_rewind_moves_them_aside() {
        let dir = scratch("rows");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("s".into());
        store.create(summary(&id, "C:/w")).unwrap();
        let meta = CallMeta {
            provider: "ollama".into(),
            model: "dusk".into(),
            usage: Usage { input: 100, output: 20, ..Usage::default() },
            latency_ms: 42,
        };
        store
            .append_rows(
                &id,
                vec![
                    RowIn { mem_seq: Some(7), ..row(Message::user("one"), 0, 10) },
                    RowIn { step: Some(1), meta: Some(meta.clone()), ..row(Message::assistant("a1"), 0, 11) },
                    RowIn { mem_seq: Some(9), ..row(Message::user("two"), 1, 20) },
                    RowIn { step: Some(1), meta: Some(meta.clone()), ..row(Message::assistant("a2"), 1, 21) },
                ],
                2,
                false,
                21,
            )
            .unwrap();
        let rows = store.rows(&id).unwrap();
        assert_eq!(rows.iter().map(|r| (r.turn, r.mem_seq)).collect::<Vec<_>>(), vec![(0, Some(7)), (0, None), (1, Some(9)), (1, None)]);
        assert_eq!(rows[1].meta.as_ref().unwrap(), &meta);
        assert_eq!(rows[1].step, Some(1));

        let dropped = store.drop_from(&id, 1, 30).unwrap();
        assert_eq!(dropped, Dropped { removed: 2, mem_seq: Some(9), kept_until_ms: Some(11), first_ms: Some(20) });
        assert_eq!(store.load(&id).unwrap().len(), 2);
        assert_eq!(store.get(&id).unwrap().unwrap().turns, 1, "the next turn is the one that was dropped");
        // New rows continue the numbering with no gap.
        store.append_rows(&id, vec![row(Message::user("two again"), 1, 40)], 2, false, 40).unwrap();
        assert_eq!(store.load(&id).unwrap().iter().map(|t| t.idx).collect::<Vec<_>>(), vec![0, 1, 2]);
        // The spend of the dropped call is still on record, and so is a call that left no row.
        assert_eq!(store.hidden_spend(&id).unwrap(), vec![meta.clone()]);
        let rowless = CallMeta { latency_ms: 7, ..meta.clone() };
        store.put_spend(&id, 1, Some(2), rowless.clone(), 45).unwrap();
        assert_eq!(store.hidden_spend(&id).unwrap(), vec![meta.clone(), rowless]);
        assert!(store.hidden_spend(&SessionId("other".into())).unwrap().is_empty());
        // Dropping past the end removes nothing.
        assert_eq!(store.drop_from(&id, 9, 50).unwrap(), Dropped::default());
        assert_eq!(store.get(&id).unwrap().unwrap().turns, 2);
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turn_timings_ride_on_the_turns_first_message_and_follow_rewinds_and_forks() {
        use marlowe_proto::PhaseKind;
        let dir = scratch("timings");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("t".into());
        store.create(summary(&id, "C:/w")).unwrap();
        let rows = (0..2)
            .flat_map(|t| [row(Message::user(format!("q{t}")), t, 10 + t as i64), row(Message::assistant(format!("a{t}")), t, 11 + t as i64)])
            .collect();
        store.append_rows(&id, rows, 2, false, 20).unwrap();
        let span = |ms| PhaseSpan { kind: PhaseKind::Wire, stage: None, start_ms: 3, ms, n: None, detail: None, parent: None, est: false };
        let numbers = |ttft| TurnNumbers { ttft_ms: Some(ttft), ..TurnNumbers::default() };
        store.put_timings(&id, 0, vec![span(40)], numbers(50)).unwrap();
        store.put_timings(&id, 1, vec![span(80)], numbers(90)).unwrap();
        // A second write for the same turn replaces the first.
        store.put_timings(&id, 1, vec![span(81)], numbers(91)).unwrap();

        let turns = store.load(&id).unwrap();
        assert_eq!(turns[0].phases, vec![span(40)]);
        assert_eq!(turns[0].numbers, Some(numbers(50)));
        assert!(turns[1].phases.is_empty() && turns[1].numbers.is_none(), "only the first message of a turn");
        assert_eq!(turns[2].phases, vec![span(81)]);

        let copy = store.fork(&id, summary(&SessionId("c".into()), "C:/w"), Some(1)).unwrap();
        let copied = store.load(&copy.id).unwrap();
        assert_eq!(copied.len(), 2);
        assert_eq!(copied[0].phases, vec![span(40)], "a fork keeps the timings of the turns it copies");

        store.drop_from(&id, 1, 30).unwrap();
        let kept = store.load(&id).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].phases, vec![span(40)]);
        store.append_rows(&id, vec![row(Message::user("again"), 1, 40)], 2, false, 40).unwrap();
        assert!(store.load(&id).unwrap()[2].phases.is_empty(), "a rewound turn's timings are gone");
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fork_copies_the_turns_before_the_cut_and_is_independent() {
        let dir = scratch("fork");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("orig".into());
        store.create(summary(&id, "C:/w")).unwrap();
        let rows = (0..3)
            .flat_map(|t| [row(Message::user(format!("q{t}")), t, 10 + t as i64), row(Message::assistant(format!("a{t}")), t, 11 + t as i64)])
            .collect();
        store.append_rows(&id, rows, 3, true, 20).unwrap();
        let copy = store.fork(&id, summary(&SessionId("copy".into()), "C:/w"), Some(2)).unwrap();
        assert_eq!(copy.turns, 2);
        let copied = store.load(&copy.id).unwrap();
        assert_eq!(copied.len(), 4);
        assert!(store.tainted(&copy.id).unwrap(), "taint travels with the history");
        store.append_rows(&copy.id, vec![row(Message::user("only in the copy"), 2, 99)], 3, false, 99).unwrap();
        assert_eq!(store.load(&id).unwrap().len(), 6, "the original is untouched");
        let whole = store.fork(&id, summary(&SessionId("whole".into()), "C:/w"), None).unwrap();
        assert_eq!((whole.turns, store.load(&whole.id).unwrap().len()), (3, 6));
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn call_results_wait_for_their_batch_and_settle_with_it() {
        let dir = scratch("calls");
        let (store, handle) = open(&dir.join("cowork.db")).unwrap();
        let id = SessionId("s".into());
        store.create(summary(&id, "C:/w")).unwrap();
        let done = |call: &str| CallResultRow { call_id: call.into(), content: format!("{call} output"), is_error: false };
        store.put_call_result(&id, done("a"), 1).unwrap();
        store.put_call_result(&id, done("b"), 2).unwrap();
        assert_eq!(store.call_results(&id).unwrap().len(), 2);
        let batch = Message::tool_results([marlowe_model::ToolResult::text("a", "a output", false)]);
        store.append_rows(&id, vec![row(batch, 0, 3)], 1, false, 3).unwrap();
        assert_eq!(store.call_results(&id).unwrap(), vec![done("b")], "only the call in the stored batch settled");
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_database_gains_the_columns_and_its_turns_are_numbered() {
        let dir = scratch("migrate");
        let db = dir.join("cowork.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace TEXT, created INTEGER NOT NULL, updated INTEGER NOT NULL, next_turn INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE turns (session TEXT NOT NULL, idx INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (session, idx));
                 INSERT INTO sessions VALUES ('old', 't', NULL, 1, 1, 2);
                 INSERT INTO turns VALUES ('old', 0, 'user', '[{\"type\":\"text\",\"text\":\"q0\"}]', 1);
                 INSERT INTO turns VALUES ('old', 1, 'assistant', '[{\"type\":\"text\",\"text\":\"a0\"}]', 2);
                 INSERT INTO turns VALUES ('old', 2, 'user', '[{\"type\":\"text\",\"text\":\"q1\"}]', 3);
                 INSERT INTO turns VALUES ('old', 3, 'assistant', '[{\"type\":\"text\",\"text\":\"a1\"}]', 4);",
            )
            .unwrap();
        }
        let (store, handle) = open(&db).unwrap();
        let id = SessionId("old".into());
        let rows = store.rows(&id).unwrap();
        assert_eq!(rows.iter().map(|r| r.turn).collect::<Vec<_>>(), vec![0, 0, 1, 1]);
        assert!(rows.iter().all(|r| r.meta.is_none() && r.mem_seq.is_none()));
        assert!(!store.tainted(&id).unwrap());
        store.close();
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
