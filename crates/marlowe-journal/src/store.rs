//! SQLite backing for the journal (ADR-003: one append-only journal).
//!
//! `STRICT` tables, because a column that silently accepts the wrong type is another way for
//! two sides to disagree without anything observing it.

use std::path::Path;

use rusqlite::Connection;

use crate::error::JournalError;

pub(crate) fn create_schema(db_path: &Path) -> Result<(), JournalError> {
    let conn = Connection::open(db_path)?;
    configure(&conn)?;
    conn.execute_batch(
        r#"
        CREATE TABLE journal (
            seq            INTEGER PRIMARY KEY NOT NULL,
            ts             INTEGER NOT NULL,
            trace_id       TEXT    NOT NULL,
            session_id     TEXT,
            run_id         TEXT,
            actor          TEXT    NOT NULL,
            kind           TEXT    NOT NULL,
            -- The exact bytes that were signed. Stored as written, never re-encoded: a
            -- second encoder whose output differs from the first is a signature failure
            -- that looks like corruption.
            payload        TEXT    NOT NULL,
            prev_signature TEXT    NOT NULL,
            signature      TEXT    NOT NULL
        ) STRICT;

        CREATE INDEX journal_kind ON journal(kind);
        CREATE INDEX journal_session ON journal(session_id);
        "#,
    )?;
    Ok(())
}

pub(crate) fn open(db_path: &Path) -> Result<Connection, JournalError> {
    let conn = Connection::open(db_path)?;
    configure(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<(), JournalError> {
    // WAL for durable concurrent append (ADR-003). `synchronous = FULL` because the whole
    // claim of the journal is durability; group commit -- the mechanism that makes this
    // affordable on VPS storage -- is a later M0b session, and until it lands the honest
    // setting is the safe one rather than the fast one.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}
