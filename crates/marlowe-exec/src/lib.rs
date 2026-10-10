//! The built-in tools behind [`marlowe_agent::ToolHost`]: read, write, edit, glob, grep, bash,
//! powershell, python, web_fetch, arxiv, paper and (with an injected backend) web_search.
//!
//! The host is built per session with that session's workspace root. Relative paths resolve
//! against the root; reads are free anywhere, writes are free only inside the root, and a root
//! that is too broad (a drive root, the home directory, a system or app-data directory) frees
//! nothing. Shell tools always ask. Once untrusted content is in the conversation (a fetched
//! page, search results, a file from outside the root), web_search and web_fetch to a host not
//! yet approved this session ask too. Every child process has the credential variables from
//! [`marlowe_brains::scrubbed_env`] removed from its environment and runs in its own
//! kill-on-close Job Object, so a timeout or interrupt kills the whole tree.

#![forbid(unsafe_code)]

pub mod corpus;
mod fs_tools;
mod paper;
pub mod paths;
pub mod render;
pub mod research;
pub mod shell;
mod specs;
pub mod text;
mod web;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_model::ToolSpec;
use serde_json::Value;

pub use shell::{run_bounded, run_script, Interpreter, Leftovers, ShellLimits, ShellRun, SpawnOpts};
pub use research::{ArxivEntry, ArxivFeed, HttpGet, HttpResponse, RateLimiter, ResearchConfig};
pub use web::{SearchBackend, SearchResponse, SearchResult};

/// Window assumed when the host is not told the model's; "large file" is measured against it.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

pub struct BuiltinTools {
    root: Option<PathBuf>,
    /// Writes inside `root` are free; false when there is no root or it is too broad.
    free_writes: bool,
    /// More folders whose files are the user's own: reading them does not taint.
    read_roots: Vec<PathBuf>,
    context_tokens: u32,
    /// Serialises write/edit read-modify-write cycles within a parallel batch.
    writes: Mutex<()>,
    search: Option<Arc<dyn SearchBackend>>,
    research: ResearchConfig,
    limits: ShellLimits,
    scrub: Vec<String>,
    /// Set on every child after the scrub (the crew's no-push `GIT_CONFIG_*` entries).
    child_env: Vec<(String, String)>,
    /// Pins shell children to this directory tree (a crew sandbox); `cwd` must stay inside it.
    shell_dir: Option<PathBuf>,
    leftovers: shell::Leftovers,
    held: shell::HeldJobs,
}

impl BuiltinTools {
    /// A host rooted at `workspace` (`None`: relative paths use the process cwd and every write
    /// asks). A root that [`paths::is_broad_root`] refuses also makes every write ask.
    pub fn new(workspace: Option<PathBuf>) -> Self {
        let free_writes = workspace.as_deref().is_some_and(|r| !paths::is_broad_root(r));
        Self {
            root: workspace,
            free_writes,
            read_roots: Vec::new(),
            context_tokens: DEFAULT_CONTEXT_TOKENS,
            writes: Mutex::new(()),
            search: None,
            research: ResearchConfig::default(),
            limits: ShellLimits::production(),
            scrub: marlowe_brains::scrubbed_env().iter().map(|s| s.to_string()).collect(),
            child_env: Vec::new(),
            shell_dir: None,
            leftovers: shell::Leftovers::Release,
            held: shell::HeldJobs::default(),
        }
    }

    /// A host rooted at a crew sandbox the caller created for this purpose (under the profile's
    /// `worktrees` or `scratch` folder). Writes inside it are free and reads inside it do not
    /// taint, even though the profile lives under app data, which [`paths::is_broad_root`] refuses.
    pub fn new_sandbox(root: PathBuf) -> Self {
        let mut tools = Self::new(Some(root));
        tools.free_writes = true;
        tools
    }

    /// Reads inside `dir` (the user's own workspace) do not taint. A broad `dir` is ignored.
    pub fn with_read_root(mut self, dir: PathBuf) -> Self {
        if !paths::is_broad_root(&dir) {
            self.read_roots.push(dir);
        }
        self
    }

    /// The model's context window in tokens.
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        if tokens > 0 {
            self.context_tokens = tokens;
        }
        self
    }

    /// Enables the `web_search` tool.
    pub fn with_search(mut self, backend: Arc<dyn SearchBackend>) -> Self {
        self.search = Some(backend);
        self
    }

    /// Cache directory, rate limiter and HTTP port for `arxiv` and `paper`.
    pub fn with_research(mut self, cfg: ResearchConfig) -> Self {
        self.research = cfg;
        self
    }

    /// Overrides shell bounds (tests use short ones).
    pub fn with_shell_limits(mut self, limits: ShellLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Variables set on every child process, after the credential scrub.
    pub fn with_child_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.child_env.extend(env);
        self
    }

    /// Shell children start in `dir` instead of the workspace root, and a `cwd` argument must
    /// resolve inside it.
    pub fn with_shell_dir(mut self, dir: PathBuf) -> Self {
        self.shell_dir = Some(dir);
        self
    }

    /// What happens to background processes a finished command left running (default: they
    /// run on). [`shell::Leftovers::KillWithHost`] kills them when this host is dropped.
    pub fn with_leftovers(mut self, mode: shell::Leftovers) -> Self {
        self.leftovers = mode;
        self
    }

    /// Kills background processes held under [`shell::Leftovers::KillWithHost`].
    pub fn kill_leftovers(&self) {
        self.held.kill_all();
    }

    /// Background processes still held under [`shell::Leftovers::KillWithHost`].
    pub fn leftover_processes(&self) -> u32 {
        self.held.active()
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn root_for(&self, ctx: &ToolCtx) -> Option<PathBuf> {
        self.root.clone().or_else(|| ctx.workspace.clone())
    }

    fn scrub(&self, cmd: &mut std::process::Command) {
        for name in &self.scrub {
            cmd.env_remove(name);
        }
    }

    fn run_shell(&self, kind: Interpreter, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        // Where relative script paths and `cwd` resolve: the pinned shell dir, else the root.
        let root = self.shell_dir.clone().or_else(|| self.root_for(ctx));
        let built = match kind {
            Interpreter::Bash | Interpreter::PowerShell => {
                let Some(script) = str_arg(args, "command").filter(|c| !c.trim().is_empty()) else {
                    return ToolOutcome::error("`command` is required");
                };
                if kind == Interpreter::Bash {
                    shell::bash_command(script)
                } else {
                    Ok(shell::powershell_command(script))
                }
            }
            Interpreter::Python => {
                let extra: Vec<String> = args
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                match (str_arg(args, "code"), str_arg(args, "path")) {
                    (Some(code), None) => shell::python_command(shell::PythonTarget::Code(code), &extra),
                    (None, Some(path)) => {
                        let abs = paths::resolve(root.as_deref(), path);
                        shell::python_command(
                            shell::PythonTarget::Script(&abs.to_string_lossy()),
                            &extra,
                        )
                    }
                    _ => return ToolOutcome::error("give exactly one of `code` or `path`"),
                }
            }
        };
        let mut cmd = match built {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(e.to_string()),
        };
        let dir = match str_arg(args, "cwd") {
            Some(c) => paths::resolve(root.as_deref(), c),
            None => root.clone().unwrap_or_else(|| paths::resolve(None, ".")),
        };
        if !dir.is_dir() {
            return ToolOutcome::error(format!("cwd {} is not a directory", dir.display()));
        }
        if let Some(pin) = self.shell_dir.as_deref().filter(|p| !paths::inside(p, &dir)) {
            return ToolOutcome::error(format!(
                "cwd {} is outside the sandbox {}; commands run inside it",
                dir.display(),
                pin.display()
            ));
        }
        cmd.current_dir(&dir);
        self.scrub(&mut cmd);
        cmd.envs(self.child_env.iter().map(|(k, v)| (k, v)));
        let mut limits = self.limits;
        if let Some(ms) = int_arg(args, "timeout_ms").filter(|m| *m > 0) {
            limits = limits.with_timeout(ms as u64);
        }
        match run_bounded(cmd, limits, Some(&ctx.cancel)) {
            Err(e) => ToolOutcome::error(format!("could not start {}: {e}", kind.tool_name())),
            Ok(mut run) => {
                self.held.settle(&mut run, self.leftovers);
                shell_outcome(run, limits)
            }
        }
    }

    /// Whether `raw` lies inside a root that frees writes.
    fn inside_root(&self, raw: &str) -> bool {
        match &self.root {
            Some(root) if self.free_writes => paths::inside(root, &paths::resolve(Some(root), raw)),
            _ => false,
        }
    }

    /// Whether reading `raw` stays within the user's own files: the write root or a read root.
    fn trusted_read(&self, raw: &str) -> bool {
        if self.inside_root(raw) {
            return true;
        }
        let abs = paths::resolve(self.root.as_deref(), raw);
        self.read_roots.iter().any(|r| paths::inside(r, &abs))
    }
}

fn shell_outcome(run: ShellRun, limits: ShellLimits) -> ToolOutcome {
    let mut text = if run.text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        text::head_and_tail(&run.text, shell::MAX_SHELL_RESULT_BYTES)
    };
    let summary;
    if run.cancelled {
        text.push_str("\n[the command was stopped because the turn was interrupted.]");
        summary = "interrupted".to_string();
    } else if run.timed_out {
        text.push_str(&format!(
            "\n[the harness stopped this command after {} ms. The output above is what it had \
             produced by then; the command did not finish.]",
            limits.timeout_ms
        ));
        summary = "timed out".to_string();
    } else if run.flooded {
        text.push_str(&format!(
            "\n[the harness stopped this command after it produced more than {} bytes on one \
             stream. The output above is a prefix; the command did not finish.]",
            limits.max_output_bytes
        ));
        summary = "output cap".to_string();
    } else if run.code != 0 {
        text.push_str(&format!("\n[exit {}]", run.code));
        summary = format!("exit {}", run.code);
    } else {
        summary = format!("{} lines", run.text.lines().count());
    }
    let failed = run.code != 0 || run.timed_out || run.flooded || run.cancelled;
    ToolOutcome { content: text, is_error: failed, summary }
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The `paper` tool's input, under any of its accepted names.
fn research_input(args: &Value) -> &str {
    str_arg(args, "url_or_arxiv_id").or_else(|| str_arg(args, "url")).or_else(|| str_arg(args, "id")).unwrap_or("")
}

pub(crate) fn int_arg(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// ` (+N lines)` when `s` has lines beyond its first, so a one-line summary never hides a script.
fn more_lines(s: &str) -> String {
    match s.trim().lines().count().saturating_sub(1) {
        0 => String::new(),
        1 => " (+1 line)".into(),
        n => format!(" (+{n} lines)"),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let cut = text::floor_boundary(line, max);
    if cut < line.len() {
        format!("{}…", &line[..cut])
    } else {
        line.to_string()
    }
}

impl ToolHost for BuiltinTools {
    fn specs(&self) -> Vec<ToolSpec> {
        specs::all(self.search.is_some())
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if ctx.cancel.is_cancelled() {
            return ToolOutcome::error("interrupted before the tool ran");
        }
        let args = &call.args;
        if !args.is_object() {
            return ToolOutcome::error(format!(
                "`{}` expects a JSON object of arguments, got: {}",
                call.name,
                first_line(&args.to_string(), 80)
            ));
        }
        let root = self.root_for(ctx);
        let fs = fs_tools::Fs { tools: self, root: root.as_deref() };
        match call.name.as_str() {
            "read" => fs.read(args),
            "write" => fs.write(args),
            "edit" => fs.edit(args),
            "glob" => fs.glob(args),
            "grep" => fs.grep(args),
            "bash" => self.run_shell(Interpreter::Bash, args, ctx),
            "powershell" => self.run_shell(Interpreter::PowerShell, args, ctx),
            "python" => self.run_shell(Interpreter::Python, args, ctx),
            "web_fetch" => web::web_fetch(args, &ctx.cancel),
            "arxiv" => research::arxiv(&self.research, args, &ctx.cancel),
            "paper" => research::paper(&self.research, args, &ctx.cancel),
            "web_search" => match &self.search {
                Some(b) => web::web_search(b.as_ref(), args, &ctx.cancel),
                None => ToolOutcome::error("web_search is not available in this build"),
            },
            other => ToolOutcome::error(format!("unknown tool `{other}`")),
        }
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        let args = &call.args;
        match call.name.as_str() {
            "write" | "edit" => match str_arg(args, "path") {
                Some(p) if self.inside_root(p) => Consequence::Free,
                Some(p) => Consequence::Ask {
                    summary: format!("{} {} (outside the workspace)", call.name, first_line(p, 100)),
                },
                None => Consequence::Free, // refused by `call` for the missing argument
            },
            "bash" | "powershell" => {
                let command = str_arg(args, "command").unwrap_or("");
                Consequence::Ask {
                    summary: format!("{}: {}{}", call.name, first_line(command, 100), more_lines(command)),
                }
            }
            "python" => Consequence::Ask {
                summary: match (str_arg(args, "code"), str_arg(args, "path")) {
                    (Some(code), _) => format!("python: {}{}", first_line(code, 100), more_lines(code)),
                    (None, Some(path)) => format!("python {}", first_line(path, 100)),
                    _ => "python".to_string(),
                },
            },
            _ => Consequence::Free,
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if exposure.tainted {
            let args = &call.args;
            match call.name.as_str() {
                "web_fetch" => {
                    if let Some(host) = self.egress_host(call).filter(|h| !exposure.approved_hosts.contains(h)) {
                        return Consequence::Ask {
                            summary: format!("web_fetch {host}: a new host, after untrusted content"),
                        };
                    }
                }
                "paper" => {
                    let input = research_input(args);
                    if let Some(host) = research::paper_host(input).filter(|h| !exposure.approved_hosts.contains(h)) {
                        return Consequence::Ask {
                            summary: format!("paper {host}: a new host, after untrusted content"),
                        };
                    }
                }
                "arxiv" => {
                    return Consequence::Ask {
                        summary: format!(
                            "arxiv: {} (after untrusted content)",
                            first_line(str_arg(args, "query").unwrap_or(""), 100)
                        ),
                    }
                }
                "web_search" => {
                    return Consequence::Ask {
                        summary: format!(
                            "web_search: {} (after untrusted content)",
                            first_line(str_arg(args, "query").unwrap_or(""), 100)
                        ),
                    }
                }
                _ => {}
            }
        }
        self.consequence(call)
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        match call.name.as_str() {
            "web_fetch" | "web_search" | "arxiv" | "paper" => true,
            "read" | "glob" | "grep" => !self.trusted_read(str_arg(&call.args, "path").unwrap_or(".")),
            _ => false,
        }
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if call.name == "paper" {
            return research::paper_host(research_input(&call.args));
        }
        if call.name != "web_fetch" {
            return None;
        }
        let url = str_arg(&call.args, "url")?;
        marlowe_net::Target::parse(url).ok().map(|t| t.host)
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        let args = &call.args;
        let key = match call.name.as_str() {
            "read" | "write" | "edit" => "path",
            "glob" => "pattern",
            "grep" => "pattern",
            "bash" | "powershell" => "command",
            "python" => "code",
            "web_fetch" => "url",
            "web_search" | "arxiv" => "query",
            "paper" => "url_or_arxiv_id",
            _ => "",
        };
        let detail = str_arg(args, key)
            .or_else(|| str_arg(args, "path"))
            .map(|s| first_line(s, 60))
            .unwrap_or_default();
        if detail.is_empty() {
            call.name.clone()
        } else {
            format!("{} {}", call.name, detail)
        }
    }
}
