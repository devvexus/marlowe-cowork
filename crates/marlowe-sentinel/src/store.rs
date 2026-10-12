//! Sentinel state in `cowork.db` (WAL): `watch_roots`, `watch_files` (per-file offsets and size),
//! `watch_metrics` (last-seen flat metrics per file), `watch_best` (best-so-far per
//! family/metric), `watch_kv` (git heads, Ollama tags, run states, quiet hours), `notices`,
//! `notice_seen` (per client).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use marlowe_proto::{Notice, NoticeSource, Severity, WatchRoot};
use rusqlite::{params, Connection, OptionalExtension};

use crate::notice::NoticeDraft;
use crate::SentinelError;

/// A duplicate whose detail changed is raised again (broadcast, seen marks cleared) once the
/// previous raise is at least this old; sooner, it only refreshes.
pub const REARM: Duration = Duration::from_secs(30 * 60);

/// Notices older than this are pruned when the store opens.
const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS watch_roots (path TEXT PRIMARY KEY, added_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS watch_files (
    path TEXT PRIMARY KEY, offset INTEGER NOT NULL, len INTEGER NOT NULL, modified_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS watch_metrics (path TEXT PRIMARY KEY, metrics TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS watch_best (
    family TEXT NOT NULL, metric TEXT NOT NULL, value REAL NOT NULL, PRIMARY KEY (family, metric));
CREATE TABLE IF NOT EXISTS watch_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS notices (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL,
    severity INTEGER NOT NULL, title TEXT NOT NULL, detail TEXT NOT NULL,
    at_ms INTEGER NOT NULL, raised_ms INTEGER NOT NULL,
    UNIQUE (source, subject, kind));
CREATE INDEX IF NOT EXISTS notices_at ON notices (at_ms);
CREATE TABLE IF NOT EXISTS notice_seen (client TEXT NOT NULL, id INTEGER NOT NULL, PRIMARY KEY (client, id));
";

/// How far a file has been read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileMark {
    /// Bytes consumed; a file shorter than this was truncated or replaced and is re-read.
    pub offset: u64,
    pub len: u64,
    pub modified_ms: i64,
}

/// Synchronous access to the Sentinel tables. One connection behind a mutex.
pub struct SentinelStore {
    conn: Mutex<Connection>,
}

fn severity_num(s: Severity) -> i64 {
    match s {
        Severity::Info => 0,
        Severity::Good => 1,
        Severity::Attention => 2,
        Severity::Alarm => 3,
    }
}

fn severity_of(n: i64) -> Severity {
    match n {
        3 => Severity::Alarm,
        2 => Severity::Attention,
        1 => Severity::Good,
        _ => Severity::Info,
    }
}

fn source_str(s: NoticeSource) -> &'static str {
    match s {
        NoticeSource::Run => "run",
        NoticeSource::Log => "log",
        NoticeSource::Metric => "metric",
        NoticeSource::Trainer => "trainer",
        NoticeSource::Git => "git",
        NoticeSource::Ollama => "ollama",
        NoticeSource::Gpu => "gpu",
        NoticeSource::Discovery => "discovery",
        NoticeSource::Ci => "ci",
        NoticeSource::Experiment => "experiment",
        NoticeSource::Task => "task",
        NoticeSource::Message => "message",
        NoticeSource::Notify => "notify",
    }
}

fn source_of(s: &str) -> NoticeSource {
    match s {
        "run" => NoticeSource::Run,
        "log" => NoticeSource::Log,
        "metric" => NoticeSource::Metric,
        "trainer" => NoticeSource::Trainer,
        "git" => NoticeSource::Git,
        "ollama" => NoticeSource::Ollama,
        "gpu" => NoticeSource::Gpu,
        "ci" => NoticeSource::Ci,
        "experiment" => NoticeSource::Experiment,
        "task" => NoticeSource::Task,
        "message" => NoticeSource::Message,
        "notify" => NoticeSource::Notify,
        _ => NoticeSource::Discovery,
    }
}

const NOTICE_COLS: &str = "n.id, n.at_ms, n.source, n.severity, n.kind, n.title, n.detail, n.subject";

fn notice_row(r: &rusqlite::Row<'_>, seen: bool) -> rusqlite::Result<Notice> {
    Ok(Notice {
        id: format!("n{}", r.get::<_, i64>(0)?),
        at_ms: r.get(1)?,
        source: source_of(&r.get::<_, String>(2)?),
        severity: severity_of(r.get(3)?),
        kind: r.get(4)?,
        title: r.get(5)?,
        detail: r.get(6)?,
        subject: r.get(7)?,
        seen,
    })
}

/// A LIKE pattern matching `prefix` literally, then anything.
fn like_prefix(prefix: &str) -> String {
    format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

fn parse_id(id: &str) -> Option<i64> {
    id.strip_prefix('n').unwrap_or(id).parse().ok()
}

impl SentinelStore {
    /// Opens `cowork_db` (creating the Sentinel tables) and prunes notices older than 30 days.
    pub fn open(cowork_db: &Path) -> Result<SentinelStore, SentinelError> {
        if let Some(dir) = cowork_db.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(cowork_db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::init(conn)
    }

    /// An in-memory store for tests.
    pub fn in_memory() -> Result<SentinelStore, SentinelError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<SentinelStore, SentinelError> {
        conn.execute_batch(SCHEMA)?;
        let cutoff = crate::now_ms() - KEEP.as_millis() as i64;
        conn.execute("DELETE FROM notice_seen WHERE id IN (SELECT id FROM notices WHERE at_ms < ?1)", [cutoff])?;
        conn.execute("DELETE FROM notices WHERE at_ms < ?1", [cutoff])?;
        Ok(SentinelStore { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn roots(&self) -> Result<Vec<WatchRoot>, SentinelError> {
        let c = self.conn();
        let mut st = c.prepare("SELECT path, added_ms FROM watch_roots ORDER BY added_ms, path")?;
        let rows = st.query_map([], |r| {
            Ok(WatchRoot { path: r.get(0)?, added_ms: r.get(1)?, runs: 0, git: None, problem: None })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// False when it was already present.
    pub fn add_root(&self, path: &str, at_ms: i64) -> Result<bool, SentinelError> {
        let n = self.conn().execute("INSERT OR IGNORE INTO watch_roots (path, added_ms) VALUES (?1, ?2)", params![path, at_ms])?;
        Ok(n > 0)
    }

    /// False when it was not present.
    pub fn remove_root(&self, path: &str) -> Result<bool, SentinelError> {
        Ok(self.conn().execute("DELETE FROM watch_roots WHERE path = ?1", [path])? > 0)
    }

    /// Forgets file marks, metrics and run state recorded under `prefix` (a removed root).
    pub fn forget_under(&self, prefix: &str) -> Result<(), SentinelError> {
        let like = like_prefix(prefix);
        let c = self.conn();
        c.execute("DELETE FROM watch_files WHERE path LIKE ?1 ESCAPE '\\'", [&like])?;
        c.execute("DELETE FROM watch_metrics WHERE path LIKE ?1 ESCAPE '\\'", [&like])?;
        c.execute("DELETE FROM watch_kv WHERE key LIKE ?1 ESCAPE '\\'", [format!("run:{like}")])?;
        Ok(())
    }

    pub fn file_mark(&self, file: &str) -> Result<Option<FileMark>, SentinelError> {
        let c = self.conn();
        let mark = c
            .query_row("SELECT offset, len, modified_ms FROM watch_files WHERE path = ?1", [file], |r| {
                Ok(FileMark { offset: r.get::<_, i64>(0)? as u64, len: r.get::<_, i64>(1)? as u64, modified_ms: r.get(2)? })
            })
            .optional()?;
        Ok(mark)
    }

    /// Every mark recorded under `prefix` (one query for a root's first scan in a process).
    pub fn file_marks_under(&self, prefix: &str) -> Result<HashMap<String, FileMark>, SentinelError> {
        let c = self.conn();
        let mut st = c.prepare("SELECT path, offset, len, modified_ms FROM watch_files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = st.query_map([like_prefix(prefix)], |r| {
            let mark = FileMark { offset: r.get::<_, i64>(1)? as u64, len: r.get::<_, i64>(2)? as u64, modified_ms: r.get(3)? };
            Ok((r.get::<_, String>(0)?, mark))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_file_mark(&self, file: &str, mark: FileMark) -> Result<(), SentinelError> {
        self.set_file_marks(&[(file.to_string(), mark)])
    }

    /// Many marks in one transaction (a root's baseline scan).
    pub fn set_file_marks(&self, marks: &[(String, FileMark)]) -> Result<(), SentinelError> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO watch_files (path, offset, len, modified_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET offset = ?2, len = ?3, modified_ms = ?4",
            )?;
            for (p, m) in marks {
                st.execute(params![p, m.offset as i64, m.len as i64, m.modified_ms])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn forget_file(&self, file: &str) -> Result<(), SentinelError> {
        let c = self.conn();
        c.execute("DELETE FROM watch_files WHERE path = ?1", [file])?;
        c.execute("DELETE FROM watch_metrics WHERE path = ?1", [file])?;
        Ok(())
    }

    /// Last-seen flat metrics of a file.
    pub fn metrics(&self, file: &str) -> Result<Option<Vec<(String, f64)>>, SentinelError> {
        let text: Option<String> =
            self.conn().query_row("SELECT metrics FROM watch_metrics WHERE path = ?1", [file], |r| r.get(0)).optional()?;
        Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
    }

    pub fn set_metrics(&self, file: &str, metrics: &[(String, f64)]) -> Result<(), SentinelError> {
        let text = serde_json::to_string(metrics).unwrap_or_else(|_| "[]".into());
        self.conn().execute(
            "INSERT INTO watch_metrics (path, metrics) VALUES (?1, ?2) ON CONFLICT(path) DO UPDATE SET metrics = ?2",
            params![file, text],
        )?;
        Ok(())
    }

    /// Best value seen for `metric` within a sibling family (see [`crate::metrics::family`]).
    pub fn best(&self, family: &str, metric: &str) -> Result<Option<f64>, SentinelError> {
        let v = self
            .conn()
            .query_row("SELECT value FROM watch_best WHERE family = ?1 AND metric = ?2", params![family, metric], |r| r.get(0))
            .optional()?;
        Ok(v)
    }

    pub fn set_best(&self, family: &str, metric: &str, value: f64) -> Result<(), SentinelError> {
        self.conn().execute(
            "INSERT INTO watch_best (family, metric, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(family, metric) DO UPDATE SET value = ?3",
            params![family, metric, value],
        )?;
        Ok(())
    }

    /// Whether any best is recorded for `family`.
    pub fn has_family(&self, family: &str) -> Result<bool, SentinelError> {
        let n: i64 = self.conn().query_row("SELECT COUNT(*) FROM watch_best WHERE family = ?1", [family], |r| r.get(0))?;
        Ok(n > 0)
    }

    pub fn kv(&self, key: &str) -> Result<Option<String>, SentinelError> {
        Ok(self.conn().query_row("SELECT value FROM watch_kv WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<(), SentinelError> {
        self.conn().execute(
            "INSERT INTO watch_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_kv(&self, key: &str) -> Result<(), SentinelError> {
        self.conn().execute("DELETE FROM watch_kv WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Records a draft, deduplicated by `(source, subject, kind)`. Returns the notice when it is
    /// new, escalated in severity, or re-raised (detail changed and the last raise is older than
    /// [`REARM`]); those clear the per-client seen marks. `None` for a plain duplicate, which
    /// only refreshes `at_ms`, title and detail.
    pub fn record(&self, draft: &NoticeDraft, at_ms: i64) -> Result<Option<Notice>, SentinelError> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let src = source_str(draft.source);
        let sev = severity_num(draft.severity);
        let existing: Option<(i64, i64, String, i64)> = tx
            .query_row(
                "SELECT id, severity, detail, raised_ms FROM notices WHERE source = ?1 AND subject = ?2 AND kind = ?3",
                params![src, draft.subject, draft.kind],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let raised = match existing {
            None => {
                tx.execute(
                    "INSERT INTO notices (source, subject, kind, severity, title, detail, at_ms, raised_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![src, draft.subject, draft.kind, sev, draft.title, draft.detail, at_ms],
                )?;
                Some((tx.last_insert_rowid(), sev))
            }
            Some((id, old_sev, old_detail, raised_ms)) => {
                let escalated = sev > old_sev;
                let rearm = old_detail != draft.detail && at_ms - raised_ms >= REARM.as_millis() as i64;
                if escalated || rearm {
                    tx.execute(
                        "UPDATE notices SET severity = ?2, title = ?3, detail = ?4, at_ms = ?5, raised_ms = ?5 WHERE id = ?1",
                        params![id, sev, draft.title, draft.detail, at_ms],
                    )?;
                    tx.execute("DELETE FROM notice_seen WHERE id = ?1", [id])?;
                    Some((id, sev))
                } else {
                    tx.execute(
                        "UPDATE notices SET severity = MAX(severity, ?2), title = ?3, detail = ?4, at_ms = ?5 WHERE id = ?1",
                        params![id, sev, draft.title, draft.detail, at_ms],
                    )?;
                    None
                }
            }
        };
        tx.commit()?;
        Ok(raised.map(|(id, sev)| Notice {
            id: format!("n{id}"),
            at_ms,
            source: draft.source,
            severity: severity_of(sev),
            kind: draft.kind.clone(),
            title: draft.title.clone(),
            detail: draft.detail.clone(),
            subject: draft.subject.clone(),
            seen: false,
        }))
    }

    /// Ranked notices (severity, then newest) with `seen` for `client`.
    pub fn notices(&self, client: &str, limit: usize, unseen_only: bool) -> Result<Vec<Notice>, SentinelError> {
        let c = self.conn();
        let sql = format!(
            "SELECT {NOTICE_COLS}, s.id IS NOT NULL FROM notices n
             LEFT JOIN notice_seen s ON s.id = n.id AND s.client = ?1
             WHERE (?2 = 0 OR s.id IS NULL)
             ORDER BY n.severity DESC, n.at_ms DESC, n.id DESC LIMIT ?3"
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![client, unseen_only as i64, limit.min(i64::MAX as usize) as i64], |r| {
            let seen: bool = r.get(8)?;
            notice_row(r, seen)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Notices raised or refreshed at or after `since_ms`, ranked.
    pub fn notices_since(&self, since_ms: i64) -> Result<Vec<Notice>, SentinelError> {
        let c = self.conn();
        let sql = format!("SELECT {NOTICE_COLS} FROM notices n WHERE n.at_ms >= ?1 ORDER BY n.severity DESC, n.at_ms DESC, n.id DESC");
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map([since_ms], |r| notice_row(r, false))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// False when no such notice exists.
    pub fn mark_seen(&self, client: &str, id: &str) -> Result<bool, SentinelError> {
        let Some(n) = parse_id(id) else { return Ok(false) };
        let c = self.conn();
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id = ?1)", [n], |r| r.get(0))?;
        if exists {
            c.execute("INSERT OR IGNORE INTO notice_seen (client, id) VALUES (?1, ?2)", params![client, n])?;
        }
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: &str, sev: Severity, detail: &str) -> NoticeDraft {
        NoticeDraft {
            source: NoticeSource::Run,
            severity: sev,
            kind: kind.into(),
            title: format!("night1 {kind}"),
            detail: detail.into(),
            subject: "C:\\runs\\night1".into(),
        }
    }

    #[test]
    fn dedupes_escalates_and_rearms() {
        let s = SentinelStore::in_memory().unwrap();
        let t0 = crate::now_ms();
        let a = s.record(&draft("stalled", Severity::Attention, "12 min"), t0).unwrap().unwrap();
        // Same key, same severity: refresh only.
        assert!(s.record(&draft("stalled", Severity::Attention, "14 min"), t0 + 1000).unwrap().is_none());
        // Escalation is raised again under the same id.
        let b = s.record(&draft("stalled", Severity::Alarm, "20 min"), t0 + 2000).unwrap().unwrap();
        assert_eq!((b.id.as_str(), b.severity), (a.id.as_str(), Severity::Alarm));
        // A changed detail after REARM is raised again; an unchanged one is not.
        let later = t0 + 2000 + REARM.as_millis() as i64;
        assert!(s.record(&draft("stalled", Severity::Alarm, "20 min"), later).unwrap().is_none());
        assert!(s.record(&draft("stalled", Severity::Alarm, "45 min"), later + 1).unwrap().is_some());
        // A different kind is a different notice.
        let f = s.record(&draft("finished", Severity::Good, "done"), t0 + 5000).unwrap().unwrap();
        assert_ne!(f.id, a.id);

        let all = s.notices("tui", 10, false).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].severity, Severity::Alarm, "ranked by severity first");
        assert_eq!(all[0].detail, "45 min");
        assert!(s.mark_seen("tui", &a.id).unwrap());
        assert!(!s.mark_seen("tui", "n999").unwrap());
        assert!(!s.mark_seen("tui", "bogus").unwrap());
        assert_eq!(s.notices("tui", 10, true).unwrap().len(), 1);
        assert_eq!(s.notices("cli", 10, true).unwrap().len(), 2, "seen is per client");
        assert!(s.notices("tui", 10, false).unwrap().iter().any(|n| n.seen));
        // Re-raising clears seen marks.
        s.record(&draft("stalled", Severity::Alarm, "60 min"), later + 2 * REARM.as_millis() as i64).unwrap().unwrap();
        assert_eq!(s.notices("tui", 10, true).unwrap().len(), 2);
        assert_eq!(s.notices_since(t0 + 4000).unwrap().len(), 2);
        assert_eq!(s.notices_since(later + 10).unwrap().len(), 1);
    }

    #[test]
    fn roots_marks_metrics_best_kv() {
        let s = SentinelStore::in_memory().unwrap();
        assert!(s.add_root("C:\\r\\valiance", 5).unwrap());
        assert!(!s.add_root("C:\\r\\valiance", 6).unwrap());
        assert_eq!(s.roots().unwrap().len(), 1);
        let m = FileMark { offset: 10, len: 20, modified_ms: 30 };
        s.set_file_mark("C:\\r\\valiance\\a.log", m).unwrap();
        s.set_file_mark("C:\\r\\valiance_2\\b.log", m).unwrap();
        assert_eq!(s.file_mark("C:\\r\\valiance\\a.log").unwrap(), Some(m));
        s.set_metrics("C:\\r\\valiance\\x.json", &[("loss".into(), 1.5)]).unwrap();
        assert_eq!(s.metrics("C:\\r\\valiance\\x.json").unwrap(), Some(vec![("loss".to_string(), 1.5)]));
        s.set_best("fam", "loss", 1.2).unwrap();
        assert_eq!(s.best("fam", "loss").unwrap(), Some(1.2));
        assert!(s.has_family("fam").unwrap() && !s.has_family("other").unwrap());
        s.set_kv("quiet", "1380-420").unwrap();
        assert_eq!(s.kv("quiet").unwrap().as_deref(), Some("1380-420"));
        assert_eq!(s.file_marks_under("C:\\r\\valiance\\").unwrap().len(), 1);
        s.forget_under("C:\\r\\valiance\\").unwrap();
        assert_eq!(s.file_mark("C:\\r\\valiance\\a.log").unwrap(), None);
        assert_eq!(s.file_mark("C:\\r\\valiance_2\\b.log").unwrap(), Some(m), "prefix match is literal");
        assert!(s.remove_root("C:\\r\\valiance").unwrap());
        assert!(!s.remove_root("C:\\r\\valiance").unwrap());
    }

    #[test]
    fn persists_on_disk() {
        let dir = std::env::temp_dir().join(format!("sentinel-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cowork.db");
        {
            let s = SentinelStore::open(&path).unwrap();
            s.add_root("C:\\x", 1).unwrap();
        }
        assert_eq!(SentinelStore::open(&path).unwrap().roots().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
