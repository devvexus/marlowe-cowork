//! The code group (track C): `symbols`, `definition`, `references`, `hover`, `diagnostics` and
//! `rename_symbol`, by *name*, backed by `marlowe-code` (`index/` tags, `lsp/` servers, `udiff/`
//! for rename diffs). Post-edit diagnostics are a [`ToolMiddleware`] ([`diag`]), not a tool.
//!
//! State is a [`CodeHost`] in `BuiltinTools::state`: unconfigured hosts (tests, crew sandboxes)
//! get an in-memory tags index and no language servers; [`configure`] gives a conductor host the
//! on-disk index under `<home>\index` and the language servers, both shared by every host of
//! that profile home in the process (so sessions on one workspace share one rust-analyzer, and
//! the manager's "2 live" limit is per process). Servers run only on the local workspace port.
//! The group's tools are offered only to hosts with a workspace root. tree-sitter stays behind
//! `cfg(not(target_arch = "aarch64"))` until 9.1-i; there the servers answer alone.

mod diag;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolGroup, ToolMiddleware,
    ToolOutcome,
};
use marlowe_code::index::norm_key;
use marlowe_code::lsp::intel::{self, Caller, CodeIntel, IntelError, Roots, Source, Target};
use marlowe_code::lsp::{Diag, LspManager};
use marlowe_code::repomap::{self, MapOptions};
use marlowe_model::ToolSpec;
use serde_json::{json, Value};

use super::{spec, ToolDef};
use crate::{int_arg, paths, str_arg, BuiltinTools};

pub use diag::DiagnosticsMiddleware;

/// Most lines of a rename's diff shown in the result.
const DIFF_LINES: usize = 200;

/// The code tools' state on one host (cheap to clone; shared by the hosts of one home).
#[derive(Clone)]
pub struct CodeHost {
    inner: Arc<Shared>,
}

struct Shared {
    /// Where on-disk indexes live (`None`: in memory).
    home: Option<PathBuf>,
    lsp: LspManager,
    roots: Roots,
}

impl Default for CodeHost {
    /// In-memory index, no language servers.
    fn default() -> Self {
        Self::new(None, LspManager::disabled())
    }
}

impl std::fmt::Debug for CodeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeHost").field("home", &self.inner.home).field("lsp", &self.inner.lsp).finish()
    }
}

impl CodeHost {
    pub fn new(home: Option<PathBuf>, lsp: LspManager) -> Self {
        Self { inner: Arc::new(Shared { home, lsp, roots: Roots::default() }) }
    }

    /// The host shared by every tool host of `home` in this process, with the language servers.
    pub fn shared(home: &Path) -> Self {
        static HOMES: OnceLock<Mutex<HashMap<String, CodeHost>>> = OnceLock::new();
        let mut homes = HOMES.get_or_init(Mutex::default).lock().unwrap_or_else(|p| p.into_inner());
        homes.entry(norm_key(home)).or_insert_with(|| CodeHost::new(Some(home.to_path_buf()), LspManager::new())).clone()
    }

    pub fn lsp(&self) -> &LspManager {
        &self.inner.lsp
    }

    /// The engine for `root` (its index opened on first use; in memory if the disk one fails).
    pub fn intel(&self, root: &Path) -> Arc<CodeIntel> {
        let home = self.inner.home.as_deref();
        self.inner
            .roots
            .get(root, || {
                let index = intel::open_index(home, root).or_else(|_| intel::open_index(None, root)).ok()?;
                Some(CodeIntel::new(root, index, self.inner.lsp.clone()))
            })
            .expect("an in-memory index always opens")
    }
}

pub fn tools() -> Vec<ToolDef> {
    let read = |spec: ToolSpec, call: super::CallFn, describe: super::DescribeFn| {
        ToolDef::new(spec, ToolGroup::Code, Effects::READ, call)
            .path_args(&["path"])
            .describe(describe)
            .taints(reads_outside)
            .enabled(has_root)
    };
    vec![
        read(symbols_spec(), symbols, |c| super::describe_by(c, "query")),
        read(definition_spec(), definition, |c| super::describe_by(c, "symbol")),
        read(references_spec(), references, |c| super::describe_by(c, "symbol")),
        read(hover_spec(), hover, |c| super::describe_by(c, "symbol")),
        read(diagnostics_spec(), diagnostics, |c| super::describe_by(c, "path")),
        ToolDef::new(rename_spec(), ToolGroup::Code, Effects::WRITE, rename_symbol)
            .path_args(&["path"])
            .consequence(rename_consequence)
            .crew_consequence(crew_rename_consequence)
            .describe(describe_rename)
            .mutation(|t, _, ctx| t.root_for_ref(ctx).map_or(Mutation::None, |r| Mutation::Under(r.to_path_buf())))
            .enabled(has_root),
    ]
}

fn has_root(t: &BuiltinTools) -> bool {
    t.workspace().is_some()
}

/// A `path` outside the user's folders brings untrusted text in. (A server's answer that lands
/// outside them is shown as its location and signature only; see [`Cx::shows`].)
fn reads_outside(t: &BuiltinTools, call: &ToolCallReq) -> bool {
    str_arg(&call.args, "path").is_some_and(|p| !p.trim().is_empty() && !t.trusted_read(p))
}

/// Free when writes in the workspace are free (a rename only ever edits inside it); else asks.
fn rename_consequence(t: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    let free = t.workspace().is_some_and(|r| t.inside_root(&r.join("x").to_string_lossy()));
    if free {
        Consequence::Free
    } else {
        Consequence::Ask { summary: format!("{} (edits files across the workspace)", describe_rename(call)) }
    }
}

fn crew_rename_consequence(_t: &BuiltinTools, call: &ToolCallReq, _e: &Exposure, sandbox: &dyn SandboxView) -> Consequence {
    if sandbox.allows_write(&sandbox.root().join("x")) {
        Consequence::Free
    } else {
        Consequence::Ask { summary: format!("{} (outside the sandbox)", describe_rename(call)) }
    }
}

fn describe_rename(call: &ToolCallReq) -> String {
    let sym = str_arg(&call.args, "symbol").unwrap_or("");
    let new = str_arg(&call.args, "new_name").unwrap_or("");
    crate::first_line(&format!("rename_symbol {sym} -> {new}"), 100)
}

/// What every code tool needs: the root, its engine, the port, the cancel check and the trust.
struct Cx<'a> {
    root: &'a Path,
    intel: Arc<CodeIntel>,
    tools: &'a BuiltinTools,
    ctx: &'a ToolCtx,
    trusted: bool,
}

/// Whether language servers for `root` may run what the workspace supplies (its TypeScript, build
/// scripts, proc macros): only for the user's free-write workspace in an untainted session.
pub(super) fn trusted_servers(tools: &BuiltinTools, root: &Path, tainted: bool) -> bool {
    !tainted && tools.inside_root(&root.join("x").to_string_lossy())
}

impl<'a> Cx<'a> {
    fn new(tools: &'a BuiltinTools, ctx: &'a ToolCtx) -> Result<Self, ToolOutcome> {
        let root = tools
            .root_for_ref(ctx)
            .ok_or_else(|| ToolOutcome::error("the code tools need a workspace folder; this session has none"))?;
        let intel = tools.state::<CodeHost>().intel(root);
        let trusted = trusted_servers(tools, root, ctx.tainted);
        Ok(Cx { root, intel, tools, ctx, trusted })
    }

    /// Runs `f` as this call's [`Caller`].
    fn with<R>(&self, f: impl FnOnce(&Caller) -> R) -> R {
        let cancelled = || self.ctx.cancel.is_cancelled();
        f(&Caller::new(&cancelled, self.trusted))
    }

    /// Whether text from `p` may be shown: it is in the user's folders. Elsewhere (a dependency
    /// the server pointed at) only its location and signature are, and the model is told to
    /// `read` it, so the read goes through `read`'s taint.
    fn shows(&self, p: &Path) -> bool {
        self.tools.trusted_read(&p.to_string_lossy())
    }

    fn ws(&self) -> &dyn marlowe_os::workspace::Workspace {
        self.tools.workspace_port()
    }

    fn path(&self, args: &Value) -> Option<PathBuf> {
        str_arg(args, "path").map(str::trim).filter(|p| !p.is_empty()).map(|p| paths::resolve(Some(self.root), p))
    }

    fn rel(&self, p: &Path) -> String {
        self.intel.rel(p)
    }
}

fn limit(args: &Value, default: usize, max: usize) -> usize {
    int_arg(args, "limit").map_or(default, |n| n.clamp(1, max as i64) as usize)
}

fn fail(e: IntelError) -> ToolOutcome {
    ToolOutcome::error(e.to_string())
}

fn source_note(s: Source) -> &'static str {
    match s {
        Source::Server => "language server",
        Source::Index => "tags index",
    }
}

fn kind_of(t: &Target) -> String {
    let kind = if t.kind.is_empty() { "symbol" } else { t.kind.as_str() };
    match &t.container {
        Some(c) => format!("{kind} in {c}"),
        None => kind.to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn symbols(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let query = str_arg(args, "query").unwrap_or("").trim();
    let path = c.path(args);
    if query.is_empty() && path.is_none() {
        return ToolOutcome::error("give `query` (part of a name) or `path` (a file's symbols)");
    }
    let kind = str_arg(args, "kind").map(str::trim).filter(|k| !k.is_empty());
    let n = limit(args, 50, 500);
    let (hits, source) = match c.with(|k| c.intel.symbols(c.ws(), query, path.as_deref(), kind, n, k)) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    if hits.is_empty() {
        let what = if query.is_empty() { "symbols".to_string() } else { format!("definitions matching `{query}`") };
        let place = path.as_deref().map(|p| format!(" in {}", c.rel(p))).unwrap_or_default();
        return ToolOutcome::ok(format!("No {what}{place}."), "0 symbols");
    }
    let mut out = format!("{} ({}):\n", plural(hits.len(), "symbol", "symbols"), source_note(source));
    for h in &hits {
        let _ = writeln!(out, "{}:{}  {}  [{}]", c.rel(&h.path), h.line, h.sig.trim_end_matches('{').trim(), kind_of(h));
    }
    if hits.len() == n {
        let _ = writeln!(out, "(first {n}; narrow `query`, give `kind` or raise `limit`)");
    }
    ToolOutcome::ok(out.trim_end().to_string(), plural(hits.len(), "symbol", "symbols"))
}

fn definition(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let Some(symbol) = str_arg(args, "symbol").map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("give `symbol`, e.g. `Engine::start` or `parse`");
    };
    let path = c.path(args);
    let kind = str_arg(args, "kind").map(str::trim).filter(|k| !k.is_empty());
    let (found, source) = match c.with(|k| c.intel.resolve(c.ws(), symbol, path.as_deref(), kind, k)) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let n = limit(args, 5, 20);
    let mut out = String::new();
    if found.len() > 1 {
        let _ = writeln!(out, "{} of `{symbol}` ({}):", plural(found.len(), "definition", "definitions"), source_note(source));
    }
    for target in found.iter().take(n) {
        let _ = writeln!(out, "\n{}  ({})  {}:{}", target.qualified(), kind_of(target), c.rel(&target.path), target.line);
        if !c.shows(&target.path) {
            let _ = writeln!(out, "{:>6}│ {}", target.line, target.sig.trim());
            let _ = writeln!(out, "      │ ... (outside your folders; `read` the file to see the body)");
            continue;
        }
        let body = c.intel.body(c.ws(), target);
        for (line, text) in &body {
            let _ = writeln!(out, "{line:>6}│ {text}");
        }
        if !body.is_empty() && (target.end_line.saturating_sub(target.line) as usize) >= body.len() {
            let _ = writeln!(out, "      │ ... (to line {}; `read` for the rest)", target.end_line);
        }
    }
    if found.len() > n {
        let _ = writeln!(out, "\n... {} more; narrow with `Container::name`, `path` or `kind`", found.len() - n);
    }
    ToolOutcome::ok(out.trim().to_string(), plural(found.len(), "definition", "definitions"))
}

fn references(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let Some(symbol) = str_arg(args, "symbol").map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("give `symbol`, e.g. `Engine::start`");
    };
    let n = limit(args, 100, 1000);
    let (target, locs, source) = match c.with(|k| c.intel.references(c.ws(), symbol, c.path(args).as_deref(), n, k)) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let files = {
        let mut f: Vec<String> = locs.iter().map(|l| norm_key(&l.path)).collect();
        f.dedup();
        f.len()
    };
    let mut out = format!(
        "{} to {} ({}:{}) in {} ({}):",
        plural(locs.len(), "reference", "references"),
        target.qualified(),
        c.rel(&target.path),
        target.line,
        plural(files, "file", "files"),
        source_note(source),
    );
    if source == Source::Index {
        out.push_str("\n(by name: no language server answered, so same-named symbols are not told apart)");
    }
    let mut last = String::new();
    for l in &locs {
        let rel = c.rel(&l.path);
        if rel != last {
            let _ = write!(out, "\n{rel}");
            last = rel;
        }
        if c.shows(&l.path) {
            let _ = write!(out, "\n{:>6}:{:<3}│ {}", l.line, l.col, l.text.trim());
        } else {
            let _ = write!(out, "\n{:>6}:{:<3}│ (outside your folders; `read` to see it)", l.line, l.col);
        }
    }
    if locs.len() == n {
        let _ = write!(out, "\n(first {n}; raise `limit` for more)");
    }
    ToolOutcome::ok(out, plural(locs.len(), "reference", "references"))
}

fn hover(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let Some(symbol) = str_arg(args, "symbol").map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("give `symbol`");
    };
    let found = c.with(|k| c.intel.hover(c.ws(), symbol, c.path(args).as_deref(), k));
    match found {
        Ok((target, _, _)) if !c.shows(&target.path) => ToolOutcome::ok(
            format!(
                "{}  {}:{}  (outside your folders)\n\n{}\n\n(`read` the file for its documentation)",
                target.qualified(),
                c.rel(&target.path),
                target.line,
                target.sig.trim()
            ),
            target.qualified(),
        ),
        Ok((target, text, source)) => ToolOutcome::ok(
            format!("{}  {}:{}  ({})\n\n{text}", target.qualified(), c.rel(&target.path), target.line, source_note(source)),
            target.qualified(),
        ),
        Err(e) => fail(e),
    }
}

/// The least severe level listed (1 error .. 4 hint); default errors and warnings.
fn severity_floor(args: &Value) -> u8 {
    match str_arg(args, "severity").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("error" | "errors") => 1,
        Some("info" | "information") => 3,
        Some("hint" | "hints" | "all") => 4,
        _ => 2,
    }
}

fn diag_line(d: &Diag) -> String {
    let code = d.code.as_deref().map(|c| format!("[{c}]")).unwrap_or_default();
    format!("{}:{} {}{code}: {}", d.line + 1, d.col + 1, d.severity_name(), d.message.lines().next().unwrap_or(""))
}

fn diagnostics(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let floor = severity_floor(args);
    let n = limit(args, 100, 1000);
    let path = c.path(args);
    let found = match c.with(|k| c.intel.diagnostics(c.ws(), path.as_deref(), k)) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let mut out = String::new();
    let (mut errors, mut warnings, mut shown) = (0, 0, 0);
    for (p, diags, _) in &found {
        let e = diags.iter().filter(|d| d.severity == 1).count();
        let w = diags.iter().filter(|d| d.severity == 2).count();
        errors += e;
        warnings += w;
        let keep: Vec<&Diag> = diags.iter().filter(|d| d.severity <= floor).collect();
        if keep.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}: {}, {}", c.rel(p), plural(e, "error", "errors"), plural(w, "warning", "warnings"));
        for d in keep {
            if shown == n {
                break;
            }
            let _ = writeln!(out, "  {}", diag_line(d));
            shown += 1;
        }
    }
    if out.is_empty() {
        let what = if floor == 1 { "errors" } else { "errors or warnings" };
        out = match &path {
            Some(p) => format!("No {what} in {}.", c.rel(p)),
            None => format!("No {what} in the files the language servers have checked."),
        };
    } else if shown == n {
        let _ = writeln!(out, "(first {n}; raise `limit` for more)");
    }
    ToolOutcome::ok(out.trim_end().to_string(), format!("{}, {}", plural(errors, "error", "errors"), plural(warnings, "warning", "warnings")))
}

fn rename_symbol(t: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let c = match Cx::new(t, &cx.tool) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let (Some(symbol), Some(new_name)) = (str_arg(args, "symbol"), str_arg(args, "new_name")) else {
        return ToolOutcome::error("give `symbol` and `new_name`");
    };
    let plan = match c.with(|k| c.intel.plan_rename(c.ws(), symbol, new_name, c.path(args).as_deref(), k)) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    if c.ctx.cancel.is_cancelled() {
        return ToolOutcome::error("interrupted before the rename was written; nothing changed");
    }
    let paths: Vec<PathBuf> = plan.files.iter().map(|f| f.path.clone()).collect();
    let snap = c.with(|k| c.intel.before_edit(c.ws(), &paths, k));
    if let Err(e) = c.intel.apply_rename(c.ws(), &plan) {
        return fail(e);
    }
    let mut out = format!(
        "Renamed {} to `{}`: {} in {}.\n",
        plan.target.qualified(),
        plan.new_name,
        plural(plan.edits(), "edit", "edits"),
        plural(plan.files.len(), "file", "files"),
    );
    let diff: String = plan.files.iter().map(|f| f.diff.as_str()).collect();
    let lines: Vec<&str> = diff.lines().collect();
    for l in lines.iter().take(DIFF_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > DIFF_LINES {
        let _ = writeln!(out, "... ({} more diff lines)", lines.len() - DIFF_LINES);
    }
    let after = c.with(|k| c.intel.after_edit(c.ws(), &paths, &snap, k));
    out.push_str(&diag::render(&after, |p| c.rel(p)));
    let summary = format!("{} in {}", plural(plan.edits(), "edit", "edits"), plural(plan.files.len(), "file", "files"));
    ToolOutcome::ok(out.trim_end().to_string(), summary)
}

/// The repo map for `root` (design 3.4), for the prompt assembler and `/init`: ranked
/// definitions, cut to the budget for `context_tokens`, leaning toward `focus` files and
/// `mentioned` names. Empty without a tags index.
pub fn repo_map(tools: &BuiltinTools, root: &Path, focus: Vec<PathBuf>, mentioned: Vec<String>, context_tokens: u32) -> String {
    let intel = tools.state::<CodeHost>().intel(root);
    let opts = MapOptions { budget_tokens: repomap::budget_for(context_tokens), focus, mentioned, under: None };
    intel.repo_map(tools.workspace_port(), &opts)
}

fn symbols_spec() -> ToolSpec {
    spec(
        "symbols",
        "Find definitions by name across the workspace (functions, types, methods, constants), or \
         list one file's definitions. Faster and more precise than grep for \"where is X defined\".",
        json!({
            "query": {"type": "string", "description": "Part of a name (case-insensitive); exact matches first."},
            "path": {"type": "string", "description": "A file: list its definitions. A folder: only definitions under it."},
            "kind": {"type": "string", "description": "Only this kind: function, method, class, interface, type, constant, module, macro."},
            "limit": {"type": "integer", "description": "Most results (default 50)."}
        }),
        &[],
    )
}

fn definition_spec() -> ToolSpec {
    spec(
        "definition",
        "Show where a symbol is defined, with its full body. Name it as `name`, `Type::method`, \
         `module::name` or `obj.method`; give `path` to resolve a name used in that file.",
        json!({
            "symbol": {"type": "string", "description": "The name, e.g. `parse`, `Engine::start`."},
            "path": {"type": "string", "description": "A file that defines or uses the symbol (disambiguates)."},
            "kind": {"type": "string", "description": "Only definitions of this kind."},
            "limit": {"type": "integer", "description": "Most definitions shown (default 5)."}
        }),
        &["symbol"],
    )
}

fn references_spec() -> ToolSpec {
    spec(
        "references",
        "Every place a symbol is used, from the language server (precise: same-named symbols are \
         told apart). The symbol must name one definition; ambiguous names list the candidates.",
        json!({
            "symbol": {"type": "string", "description": "The name, e.g. `Engine::start`."},
            "path": {"type": "string", "description": "A file that defines or uses the symbol (disambiguates)."},
            "limit": {"type": "integer", "description": "Most references (default 100)."}
        }),
        &["symbol"],
    )
}

fn hover_spec() -> ToolSpec {
    spec(
        "hover",
        "A symbol's type, signature and documentation, as the language server describes it.",
        json!({
            "symbol": {"type": "string", "description": "The name, e.g. `Engine::start`."},
            "path": {"type": "string", "description": "A file that defines or uses the symbol (disambiguates)."}
        }),
        &["symbol"],
    )
}

fn diagnostics_spec() -> ToolSpec {
    spec(
        "diagnostics",
        "Compiler and type-checker errors and warnings from the language server: for one file \
         (opened and checked now), or for every file the servers have checked. Edits already \
         report new errors on their own.",
        json!({
            "path": {"type": "string", "description": "The file to check."},
            "severity": {"type": "string", "description": "error, warning (default: errors and warnings), info or hint."},
            "limit": {"type": "integer", "description": "Most diagnostics listed (default 100)."}
        }),
        &[],
    )
}

fn rename_spec() -> ToolSpec {
    spec(
        "rename_symbol",
        "Rename a symbol everywhere it is used, through the language server, in one atomic change \
         across files. Returns the diff and any new errors. Prefer this to edits for renames.",
        json!({
            "symbol": {"type": "string", "description": "The current name, e.g. `Engine::start`."},
            "new_name": {"type": "string", "description": "The new identifier."},
            "path": {"type": "string", "description": "A file that defines or uses the symbol (disambiguates)."}
        }),
        &["symbol", "new_name"],
    )
}

/// Loop middleware for this group: diagnostics after edits, and re-tagging of edited files. Only
/// for hosts with a workspace on this machine.
pub fn middleware(tools: &BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>> {
    match tools.workspace() {
        Some(root) if tools.workspace_port().kind() == "local" => {
            vec![Arc::new(DiagnosticsMiddleware::for_host(tools, root))]
        }
        _ => Vec::new(),
    }
}

/// No persona block: the tool descriptions carry the guidance.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// A conductor host with a local workspace gets the shared on-disk index and language servers,
/// and its index starts warming in the background (the conductor never waits for it).
pub fn configure(tools: BuiltinTools, setup: &super::HostSetup<'_>) -> BuiltinTools {
    let Some(root) = setup.workspace.map(Path::to_path_buf) else { return tools };
    if tools.workspace_port().kind() != "local" {
        return tools;
    }
    let host = CodeHost::shared(setup.home);
    let intel = host.intel(&root);
    let _ = std::thread::Builder::new().name("code-index-warm".into()).spawn(move || {
        let _ = intel.ensure_index(&marlowe_os::workspace::LocalFs);
    });
    tools.with_state(host)
}

/// Project files that make a workspace "code" for a local model.
const PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "package.json",
    "tsconfig.json",
    "go.mod",
    "CMakeLists.txt",
    "Makefile",
];

/// Whether a local model gets this group in `root`: a project lives there.
pub fn detect(root: Option<&Path>) -> bool {
    root.is_some_and(|r| PROJECT_MARKERS.iter().any(|m| r.join(m).is_file()))
}
