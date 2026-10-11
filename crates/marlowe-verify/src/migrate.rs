//! Running the project's migrations against a database made for the purpose.
//!
//! The default target is a throwaway sqlite file in a temp directory: the migration tool gets
//! `DATABASE_URL` (and whatever the profile's `env` / argv placeholders ask for) pointing at it,
//! runs `up`, and the schema it left is read back with sqlite so the model sees tables and
//! columns rather than a tool's exit code. With a `down` command it can also prove the round
//! trip (up, down, up gives the same schema).
//!
//! A database the user named is a different thing and asks first (the tool layer's
//! consequence). So is any database the migration's own setup names: a tool cannot be forced to
//! ignore its own config, so [`named_databases`] reads every environment value, every argument
//! and the config files the profile lists for a server URL, a sqlite file or a database path
//! that is not the throwaway's placeholder, and a migrate call that finds one asks and runs
//! against what the config says ([`Target::Config`]). Django is the exception that can be
//! redirected: its settings are wrapped by a generated module whose `DATABASES` point at the
//! throwaway files.

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

impl Throwaway {
    /// The directory holding the database file.
    pub fn dir(&self) -> &Path {
        &self.dir
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
    /// Databases the migration's own configuration names (redacted; asks before it runs). The
    /// placeholders still get a throwaway, but the tool is free to use what its config says.
    Config(Vec<String>),
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

/// What stands in for the throwaway in a value being scanned.
const THROWAWAY: &str = "@@throwaway@@";

/// A server URL, sqlite file URL or (when `bare_paths`) database file path in `text` that is not
/// the throwaway, redacted.
fn database_in(text: &str, bare_paths: bool) -> Option<String> {
    static SQLITE: OnceLock<Regex> = OnceLock::new();
    static FILE: OnceLock<Regex> = OnceLock::new();
    let sqlite = SQLITE.get_or_init(|| Regex::new(r#"(?i)\b(?:sqlite3?(?:\+\w+)?|file):[^\s'"<>]*"#).expect("static regex"));
    let file = FILE.get_or_init(|| Regex::new(r#"(?i)[^\s'"=<>]+\.(?:db|db3|sqlite3?)\b"#).expect("static regex"));
    if let Some(u) = real_database_url(text) {
        return Some(u);
    }
    for m in sqlite.find_iter(text) {
        let t = m.as_str();
        if t.contains(THROWAWAY) {
            continue;
        }
        let path = t.split_once(':').map_or("", |(_, r)| r).trim_start_matches('/');
        // In memory, or read from the environment at run time.
        if path.is_empty() || path.starts_with(":memory:") || path.contains("mode=memory") || path.starts_with('$') || t.contains("${") || t.contains("{{") {
            continue;
        }
        return Some(t.to_string());
    }
    if bare_paths {
        return file.find_iter(text).map(|m| m.as_str()).find(|p| !p.contains(THROWAWAY) && !p.contains("${")).map(String::from);
    }
    None
}

fn is_django(m: &Migrations) -> bool {
    m.tool.eq_ignore_ascii_case("django") || m.up.argv.iter().any(|a| a.replace('\\', "/").ends_with("manage.py"))
}

/// The databases other than the throwaway that running `m` would touch, redacted: named by its
/// environment values, its arguments, or the config files it lists. A placeholder (`{db_url}`,
/// `{db_path}`) is the throwaway and is not one.
pub fn named_databases(env: &Env<'_>, m: &Migrations) -> Vec<String> {
    let sub = |s: &str| s.replace("{db_url}", THROWAWAY).replace("{db_path}", THROWAWAY);
    let mut out: Vec<String> = Vec::new();
    let mut add = |what: String| {
        if !out.contains(&what) {
            out.push(what);
        }
    };
    for (k, v) in &m.env {
        if let Some(db) = database_in(&sub(v), true) {
            add(format!("{k}={db}"));
        }
    }
    for (label, cmd) in [("up", Some(&m.up)), ("down", m.down.as_ref())] {
        for a in cmd.into_iter().flat_map(|c| c.argv.iter().skip(1)) {
            if let Some(db) = database_in(&sub(a), true) {
                add(format!("{label} argument {db}"));
            }
        }
        // `manage.py migrate --settings=other` picks its own settings, so the throwaway cannot be forced.
        if is_django(m) && cmd.into_iter().flat_map(|c| c.argv.iter()).any(|a| a.starts_with("--settings")) {
            add(format!("{label} passes --settings (its own Django settings)"));
        }
    }
    if let Ok(cwd) = env.cwd(&m.cwd) {
        for f in &m.config_files {
            let p = cwd.join(f);
            if !crate::env::inside(env.root, &p) {
                continue;
            }
            if let Some(text) = env.ws.read(&p).ok().map(|b| String::from_utf8_lossy(&b[..b.len().min(1 << 20)]).into_owned()) {
                if let Some(db) = database_in(&text, false) {
                    add(format!("{f}: {db}"));
                }
            }
        }
    }
    out
}

/// The module Django's `manage.py` (or the migration's environment) selects.
fn django_settings_module(env: &Env<'_>, m: &Migrations) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| Regex::new(r#"DJANGO_SETTINGS_MODULE['"]\s*,\s*['"]([A-Za-z0-9_.]+)['"]"#).expect("static regex"));
    if let Some((_, v)) = m.env.iter().find(|(k, _)| k == "DJANGO_SETTINGS_MODULE") {
        return Some(v.clone()).filter(|v| !v.contains('{') && !v.is_empty());
    }
    let cwd = env.cwd(&m.cwd).ok()?;
    let manage = m.up.argv.iter().find(|a| a.replace('\\', "/").ends_with("manage.py")).map_or("manage.py".to_string(), |a| a.clone());
    let text = env.ws.read(&cwd.join(manage)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())?;
    r.captures(&text).map(|c| c[1].to_string())
}

/// A settings module that star-imports the project's and then points every database alias at a
/// throwaway sqlite file (`throwaway.db` for `default`), with the environment that selects it.
fn django_override(env: &Env<'_>, m: &Migrations, t: &Throwaway) -> Result<Vec<(String, String)>, String> {
    let module = django_settings_module(env, m).ok_or(
        "cannot tell which Django settings module this migration uses, so its DATABASES cannot be pointed at the throwaway; \
         set DJANGO_SETTINGS_MODULE in .marlowe/project.toml [migrations] env, or pass database_url for the database you mean (that asks first)",
    )?;
    let dir = t.dir().to_string_lossy().into_owned();
    let literal = serde_json::to_string(&dir).map_err(|e| e.to_string())?;
    let source = format!(
        "# Generated: runs the project's settings against a throwaway sqlite database.\n\
         from {module} import *  # noqa\n\
         import os as _os\n\
         _aliases = list(globals().get('DATABASES') or {{'default': None}}) or ['default']\n\
         DATABASES = {{a: {{'ENGINE': 'django.db.backends.sqlite3', 'NAME': _os.path.join({literal}, 'throwaway.db' if a == 'default' else 'throwaway.' + a + '.db')}} for a in _aliases}}\n\
         DATABASE_ROUTERS = []\n"
    );
    std::fs::write(t.dir().join("marlowe_throwaway_settings.py"), source).map_err(|e| format!("cannot write the throwaway settings: {e}"))?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    let path = match std::env::var("PYTHONPATH") {
        Ok(old) if !old.is_empty() => format!("{dir}{sep}{old}"),
        _ => dir,
    };
    Ok(vec![("DJANGO_SETTINGS_MODULE".into(), "marlowe_throwaway_settings".into()), ("PYTHONPATH".into(), path)])
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
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let (db_url, db_path, desc, inspect_path): (String, String, String, Option<PathBuf>) = match target {
        Target::Throwaway => {
            preflight(env, m)?;
            throwaway = Throwaway::create().map_err(|e| format!("cannot create the throwaway database: {e}"))?;
            if is_django(m) {
                extra_env = django_override(env, m, &throwaway)?;
            }
            (throwaway.url(), throwaway.path.to_string_lossy().replace('\\', "/"), "a throwaway sqlite database".to_string(), Some(throwaway.path.clone()))
        }
        Target::Config(names) => {
            // Approved to touch what the config says; placeholders still get a throwaway.
            throwaway = Throwaway::create().map_err(|e| format!("cannot create the throwaway database: {e}"))?;
            (throwaway.url(), throwaway.path.to_string_lossy().replace('\\', "/"), format!("{} (named by the migration's own configuration)", names.join("; ")), None)
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
        .filter(|(k, _)| !extra_env.iter().any(|(e, _)| e == k))
        .chain([
            ("MARLOWE_THROWAWAY_DB_URL".to_string(), db_url.clone()),
            ("MARLOWE_THROWAWAY_DB_PATH".to_string(), db_path.clone()),
        ])
        .chain(extra_env.iter().cloned())
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
    if let Some(p) = &inspect_path {
        report.after_up = Some(inspect(p)?);
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

    fn env_for<'a>(root: &'a Path, ws: &'a LocalFs, cancel: &'a CancelToken) -> Env<'a> {
        Env { ws, root, pin: None, scrub: &[], child_env: &[], cancel }
    }

    #[test]
    fn a_database_named_in_the_env_or_arguments_is_not_the_throwaway() {
        let Some((root, py)) = fixture("named") else { return };
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = env_for(&root, &ws, &cancel);
        let none = |m: &Migrations| named_databases(&env, m);
        // The placeholders are the throwaway, in any spelling a tool needs.
        let mut m = mig(&py, "down");
        assert!(none(&m).is_empty());
        m.env = vec![("DATABASE_URL".into(), "file:{db_path}".into()), ("OTHER".into(), "sqlite:///{db_path}".into())];
        m.up = Cmd { argv: vec!["goose".into(), "-dir".into(), "migrations".into(), "sqlite3".into(), "{db_path}".into(), "up".into()] };
        assert!(none(&m).is_empty(), "{:?}", none(&m));
        // (a) an override can point the migration at a server
        m.env = vec![("DATABASE_URL".into(), "postgresql://admin:pw@prod.internal/db".into())];
        let named = none(&m);
        assert_eq!(named, ["DATABASE_URL=postgresql://admin:***@prod.internal/db"]);
        assert!(!named.join(" ").contains("pw@"));
        // or at a file, in the environment or in an argument
        m.env = vec![("DB".into(), "sqlite:///./app.db".into())];
        assert_eq!(none(&m), ["DB=sqlite:///./app.db"]);
        m.env = vec![("DATABASE_URL".into(), "{db_url}".into())];
        m.up = Cmd { argv: vec!["tool".into(), "--database".into(), "./dev.sqlite3".into(), "up".into()] };
        assert_eq!(none(&m), ["up argument ./dev.sqlite3"]);
        m.up = Cmd { argv: vec!["tool".into(), "--url".into(), "mysql://root@localhost/app".into()] };
        assert!(none(&m)[0].starts_with("up argument mysql://root@localhost/app"), "{:?}", none(&m));
        // In memory is nothing.
        m.up = Cmd { argv: vec!["tool".into(), "sqlite://".into(), "sqlite:///:memory:".into()] };
        assert!(none(&m).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_config_file_that_names_a_sqlite_file_is_a_named_database_too() {
        let Some((root, py)) = fixture("cfgsqlite") else { return };
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = env_for(&root, &ws, &cancel);
        let m = mig(&py, "down");
        // (c) alembic's stock `sqlalchemy.url` pointing at the project's own file
        std::fs::write(root.join("alembic.ini"), "[alembic]\nscript_location = migrations\nsqlalchemy.url = sqlite:///./app.db\n").unwrap();
        assert_eq!(named_databases(&env, &m), ["alembic.ini: sqlite:///./app.db"]);
        std::fs::write(root.join("alembic.ini"), "[alembic]\nsqlalchemy.url = sqlite:///%(here)s/app.db\n").unwrap();
        assert_eq!(named_databases(&env, &m).len(), 1, "a file under the project is still a file");
        for fine in ["sqlalchemy.url = ${DATABASE_URL}", "sqlalchemy.url = sqlite://", "sqlalchemy.url = sqlite:///:memory:", "# nothing here"] {
            std::fs::write(root.join("alembic.ini"), format!("[alembic]\n{fine}\n")).unwrap();
            assert!(named_databases(&env, &m).is_empty(), "{fine}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// (b) Django ignores DATABASE_URL: its settings are wrapped so every alias is a throwaway file.
    #[test]
    fn django_settings_are_pointed_at_the_throwaway_and_the_real_database_is_untouched() {
        let Some((root, py)) = fixture("django") else { return };
        std::fs::create_dir_all(root.join("mysite")).unwrap();
        std::fs::write(
            root.join("mysite").join("settings.py"),
            "DATABASES = {\n 'default': {'ENGINE': 'django.db.backends.postgresql', 'NAME': 'prod', 'HOST': 'db.prod.internal'},\n 'archive': {'ENGINE': 'django.db.backends.sqlite3', 'NAME': 'real.sqlite3'},\n}\n",
        )
        .unwrap();
        // Stands in for Django: reads the settings module the environment names and "migrates" every alias.
        std::fs::write(
            root.join("manage.py"),
            "import os, sqlite3, importlib\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'mysite.settings')\n\
             s = importlib.import_module(os.environ['DJANGO_SETTINGS_MODULE'])\n\
             for alias, cfg in s.DATABASES.items():\n    db = sqlite3.connect(cfg['NAME'])\n    db.execute('create table if not exists t_' + alias + '(id integer)')\n    db.commit(); db.close()\n",
        )
        .unwrap();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = env_for(&root, &ws, &cancel);
        let m = Migrations {
            tool: "django".into(),
            up: Cmd { argv: vec![py.clone(), "manage.py".into(), "migrate".into()] },
            down: None,
            cwd: ".".into(),
            config_files: vec![],
            env: vec![("DATABASE_URL".into(), "{db_url}".into())],
            source: Source::Detected,
            available: true,
        };
        assert!(named_databases(&env, &m).is_empty(), "the throwaway can be forced, so nothing is named");
        let r = migrate(&env, &m, &Target::Throwaway, false, &LogStore::default()).unwrap();
        assert!(r.ok(), "{}", r.render());
        assert!(!root.join("real.sqlite3").exists(), "the project's own database was not created or touched");
        let tables: Vec<&str> = r.after_up.as_ref().unwrap().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tables, ["t_default"], "the default alias is the throwaway that was read back");
        // With --settings the throwaway cannot be forced, so it is named.
        let mut other = m.clone();
        other.up.argv.push("--settings=mysite.prod".into());
        assert!(named_databases(&env, &other)[0].contains("--settings"), "{:?}", named_databases(&env, &other));
        // A module that cannot be found is an error, not a guess.
        std::fs::write(root.join("manage.py"), "print('no settings here')\n").unwrap();
        let err = migrate(&env, &m, &Target::Throwaway, false, &LogStore::default()).unwrap_err();
        assert!(err.contains("which Django settings module"), "{err}");
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
