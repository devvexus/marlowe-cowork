//! Client state and the event reducer. Everything the screen shows is derived from [`State`];
//! everything that changes it goes through [`State::on_client`] or the input handlers in
//! `input.rs`. Handlers return the requests to send, so the whole client is testable without a
//! socket or a terminal.

use std::cell::Cell;

use marlowe_model::{Part, Role, Usage};
use marlowe_proto::{
    ApprovalDecision, ApprovalId, Envelope, Event, Request, SessionId, SessionSummary, StoredTurn,
    TurnStop,
};

use crate::composer::Composer;
use crate::library::Library;

/// What the connection thread reports.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Envelope(Envelope),
    /// The connection dropped or could not be made; the client keeps retrying.
    Disconnected { reason: String, attempt: u32 },
    /// The daemon refused the token.
    Refused(String),
}

/// Connection status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Online { daemon_version: String },
    Offline { reason: String, attempt: u32 },
    Refused(String),
}

impl Conn {
    pub fn is_online(&self) -> bool {
        matches!(self, Conn::Online { .. })
    }
}

/// A tool line's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done(String),
    Failed(String),
}

/// The colour a note carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteTone {
    Dim,
    Amber,
    Red,
}

/// One item in the conversation pane.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// The user's words. `steer` marks text sent while a turn was running.
    User { text: String, steer: bool },
    /// Marlowe's prose (markdown).
    Prose { text: String },
    /// Reasoning. `live` while it is still streaming.
    Thinking { text: String, tokens: u64, live: bool },
    Tool { call_id: String, summary: String, status: ToolStatus },
    Note { text: String, tone: NoteTone },
    /// Usage footer for a finished turn.
    Usage { usage: Usage },
}

/// A turn in flight in the open session.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTurn {
    pub turn: u32,
    pub model: marlowe_model::ModelRef,
    pub started_ms: i64,
}

/// A pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub id: ApprovalId,
    pub session: SessionId,
    pub tool: String,
    pub summary: String,
    pub detail: String,
}

/// Full-screen views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Conversation,
    Library,
}

/// A palette command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewSession,
    Sessions,
    Library,
    ToggleThinking,
    Interrupt,
    Quit,
}

impl Command {
    pub const ALL: [Command; 6] = [
        Command::NewSession,
        Command::Sessions,
        Command::Library,
        Command::ToggleThinking,
        Command::Interrupt,
        Command::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::NewSession => "New session",
            Command::Sessions => "Sessions",
            Command::Library => "Library — models and slots",
            Command::ToggleThinking => "Show or hide thinking",
            Command::Interrupt => "Interrupt the running turn",
            Command::Quit => "Quit",
        }
    }

    pub fn chord(self) -> &'static str {
        match self {
            Command::NewSession => "^n",
            Command::Sessions => "^o",
            Command::Library => "^l",
            Command::ToggleThinking => "^t",
            Command::Interrupt => "esc",
            Command::Quit => "^q",
        }
    }
}

/// Floating panels above the current view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Palette { query: String, sel: usize },
    Sessions { sel: usize },
    ModelPicker { sel: usize },
}

/// A transient one-line message under the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub tone: NoteTone,
}

/// Approval modal buttons, left to right.
pub const APPROVAL_CHOICES: [ApprovalDecision; 3] =
    [ApprovalDecision::Allow, ApprovalDecision::AllowForSession, ApprovalDecision::Deny];
/// The modal's default selection: Deny.
pub const APPROVAL_DENY: usize = 2;
/// Decision keys arriving this soon after an approval comes up are type-ahead and ignored.
pub const APPROVAL_GUARD_MS: i64 = 400;
/// Detail lines the approval modal shows at once.
pub const APPROVAL_DETAIL_ROWS: usize = 10;
/// Wrap width of the approval detail.
pub const APPROVAL_DETAIL_WIDTH: usize = 72;

impl Approval {
    /// The detail as the modal shows it: cleaned, `key  value` lines for JSON args, wrapped.
    pub fn detail_lines(&self) -> Vec<String> {
        crate::text::wrap(&crate::text::clean(&readable_detail(&self.detail)), APPROVAL_DETAIL_WIDTH)
    }

    /// Largest useful detail scroll.
    pub fn detail_scroll_max(&self) -> usize {
        self.detail_lines().len().saturating_sub(APPROVAL_DETAIL_ROWS)
    }
}

/// Tool arguments as `key  value` lines when they are a JSON object; otherwise as given.
fn readable_detail(detail: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(detail.trim()) else {
        return detail.trim().to_string();
    };
    let kw = map.keys().map(|k| crate::text::width(k)).max().unwrap_or(0);
    let mut out = Vec::new();
    for (k, v) in &map {
        let v = match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut lines = v.lines();
        out.push(format!("{}  {}", crate::text::pad(k, kw), lines.next().unwrap_or("")));
        for l in lines {
            out.push(format!("{}  {l}", " ".repeat(kw)));
        }
    }
    out.join("\n")
}

/// Everything the client knows.
#[derive(Debug, Clone)]
pub struct State {
    pub conn: Conn,
    /// Highest broadcast seq applied.
    pub last_seq: u64,
    /// Set by a Welcome; the next broadcast checks for a replay gap.
    check_gap: bool,
    pub profile_home: Option<String>,

    pub session: Option<SessionId>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    /// Workspace for a new session: the directory the client was started in.
    pub default_workspace: Option<String>,
    pub entries: Vec<Entry>,
    pub live: Option<LiveTurn>,
    /// Text sent with no session yet, shown until its TurnStarted arrives.
    pub pending_user: Option<String>,
    awaiting_new: bool,
    pending_open: Option<SessionId>,

    pub approvals: Vec<Approval>,
    pub approval_sel: usize,
    /// When the front approval came up (`now_ms`); decision keys before the guard are ignored.
    pub approval_since_ms: i64,
    /// Detail lines scrolled past in the front approval.
    pub approval_scroll: usize,
    /// The whole detail of the front approval has been on screen; allow is enabled.
    pub approval_read: bool,
    pub sessions: Option<Vec<SessionSummary>>,
    pub library: Library,

    pub view: View,
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    pub show_thinking: bool,
    /// Lines scrolled up from the bottom of the conversation; 0 follows the stream.
    pub scroll: usize,
    /// Largest useful scroll, written back by the renderer.
    pub scroll_max: Cell<usize>,
    pub flash: Option<Flash>,

    /// Wall clock for the title bar, e.g. `08:14`. Set by the run loop.
    pub clock: String,
    /// Unix ms, for relative times and the meter. Set by the run loop.
    pub now_ms: i64,
    pub quit: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::new(None)
    }
}

impl State {
    pub fn new(default_workspace: Option<String>) -> Self {
        Self {
            conn: Conn::Connecting,
            last_seq: 0,
            check_gap: false,
            profile_home: None,
            session: None,
            title: None,
            workspace: None,
            default_workspace,
            entries: Vec::new(),
            live: None,
            pending_user: None,
            awaiting_new: false,
            pending_open: None,
            approvals: Vec::new(),
            approval_sel: APPROVAL_DENY,
            approval_since_ms: 0,
            approval_scroll: 0,
            approval_read: false,
            sessions: None,
            library: Library::default(),
            view: View::Conversation,
            overlay: None,
            composer: Composer::default(),
            show_thinking: false,
            scroll: 0,
            scroll_max: Cell::new(0),
            flash: None,
            clock: String::new(),
            now_ms: 0,
            quit: false,
        }
    }

    /// The workspace shown in the title bar.
    pub fn shown_workspace(&self) -> Option<&str> {
        self.workspace.as_deref().or(self.default_workspace.as_deref())
    }

    pub fn is_busy(&self) -> bool {
        self.live.is_some()
    }

    pub fn current_approval(&self) -> Option<&Approval> {
        self.approvals.first()
    }

    /// Resets the modal for whatever approval is now at the front: Deny selected, detail at the
    /// top, the type-ahead guard restarted.
    pub(crate) fn front_approval_changed(&mut self) {
        self.approval_sel = APPROVAL_DENY;
        self.approval_since_ms = self.now_ms;
        self.approval_scroll = 0;
        self.approval_read = self.approvals.first().is_some_and(|a| a.detail_scroll_max() == 0);
    }

    pub(crate) fn flash(&mut self, text: impl Into<String>, tone: NoteTone) {
        self.flash = Some(Flash { text: text.into(), tone });
    }

    fn mine(&self, s: &SessionId) -> bool {
        self.session.as_ref() == Some(s)
    }

    /// Clears the conversation for a new session.
    pub(crate) fn reset_conversation(&mut self) {
        self.session = None;
        self.title = None;
        self.workspace = None;
        self.entries.clear();
        self.live = None;
        self.pending_user = None;
        self.awaiting_new = false;
        self.scroll = 0;
    }

    /// Marks that an Ask with no session was sent; the next new session is ours.
    pub(crate) fn expect_new_session(&mut self, text: &str) {
        self.awaiting_new = true;
        self.pending_user = Some(text.to_string());
    }

    pub(crate) fn expect_open(&mut self, s: SessionId) {
        self.pending_open = Some(s);
    }

    /// Applies one connection event. Returns requests to send.
    pub fn on_client(&mut self, ev: ClientEvent) -> Vec<Request> {
        match ev {
            ClientEvent::Envelope(env) => self.apply(&env),
            ClientEvent::Disconnected { reason, attempt } => {
                self.conn = Conn::Offline { reason, attempt };
                Vec::new()
            }
            ClientEvent::Refused(msg) => {
                self.conn = Conn::Refused(msg);
                Vec::new()
            }
        }
    }

    /// Applies one daemon event. Broadcast events at or below the last applied seq are replay
    /// overlap and are skipped; seq 0 replies always apply.
    pub fn apply(&mut self, env: &Envelope) -> Vec<Request> {
        let mut out = Vec::new();
        if env.seq != 0 {
            if env.seq <= self.last_seq {
                return out;
            }
            if self.check_gap {
                self.check_gap = false;
                if self.last_seq > 0 && env.seq > self.last_seq + 1 {
                    // Events were lost while offline: reload the open session.
                    self.reload(&mut out);
                }
            }
            self.last_seq = env.seq;
        }
        self.event(&env.event, env.at_ms, env.seq == 0, &mut out);
        out
    }

    fn reload(&mut self, out: &mut Vec<Request>) {
        if let Some(s) = self.session.clone() {
            self.live = None;
            self.pending_open = Some(s.clone());
            out.push(Request::OpenSession { session: s });
        }
    }

    /// `reply` is true for `seq: 0` events, which answer this client's own requests.
    fn event(&mut self, ev: &Event, at_ms: i64, reply: bool, out: &mut Vec<Request>) {
        match ev {
            Event::Welcome { daemon_version, profile_home, seq, .. } => {
                let restarted = *seq < self.last_seq;
                self.conn = Conn::Online { daemon_version: daemon_version.clone() };
                self.profile_home = Some(profile_home.clone());
                if restarted {
                    self.last_seq = *seq;
                    if self.live.take().is_some() {
                        self.note("the daemon restarted; the turn was lost", NoteTone::Amber);
                    }
                    self.approvals.clear();
                    self.reload(out);
                } else {
                    self.check_gap = true;
                }
                out.push(Request::LibraryState);
            }
            Event::SessionOpened { session, title, workspace } => {
                // Only a seq-0 reply is the session our Ask created; a broadcast may be another
                // client's new session.
                let adopt = self.pending_open.as_ref() == Some(session)
                    || (reply && self.awaiting_new && self.session.is_none())
                    || self.mine(session);
                if adopt {
                    if !self.mine(session) && self.pending_open.as_ref() != Some(session) {
                        self.entries.clear();
                    }
                    self.session = Some(session.clone());
                    self.title = Some(title.clone()).filter(|t| !t.is_empty());
                    self.workspace = workspace.clone();
                    self.awaiting_new = false;
                }
            }
            Event::SessionList { sessions } => {
                self.sessions = Some(sessions.clone());
                if let Some(Overlay::Sessions { sel }) = &mut self.overlay {
                    *sel = (*sel).min(sessions.len().saturating_sub(1));
                }
            }
            Event::Transcript { session, turns } => {
                if self.pending_open.as_ref() == Some(session) || self.mine(session) {
                    self.pending_open = None;
                    self.session = Some(session.clone());
                    self.entries = entries_from_transcript(turns);
                    self.live = None;
                    self.pending_user = None;
                    self.scroll = 0;
                    self.view = View::Conversation;
                }
            }
            Event::TurnStarted { session, turn, model, user } => {
                if self.mine(session) {
                    if self.title.is_none() {
                        self.title = Some(first_line(user));
                    }
                    self.close_thinking();
                    self.pending_user = None;
                    // A steer typed here is already on screen; do not show it twice.
                    let shown = matches!(self.entries.last(),
                        Some(Entry::User { text, .. }) if text == user);
                    if !shown {
                        self.entries.push(Entry::User { text: user.clone(), steer: false });
                    }
                    self.live = Some(LiveTurn { turn: *turn, model: model.clone(), started_ms: at_ms });
                    self.scroll = 0;
                }
            }
            Event::TextDelta { session, text, .. } if self.mine(session) => {
                self.close_thinking();
                match self.entries.last_mut() {
                    Some(Entry::Prose { text: t }) => t.push_str(text),
                    _ => self.entries.push(Entry::Prose { text: text.clone() }),
                }
            }
            // `tokens` counts this delta only. A provider may settle the count with an empty delta
            // after the answer began; that goes to this turn's last thinking line.
            Event::ReasoningDelta { session, text, tokens, .. } if self.mine(session) && text.is_empty() => {
                let open = self.entries.iter_mut().rev().take_while(|e| !matches!(e, Entry::User { .. }));
                for e in open {
                    if let Entry::Thinking { tokens: n, .. } = e {
                        *n += *tokens;
                        break;
                    }
                }
            }
            Event::ReasoningDelta { session, text, tokens, .. } if self.mine(session) => {
                match self.entries.last_mut() {
                    Some(Entry::Thinking { text: t, tokens: n, live: true }) => {
                        t.push_str(text);
                        *n += *tokens;
                    }
                    _ => self.entries.push(Entry::Thinking {
                        text: text.clone(),
                        tokens: *tokens,
                        live: true,
                    }),
                }
            }
            Event::TextRetracted { session, tokens, .. } if self.mine(session) => {
                if let Some(Entry::Prose { .. }) = self.entries.last() {
                    let Some(Entry::Prose { text }) = self.entries.pop() else { unreachable!() };
                    match self.entries.last_mut() {
                        Some(Entry::Thinking { text: t, tokens: n, .. }) => {
                            t.push_str(&text);
                            *n += tokens;
                        }
                        _ => self.entries.push(Entry::Thinking { text, tokens: *tokens, live: false }),
                    }
                }
            }
            Event::ToolStarted { session, call_id, tool, summary, .. } if self.mine(session) => {
                self.close_thinking();
                let summary = if summary.trim().is_empty() { tool.clone() } else { summary.clone() };
                self.entries.push(Entry::Tool {
                    call_id: call_id.clone(),
                    summary,
                    status: ToolStatus::Running,
                });
            }
            Event::ToolFinished { session, call_id, ok, summary, .. } if self.mine(session) => {
                let status = if *ok {
                    ToolStatus::Done(summary.clone())
                } else {
                    ToolStatus::Failed(summary.clone())
                };
                let found = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { call_id: c, status: s, .. } if c == call_id => Some(s),
                    _ => None,
                });
                match found {
                    Some(s) => *s = status,
                    None => self.entries.push(Entry::Tool {
                        call_id: call_id.clone(),
                        summary: call_id.clone(),
                        status,
                    }),
                }
            }
            Event::TurnFinished { session, usage, stop, .. } if self.mine(session) => {
                self.close_thinking();
                for e in &mut self.entries {
                    if let Entry::Tool { status: s @ ToolStatus::Running, .. } = e {
                        *s = ToolStatus::Failed("no result".into());
                    }
                }
                match stop {
                    TurnStop::Done | TurnStop::Error => {}
                    TurnStop::Interrupted => self.note("interrupted", NoteTone::Amber),
                    TurnStop::StepLimit => self.note("stopped at the step limit", NoteTone::Amber),
                    TurnStop::Budget => self.note("stopped at the budget", NoteTone::Amber),
                }
                if usage.total() > 0 {
                    self.entries.push(Entry::Usage { usage: *usage });
                }
                self.live = None;
            }
            Event::ApprovalRequested { approval, session, tool, summary, detail } => {
                if !self.approvals.iter().any(|a| a.id == *approval) {
                    let first = self.approvals.is_empty();
                    self.approvals.push(Approval {
                        id: *approval,
                        session: session.clone(),
                        tool: tool.clone(),
                        summary: summary.clone(),
                        detail: detail.clone(),
                    });
                    if first {
                        self.front_approval_changed();
                    }
                }
            }
            Event::ApprovalResolved { approval, .. } => {
                let was_front = self.approvals.first().map(|a| a.id) == Some(*approval);
                self.approvals.retain(|a| a.id != *approval);
                if was_front {
                    self.front_approval_changed();
                }
            }
            Event::LibraryState { models, slots, providers } => {
                self.library.set_state(models.clone(), slots.clone(), providers.clone());
            }
            Event::Catalog { provider, models } => {
                self.library.set_catalog(*provider, models.clone());
            }
            Event::Status(_) => {}
            Event::Error { session: None, message } if reply && message.starts_with("replay gap") => {
                // The daemon skipped the replay: reload what is open instead.
                self.check_gap = false;
                self.reload(out);
            }
            Event::Error { session, message } => {
                let text = crate::text::clean(message);
                if self.library.catalog_error(&text) {
                    return;
                }
                match session {
                    Some(s) if self.mine(s) => self.note(&text, NoteTone::Red),
                    Some(_) => {}
                    None => self.flash(text, NoteTone::Red),
                }
                if self.pending_user.is_some() && session.is_none() && self.session.is_none() {
                    // The ask failed before a session existed: give the text back.
                    if let Some(t) = self.pending_user.take() {
                        if self.composer.is_blank() {
                            self.composer.set(&t);
                        }
                    }
                    self.awaiting_new = false;
                }
            }
            Event::ShuttingDown => {
                self.conn = Conn::Offline { reason: "the daemon is shutting down".into(), attempt: 0 };
            }
            // Session events for other sessions.
            _ => {}
        }
    }

    pub(crate) fn note(&mut self, text: &str, tone: NoteTone) {
        self.entries.push(Entry::Note { text: text.to_string(), tone });
    }

    fn close_thinking(&mut self) {
        for e in self.entries.iter_mut().rev() {
            if let Entry::Thinking { live, .. } = e {
                *live = false;
                break;
            }
        }
    }

    /// What Marlowe is doing right now, for the title bar.
    pub fn activity(&self) -> Activity {
        if !self.conn.is_online() {
            return match self.conn {
                Conn::Connecting => Activity::Connecting,
                _ => Activity::Offline,
            };
        }
        if !self.approvals.is_empty() {
            return Activity::Awaiting;
        }
        if self.live.is_none() {
            return Activity::Idle;
        }
        for e in self.entries.iter().rev() {
            match e {
                Entry::Tool { status: ToolStatus::Running, .. } => return Activity::Running,
                Entry::Thinking { live: true, .. } => return Activity::Thinking,
                Entry::Prose { .. } => return Activity::Writing,
                Entry::User { .. } => return Activity::Thinking,
                _ => {}
            }
        }
        Activity::Thinking
    }
}

/// The title bar's status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Connecting,
    Offline,
    Idle,
    Thinking,
    Writing,
    Running,
    Awaiting,
}

impl Activity {
    pub fn label(self) -> &'static str {
        match self {
            Activity::Connecting => "connecting",
            Activity::Offline => "offline",
            Activity::Idle => "idle",
            Activity::Thinking => "thinking",
            Activity::Writing => "writing",
            Activity::Running => "working",
            Activity::Awaiting => "needs you",
        }
    }

    pub fn moving(self) -> bool {
        matches!(self, Activity::Thinking | Activity::Writing | Activity::Running)
    }
}

fn first_line(s: &str) -> String {
    crate::text::truncate(s.lines().next().unwrap_or("").trim(), 60)
}

/// Rebuilds conversation entries from a stored transcript.
pub fn entries_from_transcript(turns: &[StoredTurn]) -> Vec<Entry> {
    let mut out = Vec::new();
    for t in turns {
        match t.role {
            Role::User => {
                let text: String = t
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    out.push(Entry::User { text, steer: false });
                }
            }
            Role::Assistant => {
                for p in &t.content {
                    match p {
                        Part::Thinking(th) if !th.text.trim().is_empty() => {
                            out.push(Entry::Thinking {
                                text: th.text.clone(),
                                tokens: crate::text::width(&th.text) as u64 / 4,
                                live: false,
                            })
                        }
                        Part::Text { text } if !text.trim().is_empty() => {
                            out.push(Entry::Prose { text: text.clone() })
                        }
                        Part::ToolCall(c) => out.push(Entry::Tool {
                            call_id: c.id.clone(),
                            summary: describe_call(&c.name, &c.args),
                            status: ToolStatus::Running,
                        }),
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for r in t.content.iter().filter_map(|p| match p {
                    Part::ToolResult(r) => Some(r),
                    _ => None,
                }) {
                    let lines = r.content.lines().count();
                    let summary = if r.is_error {
                        crate::text::truncate(r.content.lines().next().unwrap_or("failed"), 60)
                    } else {
                        format!("{lines} line{}", if lines == 1 { "" } else { "s" })
                    };
                    for e in out.iter_mut().rev() {
                        if let Entry::Tool { call_id, status, .. } = e {
                            if *call_id == r.call_id {
                                *status = if r.is_error {
                                    ToolStatus::Failed(summary.clone())
                                } else {
                                    ToolStatus::Done(summary.clone())
                                };
                                break;
                            }
                        }
                    }
                }
            }
            Role::System => {}
        }
    }
    for e in &mut out {
        if let Entry::Tool { status: s @ ToolStatus::Running, .. } = e {
            *s = ToolStatus::Done(String::new());
        }
    }
    out
}

/// `read src/main.rs` from a stored call: the tool name and its first string argument.
fn describe_call(name: &str, args: &serde_json::Value) -> String {
    let first = args.as_object().and_then(|o| o.values().find_map(|v| v.as_str()));
    match first {
        Some(a) => format!("{name} {}", crate::text::truncate(a.lines().next().unwrap_or(""), 60)),
        None => name.to_string(),
    }
}
