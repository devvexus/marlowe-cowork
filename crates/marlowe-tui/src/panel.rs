//! The panels: `/config`, `/permissions`, `/hooks`, `/agents`, `/mcp`, `/memory`, `/context`,
//! `/cost`, `/rewind`, `/diff`, `/jobs`, the `ask_user` modal, `/help` and the Ctrl+O transcript.
//!
//! A panel is `Overlay::Panel` plus [`Panel`] in `State::code.panel`. Each one lists data the
//! daemon owns (rules, hooks, settings, ...), held in [`crate::coding::Coding`]; the panel itself
//! keeps only the cursor and the edit in progress. Keys come in through [`State::panel_key`] and
//! leave as requests, so each panel is testable by replaying keys.

use std::cell::{Cell, RefCell};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{
    PermissionMode, Question, Request, RewindScope, RuleAction, SessionId, SettingsScope,
};
use serde_json::Value;

use crate::coding::{Effect, Settings};
use crate::composer::Composer;
use crate::diff::Diff;
use crate::state::{NoteTone, Overlay, State};

#[derive(Debug, Clone, PartialEq)]
pub enum Panel {
    Help(Scroll),
    Config(ConfigUi),
    Permissions(PermUi),
    Hooks { sel: usize },
    Agents { sel: usize, open: bool },
    Mcp(McpUi),
    Instructions { sel: usize },
    Context,
    Cost,
    Rewind(RewindUi),
    Diff(DiffUi),
    Jobs { sel: usize },
    Question(QuestionUi),
    Transcript(Scroll),
}

/// A scroll position and the furthest it can go, which the renderer writes back (it alone knows
/// how many rows the text wrapped to).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scroll {
    pub pos: usize,
    pub max: Cell<usize>,
}

impl Scroll {
    /// Where the view is now, never past the end.
    pub fn at(&self) -> usize {
        self.pos.min(self.max.get())
    }

    pub fn by(&mut self, delta: isize) {
        let now = self.at() as isize;
        self.pos = (now + delta).clamp(0, self.max.get() as isize) as usize;
    }

    /// Jumps to `row`; `at` clamps it once the view knows how long it is (the length is only
    /// known after a draw, and a key can arrive before one).
    pub fn to(&mut self, row: usize) {
        self.pos = row;
    }
}

// ---- /config ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Text,
    Number,
    List,
}

/// A setting the panel always lists, whether or not any layer sets it yet.
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub key: &'static str,
    pub kind: SettingKind,
    pub help: &'static str,
}

pub const CATALOGUE: &[SettingDef] = &[
    SettingDef { key: "editor_url", kind: SettingKind::Text, help: "where file links open: vscode://file/{path}:{line}:{col}" },
    SettingDef { key: "statusline.command", kind: SettingKind::Text, help: "a command whose first output line becomes the status line" },
    SettingDef { key: "permissions.default_mode", kind: SettingKind::Text, help: "plan, default, accept_edits, bypass or dont_ask" },
    SettingDef { key: "network.allow_hosts", kind: SettingKind::List, help: "hosts the agent may reach without asking" },
];

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    pub key: String,
    pub kind: SettingKind,
    pub help: String,
    pub value: Option<Value>,
    pub source: Option<SettingsScope>,
}

/// The rows `/config` shows: the catalogue, then any other scalar or list key a layer sets.
pub fn config_rows(settings: Option<&Settings>) -> Vec<ConfigRow> {
    let mut rows: Vec<ConfigRow> = CATALOGUE
        .iter()
        .map(|d| ConfigRow {
            key: d.key.to_string(),
            kind: d.kind,
            help: d.help.to_string(),
            value: settings.and_then(|s| s.get(d.key)).cloned(),
            source: settings.and_then(|s| s.source(d.key)),
        })
        .collect();
    if let Some(s) = settings {
        for (k, (v, scope)) in &s.merged {
            if rows.iter().any(|r| &r.key == k) {
                continue;
            }
            // Rules and hooks are arrays of tables: they have their own panels.
            if v.as_array().is_some_and(|a| a.iter().any(Value::is_object)) {
                continue;
            }
            let kind = match v {
                Value::Bool(_) => SettingKind::Bool,
                Value::Number(_) => SettingKind::Number,
                Value::Array(_) => SettingKind::List,
                _ => SettingKind::Text,
            };
            rows.push(ConfigRow { key: k.clone(), kind, help: String::new(), value: Some(v.clone()), source: Some(*scope) });
        }
    }
    rows
}

/// A value as `/config` shows and edits it.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string)).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// What the user typed for a setting, as the JSON value `SetSetting` carries. Empty unsets.
pub fn parse_value(kind: SettingKind, input: &str) -> Result<Value, String> {
    let t = input.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    match kind {
        SettingKind::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "off" | "0" => Ok(Value::Bool(false)),
            _ => Err("true or false".into()),
        },
        SettingKind::Number => {
            if let Ok(i) = t.parse::<i64>() {
                Ok(Value::from(i))
            } else {
                t.parse::<f64>().ok().map(Value::from).ok_or_else(|| "a number".to_string())
            }
        }
        SettingKind::List => Ok(Value::Array(t.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| Value::String(s.into())).collect())),
        SettingKind::Text => Ok(Value::String(t.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigUi {
    pub sel: usize,
    /// The layer a change is written to; Tab cycles it.
    pub scope: SettingsScope,
    /// The value being typed.
    pub edit: Option<Composer>,
    pub error: Option<String>,
}

impl ConfigUi {
    fn new() -> Self {
        Self { sel: 0, scope: SettingsScope::User, edit: None, error: None }
    }
}

pub const WRITE_SCOPES: [SettingsScope; 3] = [SettingsScope::User, SettingsScope::Project, SettingsScope::Local];

pub fn scope_label(s: SettingsScope) -> &'static str {
    match s {
        SettingsScope::User => "user",
        SettingsScope::Project => "project",
        SettingsScope::Local => "local",
        SettingsScope::Session => "session",
    }
}

fn next_scope(s: SettingsScope) -> SettingsScope {
    let i = WRITE_SCOPES.iter().position(|x| *x == s).unwrap_or(0);
    WRITE_SCOPES[(i + 1) % WRITE_SCOPES.len()]
}

// ---- /permissions ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct PermUi {
    pub sel: usize,
    pub add: Option<AddRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddRule {
    pub action: RuleAction,
    pub scope: SettingsScope,
    pub pattern: Composer,
}

pub const ACTIONS: [RuleAction; 3] = [RuleAction::Deny, RuleAction::Ask, RuleAction::Allow];

pub fn action_label(a: RuleAction) -> &'static str {
    match a {
        RuleAction::Allow => "allow",
        RuleAction::Ask => "ask",
        RuleAction::Deny => "deny",
    }
}

/// Rules in the order they bind: deny, then ask, then allow; the daemon's order within each.
pub fn rules_ordered(rules: &[marlowe_proto::RuleInfo]) -> Vec<&marlowe_proto::RuleInfo> {
    let mut v: Vec<&marlowe_proto::RuleInfo> = rules.iter().collect();
    v.sort_by_key(|r| ACTIONS.iter().position(|a| *a == r.action).unwrap_or(9));
    v
}

// ---- /mcp ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct McpUi {
    pub sel: usize,
    pub add: Option<AddServer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddServer {
    pub scope: SettingsScope,
    /// `name command args...` or `name https://url`.
    pub line: Composer,
    pub error: Option<String>,
}

/// `name command args...` or `name https://host/mcp` as a name and `.mcp.json` server config.
pub fn parse_server(line: &str) -> Result<(String, Value), String> {
    let mut words = line.split_whitespace();
    let name = words.next().ok_or("a name first")?;
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("names use letters, digits, - and _".into());
    }
    let target = words.next().ok_or("then a command or an https:// URL")?;
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok((name.to_string(), serde_json::json!({ "type": "http", "url": target })));
    }
    let args: Vec<&str> = words.collect();
    Ok((name.to_string(), serde_json::json!({ "command": target, "args": args })))
}

// ---- /rewind ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RewindUi {
    pub sel: usize,
    /// After a turn is picked: the turn and which scope is highlighted.
    pub scope: Option<(u32, usize)>,
}

pub const REWIND_SCOPES: [(RewindScope, &str, &str); 3] = [
    (RewindScope::Both, "code and conversation", "files and the conversation go back to that turn"),
    (RewindScope::Code, "code only", "files go back; the conversation stays"),
    (RewindScope::Conversation, "conversation only", "the conversation goes back; files stay"),
];

// ---- /diff -----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DiffUi {
    pub title: String,
    pub loading: bool,
    pub diff: Option<Diff>,
    /// The reply as text when it is not a unified diff.
    pub raw: String,
    pub scroll: Scroll,
    /// Written by the renderer: the row each file starts on.
    pub starts: RefCell<Vec<usize>>,
}

impl DiffUi {
    pub fn new(title: &str, loading: bool, diff: Option<Diff>, raw: String) -> Self {
        Self { title: title.to_string(), loading, diff, raw, scroll: Scroll::default(), starts: RefCell::new(Vec::new()) }
    }
}

// ---- ask_user --------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionUi {
    pub session: SessionId,
    pub id: String,
    pub questions: Vec<Question>,
    pub q: usize,
    /// Per question: the highlighted row (options, then "Other").
    pub cursor: Vec<usize>,
    /// Per question: which options are picked.
    pub picked: Vec<Vec<bool>>,
    /// Per question: the free-text answer.
    pub other: Vec<Composer>,
    /// Typing into "Other" for the current question.
    pub typing: bool,
    /// When it came up: choice keys typed ahead of it are not answers.
    pub since_ms: i64,
}

impl QuestionUi {
    pub fn new(session: SessionId, id: String, questions: Vec<Question>) -> Self {
        let n = questions.len();
        let picked = questions.iter().map(|q| vec![false; q.options.len()]).collect();
        Self { session, id, questions, q: 0, cursor: vec![0; n], picked, other: vec![Composer::default(); n], typing: false, since_ms: 0 }
    }

    /// Rows of the current question: its options and the free-text row.
    pub fn rows(&self) -> usize {
        self.questions.get(self.q).map_or(0, |q| q.options.len() + 1)
    }

    pub fn answered(&self, q: usize) -> bool {
        self.picked[q].iter().any(|p| *p) || !self.other[q].is_blank()
    }

    /// One answer per question: the picked option labels (joined with `, ` when several), then the
    /// free text.
    pub fn answers(&self) -> Vec<String> {
        self.questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let mut parts: Vec<String> =
                    q.options.iter().zip(&self.picked[i]).filter(|(_, p)| **p).map(|(o, _)| o.clone()).collect();
                if !self.other[i].is_blank() {
                    parts.push(self.other[i].text().trim().to_string());
                }
                parts.join(", ")
            })
            .collect()
    }
}

// ---- Ctrl+O ----------------------------------------------------------------------------------------

// ---- State -----------------------------------------------------------------------------------------

impl State {
    pub fn panel(&self) -> Option<&Panel> {
        self.code.panel.as_ref()
    }

    pub(crate) fn ws(&self) -> Option<String> {
        self.shown_workspace().map(str::to_string)
    }

    fn need_session(&mut self) -> Option<SessionId> {
        let s = self.session.clone();
        if s.is_none() {
            self.flash("no session yet — send a message first", NoteTone::Dim);
        }
        s
    }

    /// Shows `p` and asks for what it lists.
    pub(crate) fn open_panel(&mut self, p: Panel, out: &mut Vec<Request>) {
        let ws = self.ws();
        match &p {
            Panel::Config(_) => out.push(Request::GetSettings { workspace: ws }),
            Panel::Permissions(_) => out.push(Request::ListRules { workspace: ws }),
            Panel::Hooks { .. } => out.push(Request::ListHooks { workspace: ws }),
            Panel::Agents { .. } => out.push(Request::ListAgentTypes { workspace: ws }),
            Panel::Mcp(_) => out.push(Request::McpList { workspace: ws }),
            Panel::Instructions { .. } => self.code.effects.push(Effect::FindInstructions { workspace: ws }),
            Panel::Cost => {
                if let Some(s) = self.session.clone() {
                    out.push(Request::SessionCost { session: s });
                }
            }
            Panel::Rewind(_) => {
                if let Some(s) = self.session.clone() {
                    out.push(Request::ListCheckpoints { session: s });
                }
            }
            Panel::Jobs { .. } => out.push(Request::ListJobs { session: self.session.clone() }),
            Panel::Help(_) | Panel::Context | Panel::Diff(_) | Panel::Question(_) | Panel::Transcript(_) => {}
        }
        self.code.panel = Some(p);
        self.overlay = Some(Overlay::Panel);
    }

    pub(crate) fn close_panel(&mut self) {
        self.code.panel = None;
        if self.overlay == Some(Overlay::Panel) {
            self.overlay = None;
        }
    }

    pub fn open_config(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Config(ConfigUi::new()), out);
    }
    pub fn open_permissions(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Permissions(PermUi { sel: 0, add: None }), out);
    }
    pub fn open_hooks(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Hooks { sel: 0 }, out);
    }
    pub fn open_agents(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Agents { sel: 0, open: false }, out);
    }
    pub fn open_mcp(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Mcp(McpUi { sel: 0, add: None }), out);
    }
    pub fn open_instructions(&mut self, out: &mut Vec<Request>) {
        self.code.instructions = None;
        self.open_panel(Panel::Instructions { sel: 0 }, out);
    }
    pub fn open_context_view(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Context, out);
    }
    pub fn open_cost(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Cost, out);
    }
    pub fn open_jobs(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Jobs { sel: 0 }, out);
    }
    pub fn open_help(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Help(Scroll::default()), out);
    }
    pub fn open_transcript(&mut self, out: &mut Vec<Request>) {
        // Between turns, fetch the arguments and results of calls the live events only summarised.
        if self.live.is_none() {
            self.refresh_tool_details(out);
        }
        self.open_panel(Panel::Transcript(Scroll::default()), out);
    }

    pub fn open_rewind(&mut self, out: &mut Vec<Request>) {
        if self.need_session().is_some() {
            self.code.checkpoints = None;
            self.open_panel(Panel::Rewind(RewindUi { sel: 0, scope: None }), out);
        }
    }

    pub fn open_diff(&mut self, turn: Option<u32>, out: &mut Vec<Request>) {
        let Some(s) = self.need_session() else { return };
        let title = match turn {
            Some(t) => format!("changes in turn {t}"),
            None => "changes in this session".to_string(),
        };
        self.open_panel(Panel::Diff(DiffUi::new(&title, true, None, String::new())), out);
        out.push(Request::SessionDiff { session: s, turn });
    }

    /// Shows a unified diff from somewhere else (a crew run's result).
    pub fn show_diff(&mut self, title: &str, text: &str, out: &mut Vec<Request>) {
        let diff = crate::diff::parse_unified(text);
        let raw = if diff.is_none() { text.to_string() } else { String::new() };
        self.open_panel(Panel::Diff(DiffUi::new(title, false, diff, raw)), out);
    }

    /// The daemon's `ask_user` arrived: it takes the screen like an approval does. One already
    /// on screen is never replaced; later ones wait behind it, and so does the panel they
    /// interrupted, which comes back afterwards.
    pub(crate) fn ask_questions(&mut self, session: &SessionId, id: &str, questions: &[Question]) {
        if questions.is_empty() {
            return;
        }
        let pending = crate::coding::PendingQuestion { session: session.clone(), id: id.to_string(), questions: questions.to_vec() };
        if matches!(self.code.panel, Some(Panel::Question(_))) {
            self.code.questions.push_back(pending);
        } else {
            self.show_question(pending);
        }
    }

    fn show_question(&mut self, q: crate::coding::PendingQuestion) {
        // Another panel (a /config edit, say) is put away, not discarded.
        if let Some(p) = self.code.panel.take() {
            if !matches!(p, Panel::Question(_)) {
                self.code.stashed_panel = Some(p);
            }
        }
        let mut ui = QuestionUi::new(q.session, q.id, q.questions);
        ui.since_ms = self.now_ms;
        self.code.panel = Some(Panel::Question(ui));
        self.overlay = Some(Overlay::Panel);
    }

    /// A question was answered or skipped: the next waits its turn on screen, or the panel it
    /// covered returns.
    fn next_question(&mut self) {
        if let Some(next) = self.code.questions.pop_front() {
            self.show_question(next);
        } else if let Some(p) = self.code.stashed_panel.take() {
            self.code.panel = Some(p);
            self.overlay = Some(Overlay::Panel);
        }
    }

    /// A reply updated data a panel is showing: keep its cursor on a row that exists.
    pub(crate) fn clamp_panel(&mut self) {
        let (rules, hooks, agents, mcp, jobs, instr, cps) = (
            self.code.rules.as_ref().map_or(0, Vec::len),
            self.code.hooks.as_ref().map_or(0, Vec::len),
            self.code.agents.as_ref().map_or(0, Vec::len),
            self.code.mcp.as_ref().map_or(0, Vec::len),
            self.code.jobs.len(),
            self.code.instructions.as_ref().map_or(0, Vec::len),
            self.code.checkpoints.as_ref().map_or(0, Vec::len),
        );
        let cfg = config_rows(self.code.settings.as_ref()).len();
        let last = |n: usize| n.saturating_sub(1);
        match self.code.panel.as_mut() {
            Some(Panel::Permissions(p)) => p.sel = p.sel.min(last(rules)),
            Some(Panel::Hooks { sel }) => *sel = (*sel).min(last(hooks)),
            Some(Panel::Agents { sel, .. }) => *sel = (*sel).min(last(agents)),
            Some(Panel::Mcp(m)) => m.sel = m.sel.min(last(mcp)),
            Some(Panel::Jobs { sel }) => *sel = (*sel).min(last(jobs)),
            Some(Panel::Instructions { sel }) => *sel = (*sel).min(last(instr)),
            Some(Panel::Rewind(r)) => r.sel = r.sel.min(last(cps)),
            Some(Panel::Config(c)) => c.sel = c.sel.min(last(cfg)),
            _ => {}
        }
    }

    /// A key while a panel is up.
    pub(crate) fn panel_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(mut panel) = self.code.panel.take() else {
            self.overlay = None;
            return;
        };
        let keep = match &mut panel {
            Panel::Help(sc) => scroll_key(k, sc),
            Panel::Config(c) => self.config_key(c, k, out),
            Panel::Permissions(p) => self.permissions_key(p, k, out),
            Panel::Hooks { sel } => self.hooks_key(sel, k, out),
            Panel::Agents { sel, open } => self.agents_key(sel, open, k),
            Panel::Mcp(m) => self.mcp_key(m, k, out),
            Panel::Instructions { sel } => self.instructions_key(sel, k, out),
            Panel::Context => self.context_view_key(k, out),
            Panel::Cost => !matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter),
            Panel::Rewind(r) => self.rewind_key(r, k, out),
            Panel::Diff(d) => diff_key(d, k),
            Panel::Jobs { sel } => self.jobs_key(sel, k, out),
            Panel::Question(q) => self.question_key(q, k, out),
            Panel::Transcript(sc) => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && k.code == KeyCode::Char('o') {
                    false
                } else {
                    scroll_up_key(k, sc)
                }
            }
        };
        let was_question = matches!(panel, Panel::Question(_));
        if keep {
            self.code.panel = Some(panel);
        } else {
            self.close_panel();
            if was_question {
                self.next_question();
            }
        }
    }

    /// Pasted text goes into whichever field the open panel is editing.
    pub(crate) fn panel_paste(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or("").trim();
        match self.code.panel.as_mut() {
            Some(Panel::Config(c)) => {
                if let Some(e) = c.edit.as_mut() {
                    e.insert_str(line);
                }
            }
            Some(Panel::Permissions(p)) => {
                if let Some(a) = p.add.as_mut() {
                    a.pattern.insert_str(line);
                }
            }
            Some(Panel::Mcp(m)) => {
                if let Some(a) = m.add.as_mut() {
                    a.line.insert_str(line);
                }
            }
            Some(Panel::Question(q)) if q.typing => {
                let i = q.q;
                q.other[i].insert_str(text.trim());
            }
            _ => {}
        }
    }

    /// The wheel scrolls the panels that read like documents.
    pub(crate) fn panel_wheel(&mut self, up: bool) {
        match self.code.panel.as_mut() {
            Some(Panel::Help(sc)) => sc.by(if up { -3 } else { 3 }),
            Some(Panel::Diff(d)) => d.scroll.by(if up { -3 } else { 3 }),
            Some(Panel::Transcript(sc)) => sc.by(if up { 3 } else { -3 }),
            _ => {}
        }
    }

    /// Ctrl+B: the running shell command keeps running as a background job.
    pub(crate) fn background_running(&mut self, out: &mut Vec<Request>) {
        let Some(session) = self.session.clone() else {
            self.flash("nothing is running", NoteTone::Dim);
            return;
        };
        let running = self.entries.iter().rev().find_map(|e| match e {
            crate::state::Entry::Tool { call_id, status: crate::state::ToolStatus::Running, .. }
                if self.code.tool_detail.get(call_id).is_some_and(|d| matches!(d.tool.as_str(), "bash" | "powershell" | "shell" | "python")) =>
            {
                Some(call_id.clone())
            }
            _ => None,
        });
        match running {
            Some(call_id) => {
                out.push(Request::DetachCall { session, call_id });
                self.flash("moved to the background · /jobs lists it", NoteTone::Dim);
            }
            None => self.flash("no command is running to move", NoteTone::Dim),
        }
    }

    pub(crate) fn slash_effort_cycle(&mut self, out: &mut Vec<Request>) {
        self.slash_effort("", out);
    }

    // -- /config

    fn config_key(&mut self, c: &mut ConfigUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let rows = config_rows(self.code.settings.as_ref());
        if let Some(input) = c.edit.as_mut() {
            match k.code {
                KeyCode::Esc => {
                    c.edit = None;
                    c.error = None;
                }
                KeyCode::Enter => {
                    let Some(row) = rows.get(c.sel) else { return true };
                    match parse_value(row.kind, input.text()) {
                        Ok(v) => {
                            out.push(Request::SetSetting { scope: c.scope, key: row.key.clone(), value: v, workspace: self.ws() });
                            c.edit = None;
                            c.error = None;
                        }
                        Err(e) => c.error = Some(format!("expected {e}")),
                    }
                }
                _ => edit_key(input, k),
            }
            return true;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => c.sel = c.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => c.sel = (c.sel + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Tab | KeyCode::BackTab => c.scope = next_scope(c.scope),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = rows.get(c.sel) {
                    if row.kind == SettingKind::Bool {
                        let cur = row.value.as_ref().and_then(Value::as_bool).unwrap_or(false);
                        out.push(Request::SetSetting { scope: c.scope, key: row.key.clone(), value: Value::Bool(!cur), workspace: self.ws() });
                    } else {
                        let mut input = Composer::default();
                        if let Some(v) = &row.value {
                            input.set(&value_text(v));
                        }
                        c.edit = Some(input);
                    }
                }
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                if let Some(row) = rows.get(c.sel).filter(|r| r.value.is_some()) {
                    out.push(Request::SetSetting { scope: c.scope, key: row.key.clone(), value: Value::Null, workspace: self.ws() });
                }
            }
            _ => {}
        }
        true
    }

    // -- /permissions

    fn permissions_key(&mut self, p: &mut PermUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if let Some(a) = p.add.as_mut() {
            match k.code {
                KeyCode::Esc => p.add = None,
                KeyCode::Left => a.action = ACTIONS[(ACTIONS.iter().position(|x| *x == a.action).unwrap_or(0) + 2) % 3],
                KeyCode::Right => a.action = ACTIONS[(ACTIONS.iter().position(|x| *x == a.action).unwrap_or(0) + 1) % 3],
                KeyCode::Tab | KeyCode::BackTab => a.scope = next_scope(a.scope),
                KeyCode::Enter => {
                    let pattern = a.pattern.text().trim().to_string();
                    if !pattern.is_empty() {
                        out.push(Request::AddRule { action: a.action, pattern, scope: a.scope, workspace: self.ws() });
                        p.add = None;
                    }
                }
                _ => edit_key(&mut a.pattern, k),
            }
            return true;
        }
        let rules = self.code.rules.clone().unwrap_or_default();
        let n = rules.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('a') => {
                p.add = Some(AddRule { action: RuleAction::Allow, scope: SettingsScope::Local, pattern: Composer::default() })
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(r) = rules_ordered(&rules).get(p.sel) {
                    out.push(Request::RemoveRule { pattern: r.pattern.clone(), scope: r.scope, workspace: self.ws() });
                }
            }
            _ => {}
        }
        true
    }

    // -- /hooks, /agents

    fn hooks_key(&mut self, sel: &mut usize, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        // A trust waits for a yes. It names the digest that was on screen; that is the digest sent.
        if let Some(digest) = self.code.hooks_confirm.clone() {
            match k.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.code.hooks_confirm = None;
                    if let Some(ws) = self.ws() {
                        out.push(Request::TrustHooks { workspace: ws, digest });
                    }
                }
                _ => {
                    self.code.hooks_confirm = None;
                    self.flash("not trusted", NoteTone::Dim);
                }
            }
            return true;
        }
        let hooks = self.code.hooks.clone().unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(hooks.len().saturating_sub(1)),
            KeyCode::Char('t') => match (hooks.get(*sel), self.ws()) {
                (Some(h), Some(_)) if h.scope != SettingsScope::User && !h.trusted => match self.code.hooks_digest.clone() {
                    // Trusting is trusting what was listed: the daemon refuses a digest that no
                    // longer matches the project's files.
                    Some(d) => self.code.hooks_confirm = Some(d),
                    None => self.flash("the daemon sent no digest for these hooks, so none can be trusted from here", NoteTone::Amber),
                },
                (Some(h), _) if h.trusted || h.scope == SettingsScope::User => {
                    self.flash("this hook already runs", NoteTone::Dim)
                }
                _ => self.flash("no workspace to trust hooks for", NoteTone::Dim),
            },
            _ => {}
        }
        true
    }

    /// The digest of the project hooks the daemon just listed. A trust waiting on an older digest
    /// is dropped: what it would trust is not what is on screen now.
    pub fn set_hooks_digest(&mut self, digest: &str) {
        if self.code.hooks_confirm.as_deref().is_some_and(|c| c != digest) {
            self.code.hooks_confirm = None;
            self.flash("the project's hooks changed · review them and press t again", NoteTone::Amber);
        }
        self.code.hooks_digest = Some(digest.to_string());
    }

    /// The daemon's account of the context window: its parts and anything it cut to fit. Shown in
    /// `/context` in place of the screen's own estimate once present.
    pub fn set_context_breakdown(&mut self, parts: Vec<(String, u64)>, notices: Vec<String>) {
        self.code.context_breakdown = Some(parts);
        self.code.context_notices = notices;
    }

    /// The daemon says `pattern` decided a call; `/permissions` marks the rule.
    pub fn set_rule_fired(&mut self, pattern: &str, what: &str) {
        self.code.rules_fired.insert(pattern.to_string(), what.to_string());
    }

    /// The project hooks `t` would trust, as the panel shows them.
    pub fn untrusted_project_hooks(&self) -> Vec<&marlowe_proto::HookInfo> {
        self.code.hooks.iter().flatten().filter(|h| h.scope != SettingsScope::User && !h.trusted).collect()
    }

    fn agents_key(&mut self, sel: &mut usize, open: &mut bool, k: KeyEvent) -> bool {
        let n = self.code.agents.as_ref().map_or(0, Vec::len);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => *open = !*open,
            _ => {}
        }
        true
    }

    // -- /mcp

    fn mcp_key(&mut self, m: &mut McpUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if let Some(a) = m.add.as_mut() {
            match k.code {
                KeyCode::Esc => m.add = None,
                KeyCode::Tab | KeyCode::BackTab => a.scope = next_scope(a.scope),
                KeyCode::Enter => match parse_server(a.line.text()) {
                    Ok((name, config)) => {
                        out.push(Request::McpAdd { name, scope: a.scope, config, workspace: self.ws() });
                        m.add = None;
                    }
                    Err(e) => a.error = Some(e),
                },
                _ => {
                    a.error = None;
                    edit_key(&mut a.line, k)
                }
            }
            return true;
        }
        let servers = self.code.mcp.clone().unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => m.sel = m.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => m.sel = (m.sel + 1).min(servers.len().saturating_sub(1)),
            KeyCode::Char('a') => m.add = Some(AddServer { scope: SettingsScope::Project, line: Composer::default(), error: None }),
            KeyCode::Char('r') => out.push(Request::McpList { workspace: self.ws() }),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(s) = servers.get(m.sel) {
                    out.push(Request::McpRemove { name: s.name.clone(), scope: s.scope, workspace: self.ws() });
                }
            }
            _ => {}
        }
        true
    }

    // -- /memory

    fn instructions_key(&mut self, sel: &mut usize, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let files = self.code.instructions.clone().unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(files.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(f) = files.get(*sel) {
                    self.code.effects.push(Effect::OpenEditor { path: f.path.clone() });
                }
            }
            KeyCode::Char('r') => {
                if let Some(s) = self.need_session() {
                    out.push(Request::ReloadInstructions { session: s });
                    self.flash("instruction files reloaded", NoteTone::Dim);
                }
            }
            _ => {}
        }
        true
    }

    /// The editor exited: tell the conductor to re-read what it may have changed.
    pub(crate) fn editor_closed(&mut self, path: &str, result: &Result<(), String>, out: &mut Vec<Request>) {
        match result {
            Ok(()) => {
                if let Some(s) = self.session.clone() {
                    out.push(Request::ReloadInstructions { session: s });
                    self.flash(format!("reloaded after editing {}", crate::text::short_path(path)), NoteTone::Dim);
                } else {
                    self.flash(format!("edited {}; it loads with the next session", crate::text::short_path(path)), NoteTone::Dim);
                }
            }
            Err(e) => self.flash(format!("the editor failed: {}", crate::text::clean(e)), NoteTone::Amber),
        }
    }

    // -- /context

    fn context_view_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Char('c') => {
                if let Some(s) = self.need_session() {
                    out.push(Request::Compact { session: s, focus: None });
                    self.flash("compacting…", NoteTone::Dim);
                    return false;
                }
            }
            KeyCode::Char('p') => {
                self.open_context_for_conductor(out);
                return false;
            }
            _ => {}
        }
        true
    }

    // -- /rewind

    fn rewind_key(&mut self, r: &mut RewindUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let cps = self.code.checkpoints.clone().unwrap_or_default();
        if let Some((turn, sel)) = r.scope.as_mut() {
            match k.code {
                KeyCode::Esc => r.scope = None,
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Left => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Right => *sel = (*sel + 1).min(REWIND_SCOPES.len() - 1),
                KeyCode::Enter => {
                    if let Some(s) = self.session.clone() {
                        out.push(Request::Rewind { session: s, turn: *turn, scope: REWIND_SCOPES[*sel].0 });
                    }
                    return false;
                }
                _ => {}
            }
            return true;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => r.sel = r.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => r.sel = (r.sel + 1).min(cps.len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(c) = cps.get(r.sel) {
                    r.scope = Some((c.turn, 0));
                }
            }
            _ => {}
        }
        true
    }

    // -- /jobs

    fn jobs_key(&mut self, sel: &mut usize, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let n = self.code.jobs.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(j) = self.code.jobs.get(*sel).filter(|j| j.exit_code.is_none()) {
                    out.push(Request::KillJob { id: j.id.clone() });
                }
            }
            _ => {}
        }
        true
    }

    // -- ask_user

    fn question_key(&mut self, q: &mut QuestionUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        // What was being typed in the Message box as the question arrived is not an answer.
        let ahead = self.now_ms.saturating_sub(q.since_ms) < crate::state::APPROVAL_GUARD_MS;
        if ahead && !q.typing && matches!(k.code, KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('0'..='9')) {
            return true;
        }
        let n_opts = q.questions[q.q].options.len();
        let multi = q.questions[q.q].multi_select;
        if q.typing {
            match k.code {
                KeyCode::Esc => q.typing = false,
                KeyCode::Enter => {
                    q.typing = false;
                    return self.question_advance(q, out);
                }
                _ => edit_key(&mut q.other[q.q], k),
            }
            return true;
        }
        match k.code {
            KeyCode::Esc => {
                // Declined: the model gets no answers and decides what to do.
                out.push(Request::AnswerQuestion { session: q.session.clone(), id: q.id.clone(), answers: Vec::new() });
                self.flash("question skipped", NoteTone::Dim);
                return false;
            }
            KeyCode::Up | KeyCode::Char('k') => q.cursor[q.q] = q.cursor[q.q].saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => q.cursor[q.q] = (q.cursor[q.q] + 1).min(n_opts),
            KeyCode::Left | KeyCode::BackTab => q.q = q.q.saturating_sub(1),
            KeyCode::Right | KeyCode::Tab => q.q = (q.q + 1).min(q.questions.len() - 1),
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let i = c as usize - '1' as usize;
                if i < n_opts {
                    q.cursor[q.q] = i;
                    pick(q, i, multi);
                    if !multi {
                        return self.question_advance(q, out);
                    }
                }
            }
            KeyCode::Char(' ') => {
                let i = q.cursor[q.q];
                if i < n_opts {
                    pick(q, i, multi);
                } else {
                    q.typing = true;
                }
            }
            KeyCode::Enter => {
                let i = q.cursor[q.q];
                if i == n_opts {
                    q.typing = true;
                } else if multi {
                    if !q.answered(q.q) {
                        pick(q, i, true);
                    }
                    return self.question_advance(q, out);
                } else {
                    pick(q, i, false);
                    return self.question_advance(q, out);
                }
            }
            KeyCode::Char(c) if q.cursor[q.q] == n_opts && !k.modifiers.contains(KeyModifiers::CONTROL) => {
                // Typing on the free-text row starts the answer.
                q.typing = true;
                q.other[q.q].insert(c);
            }
            _ => {}
        }
        true
    }

    /// Next unanswered question, or send everything once all are answered.
    fn question_advance(&mut self, q: &mut QuestionUi, out: &mut Vec<Request>) -> bool {
        if let Some(next) = (0..q.questions.len()).map(|i| (q.q + 1 + i) % q.questions.len()).find(|i| !q.answered(*i)) {
            q.q = next;
            return true;
        }
        out.push(Request::AnswerQuestion { session: q.session.clone(), id: q.id.clone(), answers: q.answers() });
        false
    }

    // -- permission mode

    /// Shift+Tab: the next mode (default, accept edits, plan).
    pub fn cycle_mode(&mut self, out: &mut Vec<Request>) {
        let next = self.code.mode.cycle();
        self.set_mode(next, out);
    }

    pub fn set_mode(&mut self, mode: PermissionMode, out: &mut Vec<Request>) {
        self.code.mode = mode;
        match self.session.clone() {
            Some(session) => out.push(Request::SetMode { session, mode }),
            None => self.code.pending_mode = Some(mode),
        }
        self.code.statusline.dirty = true;
    }
}

/// Edits a one-line field.
fn edit_key(c: &mut Composer, k: KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Backspace if ctrl => c.backspace_word(),
        KeyCode::Backspace => c.backspace(),
        KeyCode::Delete => c.delete(),
        KeyCode::Left => c.left(),
        KeyCode::Right => c.right(),
        KeyCode::Home => c.home(),
        KeyCode::End => c.end(),
        KeyCode::Char('u') if ctrl => c.clear(),
        KeyCode::Char('w') if ctrl => c.backspace_word(),
        KeyCode::Char(ch) if !ctrl => c.insert(ch),
        _ => {}
    }
}

fn pick(q: &mut QuestionUi, i: usize, multi: bool) {
    if multi {
        q.picked[q.q][i] = !q.picked[q.q][i];
    } else {
        for (j, p) in q.picked[q.q].iter_mut().enumerate() {
            *p = j == i;
        }
        q.other[q.q].clear();
    }
}

/// Scrolls a top-anchored view. False closes.
fn scroll_key(k: KeyEvent, sc: &mut Scroll) -> bool {
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return false,
        KeyCode::Up | KeyCode::Char('k') => sc.by(-1),
        KeyCode::Down | KeyCode::Char('j') => sc.by(1),
        KeyCode::PageUp => sc.by(-10),
        KeyCode::PageDown => sc.by(10),
        KeyCode::Home => sc.to(0),
        KeyCode::End => sc.to(usize::MAX),
        _ => {}
    }
    true
}

/// Scrolls a bottom-anchored view: `pos` counts rows up from the end. False closes.
fn scroll_up_key(k: KeyEvent, sc: &mut Scroll) -> bool {
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return false,
        KeyCode::Up | KeyCode::Char('k') => sc.by(1),
        KeyCode::Down | KeyCode::Char('j') => sc.by(-1),
        KeyCode::PageUp => sc.by(15),
        KeyCode::PageDown => sc.by(-15),
        KeyCode::Home => sc.to(usize::MAX),
        KeyCode::End => sc.to(0),
        _ => {}
    }
    true
}

fn diff_key(d: &mut DiffUi, k: KeyEvent) -> bool {
    let top = d.scroll.at();
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return false,
        KeyCode::Up | KeyCode::Char('k') => d.scroll.by(-1),
        KeyCode::Down | KeyCode::Char('j') => d.scroll.by(1),
        KeyCode::PageUp => d.scroll.by(-15),
        KeyCode::PageDown => d.scroll.by(15),
        KeyCode::Home | KeyCode::Char('g') => d.scroll.to(0),
        KeyCode::End | KeyCode::Char('G') => d.scroll.to(usize::MAX),
        KeyCode::Char('n') => {
            let next = d.starts.borrow().iter().copied().find(|s| *s > top);
            if let Some(n) = next {
                d.scroll.to(n);
            }
        }
        KeyCode::Char('p') => {
            let prev = d.starts.borrow().iter().copied().rev().find(|s| *s < top);
            d.scroll.to(prev.unwrap_or(0));
        }
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_by_kind_and_empty_unsets() {
        assert_eq!(parse_value(SettingKind::Bool, "yes"), Ok(Value::Bool(true)));
        assert!(parse_value(SettingKind::Bool, "maybe").is_err());
        assert_eq!(parse_value(SettingKind::Number, "42"), Ok(serde_json::json!(42)));
        assert_eq!(parse_value(SettingKind::List, "a, b ,,c"), Ok(serde_json::json!(["a", "b", "c"])));
        assert_eq!(parse_value(SettingKind::Text, "  "), Ok(Value::Null));
    }

    #[test]
    fn a_server_line_is_a_name_and_a_config() {
        let (n, c) = parse_server("files npx -y @mcp/files /tmp").unwrap();
        assert_eq!(n, "files");
        assert_eq!(c, serde_json::json!({"command": "npx", "args": ["-y", "@mcp/files", "/tmp"]}));
        let (_, c) = parse_server("docs https://x.dev/mcp").unwrap();
        assert_eq!(c, serde_json::json!({"type": "http", "url": "https://x.dev/mcp"}));
        assert!(parse_server("bad!name x").is_err());
        assert!(parse_server("justaname").is_err());
    }

    #[test]
    fn rules_bind_deny_first() {
        let r = |a, p: &str| marlowe_proto::RuleInfo { action: a, pattern: p.into(), scope: SettingsScope::User, source: None };
        let rules = vec![r(RuleAction::Allow, "a"), r(RuleAction::Deny, "d"), r(RuleAction::Ask, "k")];
        let order: Vec<&str> = rules_ordered(&rules).iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(order, ["d", "k", "a"]);
    }

    #[test]
    fn answers_join_picks_and_free_text() {
        let q = Question { question: "Which?".into(), header: "pick".into(), options: vec!["a".into(), "b".into(), "c".into()], multi_select: true, option_details: vec![] };
        let mut ui = QuestionUi::new(SessionId::from("s"), "q1".into(), vec![q]);
        ui.picked[0][0] = true;
        ui.picked[0][2] = true;
        ui.other[0].set("and d");
        assert_eq!(ui.answers(), ["a, c, and d"]);
    }
}
