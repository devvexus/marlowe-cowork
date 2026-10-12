//! `/loop`, cron and one-shot schedules (tracks F, design X19, and N, BG5-BG6): ListSchedules,
//! AddSchedule, RemoveSchedule, CronCreate.
//!
//! A `schedules` table in `cowork.db` and one ticking thread. Three kinds of row:
//!
//! - a `/loop` (`every_secs`): a due row runs its text as an ordinary ask in its session (a
//!   schedule without a session gets one on its first run), so it never blocks anything: a session
//!   still busy when its schedule comes due skips that run. A schedule that came due while the
//!   daemon was down runs once at the next start, not once per missed interval.
//! - a cron (`cron`, 5 fields, local time) and a one-shot (`once_at_ms`), made by the
//!   `cron_create` and `schedule_wakeup` tools: a due row injects its prompt into its session as
//!   news (never as the user's words), which the session takes at its next step boundary or, idle,
//!   in a relay turn. One missed while the machine slept fires once at wake and says how many
//!   fires it missed.
//! - `durable` rows live in the table and survive a restart; the rest are held in memory and die
//!   with the daemon, which is when their session ends.
//!
//! Every cron and one-shot row is also a `cron_*` task in the registry (`task_list`, `task_stop`).
//! [`tick_at`] fires what is due at a given time, so tests drive a fake clock.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use marlowe_agent::background::CronRequest;
use marlowe_agent::cron::{self, CronExpr};
use marlowe_agent::{Injection, InjectionSource, NewTask, TaskControl, TaskKind, TaskStatus};
use marlowe_proto::{paths, Event, Request, ScheduleInfo, SessionId};
use rusqlite::{params, Connection};

use crate::daemon::Core;
use crate::{lock, now_ms};

/// The shortest interval accepted.
pub(crate) const MIN_EVERY_SECS: u64 = 1;
/// How often the thread looks for due schedules.
const TICK: Duration = Duration::from_millis(200);
/// A fire later than this was missed (the machine slept, or the daemon was down).
const LATE_MS: i64 = 120_000;
/// Fires counted as missed, at most.
const MISSED_CAP: u32 = 99;
/// Schedules one session may hold.
pub(crate) const MAX_PER_SESSION: usize = 50;

/// One schedule and what is only known in memory.
#[derive(Clone)]
struct Row {
    info: ScheduleInfo,
    /// The session that made it had read untrusted content: what it fires is untrusted too.
    tainted: bool,
    /// A `schedule_wakeup`'s reason.
    reason: Option<String>,
    /// A wake-up, not a cron.
    wakeup: bool,
}

#[derive(Default)]
struct Schedules {
    db: Mutex<Option<Connection>>,
    rows: Mutex<Vec<Row>>,
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
    // Wave 2 columns, added to tables an earlier version created.
    let have: Vec<String> = {
        let mut st = conn.prepare("PRAGMA table_info(schedules)").map_err(|e| e.to_string())?;
        let names = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
        names.filter_map(Result::ok).collect()
    };
    for (col, def) in [
        ("cron", "TEXT"),
        ("once_at_ms", "INTEGER"),
        ("missed", "INTEGER NOT NULL DEFAULT 0"),
        ("tainted", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !have.iter().any(|h| h == col) {
            conn.execute_batch(&format!("ALTER TABLE schedules ADD COLUMN {col} {def};")).map_err(|e| e.to_string())?;
        }
    }
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

fn load(core: &Core) -> Result<Vec<Row>, String> {
    with_db(core, |c| {
        let mut st = c.prepare(
            "SELECT id, session, every_secs, text, next_at_ms, cron, once_at_ms, missed, tainted FROM schedules ORDER BY created_ms",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Row {
                info: ScheduleInfo {
                    id: r.get(0)?,
                    session: r.get::<_, Option<String>>(1)?.map(SessionId),
                    every_secs: r.get::<_, i64>(2)? as u64,
                    text: r.get(3)?,
                    next_at_ms: Some(r.get(4)?),
                    cron: r.get(5)?,
                    once_at_ms: r.get(6)?,
                    // Everything in the table survives a restart.
                    durable: true,
                    missed: r.get::<_, i64>(7)? as u32,
                },
                tainted: r.get::<_, i64>(8)? != 0,
                reason: None,
                wakeup: false,
            })
        })?;
        let all: rusqlite::Result<Vec<Row>> = rows.collect();
        all
    })
}

fn save(core: &Core, row: &Row) -> Result<(), String> {
    let s = &row.info;
    if !s.durable {
        return Ok(());
    }
    with_db(core, |c| {
        c.execute(
            "INSERT INTO schedules (id, session, every_secs, text, next_at_ms, created_ms, cron, once_at_ms, missed, tainted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET session = ?2, every_secs = ?3, text = ?4, next_at_ms = ?5, cron = ?7,
                                           once_at_ms = ?8, missed = ?9, tainted = ?10",
            params![
                s.id,
                s.session.as_ref().map(|x| x.0.clone()),
                s.every_secs as i64,
                s.text,
                s.next_at_ms.unwrap_or(0),
                now_ms(),
                s.cron,
                s.once_at_ms,
                s.missed as i64,
                row.tainted as i64
            ],
        )
        .map(|_| ())
    })
}

fn new_id(core: &Core, prefix: &str) -> String {
    let n = schedules(core).next_id.fetch_add(1, Ordering::SeqCst) % 256;
    format!("{prefix}{:x}{:02x}", now_ms(), n)
}

/// Adds a `/loop` schedule (from the `/loop` command or `AddSchedule`) and starts the ticker if
/// needed. Its first timed run is one interval from now (`/loop` runs the prompt at once itself).
pub(crate) fn add(core: &Arc<Core>, session: Option<SessionId>, every_secs: u64, text: &str) -> Result<ScheduleInfo, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a schedule needs a prompt or command".into());
    }
    if every_secs < MIN_EVERY_SECS {
        return Err(format!("the interval must be at least {MIN_EVERY_SECS} s"));
    }
    let info = ScheduleInfo {
        id: new_id(core, "sch-"),
        session,
        every_secs,
        text: text.to_string(),
        next_at_ms: Some(now_ms() + (every_secs * 1_000) as i64),
        durable: true,
        ..Default::default()
    };
    let row = Row { info: info.clone(), tainted: false, reason: None, wakeup: false };
    save(core, &row)?;
    lock(&schedules(core).rows).push(row);
    register_task(core, &info);
    start(core);
    Ok(info)
}

/// A cron or one-shot schedule for `session` (`cron_create`, `schedule_wakeup`). `now` is the
/// time the schedule is made at.
pub(crate) fn create(core: &Arc<Core>, session: &SessionId, req: CronRequest, tainted: bool, now: i64) -> Result<ScheduleInfo, String> {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err("a schedule needs a prompt".into());
    }
    let s = schedules(core);
    let mine = lock(&s.rows).iter().filter(|r| r.info.session.as_ref() == Some(session)).count();
    if mine >= MAX_PER_SESSION {
        return Err(format!("this session already has {MAX_PER_SESSION} schedules; delete some first"));
    }
    let wakeup = req.wakeup.is_some();
    let (cron, once, next) = match (&req.cron, req.once_at_ms) {
        (Some(c), None) => {
            let expr = CronExpr::parse(c)?;
            let next = cron::next_fire_ms(&expr, now).ok_or_else(|| format!("`{c}` never fires"))?;
            (Some(expr.as_str().to_string()), None, next)
        }
        (None, Some(at)) => {
            if at <= now {
                return Err("that time has passed".into());
            }
            (None, Some(at), at)
        }
        _ => return Err("give exactly one of a cron expression and a time".into()),
    };
    let info = ScheduleInfo {
        id: new_id(core, if wakeup { "cron_w" } else { "cron_" }),
        session: Some(session.clone()),
        every_secs: 0,
        text: prompt.to_string(),
        next_at_ms: Some(next),
        cron,
        once_at_ms: once,
        durable: req.durable && !wakeup,
        missed: 0,
    };
    let row = Row { info: info.clone(), tainted, reason: req.wakeup.filter(|r| !r.is_empty()), wakeup };
    save(core, &row)?;
    lock(&s.rows).push(row);
    register_task(core, &info);
    start(core);
    Ok(info)
}

/// The row `id` names: the exact id, the id without or with the `cron_` task prefix.
fn find_row(rows: &[Row], id: &str) -> Option<usize> {
    let id = id.trim();
    let candidates = [id.to_string(), id.strip_prefix("cron_").unwrap_or(id).to_string(), format!("cron_{id}")];
    candidates.iter().find_map(|c| rows.iter().position(|r| &r.info.id == c))
}

/// Deletes a schedule `session` may delete: its own, or a durable one.
pub(crate) fn delete_for(core: &Arc<Core>, session: &SessionId, id: &str) -> Result<(), String> {
    let row = {
        let s = schedules(core);
        let rows = lock(&s.rows);
        let i = find_row(&rows, id).ok_or_else(|| format!("no schedule `{id}`; cron_list shows them"))?;
        rows[i].clone()
    };
    if row.info.session.as_ref() != Some(session) && !row.info.durable {
        return Err(format!("no schedule `{id}`; cron_list shows them"));
    }
    remove(core, &row.info.id)?;
    Ok(())
}

/// The schedules `session` can see: its own and the durable ones.
pub(crate) fn list_for(core: &Core, session: &SessionId) -> Vec<ScheduleInfo> {
    lock(&schedules(core).rows)
        .iter()
        .filter(|r| r.info.session.as_ref() == Some(session) || (r.info.durable && r.info.session.is_none()))
        .map(|r| r.info.clone())
        .collect()
}

fn remove(core: &Core, id: &str) -> Result<bool, String> {
    let s = schedules(core);
    let removed = {
        let mut rows = lock(&s.rows);
        let before = rows.len();
        rows.retain(|r| r.info.id != id);
        rows.len() < before
    };
    with_db(core, |c| c.execute("DELETE FROM schedules WHERE id = ?1", params![id]))?;
    if removed {
        let tid = task_id(id);
        crate::tasks::handle(core).set_status(&tid, TaskStatus::Stopped, Some("deleted".into()));
    }
    Ok(removed)
}

fn list(core: &Core) -> Vec<ScheduleInfo> {
    lock(&schedules(core).rows).iter().map(|r| r.info.clone()).collect()
}

/// `cron_<raw>`: the schedule's task id. A cron's own id already starts with `cron_`.
fn task_id(row_id: &str) -> String {
    TaskKind::Cron.id(row_id.strip_prefix("cron_").unwrap_or(row_id))
}

struct CronControl {
    core: Weak<Core>,
    row: String,
}

impl TaskControl for CronControl {
    fn stop(&self) -> Result<(), String> {
        let core = self.core.upgrade().ok_or("the daemon is stopping")?;
        remove(&core, &self.row).map(|_| ())
    }
}

fn describe(info: &ScheduleInfo) -> String {
    match (&info.cron, info.once_at_ms, info.every_secs) {
        (Some(c), _, _) => format!("cron `{c}`"),
        (None, Some(_), _) => "once".into(),
        (None, None, n) => format!("every {}", marlowe_agent::commands::describe_interval(n)),
    }
}

fn status_detail(info: &ScheduleInfo) -> String {
    let next = info.next_at_ms.map(cron::describe_ms).unwrap_or_else(|| "-".into());
    let scope = if info.durable { "durable" } else { "this session" };
    format!("{} · next {next} · {scope}", describe(info))
}

/// Makes the schedule a `cron_*` task, waiting for its next fire.
fn register_task(core: &Arc<Core>, info: &ScheduleInfo) {
    let reg = crate::tasks::handle(core);
    let raw = info.id.strip_prefix("cron_").unwrap_or(&info.id).to_string();
    let title: String = info.text.lines().next().unwrap_or("").chars().take(80).collect();
    let id = reg.register(
        NewTask::new(TaskKind::Cron, title)
            .raw_id(raw)
            .session(info.session.clone())
            .control(Arc::new(CronControl { core: Arc::downgrade(core), row: info.id.clone() })),
    );
    reg.set_status(&id, TaskStatus::Waiting, Some(status_detail(info)));
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
        Request::CronCreate { session, cron, once_at_ms, text, durable } => {
            let Some(session) = session else {
                return core.bus.reply(client, Event::Error { session: None, message: "cron_create needs a session".into() });
            };
            let req = CronRequest { cron, once_at_ms, prompt: text, durable, wakeup: None, tainted: false };
            match create(core, &session, req.clone(), req.tainted, now_ms()) {
                Ok(_) => reply(Event::Schedules { list: list(core) }),
                Err(e) => core.bus.reply(client, Event::Error { session: Some(session), message: e }),
            }
        }
        other => crate::client::not_yet(core, client, &other, "F"),
    }
}

/// Re-arms stored schedules at daemon start (`boot.rs`), and marks the background agents the
/// last daemon was running as lost (`conductor_ops::on_boot`). Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    crate::conductor_ops::on_boot(core);
    match load(core) {
        Ok(rows) if !rows.is_empty() => {
            for r in &rows {
                register_task(core, &r.info);
            }
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
    let c = Arc::downgrade(core);
    match std::thread::Builder::new().name("marlowe-schedules".into()).spawn(move || tick_loop(c)) {
        Ok(h) => core.add_writer(h),
        Err(e) => {
            schedules(core).started.store(false, Ordering::SeqCst);
            eprintln!("marlowe: the scheduler did not start: {e}");
        }
    }
}

fn tick_loop(core: Weak<Core>) {
    loop {
        std::thread::sleep(TICK);
        let Some(core) = core.upgrade() else { return };
        if core.is_shutting_down() {
            return;
        }
        tick_at(&core, now_ms());
    }
}

/// Fires every schedule due at `now`.
pub(crate) fn tick_at(core: &Arc<Core>, now: i64) {
    let due: Vec<Row> =
        lock(&schedules(core).rows).iter().filter(|r| r.info.next_at_ms.is_some_and(|t| t <= now)).cloned().collect();
    for mut row in due {
        if core.is_shutting_down() {
            return;
        }
        let due_at = row.info.next_at_ms.unwrap_or(now);
        let one_shot = row.info.cron.is_none() && row.info.once_at_ms.is_some();
        if row.info.cron.is_some() || one_shot {
            let missed = fire_injected(core, &row, due_at, now);
            row.info.missed = missed;
            if one_shot {
                finish_one_shot(core, &row);
                continue;
            }
            // Next fire strictly after now: never a burst of catch-up runs.
            row.info.next_at_ms = row
                .info
                .cron
                .as_deref()
                .and_then(|c| CronExpr::parse(c).ok())
                .and_then(|e| cron::next_fire_ms(&e, now));
        } else {
            row.info.session = fire_ask(core, &row.info);
            // Never a burst of catch-up runs: the next run is one interval from now.
            row.info.next_at_ms = Some(now + (row.info.every_secs * 1_000) as i64);
        }
        {
            let s = schedules(core);
            let mut rows = lock(&s.rows);
            match rows.iter_mut().find(|r| r.info.id == row.info.id) {
                Some(r) => *r = row.clone(),
                None => continue, // removed meanwhile
            }
        }
        crate::tasks::handle(core).set_status(&task_id(&row.info.id), TaskStatus::Waiting, Some(status_detail(&row.info)));
        if let Err(e) = save(core, &row) {
            core.bus.publish(Event::Error { session: row.info.session.clone(), message: format!("schedule {} not saved: {e}", row.info.id) });
        }
    }
}

/// A one-shot fired: it is done and gone.
fn finish_one_shot(core: &Arc<Core>, row: &Row) {
    let s = schedules(core);
    lock(&s.rows).retain(|r| r.info.id != row.info.id);
    let _ = with_db(core, |c| c.execute("DELETE FROM schedules WHERE id = ?1", params![row.info.id]));
    crate::tasks::handle(core).set_status(&task_id(&row.info.id), TaskStatus::Done, Some("fired".into()));
}

/// Injects the prompt of a cron or one-shot row into its session. Returns the fires missed.
fn fire_injected(core: &Arc<Core>, row: &Row, due_at: i64, now: i64) -> u32 {
    let Some(session) = row.info.session.clone() else { return 0 };
    let late = now - due_at > LATE_MS;
    let missed = match (&row.info.cron, late) {
        (Some(c), true) => CronExpr::parse(c)
            .map(|e| cron::count_fires(&e, due_at - 1, now, MISSED_CAP).saturating_sub(1))
            .unwrap_or(0),
        _ => 0,
    };
    let task = task_id(&row.info.id);
    let head = if row.wakeup {
        match &row.reason {
            Some(r) => format!("[wake-up {task}: you asked to be woken — {r}]"),
            None => format!("[wake-up {task}: you asked to be woken]"),
        }
    } else if let Some(c) = &row.info.cron {
        format!("[schedule {task} fired · cron `{c}`]")
    } else {
        format!("[reminder {task} fired]")
    };
    let mut text = format!("{head}\n{}", row.info.text);
    if late {
        let when = cron::describe_ms(due_at);
        if missed > 0 {
            text.push_str(&format!("\n[harness: this was due {when} and Marlowe was not running; {missed} earlier fire{} missed in between.]", if missed == 1 { " was" } else { "s were" }));
        } else {
            text.push_str(&format!("\n[harness: this was due {when} and Marlowe was not running, so it fired late.]"));
        }
    }
    let source = if row.wakeup { InjectionSource::Wakeup } else { InjectionSource::Cron };
    crate::tasks::handle(core).inject(Injection::new(session, source, text).task(task).untrusted(row.tainted));
    missed
}

/// Runs a `/loop` row as an ask; returns the session it ran in. A busy session skips this run.
fn fire_ask(core: &Arc<Core>, row: &ScheduleInfo) -> Option<SessionId> {
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
