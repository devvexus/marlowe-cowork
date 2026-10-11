//! `.marlowe/project.toml`: the user's word over detection.
//!
//! ```toml
//! [commands]                      # replaces every detected command of that kind
//! test = "pytest -q tests/unit"
//! build = ["cargo", "build", "--release"]
//! lint = { run = "ruff check src", cwd = "backend", timeout_s = 300 }
//!
//! [[services]]                    # replaces a detected service of the same name, or adds one
//! name = "web"
//! command = "npm run dev"
//! cwd = "frontend"
//! ready_url = "http://localhost:5173/"       # or ready_port = 5173, or ready_log = "Local:"
//! env_allow = ["HF_TOKEN"]
//!
//! [migrations]
//! up = "alembic upgrade head"
//! down = "alembic downgrade base"
//! env = ["DATABASE_URL=sqlite:///{db_path}"]
//!
//! [hubs]
//! files = ["src/routes/*.ts", "proto/*.proto"]   # more hub-file patterns
//! ```
//!
//! Commands are a program and arguments, never a shell string ([`crate::cmd`]). A problem with
//! the file is a note on the profile, never an error: detection still stands.

use std::path::Path;

use toml::{Table, Value};

use crate::cmd::{valid_env_name, Cmd};
use crate::profile::{Fs, Kind, Migrations, ProjectCmd, ProjectProfile, ServiceDef, Source};

pub(crate) fn apply(fs: &Fs<'_>, root: &Path, p: &mut ProjectProfile) {
    let path = root.join(".marlowe").join("project.toml");
    let Some(text) = fs.text(&path) else { return };
    let table: Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            let first = e.to_string();
            p.warnings.push(format!(".marlowe/project.toml is not valid TOML ({}); using detection only", first.lines().next().unwrap_or("")));
            return;
        }
    };
    p.has_override = true;
    for key in table.keys() {
        if !matches!(key.as_str(), "commands" | "services" | "migrations" | "hubs") {
            p.warnings.push(format!(".marlowe/project.toml: unknown section `{key}` ignored"));
        }
    }
    if let Some(Value::Table(cmds)) = table.get("commands") {
        for (name, v) in cmds {
            let Some(kind) = Kind::parse(name) else {
                p.warnings.push(format!(".marlowe/project.toml: `{name}` is not a command kind (install, format, lint, typecheck, build, test)"));
                continue;
            };
            match command_of(kind, v, root) {
                Ok(c) => {
                    p.commands.retain(|d| d.kind != kind);
                    p.commands.push(c);
                }
                Err(e) => p.warnings.push(format!(".marlowe/project.toml: commands.{name}: {e}")),
            }
        }
    }
    if let Some(Value::Array(list)) = table.get("services") {
        for (i, v) in list.iter().enumerate() {
            match service_of(v, root) {
                Ok(s) => {
                    p.services.retain(|d| !d.name.eq_ignore_ascii_case(&s.name));
                    p.services.push(s);
                }
                Err(e) => p.warnings.push(format!(".marlowe/project.toml: services[{i}]: {e}")),
            }
        }
    }
    if let Some(Value::Table(m)) = table.get("migrations") {
        match migrations_of(m, root) {
            Ok(m) => {
                p.migrations.clear();
                p.migrations.push(m);
            }
            Err(e) => p.warnings.push(format!(".marlowe/project.toml: migrations: {e}")),
        }
    }
    if let Some(Value::Table(h)) = table.get("hubs") {
        if let Some(Value::Array(files)) = h.get("files") {
            p.extra_hub_patterns.extend(files.iter().filter_map(|v| v.as_str().map(|s| s.replace('\\', "/"))));
        }
    }
}

fn cmd_of(v: &Value) -> Result<Cmd, String> {
    match v {
        Value::String(s) => Cmd::parse(s).map_err(|e| e.to_string()),
        Value::Array(a) => {
            let argv: Vec<String> = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            if argv.is_empty() || argv.len() != a.len() {
                return Err("an array command must be a non-empty list of strings".into());
            }
            Ok(Cmd { argv })
        }
        _ => Err("expected a string or a list of strings".into()),
    }
}

/// A directory below the project root, as a relative path (`.` for the root).
fn cwd_of(t: &Table, root: &Path) -> Result<String, String> {
    let Some(raw) = t.get("cwd").and_then(Value::as_str) else { return Ok(".".into()) };
    let rel = raw.trim().replace('\\', "/");
    let rel = rel.trim_start_matches("./").trim_end_matches('/');
    if rel.is_empty() || rel == "." {
        return Ok(".".into());
    }
    if Path::new(rel).is_absolute() || rel.split('/').any(|s| s == ".." || s.contains(':')) {
        return Err(format!("cwd `{raw}` must be a directory inside the project"));
    }
    let _ = root;
    Ok(rel.to_string())
}

fn command_of(kind: Kind, v: &Value, root: &Path) -> Result<ProjectCmd, String> {
    let (cmd, cwd, extra) = match v {
        Value::Table(t) => {
            let run = t.get("run").ok_or("a table command needs `run`")?;
            (cmd_of(run)?, cwd_of(t, root)?, Some(t))
        }
        other => (cmd_of(other)?, ".".to_string(), None),
    };
    let mut c = ProjectCmd::new(kind, crate::profile::Stack::Make, &cwd, &[]);
    c.cmd = cmd;
    c.stack = None;
    c.source = Source::Override;
    if let Some(t) = extra {
        c.timeout_s = t.get("timeout_s").and_then(Value::as_integer).filter(|n| *n > 0).map(|n| n as u64);
        c.fail_on_output = t.get("fail_on_output").and_then(Value::as_bool).unwrap_or(false);
        c.registry_host = t.get("registry_host").and_then(Value::as_str).map(String::from);
        c.args_separator = t.get("args_separator").and_then(Value::as_str).map(String::from);
    }
    Ok(c)
}

fn service_of(v: &Value, root: &Path) -> Result<ServiceDef, String> {
    let t = v.as_table().ok_or("expected a table")?;
    let name = t.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).ok_or("`name` is required")?;
    let cmd = cmd_of(t.get("command").ok_or("`command` is required")?)?;
    let env_allow: Vec<String> = t
        .get("env_allow")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if let Some(bad) = env_allow.iter().find(|n| !valid_env_name(n)) {
        return Err(format!("env_allow `{bad}` is not a variable name"));
    }
    Ok(ServiceDef {
        name: name.to_string(),
        cmd,
        cwd: cwd_of(t, root)?,
        ready_url: t.get("ready_url").and_then(Value::as_str).map(String::from),
        ready_port: t.get("ready_port").and_then(Value::as_integer).and_then(|p| u16::try_from(p).ok()),
        ready_log: t.get("ready_log").and_then(Value::as_str).map(String::from),
        env_allow,
        source: Source::Override,
        available: true,
        runs: None,
    })
}

fn migrations_of(t: &Table, root: &Path) -> Result<Migrations, String> {
    let up = cmd_of(t.get("up").ok_or("`up` is required")?)?;
    let down = t.get("down").map(cmd_of).transpose()?;
    let env: Vec<(String, String)> = t
        .get("env")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|s| s.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.to_string())).collect())
        .unwrap_or_else(|| vec![("DATABASE_URL".into(), "{db_url}".into())]);
    if let Some((bad, _)) = env.iter().find(|(k, _)| !valid_env_name(k)) {
        return Err(format!("env `{bad}` is not a variable name"));
    }
    Ok(Migrations {
        tool: t.get("tool").and_then(Value::as_str).unwrap_or("custom").to_string(),
        up,
        down,
        cwd: cwd_of(t, root)?,
        config_files: t
            .get("config_files")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        env,
        source: Source::Override,
        available: true,
    })
}
