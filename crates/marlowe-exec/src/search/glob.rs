//! `glob`: files by pattern, newest first (design 2.1 T10).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use marlowe_agent::ToolOutcome;
use serde_json::Value;

use super::walk::{collect, skipped_notice, Filters};
use super::{join_within, Page, SearchEnv, RESULT_BYTES};

const PARAMS: [&str; 4] = ["pattern", "path", "head_limit", "offset"];

pub fn glob(env: &SearchEnv, args: &Value) -> ToolOutcome {
    if let Some(bad) = unknown_param(args, &PARAMS, &PARAMS, "glob") {
        return bad;
    }
    let base = match env.base(args) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let page = match Page::parse(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if env.port.metadata(&base).is_ok_and(|m| m.is_file) {
        return ToolOutcome::error(
            "`path` is a file, and `glob` lists a DIRECTORY. Pass the directory that contains it — the \
             file you named is already the answer.",
        );
    }
    if env.port.metadata(&base).is_err() {
        return ToolOutcome::error(format!("`path` {} does not exist", env.shown(&base)));
    }
    let pattern = match args.get("pattern") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) if !p.trim().is_empty() => Some(p.trim()),
        Some(Value::String(_)) => None,
        Some(_) => return ToolOutcome::error("`pattern` must be a string"),
    };
    let filters = match pattern {
        Some(p) => match Filters::default().with_globs(&base, env.root, &absolute_to_relative(p, &base)) {
            Ok(f) => f,
            Err(e) => return ToolOutcome::error(e),
        },
        None => Filters::default(),
    };
    let walked = collect(env, &base, &filters);
    let mut found: Vec<(Option<SystemTime>, String)> = walked
        .files
        .iter()
        .map(|f| (env.port.metadata(f).ok().and_then(|m| m.modified), env.shown(f)))
        .collect();
    // Newest first, then by path, so the order is stable.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = found.len();
    let shown: Vec<String> = page.slice(&found).iter().map(|(_, p)| p.clone()).collect();
    let (mut body, fit) = join_within(&shown, RESULT_BYTES);
    if total == 0 {
        body = match pattern {
            Some(p) => format!(
                "no files matched `{p}` (searched {}). A pattern without `/` matches names at any depth \
                 (`*.rs`); one with `/` is relative to `path` (`src/*.rs`, `**/Cargo.toml`). Hidden \
                 files count; files ignored by .gitignore do not.",
                env.shown_dir(&base)
            ),
            None => format!("no files in {} (files ignored by .gitignore are not listed)", env.shown_dir(&base)),
        };
    } else if fit < shown.len() {
        body.push_str(&format!("\n[output cut at {RESULT_BYTES} bytes after {fit} paths; narrow `pattern` or use `offset`]"));
    } else if let Some(more) = page.more(shown.len(), total, false) {
        body.push_str(&format!("\n{more}"));
    }
    if walked.truncated {
        body.push_str(&format!(
            "\n[enumeration stopped at {} files; there may be more under this path than are listed]",
            super::walk::FILE_CAP
        ));
    }
    body.push_str(&skipped_notice(&walked.skipped));
    ToolOutcome::ok(body, format!("{total} paths"))
}

/// A pattern given as an absolute path under `base` becomes relative to it.
fn absolute_to_relative(pattern: &str, base: &Path) -> String {
    let p = pattern.replace('\\', "/");
    let b: PathBuf = base.to_path_buf();
    let b = b.to_string_lossy().replace('\\', "/");
    let b = b.trim_end_matches('/');
    match p.strip_prefix(b).and_then(|r| r.strip_prefix('/')) {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => p,
    }
}

/// The refusal for a parameter the tool does not take: the call is not run with it dropped.
pub(crate) fn unknown_param(args: &Value, allowed: &[&str], listed: &[&str], tool: &str) -> Option<ToolOutcome> {
    let obj = args.as_object()?;
    let bad = obj.keys().find(|k| !allowed.contains(&k.as_str()))?;
    Some(ToolOutcome::error(format!(
        "`{bad}` is not a parameter of `{tool}`, and the call was NOT run with it ignored. `{tool}` takes exactly \
         these: {}.",
        listed.join(", ")
    )))
}
