//! MCP server configuration: `.mcp.json` (the Claude Code shape) in four scopes, merged.
//!
//! | Scope | File | Trusted |
//! |---|---|---|
//! | `Session` | `--mcp-config`, in memory | yes (the user typed it) |
//! | `Local` | `<workspace>\.marlowe\mcp.local.json` | once, by digest |
//! | `Project` | `<workspace>\.mcp.json` | once, by digest |
//! | `User` | `<home>\mcp.json` | yes |
//!
//! A later scope in the table wins over an earlier-listed lower one for the same name (session >
//! local > project > user). A workspace's own files can start programs, so nothing from `Local`
//! or `Project` connects until the user has trusted that exact entry (`marlowe mcp trust`, or by
//! adding it with `marlowe mcp add`); changing an entry un-trusts it.
//!
//! Entry shape: `{"command", "args", "env"}` (stdio) or `{"type": "http", "url", "headers",
//! "oauth": {...}}`, plus Marlowe's `consequence` (the level every tool of the server loads with,
//! default `irreversible` so each call asks, ADR-052), `consequences` (per-tool overrides),
//! `env_allow` (user scope only: scrubbed variables the server may inherit), `timeout` (ms per
//! request) and `disabled`. Strings expand `${VAR}` and `${VAR:-default}`; a credential variable
//! (`marlowe_os::SCRUBBED_ENV`) is never expanded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_tools::ConsequenceLevel;
use serde_json::{json, Map, Value};

use crate::oauth::OAuthConfig;
use crate::stdio::StdioSpec;
use crate::util::{percent_encode, sha256_hex, Url};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    User,
    Project,
    Local,
    Session,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
            Scope::Session => "session",
        }
    }

    /// Whether entries of this scope need the user's trust before they connect.
    pub fn needs_trust(self) -> bool {
        matches!(self, Scope::Project | Scope::Local)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Transport {
    Stdio(StdioSpec),
    Http { url: String, headers: Vec<(String, String)>, oauth: Option<OAuthConfig> },
    /// The deprecated SSE transport: recognised so it can be reported, not connected.
    Sse { url: String },
}

impl Transport {
    pub fn name(&self) -> &'static str {
        match self {
            Transport::Stdio(_) => "stdio",
            Transport::Http { .. } => "http",
            Transport::Sse { .. } => "sse",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub scope: Scope,
    pub transport: Transport,
    pub consequence: ConsequenceLevel,
    pub tool_consequences: BTreeMap<String, ConsequenceLevel>,
    pub disabled: bool,
    pub timeout: Option<Duration>,
    /// The entry as written (variables unexpanded).
    pub raw: Value,
}

impl ServerConfig {
    /// A digest of name and entry: what trust and connection reuse are keyed on.
    pub fn digest(&self) -> String {
        sha256_hex(format!("{}\n{}", self.name, canonical(&self.raw)).as_bytes())
    }

    /// The remote host, for an HTTP server (shown when it is added).
    pub fn host(&self) -> Option<String> {
        match &self.transport {
            Transport::Http { url, .. } | Transport::Sse { url } => Url::parse(url).ok().map(|u| u.host),
            Transport::Stdio(_) => None,
        }
    }
}

/// JSON with sorted keys and no whitespace.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            format!("{{{}}}", keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&m[*k]))).collect::<Vec<_>>().join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// Server names are used in tool names and file keys.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 48 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

// ---------------------------------------------------------------- parsing

/// `${VAR}` and `${VAR:-default}`. A credential variable is refused, not expanded.
pub fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = after.find('}').ok_or_else(|| format!("unterminated ${{ in {text:?}"))?;
        let (name, default) = match after[..end].split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (&after[..end], None),
        };
        if marlowe_os::child::is_scrubbed(name) {
            return Err(format!(
                "{name} is a credential variable and is not expanded into an MCP config; write the value in the entry or list the name in env_allow"
            ));
        }
        match (env(name), default) {
            (Some(v), _) => out.push_str(&v),
            (None, Some(d)) => out.push_str(d),
            (None, None) => return Err(format!("${{{name}}} is not set (use ${{{name}:-default}})")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn level(v: &Value) -> Option<ConsequenceLevel> {
    serde_json::from_value(v.clone()).ok()
}

/// Parses one entry. Variables are left in place; [`ServerConfig::expanded`] resolves them.
pub fn parse_entry(name: &str, scope: Scope, raw: &Value) -> Result<ServerConfig, String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a usable server name (letters, digits, `_`, `-`, `.`; at most 48)"));
    }
    let obj = raw.as_object().ok_or("the entry must be a JSON object")?;
    let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let kind = s("type").or_else(|| s("transport")).unwrap_or_else(|| if obj.contains_key("url") { "http".into() } else { "stdio".into() });
    let strings = |k: &str| -> Result<Vec<String>, String> {
        match obj.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("{k} must be strings"))).collect(),
            Some(_) => Err(format!("{k} must be an array")),
        }
    };
    let pairs = |k: &str| -> Result<Vec<(String, String)>, String> {
        match obj.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(a, b)| b.as_str().map(|b| (a.clone(), b.to_string())).ok_or_else(|| format!("{k}.{a} must be a string")))
                .collect(),
            Some(_) => Err(format!("{k} must be an object")),
        }
    };
    let transport = match kind.as_str() {
        "stdio" => Transport::Stdio(StdioSpec {
            command: s("command").filter(|c| !c.trim().is_empty()).ok_or("a stdio server needs a command")?,
            args: strings("args")?,
            env: pairs("env")?,
            env_allow: if scope == Scope::User || scope == Scope::Session { strings("env_allow")? } else { Vec::new() },
        }),
        "http" | "streamable-http" | "streamable_http" => Transport::Http {
            url: s("url").ok_or("an http server needs a url")?,
            headers: pairs("headers")?,
            oauth: obj.get("oauth").map(|o| serde_json::from_value(o.clone()).map_err(|e| format!("oauth: {e}"))).transpose()?,
        },
        "sse" => Transport::Sse { url: s("url").ok_or("an sse server needs a url")? },
        other => return Err(format!("unknown server type {other:?} (stdio, http)")),
    };
    let consequence = match obj.get("consequence") {
        None => ConsequenceLevel::Irreversible,
        Some(v) => level(v).ok_or("consequence must be inert, reversible, consequential or irreversible")?,
    };
    let tool_consequences = match obj.get("consequences") {
        None => BTreeMap::new(),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| level(v).map(|l| (k.clone(), l)).ok_or_else(|| format!("consequences.{k} is not a consequence level")))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("consequences must be an object".into()),
    };
    Ok(ServerConfig {
        name: name.to_string(),
        scope,
        transport,
        consequence,
        tool_consequences,
        disabled: obj.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        timeout: obj.get("timeout").and_then(Value::as_u64).map(Duration::from_millis),
        raw: raw.clone(),
    })
}

impl ServerConfig {
    /// The entry with every `${VAR}` resolved through `env`.
    pub fn expanded(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<ServerConfig, String> {
        let mut out = self.clone();
        let ex = |t: &str| expand(t, env).map_err(|e| format!("{}: {e}", self.name));
        out.transport = match &self.transport {
            Transport::Stdio(spec) => Transport::Stdio(StdioSpec {
                command: ex(&spec.command)?,
                args: spec.args.iter().map(|a| ex(a)).collect::<Result<_, _>>()?,
                env: spec.env.iter().map(|(k, v)| Ok((k.clone(), ex(v)?))).collect::<Result<_, String>>()?,
                env_allow: spec.env_allow.clone(),
            }),
            Transport::Http { url, headers, oauth } => Transport::Http {
                url: ex(url)?,
                headers: headers.iter().map(|(k, v)| Ok((k.clone(), ex(v)?))).collect::<Result<_, String>>()?,
                oauth: oauth.clone(),
            },
            Transport::Sse { url } => Transport::Sse { url: ex(url)? },
        };
        Ok(out)
    }
}

/// Parses a whole `.mcp.json` text (`{"mcpServers": {...}}`, or the bare map).
pub fn parse_file(text: &str, scope: Scope) -> Result<Vec<ServerConfig>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let map = v.get("mcpServers").unwrap_or(&v).as_object().ok_or("expected an object of servers")?;
    let mut out = Vec::new();
    for (name, raw) in map {
        // One bad entry does not hide the others; it is reported by `load`.
        out.push(parse_entry(name, scope, raw)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- files

/// Where the scopes live for one session.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub workspace: Option<PathBuf>,
}

impl Paths {
    pub fn new(home: &Path, workspace: Option<&Path>) -> Self {
        Self { home: home.to_path_buf(), workspace: workspace.map(Path::to_path_buf) }
    }

    pub fn file(&self, scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::User => Some(self.home.join("mcp.json")),
            Scope::Project => self.workspace.as_ref().map(|w| w.join(".mcp.json")),
            Scope::Local => self.workspace.as_ref().map(|w| w.join(".marlowe").join("mcp.local.json")),
            Scope::Session => None,
        }
    }

    pub fn trust_file(&self) -> PathBuf {
        self.home.join("mcp-trust.json")
    }

    pub fn pins_file(&self) -> PathBuf {
        self.home.join("mcp-pins.json")
    }
}

/// What a scope's file held.
#[derive(Debug, Default)]
pub struct Loaded {
    pub servers: Vec<ServerConfig>,
    /// Entries that did not parse, as `scope name: why`.
    pub problems: Vec<String>,
}

pub fn load_scope(paths: &Paths, scope: Scope) -> Loaded {
    let mut out = Loaded::default();
    let Some(file) = paths.file(scope) else { return out };
    let Ok(text) = std::fs::read_to_string(&file) else { return out };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            out.problems.push(format!("{}: {e}", file.display()));
            return out;
        }
    };
    let Some(map) = v.get("mcpServers").unwrap_or(&v).as_object() else {
        out.problems.push(format!("{}: expected an object of servers", file.display()));
        return out;
    };
    for (name, raw) in map {
        match parse_entry(name, scope, raw) {
            Ok(c) => out.servers.push(c),
            Err(e) => out.problems.push(format!("{} {name}: {e}", scope.as_str())),
        }
    }
    out
}

/// Every scope merged: the highest scope wins a name.
pub fn load_all(paths: &Paths, session: &[ServerConfig]) -> Loaded {
    let mut merged: BTreeMap<String, ServerConfig> = BTreeMap::new();
    let mut problems = Vec::new();
    for scope in [Scope::User, Scope::Project, Scope::Local] {
        let l = load_scope(paths, scope);
        problems.extend(l.problems);
        for s in l.servers {
            merged.insert(s.name.clone(), s);
        }
    }
    for s in session {
        merged.insert(s.name.clone(), s.clone());
    }
    Loaded { servers: merged.into_values().collect(), problems }
}

/// Parses `--mcp-config` values: a file path or inline JSON, each in `.mcp.json` shape.
pub fn parse_session(values: &[String]) -> Result<Vec<ServerConfig>, String> {
    let mut out = Vec::new();
    for v in values {
        let text = if v.trim_start().starts_with('{') { v.clone() } else { std::fs::read_to_string(v).map_err(|e| format!("--mcp-config {v}: {e}"))? };
        out.extend(parse_file(&text, Scope::Session).map_err(|e| format!("--mcp-config {v}: {e}"))?);
    }
    Ok(out)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn edit_file(path: &Path, f: impl FnOnce(&mut Map<String, Value>) -> bool) -> Result<bool, String> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) => json!({}),
    };
    let obj = root.as_object_mut().ok_or_else(|| format!("{}: expected an object", path.display()))?;
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let map = servers.as_object_mut().ok_or_else(|| format!("{}: mcpServers is not an object", path.display()))?;
    let changed = f(map);
    if changed {
        write_atomic(path, &(serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n"))?;
    }
    Ok(changed)
}

/// Adds or replaces `name` in the scope's file. Returns the parsed entry.
pub fn add(paths: &Paths, scope: Scope, name: &str, entry: &Value) -> Result<ServerConfig, String> {
    let cfg = parse_entry(name, scope, entry)?;
    let file = paths.file(scope).ok_or_else(|| match scope {
        Scope::Session => "a session server is not stored".to_string(),
        _ => "this scope needs a workspace".to_string(),
    })?;
    edit_file(&file, |m| {
        m.insert(name.to_string(), entry.clone());
        true
    })?;
    Ok(cfg)
}

/// Removes `name` from the scope's file; false when it was not there.
pub fn remove(paths: &Paths, scope: Scope, name: &str) -> Result<bool, String> {
    let Some(file) = paths.file(scope) else { return Ok(false) };
    if !file.exists() {
        return Ok(false);
    }
    edit_file(&file, |m| m.remove(name).is_some())
}

// ---------------------------------------------------------------- trust

/// Entries from a workspace's own files the user has allowed to start, by digest.
pub struct Trust {
    path: PathBuf,
}

impl Trust {
    pub fn new(paths: &Paths) -> Self {
        Self { path: paths.trust_file() }
    }

    fn read(&self) -> Value {
        std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
    }

    fn key(workspace: Option<&Path>) -> String {
        workspace.map_or_else(|| "-".into(), |w| percent_encode(&w.display().to_string().to_ascii_lowercase()))
    }

    pub fn is_trusted(&self, workspace: Option<&Path>, cfg: &ServerConfig) -> bool {
        !cfg.scope.needs_trust()
            || self.read()["projects"][Self::key(workspace)][format!("{}:{}", cfg.scope.as_str(), cfg.name)] == Value::String(cfg.digest())
    }

    pub fn approve(&self, workspace: Option<&Path>, cfg: &ServerConfig) -> Result<(), String> {
        let mut root = self.read();
        let projects = root.as_object_mut().ok_or("trust file is not an object")?.entry("projects").or_insert_with(|| json!({}));
        let p = projects.as_object_mut().ok_or("trust file is damaged")?.entry(Self::key(workspace)).or_insert_with(|| json!({}));
        p.as_object_mut().ok_or("trust file is damaged")?.insert(format!("{}:{}", cfg.scope.as_str(), cfg.name), Value::String(cfg.digest()));
        write_atomic(&self.path, &serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?)
    }

    pub fn revoke(&self, workspace: Option<&Path>, cfg: &ServerConfig) -> Result<(), String> {
        let mut root = self.read();
        if let Some(p) = root["projects"].get_mut(Self::key(workspace)).and_then(Value::as_object_mut) {
            p.remove(&format!("{}:{}", cfg.scope.as_str(), cfg.name));
        }
        write_atomic(&self.path, &serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-mcp-{tag}-{}-{}", std::process::id(), crate::util::random_token(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_claude_code_mcp_json_parses() {
        let text = r#"{"mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."], "env": {"A": "1"}},
            "remote": {"type": "http", "url": "https://mcp.example.com/mcp", "headers": {"X-Key": "k"}},
            "inferred": {"url": "http://127.0.0.1:9/mcp"}
        }}"#;
        let servers = parse_file(text, Scope::Project).unwrap();
        assert_eq!(servers.len(), 3);
        let fs = servers.iter().find(|s| s.name == "fs").unwrap();
        assert!(matches!(&fs.transport, Transport::Stdio(s) if s.command == "npx" && s.args.len() == 3 && s.env == vec![("A".to_string(), "1".to_string())]));
        assert_eq!(fs.consequence, ConsequenceLevel::Irreversible, "absent means maximum");
        assert!(matches!(&servers.iter().find(|s| s.name == "remote").unwrap().transport, Transport::Http { headers, .. } if headers.len() == 1));
        assert_eq!(servers.iter().find(|s| s.name == "inferred").unwrap().transport.name(), "http");
        // The bare map works too.
        assert_eq!(parse_file(r#"{"a": {"command": "x"}}"#, Scope::User).unwrap().len(), 1);
    }

    #[test]
    fn bad_entries_say_what_is_wrong() {
        assert!(parse_entry("a", Scope::User, &json!({})).unwrap_err().contains("command"));
        assert!(parse_entry("a", Scope::User, &json!({"type": "http"})).unwrap_err().contains("url"));
        assert!(parse_entry("a b", Scope::User, &json!({"command": "x"})).unwrap_err().contains("usable server name"));
        assert!(parse_entry("a", Scope::User, &json!({"command": "x", "consequence": "mild"})).is_err());
        assert!(parse_entry("a", Scope::User, &json!({"type": "carrier-pigeon", "command": "x"})).is_err());
    }

    #[test]
    fn variables_expand_and_credentials_never_do() {
        let env = |n: &str| (n == "HOME_DIR").then(|| "C:\\Users\\m".to_string());
        assert_eq!(expand("${HOME_DIR}\\x", &env).unwrap(), "C:\\Users\\m\\x");
        assert_eq!(expand("${NOPE:-fallback}", &env).unwrap(), "fallback");
        assert!(expand("${NOPE}", &env).unwrap_err().contains("not set"));
        let e = expand("Bearer ${OPENROUTER_API_KEY}", &|_| Some("sk-secret".into())).unwrap_err();
        assert!(e.contains("credential variable") && !e.contains("sk-secret"), "{e}");
        assert!(expand("${HF_TOKEN:-x}", &|_| None).is_err(), "even with a default");
    }

    #[test]
    fn an_entry_expands_without_touching_the_raw_form() {
        let cfg = parse_entry("s", Scope::User, &json!({"command": "${BIN}", "args": ["--dir", "${DIR:-.}"]})).unwrap();
        let x = cfg.expanded(&|n| (n == "BIN").then(|| "srv".to_string())).unwrap();
        assert!(matches!(&x.transport, Transport::Stdio(s) if s.command == "srv" && s.args == ["--dir", "."]));
        assert_eq!(x.digest(), cfg.digest(), "trust is over the entry as written");
    }

    #[test]
    fn project_entries_cannot_widen_the_environment() {
        let raw = json!({"command": "x", "env_allow": ["OPENAI_API_KEY"]});
        let project = parse_entry("s", Scope::Project, &raw).unwrap();
        let user = parse_entry("s", Scope::User, &raw).unwrap();
        assert!(matches!(&project.transport, Transport::Stdio(s) if s.env_allow.is_empty()));
        assert!(matches!(&user.transport, Transport::Stdio(s) if s.env_allow == ["OPENAI_API_KEY"]));
    }

    #[test]
    fn scopes_merge_with_the_higher_scope_winning() {
        let home = tmp("home");
        let ws = tmp("ws");
        let paths = Paths::new(&home, Some(&ws));
        add(&paths, Scope::User, "a", &json!({"command": "user-a"})).unwrap();
        add(&paths, Scope::User, "b", &json!({"command": "user-b"})).unwrap();
        add(&paths, Scope::Project, "a", &json!({"command": "project-a"})).unwrap();
        add(&paths, Scope::Local, "b", &json!({"command": "local-b"})).unwrap();
        let session = parse_session(&[r#"{"mcpServers": {"c": {"command": "session-c"}}}"#.to_string()]).unwrap();
        let all = load_all(&paths, &session);
        let cmd = |n: &str| match &all.servers.iter().find(|s| s.name == n).unwrap().transport {
            Transport::Stdio(s) => s.command.clone(),
            _ => unreachable!(),
        };
        assert_eq!((cmd("a"), cmd("b"), cmd("c")), ("project-a".into(), "local-b".into(), "session-c".into()));
        assert!(ws.join(".mcp.json").exists() && ws.join(".marlowe").join("mcp.local.json").exists() && home.join("mcp.json").exists());
    }

    #[test]
    fn add_keeps_other_keys_and_remove_reports_absence() {
        let home = tmp("keep");
        let paths = Paths::new(&home, None);
        std::fs::write(home.join("mcp.json"), r#"{"other": 1, "mcpServers": {"old": {"command": "o"}}}"#).unwrap();
        add(&paths, Scope::User, "new", &json!({"command": "n"})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(v["other"], 1);
        assert!(v["mcpServers"]["old"].is_object() && v["mcpServers"]["new"].is_object());
        assert!(remove(&paths, Scope::User, "old").unwrap());
        assert!(!remove(&paths, Scope::User, "old").unwrap());
        assert!(add(&paths, Scope::Project, "x", &json!({"command": "n"})).unwrap_err().contains("workspace"));
    }

    #[test]
    fn a_broken_entry_does_not_hide_the_good_ones() {
        let home = tmp("broken");
        std::fs::write(home.join("mcp.json"), r#"{"mcpServers": {"good": {"command": "g"}, "bad": {"type": "http"}}}"#).unwrap();
        let l = load_scope(&Paths::new(&home, None), Scope::User);
        assert_eq!(l.servers.len(), 1);
        assert!(l.problems.iter().any(|p| p.contains("bad") && p.contains("url")), "{:?}", l.problems);
    }

    #[test]
    fn workspace_entries_run_only_after_the_user_trusts_that_exact_entry() {
        let home = tmp("trust");
        let ws = tmp("trust-ws");
        let paths = Paths::new(&home, Some(&ws));
        let trust = Trust::new(&paths);
        let project = parse_entry("s", Scope::Project, &json!({"command": "x"})).unwrap();
        let user = parse_entry("s", Scope::User, &json!({"command": "x"})).unwrap();
        assert!(trust.is_trusted(Some(&ws), &user), "user scope is the user's own");
        assert!(!trust.is_trusted(Some(&ws), &project));
        trust.approve(Some(&ws), &project).unwrap();
        assert!(trust.is_trusted(Some(&ws), &project));
        let changed = parse_entry("s", Scope::Project, &json!({"command": "evil"})).unwrap();
        assert!(!trust.is_trusted(Some(&ws), &changed), "a changed entry is a new question");
        assert!(!trust.is_trusted(Some(Path::new("C:\\elsewhere")), &project), "trust is per workspace");
        trust.revoke(Some(&ws), &project).unwrap();
        assert!(!trust.is_trusted(Some(&ws), &project));
    }
}
