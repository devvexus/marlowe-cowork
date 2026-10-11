//! Running the project's migrations against a database made for the purpose.
//!
//! The default target is a throwaway sqlite file in a temp directory: the migration tool gets
//! `DATABASE_URL` (and whatever the profile's `env` / argv placeholders ask for) pointing at it,
//! runs `up`, and the schema it left is read back with sqlite so the model sees tables and
//! columns rather than a tool's exit code. With a `down` command it can also prove the round
//! trip (up, down, up gives the same schema).
//!
//! A database the user named is a different thing and asks first (the tool layer's
//! consequence). A tool cannot be forced to ignore its own config, so before running against the
//! throwaway the config files the profile lists are scanned for a literal server URL
//! (`postgresql://...`); if one is there the run is refused, because the tool would reach that
//! server regardless of `DATABASE_URL`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};

use crate::cmd::Cmd;
use crate::env::Env;
use crate::profile::{Kind, Migrations};
use crate::run::{run_custom, LogStore, StepReport};

const MIGRATE_TIMEOUT: Duration = Duration::from_secs(300);

/// A sqlite file in its own temp directory, removed on drop.
pub struct Throwaway {
    dir: PathBuf,
    pub path: PathBuf,
}

impl Throwaway {
    pub fn create() -> std::io::Result<Self> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join("marlowe-verify-db").join(format!("{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("throwaway.db");
        Ok(Self { dir, path })
    }

    /// `sqlite:///C:/.../throwaway.db`
    pub fn url(&self) -> String {
        format!("sqlite:///{}", self.path.to_string_lossy().replace('\\', "/"))
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where a migration runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Throwaway,
    /// A database the user named (asks before it runs).
    User(String),
}

/// `scheme://user:***@host/db` with the password hidden.
pub fn redact_url(url: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| Regex::new(r"^([A-Za-z][A-Za-z0-9+.\-]*://[^:/@\s]+):[^@\s]*@").expect("static regex"));
    r.replace(url, "$1:***@").into_owned()
}

/// A literal server database URL in `text`, redacted.
pub fn real_database_url(text: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|redis|mssql|sqlserver|oracle)(?:\+\w+)?://[^\s'"<>`]+"#).expect("static regex")
    });
    r.find_iter(text)
        .map(|m| m.as_str())
        .find(|u| !u.contains("${") && !u.contains("{{") && !u.contains("%(") && !u.contains("$DATABASE"))
        .map(redact_url)
}

/// One table of the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: usize,
    pub rows: i64,
    pub sql: String,
    /// The migration tool's own bookkeeping table.
    pub bookkeeping: bool,
}

const BOOKKEEPING: &[&str] = &[
    "alembic_version", "django_migrations", "schema_migrations", "_sqlx_migrations", "__diesel_schema_migrations", "goose_db_version",
    "_prisma_migrations", "knex_migrations", "knex_migrations_lock", "sequelizemeta", "__drizzle_migrations", "flyway_schema_history",
];

/// Reads the schema of the sqlite file at `path`.
pub fn inspect(path: &Path) -> Result<Vec<Table>, String> {
    if !path.is_file() || std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Ok(Vec::new());
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("cannot open the database: {e}"))?;
    let mut stmt = db
        .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    let mut out = Vec::new();
    for (name, sql) in rows.into_iter().take(200) {
        let quoted = name.replace('"', "\"\"");
        let columns = db
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))
            .and_then(|mut s| s.query_map([], |_| Ok(())).map(|m| m.count()))
            .unwrap_or(0);
        let rows = db.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        let bookkeeping = BOOKKEEPING.contains(&name.to_ascii_lowercase().as_str());
        out.push(Table { name, columns, rows, sql, bookkeeping });
    }
    Ok(out)
}

fn normalised(t: &[Table]) -> Vec<(String, String)> {
    t.iter().map(|t| (t.name.clone(), t.sql.split_whitespace().collect::<Vec<_>>().join(" "))).collect()
}

/// A finished migration run.
#[derive(Debug)]
pub struct MigrateReport {
    pub tool: String,
    pub target: String,
    pub steps: Vec<(String, StepReport)>,
    pub after_up: Option<Vec<Table>>,
    pub after_roundtrip: Option<Vec<Table>>,
    pub roundtrip_asked: bool,
    pub notes: Vec<String>,
}

impl MigrateReport {
    pub fn ok(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|(_, s)| s.ok) && self.roundtrip_same().unwrap_or(true)
    }

    /// Whether up, down, up left the same schema (`None` when it was not run).
    pub fn roundtrip_same(&self) -> Option<bool> {
        match (&self.after_up, &self.after_roundtrip) {
            (Some(a), Some(b)) => Some(normalised(a) == normalised(b)),
            _ => None,
        }
    }

    pub fn render(&self) -> String {
        let mut out = format!("migrate ({}) against {}: {}\n", self.tool, self.target, if self.ok() { "ok" } else { "FAILED" });
        for (label, s) in &self.steps {
            out.push_str(&format!("  {label}: {} (exit {}, {:.1} s)  `{}`\n", if s.ok { "ok" } else { "FAILED" }, s.exit, s.duration_ms as f64 / 1000.0, s.command));
        }
        if let Some(tables) = &self.after_up {
            let (book, real): (Vec<&Table>, Vec<&Table>) = tables.iter().partition(|t| t.bookkeeping);
            out.push_str(&format!("schema after up: {} table(s)\n", real.len()));
            for t in &real {
                out.push_str(&format!("  {} ({} columns, {} rows)\n", t.name, t.columns, t.rows));
            }
            if !book.is_empty() {
                out.push_str(&format!("  (bookkeeping: {})\n", book.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if real.is_empty() {
                out.push_str("  nothing was created in the throwaway database: the tool did not write to it. Its config may name another database; set DATABASE_URL in .marlowe/project.toml [migrations] env.\n");
            }
        }
        match self.roundtrip_same() {
            Some(true) => out.push_str("round trip: up, down, up left the same schema\n"),
            Some(false) => out.push_str("round trip: the schema after up, down, up DIFFERS from the first up (the down migration does not undo the up)\n"),
            None if self.roundtrip_asked => out.push_str("round trip: not run (no down command, or an earlier step failed)\n"),
            None => {}
        }
        for s in self.steps.iter().filter(|(_, s)| !s.ok).map(|(_, s)| s) {
            out.push_str(&s.render());
        }
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        out.trim_end().to_string()
    }
}

fn fill(s: &str, db_url: &str, db_path: &str) -> String {
    s.replace("{db_url}", db_url).replace("{db_path}", db_path)
}

/// Why a throwaway run must not start: the config names a real server.
pub fn preflight(env: &Env<'_>, m: &Migrations) -> Result<(), String> {
    let Ok(cwd) = env.cwd(&m.cwd) else { return Ok(()) };
    for f in &m.config_files {
        let p = cwd.join(f);
        if !crate::env::inside(env.root, &p) {
            continue;
        }
        if let Some(text) = env.ws.read(&p).ok().map(|b| String::from_utf8_lossy(&b[..b.len().min(1 << 20)]).into_owned()) {
            if let Some(url) = real_database_url(&text) {
                return Err(format!(
                    "{f} names a real database ({url}); the migration tool would use it whatever DATABASE_URL says. \
                     Make the config read DATABASE_URL, or pass database_url for the database you mean (that asks first)"
                ));
            }
        }
    }
    Ok(())
}

/// Runs `m` against `target`; `roundtrip` adds down and up again when `m` has a down command.
pub fn migrate(env: &Env<'_>, m: &Migrations, target: &Target, roundtrip: bool, logs: &LogStore) -> Result<MigrateReport, String> {
    let throwaway;
    let (db_url, db_path, desc, inspect_path): (String, String, String, Option<PathBuf>) = match target {
        Target::Throwaway => {
            preflight(env, m)?;
            throwaway = Throwaway::create().map_err(|e| format!("cannot create the throwaway database: {e}"))?;
            (throwaway.url(), throwaway.path.to_string_lossy().replace('\\', "/"), "a throwaway sqlite database".to_string(), Some(throwaway.path.clone()))
        }
        Target::User(url) => {
            let path = url.strip_prefix("sqlite:///").or_else(|| url.strip_prefix("sqlite://")).or_else(|| url.strip_prefix("sqlite:")).or_else(|| url.strip_prefix("file:"));
            (url.clone(), path.unwrap_or("").to_string(), format!("{} (the database you named)", redact_url(url)), path.map(PathBuf::from).filter(|p| p.is_file()))
        }
    };
    let env_vars: Vec<(String, String)> = m
        .env
        .iter()
        .map(|(k, v)| (k.clone(), fill(v, &db_url, &db_path)))
        .chain([
            ("MARLOWE_THROWAWAY_DB_URL".to_string(), db_url.clone()),
            ("MARLOWE_THROWAWAY_DB_PATH".to_string(), db_path.clone()),
        ])
        .collect();
    let sub = |c: &Cmd| Cmd { argv: c.argv.iter().map(|a| fill(a, &db_url, &db_path)).collect() };
    let mut report = MigrateReport {
        tool: m.tool.clone(),
        target: desc,
        steps: Vec::new(),
        after_up: None,
        after_roundtrip: None,
        roundtrip_asked: roundtrip,
        notes: Vec::new(),
    };
    let up = sub(&m.up);
    let rep = run_custom(env, Kind::Build, &up, &m.cwd, MIGRATE_TIMEOUT, &env_vars, logs);
    let up_ok = rep.ok;
    report.steps.push(("up".into(), rep));
    if inspect_path.is_some() || matches!(target, Target::Throwaway) {
        if let Some(p) = &inspect_path {
            report.after_up = Some(inspect(p)?);
        }
    } else {
        report.notes.push("the schema is not read back for a database that is not a local sqlite file".into());
    }
    if roundtrip && up_ok {
        match &m.down {
            Some(down) => {
                let rep = run_custom(env, Kind::Build, &sub(down), &m.cwd, MIGRATE_TIMEOUT, &env_vars, logs);
                let down_ok = rep.ok;
                report.steps.push(("down".into(), rep));
                if down_ok {
                    let rep = run_custom(env, Kind::Build, &up, &m.cwd, MIGRATE_TIMEOUT, &env_vars, logs);
                    let again_ok = rep.ok;
                    report.steps.push(("up again".into(), rep));
                    if again_ok {
                        if let Some(p) = &inspect_path {
                            report.after_roundtrip = Some(inspect(p)?);
                        }
                    }
                }
            }
            None => report.notes.push("this project has no down migration command, so the round trip was skipped".into()),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Source;
    use marlowe_model::CancelToken;
    use marlowe_os::workspace::LocalFs;

    const SCRIPT: &str = "import os, sqlite3, sys\n\
        url = os.environ['DATABASE_URL']\n\
        path = url.removeprefix('sqlite:///')\n\
        db = sqlite3.connect(path)\n\
        if sys.argv[1] == 'up':\n\
        \x20   db.executescript('create table if not exists users(id integer primary key, name text); create table if not exists posts(id integer primary key, user_id integer references users(id)); create table if not exists alembic_version(v text);')\n\
        elif sys.argv[1] == 'broken':\n\
        \x20   db.executescript('create table if not exists users(id integer primary key);')\n\
        else:\n\
        \x20   db.executescript('drop table if exists users; drop table if exists posts;')\n\
        db.commit(); db.close()\n";

    const DRIFT: &str = r#"import os, sqlite3, sys
path = os.environ['DATABASE_URL'].removeprefix('sqlite:///')
db = sqlite3.connect(path)
if sys.argv[1] == 'up':
    n = db.execute('pragma user_version').fetchone()[0] + 1
    db.execute('create table if not exists t(id integer)')
    db.execute(f'alter table t add column c{n} integer')
    db.execute(f'pragma user_version = {n}')
db.commit()
db.close()
"#;

    fn fixture(tag: &str) -> Option<(PathBuf, String)> {
        let py = crate::cmd::resolve_program("python", None)?.to_string_lossy().into_owned();
        let root = std::env::temp_dir().join(format!("marlowe-verify-mig-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("migrate.py"), SCRIPT).unwrap();
        Some((root, py))
    }

    fn mig(py: &str, down_arg: &str) -> Migrations {
        Migrations {
            tool: "custom".into(),
            up: Cmd { argv: vec![py.into(), "migrate.py".into(), "up".into()] },
            down: Some(Cmd { argv: vec![py.into(), "migrate.py".into(), down_arg.into()] }),
            cwd: ".".into(),
            config_files: vec!["alembic.ini".into()],
            env: vec![("DATABASE_URL".into(), "{db_url}".into())],
            source: Source::Override,
            available: true,
        }
    }

    #[test]
    fn up_runs_against_a_throwaway_database_and_the_schema_is_read_back() {
        let Some((root, py)) = fixture("up") else { return };
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let r = migrate(&env, &mig(&py, "down"), &Target::Throwaway, true, &LogStore::default()).unwrap();
        assert!(r.ok(), "{}", r.render());
        let up = r.after_up.as_ref().unwrap();
        let names: Vec<&str> = up.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["alembic_version", "posts", "users"]);
        assert_eq!(up.iter().find(|t| t.name == "users").unwrap().columns, 2);
        assert!(up.iter().find(|t| t.name == "alembic_version").unwrap().bookkeeping);
        assert_eq!(r.roundtrip_same(), Some(true));
        let text = r.render();
        assert!(text.contains("users (2 columns, 0 rows)") && text.contains("(bookkeeping: alembic_version)") && text.contains("up, down, up left the same schema"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_down_that_does_not_undo_the_up_is_caught() {
        let Some((root, py)) = fixture("rt") else { return };
        // Each `up` adds a column named for how many times it has run, and `down` does nothing,
        // so up, down, up cannot leave the schema the first up did.
        std::fs::write(root.join("drift.py"), DRIFT).unwrap();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let mut m = mig(&py, "down");
        m.up = Cmd { argv: vec![py.clone(), "drift.py".into(), "up".into()] };
        m.down = Some(Cmd { argv: vec![py.clone(), "drift.py".into(), "down".into()] });
        let r = migrate(&env, &m, &Target::Throwaway, true, &LogStore::default()).unwrap();
        assert_eq!(r.roundtrip_same(), Some(false), "{}", r.render());
        assert!(!r.ok() && r.render().contains("DIFFERS"), "{}", r.render());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_config_that_hard_codes_a_server_refuses_the_throwaway_run() {
        let Some((root, py)) = fixture("cfg") else { return };
        std::fs::write(root.join("alembic.ini"), "[alembic]\nsqlalchemy.url = postgresql://admin:hunter2@db.internal:5432/prod\n").unwrap();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let err = migrate(&env, &mig(&py, "down"), &Target::Throwaway, false, &LogStore::default()).unwrap_err();
        assert!(err.contains("alembic.ini names a real database") && err.contains("postgresql://admin:***@db.internal"), "{err}");
        assert!(!err.contains("hunter2"), "the password is never echoed");
        std::fs::write(root.join("alembic.ini"), "[alembic]\nsqlalchemy.url = ${DATABASE_URL}\n").unwrap();
        assert!(preflight(&env, &mig(&py, "down")).is_ok(), "a placeholder is fine");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn urls_are_redacted_and_real_ones_found() {
        assert_eq!(redact_url("postgres://u:p%40ss@h:5432/db"), "postgres://u:***@h:5432/db");
        assert_eq!(redact_url("sqlite:///x.db"), "sqlite:///x.db");
        assert!(real_database_url("url = mysql://root@localhost/app").is_some());
        assert!(real_database_url("url = ${DATABASE_URL}\nother = sqlite:///dev.db").is_none());
        assert!(real_database_url("postgresql://%(user)s@h/db").is_none());
    }
}
