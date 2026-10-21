//! Slash commands. The client handles the ones that are about the screen or map to one request
//! (`/config`, `/diff`, `/rewind`, `/compact`, ...); a command the daemon lists (`/init`,
//! `/review`, a user's own `.marlowe/commands/*.md`) is sent as an ordinary message and the
//! conductor expands it. Typing `/` at the start of an empty line opens a completion popup.

use marlowe_model::Effort;
use marlowe_proto::{CommandInfo, ExportFormat, PermissionMode, Request};

use crate::state::{Command, NoteTone, State};

/// What a command takes after its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Args {
    None,
    Optional(&'static str),
    Required(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: Args,
    pub help: &'static str,
}

const fn b(name: &'static str, aliases: &'static [&'static str], args: Args, help: &'static str) -> Builtin {
    Builtin { name, aliases, args, help }
}

pub const BUILTINS: &[Builtin] = &[
    b("help", &["?"], Args::None, "keys and commands"),
    b("config", &["settings"], Args::None, "settings, written to the layer you pick"),
    b("context", &[], Args::None, "how full the context is, and what fills it"),
    b("permissions", &["allowed-tools"], Args::None, "allow, ask and deny rules"),
    b("hooks", &[], Args::None, "configured hooks and whether they are trusted"),
    b("agents", &[], Args::None, "agent types the agent tool can start"),
    b("mcp", &[], Args::None, "MCP servers: add, remove, status"),
    b("ide", &[], Args::None, "the connected editor: which window, which folders"),
    b("memory", &[], Args::None, "open the instruction files in your editor and reload them"),
    b("model", &[], Args::Optional("model"), "switch the conductor's model, keeping the conversation"),
    b("effort", &["think"], Args::Optional("off|low|medium|high|max"), "reasoning effort for the next turns"),
    b("mode", &[], Args::Optional("plan|default|accept_edits|bypass|dont_ask"), "permission mode (Shift+Tab cycles)"),
    b("plan", &[], Args::None, "plan mode: read-only until you approve the plan"),
    b("resume", &["sessions"], Args::None, "pick an earlier session"),
    b("clear", &["new"], Args::None, "start a new session; memory is kept"),
    b("compact", &[], Args::Optional("focus"), "summarise older turns now, optionally keeping a focus"),
    b("cost", &["usage"], Args::None, "tokens and cost for this session"),
    b("diff", &[], Args::Optional("turn"), "what changed in this session, or in one turn"),
    b("rewind", &["undo"], Args::None, "go back to an earlier turn: code, conversation or both"),
    b("rename", &[], Args::Required("title"), "name this session"),
    b("fork", &[], Args::None, "continue from a copy of this session"),
    b("export", &[], Args::Optional("markdown|json"), "write this session to a file"),
    b("add-dir", &[], Args::Required("path"), "let this session work in another directory"),
    b("loop", &[], Args::Optional("interval prompt"), "run a prompt on a schedule, e.g. /loop 10m check the build"),
    b("todos", &[], Args::None, "show or hide the todo list"),
    b("jobs", &[], Args::None, "background jobs"),
    b("tasks", &["bashes"], Args::None, "everything running in the background: stop it, read its output"),
    b("teams", &["workflows"], Args::None, "team task progress, skip an agent, saved team tasks"),
    b("team", &["workflow"], Args::Optional("name [json args]"), "run a saved team task and watch it"),
    b("findings", &[], Args::None, "the last code review's findings"),
    b("files", &[], Args::None, "files the agent sent you"),
    b("messages", &["threads"], Args::None, "messages to crew runs and agents"),
    b("bench", &["eval"], Args::None, "bench cards and coding-eval reports"),
    b("fast", &[], Args::Optional("on|off"), "run the conductor on the Quick model; history kept"),
    b("fewer-permission-prompts", &[], Args::None, "suggest allow rules from the calls you keep approving"),
    b("keep-running", &[], Args::Optional("on|off|ask"), "keep Marlowe running after its window closes; ask forgets a remembered quit choice"),
    b("exit", &["quit"], Args::None, "leave Marlowe: stop the agent, or close the window and keep it running"),
];

/// `/name args` -> `(name, args)`. A line that starts with `/` but is a path or a sentence is not
/// a command.
pub fn parse(text: &str) -> Option<(String, String)> {
    let t = text.trim_start();
    let rest = t.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let ok = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '?')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '?'));
    ok.then(|| (name.to_ascii_lowercase(), args.to_string()))
}

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name || b.aliases.contains(&name))
}

/// One row of the completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub hint: String,
    pub help: String,
    /// Handled by the client (false: the daemon expands it).
    pub builtin: bool,
    /// Needs an argument before Enter runs it.
    pub needs_args: bool,
}

/// Rows for the text typed after `/`: prefix matches first, then substring matches; the client's
/// commands before the daemon's.
pub fn rows(prefix: &str, daemon: &[CommandInfo]) -> Vec<Row> {
    let q = prefix.trim().to_ascii_lowercase();
    let mut all: Vec<Row> = BUILTINS
        .iter()
        .map(|b| Row {
            name: b.name.to_string(),
            hint: match b.args {
                Args::None => String::new(),
                Args::Optional(h) => format!("[{h}]"),
                Args::Required(h) => format!("<{h}>"),
            },
            help: b.help.to_string(),
            builtin: true,
            needs_args: matches!(b.args, Args::Required(_)),
        })
        .collect();
    for c in daemon {
        let name = c.name.trim_start_matches('/').to_string();
        if all.iter().any(|r| r.name == name) || builtin(&name).is_some() {
            continue;
        }
        all.push(Row {
            name,
            hint: c.argument_hint.clone().unwrap_or_default(),
            help: c.description.clone(),
            builtin: false,
            needs_args: false,
        });
    }
    // The name typed in full first, then names that start with it, then aliases, then the rest.
    let score = |r: &Row| -> Option<u8> {
        if q.is_empty() {
            Some(1)
        } else if r.name == q || builtin(&r.name).is_some_and(|b| b.aliases.contains(&q.as_str())) {
            Some(0)
        } else if r.name.starts_with(&q) {
            Some(1)
        } else if builtin(&r.name).is_some_and(|b| b.aliases.iter().any(|a| a.starts_with(&q))) {
            Some(2)
        } else if r.name.contains(&q) {
            Some(3)
        } else {
            None
        }
    };
    let mut scored: Vec<(u8, Row)> = all.into_iter().filter_map(|r| score(&r).map(|s| (s, r))).collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().map(|(_, r)| r).collect()
}

/// `10m`, `30s`, `2h`, `1d`, or a bare number of minutes, as seconds.
pub fn parse_interval(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s.as_str(), "m"),
    };
    let n: u64 = num.parse().ok().filter(|n| *n > 0)?;
    let secs = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3_600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };
    Some(n * secs)
}

/// `600` -> `10m`.
pub fn interval_label(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

pub fn parse_effort(s: &str) -> Option<Effort> {
    if s.trim().eq_ignore_ascii_case("none") {
        return Some(Effort::Off);
    }
    s.parse().ok()
}

pub fn parse_mode(s: &str) -> Option<PermissionMode> {
    let t = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    let t = if t == "auto" { "bypass".to_string() } else { t };
    PermissionMode::ALL.into_iter().find(|m| m.as_str() == t || (t == "acceptedits" && *m == PermissionMode::AcceptEdits))
}

/// What happened to a line typed as a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Handled,
    /// The daemon expands it: send the text as a message.
    Forward,
    Unknown(String),
}

impl State {
    /// Runs a line that starts with `/`.
    pub(crate) fn run_slash(&mut self, text: &str, out: &mut Vec<Request>) -> Outcome {
        let Some((name, args)) = parse(text) else { return Outcome::Unknown(text.trim().to_string()) };
        let Some(b) = builtin(&name) else {
            let known = self.code.commands.iter().any(|c| c.name.trim_start_matches('/').eq_ignore_ascii_case(&name));
            return if known { Outcome::Forward } else { Outcome::Unknown(name) };
        };
        if let (Args::Required(hint), true) = (b.args, args.is_empty()) {
            self.flash(format!("/{} needs {hint}", b.name), NoteTone::Amber);
            return Outcome::Handled;
        }
        // `/loop <prompt>` with no interval is self-paced (BG6): the daemon expands it, and the
        // model re-arms itself with `schedule_wakeup` until it has nothing left to watch.
        if b.name == "loop" && !args.is_empty() && !args.split_whitespace().next().is_some_and(|w| parse_interval(w).is_some()) {
            return Outcome::Forward;
        }
        match b.name {
            "help" => self.open_help(out),
            "config" => self.open_config(out),
            "context" => self.open_context_view(out),
            "permissions" => self.open_permissions(out),
            "hooks" => self.open_hooks(out),
            "agents" => self.open_agents(out),
            "mcp" => self.open_mcp(out),
            "ide" => {
                self.code.expect_ide = true;
                out.push(Request::McpList { workspace: self.ws() });
            }
            "memory" => self.open_instructions(out),
            "model" => self.slash_model(&args, out),
            "effort" => self.slash_effort(&args, out),
            "mode" => match (args.is_empty(), parse_mode(&args)) {
                (true, _) => self.cycle_mode(out),
                (false, Some(m)) => self.set_mode(m, out),
                (false, None) => self.flash("modes: plan, default, accept_edits, bypass, dont_ask", NoteTone::Amber),
            },
            "plan" => self.set_mode(PermissionMode::Plan, out),
            "resume" => self.run_command(Command::Sessions, out),
            "clear" => self.run_command(Command::NewSession, out),
            "compact" => {
                if let Some(session) = self.session.clone() {
                    out.push(Request::Compact { session, focus: Some(args).filter(|a| !a.is_empty()) });
                    self.flash("compacting…", NoteTone::Dim);
                } else {
                    self.flash("no session yet — nothing to compact", NoteTone::Dim);
                }
            }
            "cost" => self.open_cost(out),
            "diff" => {
                let turn = args.split_whitespace().next().and_then(|t| t.trim_start_matches('#').parse::<u32>().ok());
                self.open_diff(turn, out);
            }
            "rewind" => self.open_rewind(out),
            "rename" => {
                if let Some(session) = self.session.clone() {
                    self.title = Some(args.clone());
                    out.push(Request::RenameSession { session, title: args });
                } else {
                    self.flash("no session yet — send a message first", NoteTone::Dim);
                }
            }
            "fork" => {
                if let Some(session) = self.session.clone() {
                    self.code.expect_fork = true;
                    out.push(Request::ForkSession { session, at_turn: None });
                } else {
                    self.flash("no session yet — nothing to fork", NoteTone::Dim);
                }
            }
            "export" => {
                if let Some(session) = self.session.clone() {
                    let format = if args.to_ascii_lowercase().starts_with("json") { ExportFormat::Json } else { ExportFormat::Markdown };
                    out.push(Request::ExportSession { session, format });
                } else {
                    self.flash("no session yet — nothing to export", NoteTone::Dim);
                }
            }
            "add-dir" => {
                if let Some(session) = self.session.clone() {
                    out.push(Request::AddDir { session, path: args.trim_matches('"').to_string() });
                    self.flash(format!("added {}", crate::text::short_path(args.trim_matches('"'))), NoteTone::Dim);
                } else {
                    self.flash("no session yet — start one in that directory instead", NoteTone::Dim);
                }
            }
            "loop" => self.slash_loop(&args, out),
            "todos" => {
                self.code.todos_hidden = !self.code.todos_hidden;
                if self.code.todos.is_empty() {
                    self.flash("no todo list yet — the agent writes one for longer tasks", NoteTone::Dim);
                }
            }
            "jobs" => self.open_jobs(out),
            "tasks" => self.open_tasks(out),
            "teams" => self.open_workflows(out),
            "team" => {
                if args.is_empty() {
                    self.open_workflows(out);
                } else if args.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case("convert")) {
                    return Outcome::Forward;
                } else {
                    self.slash_workflow(&args, out);
                }
            }
            "findings" => self.slash_findings(out),
            "files" => self.open_files(out),
            "messages" => self.open_threads(None, out),
            "bench" => self.open_bench(out),
            "fast" => self.slash_fast(&args, out),
            "fewer-permission-prompts" => {
                self.bg.expect_suggestions = true;
                out.push(Request::SuggestRules { workspace: self.ws() });
                self.flash("reading this project's sessions...", NoteTone::Dim);
            }
            "keep-running" => self.slash_keep_running(&args, out),
            "exit" => self.request_quit(),
            other => return Outcome::Unknown(other.to_string()),
        }
        Outcome::Handled
    }

    fn slash_model(&mut self, args: &str, out: &mut Vec<Request>) {
        if args.is_empty() {
            self.run_command(Command::Model, out);
            return;
        }
        let q = args.to_ascii_lowercase();
        let hit = self.library.models.iter().find(|m| {
            m.model.model_id.eq_ignore_ascii_case(args)
                || m.model.model_id.to_ascii_lowercase().contains(&q)
                || m.info.display.to_ascii_lowercase().contains(&q)
        });
        match hit.map(|m| m.model.clone()) {
            Some(model) => self.switch_model(model, out),
            None => self.flash(format!("no model in the library matches '{}' — /model lists them", crate::text::clean(args)), NoteTone::Amber),
        }
    }

    /// Makes `model` the conductor and, in an open session, switches it there too with the
    /// history kept.
    pub(crate) fn switch_model(&mut self, model: marlowe_model::ModelRef, out: &mut Vec<Request>) {
        let effort = self.library.slot_rows()[0].effort;
        out.push(Request::SetSlot { slot: marlowe_proto::Slot::Conductor, model: Some(model.clone()), effort });
        if let Some(session) = self.session.clone() {
            out.push(Request::SwitchModel { session, model: model.clone(), effort: Some(effort) });
        }
        out.push(Request::LibraryState);
        let row = marlowe_proto::SlotAssignment { slot: marlowe_proto::Slot::Conductor, model: Some(model), effort, inherited: false };
        match self.library.slots.iter_mut().find(|s| s.slot == marlowe_proto::Slot::Conductor) {
            Some(s) => *s = row,
            None => self.library.slots.push(row),
        }
        self.code.statusline.dirty = true;
    }

    pub(crate) fn slash_effort(&mut self, args: &str, out: &mut Vec<Request>) {
        let current = self.library.slot_rows()[0].effort;
        let next = if args.is_empty() {
            let allowed = self
                .library
                .conductor()
                .and_then(|m| self.library.models.iter().find(|x| x.model == m))
                .map(|m| m.info.effort_levels.clone())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| Effort::ALL.to_vec());
            current.cycle(&allowed)
        } else if let Some(e) = parse_effort(args) {
            e
        } else {
            self.flash("effort: off, low, medium, high or max", NoteTone::Amber);
            return;
        };
        self.set_effort(next, out);
    }

    /// Sets the conductor's reasoning effort for this and later turns.
    pub fn set_effort(&mut self, effort: Effort, out: &mut Vec<Request>) {
        let Some(model) = self.library.conductor() else {
            self.flash("add a model first", NoteTone::Amber);
            return;
        };
        out.push(Request::SetSlot { slot: marlowe_proto::Slot::Conductor, model: Some(model.clone()), effort });
        if let Some(session) = self.session.clone() {
            out.push(Request::SwitchModel { session, model: model.clone(), effort: Some(effort) });
        }
        if let Some(s) = self.library.slots.iter_mut().find(|s| s.slot == marlowe_proto::Slot::Conductor) {
            s.effort = effort;
            s.inherited = false;
        }
        self.flash(format!("thinking effort: {effort}"), NoteTone::Dim);
    }

    /// `/team <name|path> [json args]`: starts the run and opens its progress when it appears.
    fn slash_workflow(&mut self, args: &str, out: &mut Vec<Request>) {
        let (name, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let rest = rest.trim();
        let value = if rest.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_str::<serde_json::Value>(rest) {
                Ok(v) => v,
                Err(e) => {
                    self.flash(format!("the arguments are JSON: {}", crate::text::clean(&e.to_string())), NoteTone::Amber);
                    return;
                }
            }
        };
        let is_path = name.contains(['/', '\\']) || name.ends_with(".star");
        let (name, script_path) = if is_path { (None, Some(name.to_string())) } else { (Some(name.to_string()), None) };
        self.bg.expect_run = true;
        out.push(Request::RunWorkflow { session: self.session.clone(), name, script_path, args: value, resume_from: None });
        self.flash("starting the team task...", NoteTone::Dim);
    }

    /// `/fast [on|off]`: the conductor on the Quick slot's model, or back.
    fn slash_fast(&mut self, args: &str, out: &mut Vec<Request>) {
        let Some(session) = self.session.clone() else {
            self.flash("no session yet \u{2014} send a message first", NoteTone::Dim);
            return;
        };
        let on = match args.trim().to_ascii_lowercase().as_str() {
            "" => self.bg.fast_on(Some(&session)).is_none(),
            "on" | "yes" | "true" => true,
            "off" | "no" | "false" => false,
            other => {
                self.flash(format!("/fast takes on or off, not `{other}`"), NoteTone::Amber);
                return;
            }
        };
        out.push(Request::SetFast { session, on });
    }

    fn slash_loop(&mut self, args: &str, out: &mut Vec<Request>) {
        if args.is_empty() {
            out.push(Request::ListSchedules);
            return;
        }
        let (interval, text) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        match (parse_interval(interval), text.trim()) {
            (Some(every_secs), t) if !t.is_empty() => {
                out.push(Request::AddSchedule { session: self.session.clone(), every_secs, text: t.to_string() });
                self.flash(format!("running that every {interval}"), NoteTone::Dim);
            }
            _ => self.flash("usage: /loop 10m check the build", NoteTone::Amber),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_a_name_and_the_rest() {
        assert_eq!(parse("/compact keep the gate work"), Some(("compact".into(), "keep the gate work".into())));
        assert_eq!(parse("  /Config "), Some(("config".into(), String::new())));
        assert_eq!(parse("/mcp__srv__prompt arg"), Some(("mcp__srv__prompt".into(), "arg".into())));
        assert_eq!(parse("/usr/bin/python is where?"), None, "a path is not a command");
        assert_eq!(parse("/ alone"), None);
        assert_eq!(parse("not a command"), None);
    }

    #[test]
    fn completion_prefers_prefixes_and_lists_the_daemons_commands_after() {
        let daemon = vec![
            CommandInfo { name: "init".into(), description: "write MARLOWE.md".into(), argument_hint: None, source: "builtin".into() },
            CommandInfo { name: "review".into(), description: "review a PR".into(), argument_hint: Some("<pr>".into()), source: "x".into() },
            CommandInfo { name: "config".into(), description: "dup".into(), argument_hint: None, source: "x".into() },
        ];
        let names = |q: &str| rows(q, &daemon).into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names("co")[..2], ["config".to_string(), "context".to_string()]);
        assert!(names("re").contains(&"review".to_string()));
        assert_eq!(names("re").iter().filter(|n| *n == "config").count(), 0);
        assert_eq!(names("").iter().filter(|n| *n == "config").count(), 1, "a daemon command never shadows a builtin");
        assert!(names("zzzz").is_empty());
        assert_eq!(rows("rev", &daemon)[0].hint, "<pr>");
    }

    #[test]
    fn intervals_and_arguments_parse() {
        assert_eq!(parse_interval("10m"), Some(600));
        assert_eq!(parse_interval("30s"), Some(30));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("5"), Some(300));
        assert_eq!(parse_interval("0m"), None);
        assert_eq!(parse_interval("soon"), None);
        assert_eq!(parse_mode("accept-edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(parse_mode("auto"), Some(PermissionMode::Bypass));
        assert_eq!(parse_mode("nope"), None);
        assert_eq!(parse_effort("HIGH"), Some(Effort::High));
    }

    #[test]
    fn every_builtin_name_and_alias_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BUILTINS {
            assert!(seen.insert(b.name), "{}", b.name);
            for a in b.aliases {
                assert!(seen.insert(a), "{a}");
            }
        }
    }
}
