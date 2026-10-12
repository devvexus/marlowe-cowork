//! The walker behind `glob` and `grep`: files under a base, rg's ignore rules, deterministic
//! results (the caller sorts).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use ignore::{WalkBuilder, WalkState};
use marlowe_os::workspace::WalkOpts;

use super::SearchEnv;

/// Files one walk enumerates before it stops and says so.
pub(crate) const FILE_CAP: usize = 200_000;

/// Version control, never searched.
const VCS_DIRS: [&str; 5] = [".git", ".hg", ".svn", ".bzr", ".jj"];

/// Build output and caches that are skipped unless named as the base (or already ignored, in
/// which case nothing is said).
const SKIP_DIRS: [&str; 5] = ["target", "node_modules", ".venv", "__pycache__", "dist"];

/// Which files a call wants, beyond ignore rules.
#[derive(Default, Clone)]
pub(crate) struct Filters {
    pub overrides: Option<Override>,
    pub types: Option<Types>,
}

impl Filters {
    /// Gitignore-style globs (`*.rs`, `src/**/*.ts`, `!vendor/**`, `*.{ts,tsx}`) anchored at
    /// `base`: a name-only glob matches anywhere beneath it, one with a `/` is relative to it. A
    /// glob that starts with the base's own path relative to `root` has that prefix removed, so
    /// `path: crates/x` with `glob: crates/x/**/*.rs` finds what the model meant.
    pub fn with_globs(mut self, base: &Path, root: Option<&Path>, spec: &str) -> Result<Self, String> {
        let mut b = OverrideBuilder::new(if base.is_file() { base.parent().unwrap_or(base) } else { base });
        let rel_base = root
            .and_then(|r| base.strip_prefix(r).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty());
        let mut any = false;
        for g in split_globs(spec) {
            let mut g = g.replace('\\', "/");
            let neg = g.starts_with('!');
            if neg {
                g.remove(0);
            }
            if let Some(rest) = g.strip_prefix("./") {
                g = rest.to_string();
            }
            if let Some(rb) = &rel_base {
                if let Some(rest) = g.strip_prefix(&format!("{rb}/")) {
                    g = rest.to_string();
                }
            }
            if g.is_empty() {
                continue;
            }
            let g = if neg { format!("!{g}") } else { g };
            b.add(&g).map_err(|e| format!("`{g}` is not a valid glob: {e}"))?;
            any = true;
        }
        if any {
            self.overrides = Some(b.build().map_err(|e| format!("glob: {e}"))?);
        }
        Ok(self)
    }

    /// `rg --type name`: one of ripgrep's file types (`rust`, `py`, `js`, `ts`, ...).
    pub fn with_type(mut self, name: &str) -> Result<Self, String> {
        let mut b = TypesBuilder::new();
        b.add_defaults();
        b.select(name);
        self.types = Some(b.build().map_err(|e| {
            format!(
                "`type` \"{name}\" is not a file type ripgrep knows ({e}). Common ones: rust, py, js, ts, go, c, cpp, \
                 java, md, json, yaml, toml, html, css, sh. `glob` takes patterns such as `*.rs` instead."
            )
        })?);
        Ok(self)
    }

    fn accepts(&self, path: &Path) -> bool {
        // With any whitelist glob, an override ignores every file no glob names.
        if self.overrides.as_ref().is_some_and(|o| o.matched(path, false).is_ignore()) {
            return false;
        }
        if let Some(t) = &self.types {
            return !t.matched(path, false).is_ignore();
        }
        true
    }
}

/// Splits a `glob` argument on whitespace and commas that are not inside `{...}`, so
/// `"*.rs *.toml"` and `"*.{ts,tsx}"` both work.
pub(crate) fn split_globs(spec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in spec.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            c if (c.is_whitespace() || c == ',') && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// What a walk found.
pub(crate) struct Walked {
    pub files: Vec<PathBuf>,
    /// Directory names skipped as build output (not VCS, not already ignored).
    pub skipped: Vec<String>,
    /// The cap stopped the walk: there may be more.
    pub truncated: bool,
}

/// Regular files under `base` (or `base` itself when it is a file) that pass `filters`.
pub(crate) fn collect(env: &SearchEnv, base: &Path, filters: &Filters) -> Walked {
    if env.is_local() {
        collect_local(base, filters)
    } else {
        collect_port(env, base, filters)
    }
}

fn collect_local(base: &Path, filters: &Filters) -> Walked {
    let mut b = WalkBuilder::new(base);
    b.hidden(false).follow_links(false).add_custom_ignore_filename(".rgignore");
    if let Some(o) = &filters.overrides {
        b.overrides(o.clone());
    }
    if let Some(t) = &filters.types {
        b.types(t.clone());
    }
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    b.threads(threads);
    let skipped = Arc::new(Mutex::new(BTreeSet::new()));
    {
        let skipped = Arc::clone(&skipped);
        b.filter_entry(move |e| {
            if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_dir()) {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if VCS_DIRS.iter().any(|d| name == *d) {
                return false;
            }
            if SKIP_DIRS.iter().any(|d| name == *d) {
                skipped.lock().unwrap_or_else(|p| p.into_inner()).insert(name.into_owned());
                return false;
            }
            true
        });
    }
    let found = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    struct Local {
        buf: Vec<PathBuf>,
        sink: Arc<Mutex<Vec<PathBuf>>>,
    }
    impl Drop for Local {
        fn drop(&mut self) {
            self.sink.lock().unwrap_or_else(|p| p.into_inner()).append(&mut self.buf);
        }
    }
    b.build_parallel().run(|| {
        let mut local = Local { buf: Vec::new(), sink: Arc::clone(&found) };
        let count = Arc::clone(&count);
        Box::new(move |res| {
            let Ok(entry) = res else { return WalkState::Continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if count.fetch_add(1, Ordering::Relaxed) >= FILE_CAP {
                return WalkState::Quit;
            }
            local.buf.push(entry.into_path());
            WalkState::Continue
        })
    });
    let mut files = std::mem::take(&mut *found.lock().unwrap_or_else(|p| p.into_inner()));
    files.sort();
    let truncated = count.load(Ordering::Relaxed) > FILE_CAP;
    let skipped = skipped.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect();
    Walked { files, skipped, truncated }
}

/// A container (or any non-local port): the port's own walk, the same globs and types, no ignore
/// files.
fn collect_port(env: &SearchEnv, base: &Path, filters: &Filters) -> Walked {
    let meta = env.port.metadata(base);
    if meta.as_ref().is_ok_and(|m| m.is_file) {
        return Walked { files: vec![base.to_path_buf()], skipped: Vec::new(), truncated: false };
    }
    let skip_dirs: Vec<String> = VCS_DIRS.iter().chain(SKIP_DIRS.iter()).map(|s| s.to_string()).collect();
    let opts = WalkOpts { max_depth: None, max_entries: Some(FILE_CAP + 1), skip_dirs };
    let entries = env.port.walk(base, &opts).unwrap_or_default();
    let truncated = entries.len() > FILE_CAP;
    let mut files: Vec<PathBuf> = entries.into_iter().filter(|e| !e.is_dir).map(|e| e.path).collect();
    files.retain(|p| filters.accepts(p));
    files.sort();
    Walked { files, skipped: Vec::new(), truncated }
}

/// The skip notice appended to a result, or nothing.
pub(crate) fn skipped_notice(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!(
        "\n[not searched, because they are build output or caches: {}. Name one as `path` to look inside it.]",
        skipped.join(", ")
    )
}
