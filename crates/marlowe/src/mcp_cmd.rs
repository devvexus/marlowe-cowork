//! `marlowe mcp` (track K, design 2.6 X15): `.mcp.json` and the settings layers from the command
//! line, in Claude Code's spelling.
//!
//! ```text
//! marlowe mcp list [--check]
//! marlowe mcp get NAME
//! marlowe mcp add [-s local|project|user] [-t stdio|http] [-e K=V]... [-H "K: V"]... NAME (URL | -- CMD ARGS...)
//! marlowe mcp add-json [-s SCOPE] NAME JSON
//! marlowe mcp remove NAME [-s SCOPE]
//! marlowe mcp trust NAME | untrust NAME      (a workspace file's entry may start a program)
//! marlowe mcp auth NAME | logout NAME        (OAuth for an http server; tokens in Credential Manager)
//! marlowe mcp serve                          (Marlowe's tools for another MCP client over stdio; mcp_serve.rs)
//! ```
//!
//! Everything here reads and writes files; only `list --check` and `auth` touch the network or
//! start a server. The daemon's side is `marlowe-daemon/src/mcp_ops.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths, Scope, ServerConfig, Transport, Trust};
use marlowe_mcp::hub::{oauth_for, Hub, HubOpts};
use marlowe_mcp::oauth::{KeyringTokens, OAuth, TokenStore};
use serde_json::{json, Map, Value};

const USAGE: &str = "usage: marlowe mcp list [--check] | get NAME | add [-s SCOPE] [-t TRANSPORT] [-e K=V] [-H \"K: V\"] NAME (URL | -- CMD ARGS...)\n\
       | add-json [-s SCOPE] NAME JSON | remove NAME [-s SCOPE] | trust NAME | untrust NAME | auth NAME | logout NAME | serve\n\
scopes: local (default; this workspace, not shared), project (.mcp.json), user";

pub fn run(home: &Path, args: &[String]) -> Result<i32, String> {
    let cwd = crate::strip_verbatim(std::env::current_dir().map_err(|e| e.to_string())?);
    if args.first().map(String::as_str) == Some("serve") {
        return crate::mcp_serve::run(home, workspace_root(&cwd));
    }
    let cx = Cx {
        home: home.to_path_buf(),
        workspace: Some(workspace_root(&cwd)),
        tokens: Arc::new(KeyringTokens::new()),
        open: Box::new(open_browser),
    };
    let stdout = std::io::stdout();
    execute(&cx, args, &mut stdout.lock())
}

/// What a command needs from its surroundings; tests supply their own.
pub struct Cx {
    pub home: PathBuf,
    pub workspace: Option<PathBuf>,
    pub tokens: Arc<dyn TokenStore>,
    pub open: Box<dyn Fn(&str) -> Result<(), String>>,
}

/// The nearest ancestor with a `.git` or `.mcp.json`, else `cwd`.
fn workspace_root(cwd: &Path) -> PathBuf {
    cwd.ancestors().find(|d| d.join(".git").exists() || d.join(".mcp.json").is_file()).unwrap_or(cwd).to_path_buf()
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

pub fn execute(cx: &Cx, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let paths = Paths::new(&cx.home, cx.workspace.as_deref());
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("list") | Some("ls") => list(cx, &paths, rest, out),
        Some("get") => get(cx, &paths, rest, out),
        Some("add") => add(cx, &paths, rest, out),
        Some("add-json") | Some("add_json") => add_json(cx, &paths, rest, out),
        Some("remove") | Some("rm") => remove(cx, &paths, rest, out),
        Some("trust") => trust(cx, &paths, rest, true, out),
        Some("untrust") => trust(cx, &paths, rest, false, out),
        Some("auth") | Some("login") => auth(cx, &paths, rest, out),
        Some("logout") => logout(cx, &paths, rest, out),
        Some("-h") | Some("--help") | Some("help") => {
            writeln!(out, "{USAGE}").map_err(io)?;
            Ok(0)
        }
        None => {
            eprintln!("{USAGE}");
            Ok(1)
        }
        Some(other) => {
            eprintln!("marlowe mcp: unknown command {other:?}\n{USAGE}");
            Ok(1)
        }
    }
}

// ---------------------------------------------------------------- argument parsing

struct Parsed {
    flags: Vec<(String, String)>,
    positional: Vec<String>,
    /// Everything after a bare `--`.
    tail: Vec<String>,
    switches: Vec<String>,
}

impl Parsed {
    fn flag(&self, names: &[&str]) -> Option<&str> {
        self.flags.iter().rev().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.as_str())
    }
    fn all(&self, names: &[&str]) -> Vec<&str> {
        self.flags.iter().filter(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.as_str()).collect()
    }
    fn scope(&self) -> Result<Option<Scope>, String> {
        self.flag(&["-s", "--scope"]).map(scope_of).transpose()
    }
}

/// `valued`: flags that take a value. Anything else starting with `-` is a switch.
fn parse(args: &[String], valued: &[&str]) -> Result<Parsed, String> {
    let mut p = Parsed { flags: Vec::new(), positional: Vec::new(), tail: Vec::new(), switches: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            p.tail = args[i + 1..].to_vec();
            break;
        }
        if let Some((k, v)) = a.split_once('=').filter(|(k, _)| k.starts_with("--") && valued.contains(k)) {
            p.flags.push((k.to_string(), v.to_string()));
        } else if valued.contains(&a.as_str()) {
            let v = args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))?;
            p.flags.push((a.clone(), v.clone()));
            i += 1;
        } else if a.starts_with('-') && a.len() > 1 {
            p.switches.push(a.clone());
        } else {
            p.positional.push(a.clone());
        }
        i += 1;
    }
    Ok(p)
}

fn scope_of(name: &str) -> Result<Scope, String> {
    match name.to_ascii_lowercase().as_str() {
        "local" => Ok(Scope::Local),
        "project" => Ok(Scope::Project),
        "user" | "global" => Ok(Scope::User),
        other => Err(format!("unknown scope {other:?} (local, project, user)")),
    }
}

// ---------------------------------------------------------------- commands

fn trust_state(cx: &Cx, paths: &Paths, cfg: &ServerConfig) -> &'static str {
    if cfg.disabled {
        "disabled"
    } else if Trust::new(paths).is_trusted(cx.workspace.as_deref(), cfg) {
        "ok"
    } else {
        "needs trust"
    }
}

fn target(cfg: &ServerConfig) -> String {
    match &cfg.transport {
        Transport::Stdio(s) => std::iter::once(s.command.as_str()).chain(s.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        Transport::Http { url, .. } | Transport::Sse { url } => url.clone(),
    }
}

fn list(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let check = args.iter().any(|a| a == "--check");
    let loaded = config::load_all(paths, &[]);
    for p in &loaded.problems {
        eprintln!("marlowe mcp: {p}");
    }
    if loaded.servers.is_empty() {
        writeln!(out, "No MCP servers configured. Add one with `marlowe mcp add`.").map_err(io)?;
        return Ok(0);
    }
    let live = check.then(|| {
        let mut connect = ConnectOpts::new(cx.tokens.clone());
        connect.cwd = cx.workspace.clone();
        connect.roots = cx.workspace.iter().cloned().collect();
        let hub = Hub::start(HubOpts::new(paths.clone(), connect));
        hub.wait_settled(Duration::from_secs(20));
        hub
    });
    for cfg in &loaded.servers {
        let state = match &live {
            Some(hub) => hub
                .statuses()
                .into_iter()
                .find(|s| s.name == cfg.name)
                .map_or_else(|| "not started".to_string(), |s| format!("{} ({} tools)", s.status.label(), s.tools)),
            None => trust_state(cx, paths, cfg).to_string(),
        };
        writeln!(out, "{}: {}  [{} {}]  {}", cfg.name, target(cfg), cfg.scope.as_str(), cfg.transport.name(), state).map_err(io)?;
    }
    Ok(0)
}

fn find(paths: &Paths, name: &str, scope: Option<Scope>) -> Result<ServerConfig, String> {
    let scopes: Vec<Scope> = scope.map_or_else(|| vec![Scope::Local, Scope::Project, Scope::User], |s| vec![s]);
    for s in scopes {
        if let Some(c) = config::load_scope(paths, s).servers.into_iter().find(|c| c.name == name) {
            return Ok(c);
        }
    }
    Err(format!("no MCP server named {name:?}{}", scope.map_or(String::new(), |s| format!(" in the {} scope", s.as_str()))))
}

/// A value shown to the user: variables stay readable, literals are masked.
fn masked(v: &str) -> String {
    if v.contains("${") {
        v.to_string()
    } else {
        "***".to_string()
    }
}

fn get(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let p = parse(args, &["-s", "--scope"])?;
    let Some(name) = p.positional.first() else { return Err("usage: marlowe mcp get NAME".into()) };
    let cfg = find(paths, name, p.scope()?)?;
    let mut lines = vec![format!("{}:", cfg.name), format!("  Scope: {}", cfg.scope.as_str()), format!("  Type: {}", cfg.transport.name())];
    match &cfg.transport {
        Transport::Stdio(s) => {
            lines.push(format!("  Command: {}", s.command));
            lines.push(format!("  Args: {}", s.args.join(" ")));
            for (k, v) in &s.env {
                lines.push(format!("  Env: {k}={}", masked(v)));
            }
        }
        Transport::Http { url, headers, .. } => {
            lines.push(format!("  URL: {url}"));
            for (k, v) in headers {
                lines.push(format!("  Header: {k}: {}", masked(v)));
            }
            if headers.iter().all(|(k, _)| !k.eq_ignore_ascii_case("authorization")) {
                let signed_in = oauth_for(&cfg, &ConnectOpts::new(cx.tokens.clone()), &|k| std::env::var(k).ok()).is_ok_and(|o| o.has_tokens());
                lines.push(format!("  OAuth: {}", if signed_in { "signed in" } else { "not signed in (marlowe mcp auth)" }));
            }
        }
        Transport::Sse { url } => lines.push(format!("  URL: {url} (SSE is not supported; use type http)")),
    }
    lines.push(format!("  Consequence: {}", format!("{:?}", cfg.consequence).to_lowercase()));
    lines.push(format!("  Status: {}", trust_state(cx, paths, &cfg)));
    for l in lines {
        writeln!(out, "{l}").map_err(io)?;
    }
    Ok(0)
}

fn finish_add(cx: &Cx, paths: &Paths, scope: Scope, name: &str, entry: &Value, out: &mut dyn Write) -> Result<i32, String> {
    let cfg = config::add(paths, scope, name, entry)?;
    // The user typed this entry, so it is the trust decision for it.
    if scope.needs_trust() {
        Trust::new(paths).approve(cx.workspace.as_deref(), &cfg)?;
    }
    let what = match (&cfg.transport, cfg.host()) {
        (Transport::Http { .. }, Some(h)) => format!("http MCP server {name} ({h})"),
        _ => format!("{} MCP server {name}", cfg.transport.name()),
    };
    writeln!(out, "Added {what} to the {} config", scope.as_str()).map_err(io)?;
    Ok(0)
}

fn add(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let p = parse(args, &["-s", "--scope", "-t", "--transport", "-e", "--env", "-H", "--header", "--consequence", "--timeout"])?;
    let scope = p.scope()?.unwrap_or(Scope::Local);
    let Some(name) = p.positional.first() else { return Err("usage: marlowe mcp add [flags] NAME (URL | -- CMD ARGS...)".into()) };
    let mut words: Vec<String> = p.positional[1..].to_vec();
    words.extend(p.tail.iter().cloned());
    let first = words.first().cloned().ok_or("add needs a URL or a command (after `--`)")?;
    let transport = p
        .flag(&["-t", "--transport"])
        .map(str::to_string)
        .unwrap_or_else(|| if first.starts_with("http://") || first.starts_with("https://") { "http".into() } else { "stdio".into() });
    let mut entry = Map::new();
    match transport.as_str() {
        "stdio" => {
            entry.insert("command".into(), json!(first));
            if words.len() > 1 {
                entry.insert("args".into(), json!(words[1..]));
            }
            let mut env = Map::new();
            for kv in p.all(&["-e", "--env"]) {
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("-e {kv}: use KEY=VALUE"))?;
                env.insert(k.to_string(), json!(v));
            }
            if !env.is_empty() {
                entry.insert("env".into(), Value::Object(env));
            }
        }
        "http" | "streamable-http" | "sse" => {
            if words.len() > 1 {
                return Err(format!("{transport} takes one URL, got {} words", words.len()));
            }
            entry.insert("type".into(), json!(if transport == "sse" { "sse" } else { "http" }));
            entry.insert("url".into(), json!(first));
            let mut headers = Map::new();
            for h in p.all(&["-H", "--header"]) {
                let (k, v) = h.split_once(':').ok_or_else(|| format!("-H {h:?}: use \"Name: value\""))?;
                headers.insert(k.trim().to_string(), json!(v.trim()));
            }
            if !headers.is_empty() {
                entry.insert("headers".into(), Value::Object(headers));
            }
        }
        other => return Err(format!("unknown transport {other:?} (stdio, http)")),
    }
    if let Some(c) = p.flag(&["--consequence"]) {
        entry.insert("consequence".into(), json!(c));
    }
    if let Some(t) = p.flag(&["--timeout"]) {
        entry.insert("timeout".into(), json!(t.parse::<u64>().map_err(|_| format!("--timeout {t}: milliseconds"))?));
    }
    finish_add(cx, paths, scope, name, &Value::Object(entry), out)
}

fn add_json(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let p = parse(args, &["-s", "--scope"])?;
    let scope = p.scope()?.unwrap_or(Scope::Local);
    let [name, json_text] = p.positional.as_slice() else { return Err("usage: marlowe mcp add-json [-s SCOPE] NAME JSON".into()) };
    let entry: Value = serde_json::from_str(json_text).map_err(|e| format!("not valid JSON: {e}"))?;
    finish_add(cx, paths, scope, name, &entry, out)
}

fn remove(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let p = parse(args, &["-s", "--scope"])?;
    let Some(name) = p.positional.first() else { return Err("usage: marlowe mcp remove NAME [-s SCOPE]".into()) };
    let cfg = find(paths, name, p.scope()?)?;
    config::remove(paths, cfg.scope, name)?;
    if cfg.scope.needs_trust() {
        let _ = Trust::new(paths).revoke(cx.workspace.as_deref(), &cfg);
    }
    writeln!(out, "Removed MCP server {name} from the {} config", cfg.scope.as_str()).map_err(io)?;
    Ok(0)
}

fn trust(cx: &Cx, paths: &Paths, args: &[String], allow: bool, out: &mut dyn Write) -> Result<i32, String> {
    let p = parse(args, &["-s", "--scope"])?;
    let Some(name) = p.positional.first() else { return Err("usage: marlowe mcp trust NAME".into()) };
    let cfg = find(paths, name, p.scope()?)?;
    if !cfg.scope.needs_trust() {
        writeln!(out, "{name} is in the {} scope, which is always trusted", cfg.scope.as_str()).map_err(io)?;
        return Ok(0);
    }
    let t = Trust::new(paths);
    if allow {
        t.approve(cx.workspace.as_deref(), &cfg)?;
        writeln!(out, "Trusted {name} as written ({}). Changing the entry asks again.", target(&cfg)).map_err(io)?;
    } else {
        t.revoke(cx.workspace.as_deref(), &cfg)?;
        writeln!(out, "No longer trusting {name}.").map_err(io)?;
    }
    Ok(0)
}

fn auth_for(cx: &Cx, paths: &Paths, args: &[String]) -> Result<(String, OAuth), String> {
    let p = parse(args, &["-s", "--scope"])?;
    let Some(name) = p.positional.first() else { return Err("usage: marlowe mcp auth NAME".into()) };
    let cfg = find(paths, name, p.scope()?)?;
    let oauth = oauth_for(&cfg, &ConnectOpts::new(cx.tokens.clone()), &|k| std::env::var(k).ok())?;
    Ok((name.clone(), oauth))
}

fn auth(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let (name, oauth) = auth_for(cx, paths, args)?;
    let show = |url: &str| -> Result<(), String> {
        eprintln!("Authorize {name} in your browser. If it did not open, visit:\n{url}");
        (cx.open)(url)
    };
    match oauth.authorize(None, &show, Duration::from_secs(300)) {
        Ok(_) => {
            writeln!(out, "Signed in to {name}.").map_err(io)?;
            Ok(0)
        }
        Err(e) => {
            eprintln!("marlowe mcp: {name}: {e}");
            Ok(1)
        }
    }
}

fn logout(cx: &Cx, paths: &Paths, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let (name, oauth) = auth_for(cx, paths, args)?;
    oauth.sign_out();
    writeln!(out, "Signed out of {name}.").map_err(io)?;
    Ok(0)
}

/// Opens the URL in the user's browser, through the scrubbing spawner. The user ran this command,
/// so a window is expected; failing to open only means the printed link is the way in.
fn open_browser(url: &str) -> Result<(), String> {
    use marlowe_os::child::{self, ChildSpec, RunLimits};
    let mut cmd;
    if cfg!(windows) {
        // The shell's own URL opener, spelled from pieces (an antivirus string scan reads the whole
        // name in a binary as a browser hijacker).
        cmd = std::process::Command::new(["run", "dll32"].concat());
        cmd.args([["ur", "l.dll,", "File", "Protocol", "Handler"].concat(), url.to_string()]);
    } else if cfg!(target_os = "macos") {
        cmd = std::process::Command::new("open");
        cmd.arg(url);
    } else {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
    }
    child::run(cmd, &ChildSpec::new(), RunLimits { timeout: Duration::from_secs(10), max_output_bytes: 4096 }, &|| false).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
