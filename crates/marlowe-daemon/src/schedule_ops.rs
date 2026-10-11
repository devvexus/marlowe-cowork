//! `/loop` and scheduled tasks (track F, design X19): ListSchedules, AddSchedule, RemoveSchedule.
//!
//! A `schedules` table in `cowork.db` and one ticking thread. A due schedule runs its text as an
//! ordinary ask in its session (a schedule without a session gets one on its first run), so it
//! never blocks anything: a session still busy when its schedule comes due skips that run. A
//! schedule that came due while the daemon was down runs once at the next start, not once per
//! missed interval.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_proto::{paths, Event, Request, ScheduleInfo, SessionId};
use rusqlite::{params, Connection};

use crate::daemon::Core;
use crate::{lock, now_ms};

/// The shortest interval accepted.
pub(crate) const MIN_EVERY_SECS: u64 = 1;
/// How often the thread looks for due schedules.
const TICK: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Schedules {
    db: Mutex<Option<Connection>>,
    rows: Mutex<Vec<ScheduleInfo>>,
    started: AtomicBool,
    next_id: AtomicU64,
}

fn schedules(core: &Core) -> Arc<Schedules> {
    core.ext.get_or_init(Schedules::default)
}

fn open(core: &Core) -> Result<Connection, String> {
    let conn = Connection::open(paths::cowork_db(&core.home)).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schedules (
            id TEXT PRIMARY KEY,
            session TEXT,
            every_secs INTEGER NOT NULL,
            text TEXT NOT NULL,
            next_at_ms INTEGER NOT NULL,
            created_ms INTEGER NOT NULL
        );",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Runs `f` on the table's connection, opening it on first use.
fn with_db<T>(core: &Core, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
    let s = schedules(core);
    let mut db = lock(&s.db);
    if db.is_none() {
        *db = Some(open(core)?);
    }
    f(db.as_ref().expect("opened")).map_err(|e| e.to_string())
}

fn load(core: &Core) -> Result<Vec<ScheduleInfo>, String> {
    with_db(core, |c| {
        let mut st = c.prepare("SELECT id, session, every_secs, text, next_at_ms FROM schedules ORDER BY created_ms")?;
        let rows = st.query_map([], |r| {
            Ok(ScheduleInfo {
                id: r.get(0)?,
                session: r.get::<_, Option<String>>(1)?.map(SessionId),
                every_secs: r.get::<_, i64>(2)? as u64,
                text: r.get(3)?,
                next_at_ms: Some(r.get(4)?),
                ..Default::default()
            })
        })?;
        let all: rusqlite::Result<Vec<ScheduleInfo>> = rows.collect();
        all
    })
}

fn save(core: &Core, s: &ScheduleInfo) -> Result<(), String> {
    with_db(core, |c| {
        c.execute(
            "INSERT INTO schedules (id, session, every_secs, text, next_at_ms, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET session = ?2, every_secs = ?3, text = ?4, next_at_ms = ?5",
            params![s.id, s.session.as_ref().map(|x| x.0.clone()), s.every_secs as i64, s.text, s.next_at_ms.unwrap_or(0), now_ms()],
        )
        .map(|_| ())
    })
}

/// Adds a schedule (from `/loop` or `AddSchedule`) and starts the ticker if needed. Its first
/// timed run is one interval from now (`/loop` runs the prompt at once itself).
pub(crate) fn add(core: &Arc<Core>, session: Option<SessionId>, every_secs: u64, text: &str) -> Result<ScheduleInfo, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a schedule needs a prompt or command".into());
    }
    if every_secs < MIN_EVERY_SECS {
        return Err(format!("the interval must be at least {MIN_EVERY_SECS} s"));
    }
    let s = schedules(core);
    let id = format!("sch-{:x}{:02x}", now_ms(), s.next_id.fetch_add(1, Ordering::SeqCst) % 256);
    let info = ScheduleInfo {
        id,
        session,
        every_secs,
        text: text.to_string(),
        next_at_ms: Some(now_ms() + (every_secs * 1_000) as i64),
        ..Default::default()
    };
    save(core, &info)?;
    lock(&s.rows).push(info.clone());
    start(core);
    Ok(info)
}

fn remove(core: &Core, id: &str) -> Result<bool, String> {
    let s = schedules(core);
    let had = {
        let mut rows = lock(&s.rows);
        let before = rows.len();
        rows.retain(|r| r.id != id);
        rows.len() < before
    };
    with_db(core, |c| c.execute("DELETE FROM schedules WHERE id = ?1", params![id]))?;
    Ok(had)
}

fn list(core: &Core) -> Vec<ScheduleInfo> {
    lock(&schedules(core).rows).clone()
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let refuse = |message: String| reply(Event::Error { session: None, message });
    match req {
        Request::ListSchedules => reply(Event::Schedules { list: list(core) }),
        Request::AddSchedule { session, every_secs, text } => match add(core, session, every_secs, &text) {
            Ok(_) => reply(Event::Schedules { list: list(core) }),
            Err(e) => refuse(e),
        },
        Request::RemoveSchedule { id } => match remove(core, &id) {
            Ok(true) => reply(Event::Schedules { list: list(core) }),
            Ok(false) => refuse(format!("no schedule {id}")),
            Err(e) => refuse(e),
        },
        other => crate::client::not_yet(core, client, &other, "F"),
    }
}

/// Re-arms stored schedules at daemon start (`boot.rs`), and marks the background agents the
/// last daemon was running as lost (`conductor_ops::on_boot`). Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    crate::conductor_ops::on_boot(core);
    match load(core) {
        Ok(rows) if !rows.is_empty() => {
            *lock(&schedules(core).rows) = rows;
            start(core);
        }
        Ok(_) => {}
        Err(e) => eprintln!("marlowe: schedules not loaded: {e}"),
    }
}

/// Starts the ticking thread once.
fn start(core: &Arc<Core>) {
    if schedules(core).started.swap(true, Ordering::SeqCst) {
        return;
    }
    let c = core.clone();
    match std::thread::Builder::new().name("marlowe-schedules".into()).spawn(move || tick_loop(&c)) {
        Ok(h) => core.add_writer(h),
        Err(e) => {
            schedules(core).started.store(false, Ordering::SeqCst);
            eprintln!("marlowe: the scheduler did not start: {e}");
        }
    }
}

fn tick_loop(core: &Arc<Core>) {
    while !core.is_shutting_down() {
        std::thread::sleep(TICK);
        let now = now_ms();
        let due: Vec<ScheduleInfo> =
            lock(&schedules(core).rows).iter().filter(|r| r.next_at_ms.is_some_and(|t| t <= now)).cloned().collect();
        for mut row in due {
            if core.is_shutting_down() {
                return;
            }
            row.session = fire(core, &row);
            // Never a burst of catch-up runs: the next run is one interval from now.
            row.next_at_ms = Some(now_ms() + (row.every_secs * 1_000) as i64);
            {
                let s = schedules(core);
                let mut rows = lock(&s.rows);
                match rows.iter_mut().find(|r| r.id == row.id) {
                    Some(r) => *r = row.clone(),
                    None => continue, // removed meanwhile
                }
            }
            if let Err(e) = save(core, &row) {
                core.bus.publish(Event::Error { session: row.session.clone(), message: format!("schedule {} not saved: {e}", row.id) });
            }
        }
    }
}

/// Runs one schedule as an ask; returns the session it ran in. A busy session skips this run.
fn fire(core: &Arc<Core>, row: &ScheduleInfo) -> Option<SessionId> {
    let session = match &row.session {
        Some(id) => match core.session(id) {
            Ok(h) if h.is_busy() => return Some(id.clone()),
            Ok(_) => id.clone(),
            Err((s, m)) => {
                core.bus.publish(Event::Error { session: s, message: format!("schedule {}: {m}", row.id) });
                return row.session.clone();
            }
        },
        None => match core.create_session(0, None) {
            Ok(h) => h.id.clone(),
            Err((s, m)) => {
                core.bus.publish(Event::Error { session: s, message: format!("schedule {}: {m}", row.id) });
                return None;
            }
        },
    };
    if let Err((s, m)) = core.ask(0, Some(session.clone()), row.text.clone(), None) {
        core.bus.publish(Event::Error { session: s, message: format!("schedule {}: {m}", row.id) });
    }
    Some(session)
}
