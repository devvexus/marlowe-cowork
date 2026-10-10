//! The per-repo tags index (design 3.3 L1): tree-sitter definitions and references for every
//! source file under a root, in `<home>\index\<repo-hash>.db`, keyed by (path, blake3) so a
//! refresh re-tags only files whose content changed.
//!
//! Files are found through the [`Workspace`] port (so a container workspace indexes the same way),
//! honouring `.gitignore` files and `.git/info/exclude`, skipping build and VCS directories and
//! files over [`MAX_FILE_BYTES`]. A refresh stats every file and re-reads only those whose size or
//! mtime moved; tagging runs on a few worker threads. [`Index::update_file`] re-tags one file
//! after an edit (A8: <= 50 ms).

pub mod lang;
pub mod tags;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use marlowe_os::workspace::{WalkOpts, Workspace};
use rusqlite::{params, Connection, OptionalExtension};

pub use lang::Lang;
pub use tags::RawTag;

/// Files larger than this are generated or vendored; they are not tagged.
pub const MAX_FILE_BYTES: u64 = 1_000_000;

/// Directories never descended into.
pub const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "target", "node_modules", ".venv", "venv", "__pycache__", ".mypy_cache",
    ".pytest_cache", ".ruff_cache", ".tox", ".next", ".turbo", "dist", "build", ".gradle", ".idea", ".vs",
];

/// Most entries one index walks.
pub const MAX_FILES: usize = 400_000;

const SCHEMA: i64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// One tag as stored, with its file. Lines and columns are 1-based; `col` counts bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Absolute path.
    pub path: PathBuf,
    pub name: String,
    pub kind: String,
    pub is_def: bool,
    pub line: u32,
    pub col: u32,
    /// The definition's last line (1-based); a reference's own line.
    pub end_line: u32,
    pub container: Option<String>,
    pub sig: String,
}

impl Symbol {
    /// `Container::name` when it has a container.
    pub fn qualified(&self) -> String {
        match &self.container {
            Some(c) => format!("{c}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// What one refresh did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Source files under the root.
    pub files: usize,
    /// Files (re)tagged.
    pub tagged: usize,
    /// Files dropped (deleted or now ignored).
    pub removed: usize,
    /// Lines in the files tagged.
    pub lines: usize,
    pub elapsed: Duration,
}

/// The index of one root.
pub struct Index {
    root: PathBuf,
    conn: Mutex<Connection>,
    last_refresh: Mutex<Option<Instant>>,
    /// Tagging threads for a refresh.
    workers: usize,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("root", &self.root).finish_non_exhaustive()
    }
}

/// `<home>\index\<repo-hash>.db`: the first 16 hex digits of blake3 over the root's normalised
/// path.
pub fn db_path(home: &Path, root: &Path) -> PathBuf {
    let key = norm_key(root);
    let hash = blake3::hash(key.as_bytes()).to_hex();
    home.join("index").join(format!("{}.db", &hash[..16]))
}

/// A path as a comparison key: `/` separators, no trailing `/`, lower case on Windows (where paths
/// are case-insensitive), no `\\?\` prefix.
pub fn norm_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

impl Index {
    /// Opens (creating) the on-disk index for `root` at `db`.
    pub fn open(db: &Path, root: &Path) -> Result<Self> {
        if let Some(dir) = db.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::with_conn(conn, root)
    }

    /// An index that lives only in memory (crew sandboxes, tests, hosts without a profile home).
    pub fn in_memory(root: &Path) -> Result<Self> {
        Self::with_conn(Connection::open_in_memory()?, root)
    }

    fn with_conn(conn: Connection, root: &Path) -> Result<Self> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA {
            conn.execute_batch("DROP TABLE IF EXISTS tags; DROP TABLE IF EXISTS files;")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files(
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                lang TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                hash BLOB NOT NULL,
                lines INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tags(
                file INTEGER NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                def INTEGER NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                container TEXT,
                sig TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS tags_name ON tags(name);
             CREATE INDEX IF NOT EXISTS tags_file ON tags(file);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA)?;
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 8));
        Ok(Self { root: root.to_path_buf(), conn: Mutex::new(conn), last_refresh: Mutex::new(None), workers })
    }

    /// Tagging threads per refresh (default: half the logical CPUs, 1-8).
    pub fn with_workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The path stored for `abs`: relative to the root, `/`-separated. `None` outside the root.
    pub fn rel(&self, abs: &Path) -> Option<String> {
        let (a, r) = (norm_key(abs), norm_key(&self.root));
        if !(a.len() > r.len() && a.starts_with(&r) && a.as_bytes()[r.len()] == b'/') {
            return None;
        }
        // The original spelling of the tail (keys are lower-cased on Windows).
        let tail = abs.strip_prefix(&self.root).map(Path::to_path_buf).unwrap_or_else(|_| {
            let s = abs.to_string_lossy().replace('\\', "/");
            let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
            PathBuf::from(&s[s.len() - (a.len() - r.len() - 1)..])
        });
        let mut parts = Vec::new();
        for c in tail.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => return None,
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in rel.split('/') {
            p.push(part);
        }
        p
    }

    /// Whether a refresh ran within `max_age`.
    pub fn fresh_within(&self, max_age: Duration) -> bool {
        self.last_refresh.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|t| t.elapsed() <= max_age)
    }

    /// Brings the index up to date with the files under the root: tags new and changed files,
    /// drops deleted and ignored ones.
    pub fn refresh(&self, ws: &dyn Workspace) -> Result<RefreshStats> {
        let started = Instant::now();
        let files = self.source_files(ws)?;
        let known: HashMap<String, (i64, u64, i64, Vec<u8>)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare("SELECT path, id, size, mtime, hash FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?)))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        // Stat pass: what moved by size or mtime.
        let mut todo: Vec<Job> = Vec::new();
        let mut seen = HashSet::new();
        for (rel, abs) in &files {
            let Ok(meta) = ws.metadata(abs) else { continue };
            if !meta.is_file || meta.len > MAX_FILE_BYTES {
                continue;
            }
            seen.insert(rel.clone());
            let mtime = mtime_ns(meta.modified);
            match known.get(rel) {
                Some((_, size, m, _)) if *size == meta.len && *m == mtime => {}
                Some((_, _, _, hash)) => todo.push((rel.clone(), abs.clone(), meta.len, mtime, Some(hash.clone()))),
                None => todo.push((rel.clone(), abs.clone(), meta.len, mtime, None)),
            }
        }

        // Tag pass, in parallel: each worker reads and tags its share.
        let workers = self.workers.min(todo.len()).max(1);
        let chunk = todo.len().div_ceil(workers).max(1);
        let results: Vec<Tagged> = std::thread::scope(|s| {
            let handles: Vec<_> = todo
                .chunks(chunk)
                .map(|part| s.spawn(move || part.iter().filter_map(|t| tag_file(ws, t)).collect::<Vec<_>>()))
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });

        let mut stats = RefreshStats { files: seen.len(), ..RefreshStats::default() };
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for t in &results {
            match t {
                Tagged::Touch { rel, mtime } => {
                    tx.execute("UPDATE files SET mtime = ?1 WHERE path = ?2", params![mtime, rel])?;
                }
                Tagged::Tags(f) => {
                    store(&tx, f)?;
                    stats.tagged += 1;
                    stats.lines += f.lines;
                }
            }
        }
        for (rel, (id, ..)) in &known {
            if !seen.contains(rel) {
                tx.execute("DELETE FROM tags WHERE file = ?1", params![id])?;
                tx.execute("DELETE FROM files WHERE id = ?1", params![id])?;
                stats.removed += 1;
            }
        }
        tx.commit()?;
        drop(conn);
        *self.last_refresh.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// Re-tags one file after an edit (or drops it when it is gone or no longer a source file).
    /// Returns whether its stored tags changed.
    pub fn update_file(&self, ws: &dyn Workspace, abs: &Path) -> Result<bool> {
        let Some(rel) = self.rel(abs) else { return Ok(false) };
        let meta = ws.metadata(abs).ok().filter(|m| m.is_file && m.len <= MAX_FILE_BYTES);
        let (Some(meta), Some(_)) = (meta, Lang::of(abs)) else {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let id: Option<i64> =
                tx.query_row("SELECT id FROM files WHERE path = ?1", [&rel], |r| r.get(0)).optional()?;
            if let Some(id) = id {
                tx.execute("DELETE FROM tags WHERE file = ?1", params![id])?;
                tx.execute("DELETE FROM files WHERE id = ?1", params![id])?;
            }
            tx.commit()?;
            return Ok(id.is_some());
        };
        let old: Option<Vec<u8>> =
            self.conn().query_row("SELECT hash FROM files WHERE path = ?1", [&rel], |r| r.get(0)).optional()?;
        let job = (rel, abs.to_path_buf(), meta.len, mtime_ns(meta.modified), old);
        let Some(t) = tag_file(ws, &job) else { return Ok(false) };
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = match &t {
            Tagged::Touch { rel, mtime } => {
                tx.execute("UPDATE files SET mtime = ?1 WHERE path = ?2", params![mtime, rel])?;
                false
            }
            Tagged::Tags(f) => {
                store(&tx, f)?;
                true
            }
        };
        tx.commit()?;
        Ok(changed)
    }

    /// Every source file under the root that is not ignored, as (relative, absolute).
    fn source_files(&self, ws: &dyn Workspace) -> Result<Vec<(String, PathBuf)>> {
        let opts = WalkOpts {
            max_depth: None,
            max_entries: Some(MAX_FILES),
            skip_dirs: SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
        };
        let entries = ws.walk(&self.root, &opts)?;
        let ignores = Ignores::load(ws, &self.root, entries.iter().filter(|e| !e.is_dir).map(|e| e.path.as_path()));
        let mut out = Vec::new();
        for e in entries {
            if e.is_dir || Lang::of(&e.path).is_none() || ignores.ignored(&e.path) {
                continue;
            }
            if let Some(rel) = self.rel(&e.path) {
                out.push((rel, e.path));
            }
        }
        Ok(out)
    }

    fn query(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<Symbol> {
        let rows = self.query_rows(sql, params);
        rows.into_iter()
            .map(|(rel, mut s)| {
                s.path = self.abs(&rel);
                s
            })
            .collect()
    }

    fn query_rows(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<(String, Symbol)> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(sql) else { return Vec::new() };
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                Symbol {
                    path: PathBuf::new(),
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    is_def: r.get::<_, i64>(3)? != 0,
                    line: r.get::<_, i64>(4)? as u32 + 1,
                    col: r.get::<_, i64>(5)? as u32 + 1,
                    end_line: r.get::<_, i64>(6)? as u32 + 1,
                    container: r.get(7)?,
                    sig: r.get(8)?,
                },
            ))
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    const COLS: &'static str =
        "SELECT f.path, t.name, t.kind, t.def, t.line, t.col, t.end_line, t.container, t.sig FROM tags t JOIN files f ON f.id = t.file";

    /// Definitions named exactly `name`.
    pub fn defs(&self, name: &str) -> Vec<Symbol> {
        self.query(&format!("{} WHERE t.name = ?1 AND t.def = 1 ORDER BY f.path, t.line", Self::COLS), &[&name])
    }

    /// References to `name` the tagger saw (calls, type uses, impls), up to `limit`.
    pub fn refs(&self, name: &str, limit: usize) -> Vec<Symbol> {
        self.query(
            &format!("{} WHERE t.name = ?1 AND t.def = 0 ORDER BY f.path, t.line LIMIT ?2", Self::COLS),
            &[&name, &(limit as i64)],
        )
    }

    /// Definitions whose name contains `query` (case-insensitive), best first: exact, then
    /// prefix, then substring; shorter names and paths first within a tier.
    pub fn search(&self, query: &str, kind: Option<&str>, limit: usize) -> Vec<Symbol> {
        self.search_in(query, kind, None, limit)
    }

    /// [`Self::search`] over the files under folder `under` (absolute) only; the folder is part
    /// of the query, so the limit counts only its definitions. A folder outside the root has none.
    pub fn search_in(&self, query: &str, kind: Option<&str>, under: Option<&Path>, limit: usize) -> Vec<Symbol> {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{}%", esc(query));
        let folder = match under {
            Some(d) if norm_key(d) != norm_key(&self.root) => match self.rel(d) {
                Some(rel) => Some(format!("{}/%", esc(&rel))),
                None => return Vec::new(),
            },
            _ => None,
        };
        let mut hits = match &folder {
            None => self.query(&format!("{} WHERE t.def = 1 AND t.name LIKE ?1 ESCAPE '\\' LIMIT 20000", Self::COLS), &[&pattern]),
            Some(f) => self.query(
                &format!("{} WHERE t.def = 1 AND t.name LIKE ?1 ESCAPE '\\' AND f.path LIKE ?2 ESCAPE '\\' LIMIT 20000", Self::COLS),
                &[&pattern, f],
            ),
        };
        if let Some(k) = kind {
            hits.retain(|s| s.kind.eq_ignore_ascii_case(k));
        }
        let q = query.to_lowercase();
        hits.sort_by_cached_key(|s| {
            let n = s.name.to_lowercase();
            let tier = if s.name == query {
                0
            } else if n == q {
                1
            } else if n.starts_with(&q) {
                2
            } else {
                3
            };
            (tier, s.name.len(), s.path.as_os_str().len(), s.path.clone(), s.line)
        });
        hits.truncate(limit);
        hits
    }

    /// The tags of one file, in line order.
    pub fn file_tags(&self, abs: &Path, defs_only: bool) -> Vec<Symbol> {
        let Some(rel) = self.rel(abs) else { return Vec::new() };
        let filter = if defs_only { " AND t.def = 1" } else { "" };
        self.query(&format!("{} WHERE f.path = ?1{filter} ORDER BY t.line, t.col", Self::COLS), &[&rel])
    }

    /// Whether `abs` is indexed.
    pub fn contains(&self, abs: &Path) -> bool {
        let Some(rel) = self.rel(abs) else { return false };
        self.conn().query_row("SELECT 1 FROM files WHERE path = ?1", [&rel], |_| Ok(())).optional().ok().flatten().is_some()
    }

    /// (files, lines, definitions, references) in the index.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let conn = self.conn();
        let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0)).ok().flatten().unwrap_or(0) as usize;
        (
            one("SELECT COUNT(*) FROM files"),
            one("SELECT SUM(lines) FROM files"),
            one("SELECT COUNT(*) FROM tags WHERE def = 1"),
            one("SELECT COUNT(*) FROM tags WHERE def = 0"),
        )
    }

    /// Every definition, and every (file, name, count) reference, for the repo map.
    pub fn graph(&self) -> (Vec<Symbol>, Vec<(PathBuf, String, u32)>) {
        let defs = self.query(&format!("{} WHERE t.def = 1", Self::COLS), &[]);
        let conn = self.conn();
        let refs = conn
            .prepare(
                "SELECT f.path, t.name, COUNT(*) FROM tags t JOIN files f ON f.id = t.file WHERE t.def = 0 \
                 GROUP BY f.path, t.name",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u32)))?
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        drop(conn);
        (defs, refs.into_iter().map(|(p, n, c)| (self.abs(&p), n, c)).collect())
    }
}

/// (relative path, absolute path, size, mtime, stored hash).
type Job = (String, PathBuf, u64, i64, Option<Vec<u8>>);

/// One file's refresh result.
enum Tagged {
    /// Same content, new mtime.
    Touch { rel: String, mtime: i64 },
    Tags(FileTags),
}

struct FileTags {
    rel: String,
    lang: Lang,
    size: u64,
    mtime: i64,
    hash: Vec<u8>,
    lines: usize,
    tags: Vec<RawTag>,
}

fn tag_file(ws: &dyn Workspace, job: &Job) -> Option<Tagged> {
    let (rel, abs, _size, mtime, old_hash) = job;
    let lang = Lang::of(abs)?;
    let bytes = ws.read(abs).ok()?;
    let hash = blake3::hash(&bytes).as_bytes().to_vec();
    if old_hash.as_deref() == Some(hash.as_slice()) {
        return Some(Tagged::Touch { rel: rel.clone(), mtime: *mtime });
    }
    let tags = tags::tag(lang, &bytes).unwrap_or_default();
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    Some(Tagged::Tags(FileTags { rel: rel.clone(), lang, size: bytes.len() as u64, mtime: *mtime, hash, lines, tags }))
}

fn store(tx: &rusqlite::Transaction<'_>, f: &FileTags) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO files(path, lang, size, mtime, hash, lines) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET lang = ?2, size = ?3, mtime = ?4, hash = ?5, lines = ?6",
        params![f.rel, f.lang.name(), f.size as i64, f.mtime, f.hash, f.lines as i64],
    )?;
    let id: i64 = tx.query_row("SELECT id FROM files WHERE path = ?1", [&f.rel], |r| r.get(0))?;
    tx.execute("DELETE FROM tags WHERE file = ?1", params![id])?;
    let mut ins = tx.prepare_cached(
        "INSERT INTO tags(file, name, kind, def, line, col, end_line, container, sig) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for t in &f.tags {
        ins.execute(params![
            id,
            t.name,
            t.kind,
            t.is_def as i64,
            t.line as i64,
            t.col as i64,
            t.end_line.max(t.line) as i64,
            t.container,
            t.sig
        ])?;
    }
    Ok(())
}

fn mtime_ns(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

/// `.gitignore` files under the root plus `.git/info/exclude`, each applying to its subtree; the
/// deepest one with an opinion decides.
struct Ignores {
    /// (directory key, matcher), shallowest first.
    sets: Vec<(String, Gitignore)>,
}

impl Ignores {
    fn load<'a>(ws: &dyn Workspace, root: &Path, files: impl Iterator<Item = &'a Path>) -> Self {
        let mut sets = Vec::new();
        let mut add = |dir: &Path, file: &Path| {
            let Ok(bytes) = ws.read(file) else { return };
            let mut b = GitignoreBuilder::new(dir);
            for line in String::from_utf8_lossy(&bytes).lines() {
                let _ = b.add_line(Some(file.to_path_buf()), line);
            }
            if let Ok(g) = b.build() {
                sets.push((norm_key(dir), g));
            }
        };
        add(root, &root.join(".git").join("info").join("exclude"));
        for f in files {
            if f.file_name().is_some_and(|n| n == ".gitignore") {
                if let Some(dir) = f.parent() {
                    add(dir, f);
                }
            }
        }
        sets.sort_by_key(|(k, _)| k.len());
        Self { sets }
    }

    fn ignored(&self, path: &Path) -> bool {
        let key = norm_key(path);
        for (dir, g) in self.sets.iter().rev() {
            if !(key.starts_with(dir.as_str()) && key.as_bytes().get(dir.len()) == Some(&b'/')) {
                continue;
            }
            // `ignore` asserts the path is under the matcher's root as spelled.
            if path.strip_prefix(g.path()).is_err() {
                continue;
            }
            let m = g.matched_path_or_any_parents(path, false);
            if m.is_ignore() {
                return true;
            }
            if m.is_whitelist() {
                return false;
            }
        }
        false
    }
}
