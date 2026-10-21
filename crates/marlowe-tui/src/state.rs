//! Client state and the event reducer. Everything the screen shows is derived from [`State`];
//! everything that changes it goes through [`State::on_client`] or the input handlers in
//! `input.rs`. Handlers return the requests to send, so the whole client is testable without a
//! socket or a terminal.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use marlowe_model::{Part, Role, Usage};
use marlowe_proto::{
    ApprovalDecision, ApprovalId, Envelope, Event, PhaseSpan, Request, RunId, SessionId, SessionSummary, StageMs,
    StoredTurn, TurnNumbers, TurnStop,
};

use crate::composer::Composer;
use crate::crew::Crew;
use crate::library::Library;
use crate::memory::{ImportStatus, LegacyOffer, MemoryState};
use crate::onboard::{Onboard, UserProfile};
use crate::wake::Wake;

/// What the connection thread reports.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Envelope(Envelope),
    /// The connection dropped or could not be made; the client keeps retrying.
    Disconnected { reason: String, attempt: u32 },
    /// The daemon refused the token.
    Refused(String),
    /// A worker finished something the state asked for (see [`crate::coding::Effect`]).
    Local(crate::coding::LocalEvent),
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
    /// Reasoning. `live` while it is still streaming; `ms` is how long it took, from its first
    /// delta to what closed it (0 when not known, as in a stored transcript).
    ///
    /// `read` is what the model call that began it read (its header says so).
    Thinking { text: String, tokens: u64, live: bool, ms: u64, read: Option<crate::live::Read> },
    Tool { call_id: String, summary: String, status: ToolStatus },
    Note { text: String, tone: NoteTone },
    /// Usage footer for a finished turn.
    Usage { usage: Usage },
    /// Memories the daemon injected before the turn, and (from a daemon that measures it) how long
    /// each stage of finding them took.
    Recall { count: u32, summary: String, timing: Option<Box<RecallTiming>> },
    /// A turn's measured phases, shown with reasoning open (^t).
    Timings(Box<Timings>),
    /// A crew run spinning up or research channels coming online, updated in place.
    Seq(Box<crate::seq::Block>),
    /// The Wake brief, with the directories it offers to watch.
    Brief { text: String, offers: Vec<marlowe_proto::WatchCandidate> },
    /// A background job or the user's `!` shell: its command and what it has printed.
    Job { id: String, command: String, output: String, code: Option<i32> },
    /// News from background work (a monitor's lines, a job that ended, a schedule that fired, a
    /// crew run's result, a message from another session): not the user's words.
    News { label: String, text: String },
    /// A file the agent sent with `send_file`.
    File { file: marlowe_proto::SentFile },
}

/// How long a recall took, stage by stage.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallTiming {
    pub total_ms: u64,
    pub stages: Vec<StageMs>,
    pub abstained: bool,
}

/// A finished turn's measured phases and totals.
#[derive(Debug, Clone, PartialEq)]
pub struct Timings {
    pub spans: Vec<PhaseSpan>,
    pub numbers: TurnNumbers,
    /// The TUI's share of token-to-screen (the first token applied to it drawn), in ms.
    pub screen_ms: Option<u64>,
}

/// A turn in flight in the open session.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTurn {
    pub turn: u32,
    pub model: marlowe_model::ModelRef,
    pub started_ms: i64,
}

/// Where the last frame put the conversation pane. Written by the renderer, which needs the
/// previous frame to keep the view steady while a reply streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewport {
    /// The transcript line at the top of the pane.
    pub top: usize,
    /// The transcript's length in lines.
    pub total: usize,
    /// The pane's size: a resize lays everything out again, so a line index from another size
    /// means nothing.
    pub size: (u16, u16),
    /// Rows the live strip took under the pane when it was drawn. The pane's height changing by
    /// exactly this is the strip coming or going, not a resize.
    pub strip: u16,
    /// [`State::scroll`] when this frame was drawn. While it is unchanged and non-zero, the view
    /// stays on [`Self::top`] however much arrives below.
    pub scroll: usize,
    /// The turn streaming when it was drawn.
    pub turn: Option<u32>,
    /// [`State::epoch`] when it was drawn.
    pub epoch: u32,
    /// Held where a fold put it ([`Pin`]).
    pub pin: Pin,
}

/// A view a fold toggle placed: the toggled marker stayed on its row, so the pane may no longer
/// be at the tail, or may stand past it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pin {
    #[default]
    None,
    /// Above the tail with [`State::scroll`] still 0: held like a scrolled-up view until the
    /// reader scrolls.
    Above,
    /// Past the tail: the rows under a fold closed near the end stay empty rather than pull every
    /// line down, until new text fills them or the reader scrolls.
    Tail,
}

impl Pin {
    pub(crate) fn of(top: usize, max: usize) -> Pin {
        match top.cmp(&max) {
            std::cmp::Ordering::Less => Pin::Above,
            std::cmp::Ordering::Equal => Pin::None,
            std::cmp::Ordering::Greater => Pin::Tail,
        }
    }
}

/// Where a frame drew a reasoning block's fold marker: the entry and its transcript line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub entry: usize,
    pub line: usize,
}

/// A fold just toggled: the next frame draws its marker on the same pane row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub entry: usize,
    pub row: usize,
    pub epoch: u32,
}

/// The reasoning block Enter folds: the one the reader picked while its marker is on screen, else
/// the lowest marker on screen. `rows` is the pane's height and `top` its first transcript line.
pub fn pick_thought(marks: &[Mark], top: usize, rows: usize, picked: Option<usize>) -> Option<usize> {
    let shown = |m: &&Mark| m.line >= top && m.line < top + rows;
    picked
        .filter(|p| marks.iter().filter(shown).any(|m| m.entry == *p))
        .or_else(|| marks.iter().rfind(shown).map(|m| m.entry))
}

/// A pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub id: ApprovalId,
    pub session: SessionId,
    pub tool: String,
    pub summary: String,
    pub detail: String,
    /// The crew run asking, when it is one; `session` is then the run's parent.
    pub run: Option<RunId>,
    /// The rule "always allow" would add, when the daemon proposes one.
    pub proposed_rule: Option<String>,
    /// The edit this call makes, when the arguments are an edit, a multi-edit or a write.
    pub diff: Option<crate::diff::Diff>,
}

/// Full-screen views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Conversation,
    Library,
    Memory,
    /// Watch roots, runs, notices and discovery offers (^w).
    Watch,
    /// Crew runs: the list, or one run's view (alt-r).
    Crew,
}

/// Which box on the conversation screen the keys go to. Reading order is `Tab` order.
///
/// Plain letters are text while the Message box has focus; anywhere else they are the boxes'
/// hotkeys (the `(c)`, `(i)`, `(m)`, `(w)`, `(1)`… on their borders). Ctrl chords never depend on
/// focus, and a letter that is not a hotkey goes back to the Message box and is typed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    Conversation,
    #[default]
    Message,
    Model,
    Workspace,
    /// The Situation Board; which section is `State::board_tab`.
    Board,
}

impl Focus {
    pub const ORDER: [Focus; 5] = [Focus::Conversation, Focus::Message, Focus::Model, Focus::Workspace, Focus::Board];

    pub fn next(self) -> Focus {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }

    pub fn prev(self) -> Focus {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Self::ORDER[(i + Self::ORDER.len() - 1) % Self::ORDER.len()]
    }
}

/// The Situation Board's sections, numbered as their hotkeys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoardTab {
    #[default]
    Crew,
    Watch,
    Threads,
    Memory,
}

impl BoardTab {
    pub const ALL: [BoardTab; 4] = [BoardTab::Crew, BoardTab::Watch, BoardTab::Threads, BoardTab::Memory];

    pub fn label(self) -> &'static str {
        match self {
            BoardTab::Crew => "Crew",
            BoardTab::Watch => "Watch",
            BoardTab::Threads => "Threads",
            BoardTab::Memory => "Memory",
        }
    }

    /// `'1'`..=`'4'`.
    pub fn key(self) -> char {
        (b'1' + self.index() as u8) as char
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn from_key(c: char) -> Option<BoardTab> {
        c.to_digit(10).and_then(|d| Self::ALL.get((d as usize).checked_sub(1)?).copied())
    }
}

/// Something on screen the pointer can light and click. The renderer records where each one was
/// drawn ([`State::hits`]); the mouse handler looks the pointer up there, so a click always lands
/// on what the user saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A box on the conversation screen.
    Box(Focus),
    /// A board section: its tab label (tabbed) or its whole box (stacked).
    Tab(BoardTab),
    /// A crew run, on the board or in the run list: opens its Run view.
    Run(RunId),
    /// A watched run or notice on the board, by path: opens Watch at the root that holds it.
    Watched(String),
    /// A footer chord: runs its command.
    Chord(Command),
    /// The footer's `^k palette`.
    Palette,
    /// The header's live summary: opens the crew.
    Summary,
    /// Row `i` of the list the open view or overlay shows.
    Row(usize),
    /// Library overview rows: a slot, a model, and "add another model".
    Slot(usize),
    Model(usize),
    AddModel,
    /// An approval button, left to right.
    Button(usize),
    /// A button of the quit or resume modal. Not `Button`: an approval can sit under the modal.
    ChoiceButton(usize),
    /// The Run view's steer line.
    Steer,
    /// The "stop run" control in the Run view's header and the workflow view's title: asks y/n.
    StopRun,
    /// The Model box's context meter: opens the planner.
    ContextMeter,
    /// A Library model's window column: opens the planner for model `i`.
    ModelCtx(usize),
    /// A context in the planner's table (row `i`), and optionally a KV column.
    PlanCell(usize, Option<usize>),
    /// A planner control: focus it.
    PlanField(crate::context::PlanField),
    /// A reasoning block's fold marker, by entry index: opens or closes it.
    Thought(usize),
    /// "Remember my choice" in the quit dialog.
    Remember,
    /// A control on a research plan that waits for the user: the main window's banner and the
    /// Run view's plan panel.
    Plan(RunId, crate::crew::PlanAction),
    /// A row of the plan editor (questions, then add, breadth, depth, sources).
    PlanRow(usize),
}

/// A palette command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewSession,
    Sessions,
    Model,
    Workspace,
    Library,
    Context,
    Memory,
    Watch,
    Crew,
    Dispatch,
    Brief,
    ToggleVoice,
    ImportMemories,
    Introduce,
    ToggleThinking,
    Interrupt,
    Quit,
    Transcript,
    HistorySearch,
    CycleMode,
    CycleEffort,
    Background,
    Config,
    Permissions,
    Hooks,
    Agents,
    Mcp,
    Instructions,
    ContextUsage,
    Cost,
    Diff,
    Rewind,
    Jobs,
    Todos,
    Help,
    Tasks,
    Workflows,
    Findings,
    Files,
    Messages,
    Bench,
    Fast,
    ToggleTeams,
}

impl Command {
    pub const ALL: [Command; 43] = [
        Command::NewSession,
        Command::Sessions,
        Command::Model,
        Command::Workspace,
        Command::Library,
        Command::Context,
        Command::Memory,
        Command::Watch,
        Command::Crew,
        Command::Dispatch,
        Command::Brief,
        Command::ToggleVoice,
        Command::ImportMemories,
        Command::Introduce,
        Command::ToggleThinking,
        Command::Interrupt,
        Command::Quit,
        Command::Transcript,
        Command::HistorySearch,
        Command::CycleMode,
        Command::CycleEffort,
        Command::Background,
        Command::Config,
        Command::Permissions,
        Command::Hooks,
        Command::Agents,
        Command::Mcp,
        Command::Instructions,
        Command::ContextUsage,
        Command::Cost,
        Command::Diff,
        Command::Rewind,
        Command::Jobs,
        Command::Todos,
        Command::Help,
        Command::Tasks,
        Command::Workflows,
        Command::Findings,
        Command::Files,
        Command::Messages,
        Command::Bench,
        Command::Fast,
        Command::ToggleTeams,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::NewSession => "New session",
            Command::Sessions => "Sessions",
            Command::Model => "Model — the conductor",
            Command::Workspace => "Workspace — where new sessions start",
            Command::Library => "Library — models and slots",
            Command::Context => "Context & hardware — what fits on this GPU",
            Command::Memory => "Memory — search, correct, forget",
            Command::Watch => "Watch — runs, notices, roots",
            Command::Crew => "Crew — runs and results",
            Command::Dispatch => "Dispatch a crew run",
            Command::Brief => "Brief me now",
            Command::ToggleVoice => "Turn the voice on or off",
            Command::ImportMemories => "Import memories from the old harness",
            Command::Introduce => "Your name and how I greet you",
            Command::ToggleThinking => "Show or hide thinking",
            Command::Interrupt => "Interrupt the running turn",
            Command::Quit => "Quit",
            Command::Transcript => "Transcript — every tool call in full",
            Command::HistorySearch => "Search your earlier prompts",
            Command::CycleMode => "Permission mode — next",
            Command::CycleEffort => "Thinking effort — next",
            Command::Background => "Move the running command to the background",
            Command::Config => "Config — settings",
            Command::Permissions => "Permissions — allow, ask, deny rules",
            Command::Hooks => "Hooks",
            Command::Agents => "Agents — types the agent tool can start",
            Command::Mcp => "MCP servers",
            Command::Instructions => "Instructions — MARLOWE.md and friends, in your editor",
            Command::ContextUsage => "Context usage — how full, and with what",
            Command::Cost => "Cost — tokens and spend this session",
            Command::Diff => "Diff — what this session changed",
            Command::Rewind => "Rewind — back to an earlier turn",
            Command::Jobs => "Background jobs",
            Command::Todos => "Show or hide the todo list",
            Command::Help => "Help — keys and commands",
            Command::Tasks => "Tasks — everything running in the background",
            Command::Workflows => "Teams — team task progress, skip an agent, saved scripts",
            Command::Findings => "Findings — the last code review",
            Command::Files => "Files — what the agent sent you",
            Command::Messages => "Messages — threads to crew runs and agents",
            Command::Bench => "Bench and eval reports",
            Command::Fast => "Fast — run the conductor on the Quick model",
            Command::ToggleTeams => "Teams — turn the standing opt-in to team tasks on or off",
        }
    }

    pub fn chord(self) -> &'static str {
        match self {
            Command::NewSession => "^n",
            Command::Sessions => "alt-s",
            Command::Model => "",
            Command::Workspace => "",
            Command::Library => "^l",
            Command::Context => "",
            Command::Memory => "^g",
            Command::Watch => "^w",
            Command::Crew => "alt-r",
            Command::Dispatch => "^d",
            Command::Brief => "",
            Command::ToggleVoice => "alt-v",
            Command::ImportMemories => "",
            Command::Introduce => "",
            Command::ToggleThinking => "^t",
            Command::Interrupt => "esc",
            Command::Quit => "^q",
            Command::Transcript => "^o",
            Command::HistorySearch => "^r",
            Command::CycleMode => "shift-tab",
            Command::CycleEffort => "alt-t",
            Command::Background => "^b",
            Command::Config => "/config",
            Command::Permissions => "/permissions",
            Command::Hooks => "/hooks",
            Command::Agents => "/agents",
            Command::Mcp => "/mcp",
            Command::Instructions => "/memory",
            Command::ContextUsage => "/context",
            Command::Cost => "/cost",
            Command::Diff => "/diff",
            Command::Rewind => "/rewind",
            Command::Jobs => "/jobs",
            Command::Todos => "/todos",
            Command::Help => "/help",
            Command::Tasks => "/tasks",
            Command::Workflows => "/teams",
            Command::Findings => "/findings",
            Command::Files => "/files",
            Command::Messages => "/messages",
            Command::Bench => "/bench",
            Command::Fast => "/fast",
            Command::ToggleTeams => "alt-m",
        }
    }
}

/// Floating panels above the current view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Palette { query: String, sel: usize },
    Sessions { sel: usize },
    ModelPicker { sel: usize },
    /// Where new sessions start: typed path or a known directory.
    WorkspacePicker { query: String, sel: usize },
    /// First-run offer to import the old harness memories; `sel` 0 is Yes, 1 is Not now.
    ImportOffer { sel: usize },
    /// First-run onboarding; its state is `State::onboard`.
    Onboard,
    /// The ^d dispatch prompt; its state is `State::crew.prompt`.
    Dispatch,
    /// A coding panel (`/config`, the transcript, `ask_user`, ...); its state is
    /// `State::code.panel`.
    Panel,
    /// A question with buttons: quit with work running, or resume what a close interrupted. What
    /// it asks is `State::closing.dialog`.
    Choice { sel: usize },
    /// The context and hardware planner for `model`: the field ←/→ adjust, the policy being
    /// edited (`None` until the plan arrives), and whether the review screen is up.
    ContextPlan {
        model: marlowe_model::ModelRef,
        field: crate::context::PlanField,
        draft: Option<marlowe_proto::ContextPolicy>,
        review: bool,
    },
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
        // A plan is prose: shown as written, not as a `plan  ...` argument row.
        if self.tool == "exit_plan" {
            if let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(self.detail.trim()) {
                if let Some(plan) = m.get("plan").and_then(serde_json::Value::as_str) {
                    return crate::text::wrap(&crate::text::clean(plan.trim()), APPROVAL_DETAIL_WIDTH);
                }
            }
        }
        crate::text::wrap(&crate::text::clean(&readable_detail(&self.detail)), APPROVAL_DETAIL_WIDTH)
    }

    /// For an approval shown as a diff: the arguments the diff does not show (`replace_all`), so
    /// the modal never says less than the call does.
    pub fn extra_lines(&self) -> Vec<String> {
        if self.diff.is_none() {
            return Vec::new();
        }
        let Ok(args) = serde_json::from_str::<serde_json::Value>(self.detail.trim()) else { return Vec::new() };
        crate::diff::unshown_args(&args).iter().flat_map(|l| crate::text::wrap(l, APPROVAL_DETAIL_WIDTH)).collect()
    }

    /// Rows the detail takes: the diff of an edit, else the wrapped arguments.
    pub fn detail_len(&self) -> usize {
        match &self.diff {
            Some(d) => self.extra_lines().len() + crate::diff::render(d, crate::diff::Layout::full(APPROVAL_DETAIL_WIDTH), &marlowe_surface::Theme::default_truecolor()).lines.len(),
            None => self.detail_lines().len(),
        }
    }

    /// Largest useful detail scroll.
    pub fn detail_scroll_max(&self) -> usize {
        self.detail_len().saturating_sub(APPROVAL_DETAIL_ROWS)
    }

    /// The extra option after the buttons: deny and say why. Its index is `choices().len()`.
    pub fn why_index(&self) -> usize {
        self.choices().len()
    }

    /// What the buttons are: allow, then (when the daemon proposed a rule) allow-this-rule for the
    /// session and for the project, or (with none) allow-for-session, then deny.
    pub fn choices(&self) -> Vec<ApprovalDecision> {
        if self.tool == "exit_plan" {
            // Leaving plan mode is approved or refused: there is nothing to remember.
            vec![ApprovalDecision::Allow, ApprovalDecision::Deny]
        } else if self.proposed_rule.is_some() {
            vec![
                ApprovalDecision::Allow,
                ApprovalDecision::AllowPattern { scope: marlowe_proto::SettingsScope::Session },
                ApprovalDecision::AllowPattern { scope: marlowe_proto::SettingsScope::Local },
                ApprovalDecision::Deny,
            ]
        } else {
            APPROVAL_CHOICES.to_vec()
        }
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
    /// Workspace for a new session: the directory the client was started in, until the
    /// workspace picker chooses another.
    pub default_workspace: Option<String>,
    /// The directory the client was started in; always offered by the workspace picker.
    pub launch_workspace: Option<String>,
    pub entries: Vec<Entry>,
    pub live: Option<LiveTurn>,
    /// What the turn is doing now, line by line ([`crate::live`]).
    pub strip: crate::live::Strip,
    /// Text sent and not yet a turn, shown until its TurnStarted arrives.
    pub pending_user: Option<String>,
    awaiting_new: bool,
    pub(crate) pending_open: Option<SessionId>,

    pub approvals: Vec<Approval>,
    pub approval_sel: usize,
    /// "Deny, and tell Marlowe why": the reason being typed in the modal, one line.
    pub approval_reason: Option<Composer>,
    /// When the front approval came up (`now_ms`); decision keys before the guard are ignored.
    pub approval_since_ms: i64,
    /// Detail lines scrolled past in the front approval.
    pub approval_scroll: usize,
    /// The whole detail of the front approval has been on screen; allow is enabled.
    pub approval_read: bool,
    pub sessions: Option<Vec<SessionSummary>>,
    pub library: Library,
    pub memory: MemoryState,
    /// The old harness profile the daemon found and nothing has imported yet.
    pub legacy: Option<LegacyOffer>,
    /// The user chose Not now; the offer stays in the palette only.
    pub legacy_dismissed: bool,
    pub import: Option<ImportStatus>,
    /// Who the user is, once the daemon has said.
    pub user: Option<UserProfile>,
    pub onboard: Onboard,
    /// The boot, the Sentinel's runs and notices, toasts, the voice.
    pub wake: Wake,
    /// Crew runs, the Run view, the dispatch prompt.
    pub crew: Crew,
    /// Modes, queued input, history, attachments, the coding panels (M4.6).
    pub code: crate::coding::Coding,
    /// Context windows, the live meter, the planner's plan.
    pub context: crate::context::ContextUi,
    /// Quitting, closing the window and resuming what a close interrupted (track N).
    pub closing: crate::closing::Closing,
    /// Background tasks, workflows, findings, files, plans, threads (M4.6 wave 2, track H3).
    pub bg: crate::bg::Bg,

    pub view: View,
    /// Which box on the conversation screen has the keys.
    pub focus: Focus,
    /// The Situation Board section shown (tabbed) or focused (stacked).
    pub board_tab: BoardTab,
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    /// Every reasoning block open (^t). [`Self::folds`] overrides it block by block.
    pub show_thinking: bool,
    /// Reasoning blocks opened or closed one at a time (Enter or a click on the marker), by entry
    /// index. ^t clears them, so every block follows it again.
    pub folds: BTreeMap<usize, bool>,
    /// The reasoning block the Conversation box's cursor is on (entry index), while its marker is
    /// on screen; otherwise the lowest marker on screen has it ([`pick_thought`]).
    pub thought: Option<usize>,
    /// Where the last frame drew each foldable block's marker. Written by the renderer.
    pub marks: RefCell<Vec<Mark>>,
    /// Set by a fold toggle, taken by the next frame, which keeps the marker on its row.
    pub anchor: Cell<Option<Anchor>>,
    /// When the live reasoning block's first delta arrived.
    thinking_since: Option<i64>,
    /// Lines scrolled up from the bottom of the conversation; 0 follows the stream. Scrolled up,
    /// the pane stays on the lines it shows while text arrives below ([`Viewport`]), so the lines
    /// from the bottom grow; [`State::settle_scroll`] brings this back in step before input uses it.
    pub scroll: usize,
    /// Largest useful scroll, written back by the renderer.
    pub scroll_max: Cell<usize>,
    /// The transcript's rows between frames ([`crate::layout_cache`]).
    pub layout: RefCell<crate::layout_cache::TranscriptCache>,
    /// Lay every entry out again every frame: the reference the cache is tested against.
    pub no_layout_cache: bool,
    /// The conversation pane as last drawn, written back by the renderer.
    pub viewport: Cell<Viewport>,
    /// Bumped whenever the transcript is replaced wholesale (a new or reopened session) or laid
    /// out anew by the reader (reasoning shown or hidden), so the renderer does not hold a view
    /// from the old layout.
    pub epoch: u32,
    pub flash: Option<Flash>,
    /// What the pointer is over, as of the last mouse event.
    pub hover: Option<Target>,
    /// Where the last frame drew each clickable thing, in drawing order (later is on top).
    /// Written by the renderer, read by the mouse handler.
    pub hits: RefCell<Vec<(ratatui::layout::Rect, Target)>>,

    /// Wall clock for the title bar, e.g. `08:14`. Set by the run loop.
    pub clock: String,
    /// Local date for the rail's TODAY, e.g. `Friday 9 October`. Set by the run loop.
    pub date: String,
    /// Unix ms, for relative times and the meter. Set by the run loop.
    pub now_ms: i64,
    pub quit: bool,
    /// The daemon's shutdown in progress after a quit that stops it (see [`crate::seq::Farewell`]).
    pub farewell: Option<crate::seq::Farewell>,
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
            launch_workspace: default_workspace.clone(),
            default_workspace,
            entries: Vec::new(),
            live: None,
            strip: crate::live::Strip::default(),
            pending_user: None,
            awaiting_new: false,
            pending_open: None,
            approvals: Vec::new(),
            approval_sel: APPROVAL_DENY,
            approval_reason: None,
            approval_since_ms: 0,
            approval_scroll: 0,
            approval_read: false,
            sessions: None,
            library: Library::default(),
            memory: MemoryState::default(),
            legacy: None,
            legacy_dismissed: false,
            import: None,
            user: None,
            onboard: Onboard::Idle,
            wake: Wake::default(),
            crew: Crew::default(),
            code: crate::coding::Coding::default(),
            context: crate::context::ContextUi::default(),
            closing: Default::default(),
            bg: Default::default(),
            view: View::Conversation,
            focus: Focus::Message,
            board_tab: BoardTab::Crew,
            overlay: None,
            composer: Composer::default(),
            show_thinking: false,
            folds: BTreeMap::new(),
            thought: None,
            marks: RefCell::new(Vec::new()),
            anchor: Cell::new(None),
            thinking_since: None,
            scroll: 0,
            scroll_max: Cell::new(0),
            layout: RefCell::new(crate::layout_cache::TranscriptCache::default()),
            no_layout_cache: false,
            viewport: Cell::new(Viewport::default()),
            epoch: 0,
            flash: None,
            hover: None,
            hits: RefCell::new(Vec::new()),
            clock: String::new(),
            date: String::new(),
            now_ms: 0,
            quit: false,
            farewell: None,
        }
    }

    /// The topmost thing the last frame drew at `p`.
    pub fn target_at(&self, p: ratatui::layout::Position) -> Option<Target> {
        self.hits.borrow().iter().rev().find(|(r, _)| r.contains(p)).map(|(_, t)| t.clone())
    }

    /// The header's name for the session: its title, else `new session`.
    pub fn session_label(&self) -> String {
        match self.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => crate::text::clean_maths(t),
            None => "new session".to_string(),
        }
    }

    /// The terminal window's title: `marlowe — <session>`.
    pub fn window_title(&self) -> String {
        let t: String = format!("marlowe — {}", self.session_label())
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        crate::text::truncate(&t, 80)
    }

    /// The header's live summary: `2 runs · 1 needs you · 3 new`. Runs are the crew's unfinished
    /// runs plus watched runs alive; "needs you" counts runs waiting on the user and approvals;
    /// "new" is unseen notices above info.
    pub fn summary(&self) -> String {
        let crew = self.crew.runs.iter().filter(|r| !r.status.is_terminal()).count();
        let watched = self
            .wake
            .watch
            .as_ref()
            .map_or(0, |w| w.runs.iter().filter(|r| r.status == marlowe_proto::RunStatus::Alive).count());
        let runs = crew + watched;
        let needs = self.crew.count(marlowe_proto::CrewRunStatus::NeedsYou) + self.approvals.len();
        let mut parts = vec![
            format!("{runs} run{}", if runs == 1 { "" } else { "s" }),
            format!("{needs} need{} you", if needs == 1 { "s" } else { "" }),
        ];
        let fresh = self
            .wake
            .notices
            .iter()
            .filter(|n| !n.seen && n.severity > marlowe_proto::Severity::Info)
            .count();
        if fresh > 0 {
            parts.push(format!("{fresh} new"));
        }
        parts.join(" · ")
    }

    /// The workspace shown in the title bar.
    pub fn shown_workspace(&self) -> Option<&str> {
        self.workspace.as_deref().or(self.default_workspace.as_deref())
    }

    /// What the workspace picker offers for `query`: the typed path first when it looks like one,
    /// then the shown workspace, the launch directory, recent sessions' and the watched roots —
    /// de-duplicated (case and separators ignored) and filtered by the query.
    pub fn workspace_choices(&self, query: &str) -> Vec<String> {
        let q = query.trim().trim_matches('"').trim();
        let mut out: Vec<String> = Vec::new();
        let mut push = |p: &str| {
            let n = crate::wake::norm(p);
            if !p.trim().is_empty() && !out.iter().any(|o| crate::wake::norm(o) == n) {
                out.push(p.to_string());
            }
        };
        if q.contains(['/', '\\', ':']) || q.starts_with('~') {
            push(q);
        }
        let ql = q.to_lowercase().replace('\\', "/");
        let mut known: Vec<&str> = Vec::new();
        known.extend(self.shown_workspace());
        known.extend(self.launch_workspace.as_deref());
        known.extend(self.sessions.iter().flatten().filter_map(|s| s.workspace.as_deref()));
        known.extend(self.wake.watch.iter().flat_map(|w| w.roots.iter().map(|r| r.path.as_str())));
        for p in known {
            if ql.is_empty() || p.to_lowercase().replace('\\', "/").contains(&ql) {
                push(p);
            }
        }
        out
    }

    pub fn is_busy(&self) -> bool {
        self.live.is_some()
    }

    /// Whether the live strip's rows are reserved: from Enter until the turn is over, and while
    /// the memory write or a load or compaction result it left is still showing.
    pub fn strip_up(&self) -> bool {
        self.live.is_some() || self.pending_user.is_some() || self.strip.turn_up() || self.strip.trailing(self.now_ms)
    }

    pub fn current_approval(&self) -> Option<&Approval> {
        self.approvals.first()
    }

    /// Resets the modal for whatever approval is now at the front: Deny selected, detail at the
    /// top, the type-ahead guard restarted.
    pub(crate) fn front_approval_changed(&mut self) {
        self.approval_sel = self.approvals.first().map_or(APPROVAL_DENY, |a| a.choices().len() - 1);
        self.approval_since_ms = self.now_ms;
        self.approval_reason = None;
        self.approval_scroll = 0;
        self.approval_read = self.approvals.first().is_some_and(|a| a.detail_scroll_max() == 0);
    }

    pub(crate) fn flash(&mut self, text: impl Into<String>, tone: NoteTone) {
        self.flash = Some(Flash { text: text.into(), tone });
    }

    pub(crate) fn mine(&self, s: &SessionId) -> bool {
        self.session.as_ref() == Some(s)
    }

    /// Brings [`Self::scroll`] in step with what is on screen. Scrolled up, the renderer keeps the
    /// pane on the same lines while text arrives below, so the lines from the bottom have grown
    /// since `scroll` was set; input adjusts from what the reader sees, not from the stale count.
    pub(crate) fn settle_scroll(&mut self) {
        let mut v = self.viewport.get();
        // A fold can leave the view above the tail with `scroll` still 0.
        if v.scroll != self.scroll || (self.scroll == 0 && v.pin != Pin::Above) {
            return;
        }
        self.scroll = self.scroll_max.get().saturating_sub(v.top);
        v.scroll = self.scroll;
        self.viewport.set(v);
    }

    /// Back to following the tail, from wherever the reader or a fold left the view. A fold toggled
    /// earlier in the same batch of input no longer holds its marker's row.
    pub(crate) fn follow_tail(&mut self) {
        self.anchor.set(None);
        self.scroll = 0;
        let mut v = self.viewport.get();
        v.pin = Pin::None;
        self.viewport.set(v);
    }

    /// Whether reasoning block `entry` is open: its own fold if it has one, else ^t's.
    pub fn thought_open(&self, entry: usize) -> bool {
        self.folds.get(&entry).copied().unwrap_or(self.show_thinking)
    }

    /// The transcript was replaced: entry indices mean something else now.
    fn forget_folds(&mut self) {
        self.folds.clear();
        self.thought = None;
        self.anchor.set(None);
        self.marks.borrow_mut().clear();
        self.thinking_since = None;
    }

    /// Clears the conversation for a new session.
    pub(crate) fn reset_conversation(&mut self) {
        self.session = None;
        self.title = None;
        self.workspace = None;
        self.entries.clear();
        self.forget_folds();
        self.epoch = self.epoch.wrapping_add(1);
        self.live = None;
        self.strip.reset();
        self.pending_user = None;
        self.awaiting_new = false;
        self.code.teams = false;
        self.bg.reset_session();
        self.follow_tail();
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
                if let Some(f) = &mut self.farewell {
                    f.disconnected();
                }
                self.conn = Conn::Offline { reason, attempt };
                Vec::new()
            }
            ClientEvent::Refused(msg) => {
                self.conn = Conn::Refused(msg);
                Vec::new()
            }
            ClientEvent::Local(ev) => {
                let mut out = Vec::new();
                self.local_event(ev, &mut out);
                out
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
        self.closing_observe(&env.event);
        if let Some(f) = &mut self.farewell {
            f.on_event(&env.event);
        }
        self.event(&env.event, env.at_ms, env.seq == 0, &mut out);
        out
    }

    pub(crate) fn reload(&mut self, out: &mut Vec<Request>) {
        if let Some(s) = self.session.clone() {
            self.live = None;
            self.strip.reset();
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
                    self.strip.reset();
                    if self.live.take().is_some() {
                        self.note("the daemon restarted; the turn was lost", NoteTone::Amber);
                    }
                    self.approvals.clear();
                    // A plan that still waits comes back with the run list (and is proposed
                    // again by a daemon that crashed); one answered meanwhile must not linger.
                    self.crew.plans.clear();
                    self.reload(out);
                } else {
                    self.check_gap = true;
                }
                out.push(Request::LibraryState);
                out.push(Request::ContextPolicies);
                self.wake_probes(out);
                self.code.bootstrapped = false;
            }
            Event::SessionOpened { session, .. } if reply && self.code.expect_fork && !self.mine(session) => {
                // `/fork`: the copy opens here.
                self.code.expect_fork = false;
                self.reset_conversation();
                self.expect_open(session.clone());
                out.push(Request::OpenSession { session: session.clone() });
            }
            Event::SessionOpened { session, title, workspace } => {
                // Only a seq-0 reply is the session our Ask created; a broadcast may be another
                // client's new session.
                let adopt = self.pending_open.as_ref() == Some(session)
                    || (reply && self.awaiting_new && self.session.is_none())
                    || self.mine(session);
                if adopt {
                    if !self.mine(session) && self.pending_open.as_ref() != Some(session) {
                        // The brief stays as the conversation's opening.
                        self.entries.retain(|e| matches!(e, Entry::Brief { .. }));
                        self.forget_folds();
                    }
                    self.session = Some(session.clone());
                    self.title = Some(title.clone()).filter(|t| !t.is_empty());
                    self.workspace = workspace.clone();
                    self.awaiting_new = false;
                    self.session_adopted(out);
                }
            }
            Event::SessionList { sessions } => {
                self.sessions = Some(sessions.clone());
                if let Some(Overlay::Sessions { sel }) = &mut self.overlay {
                    *sel = (*sel).min(sessions.len().saturating_sub(1));
                }
            }
            // The details of this session's tool calls, asked for after a turn: only the details
            // are taken; the conversation on screen is not rebuilt.
            Event::Transcript { session, turns }
                if self.pending_open.as_ref() != Some(session) && self.mine(session) && self.code.detail_refresh.as_ref() == Some(session) =>
            {
                self.code.detail_refresh = None;
                self.merge_tool_details(turns);
            }
            Event::Transcript { session, turns } => {
                if self.pending_open.as_ref() == Some(session) || self.mine(session) {
                    self.pending_open = None;
                    self.session = Some(session.clone());
                    self.entries = entries_from_transcript(turns);
                    self.forget_folds();
                    self.epoch = self.epoch.wrapping_add(1);
                    self.code.tool_detail = transcript_details(turns);
                    self.code.detail_gen += 1;
                    self.live = None;
                    self.strip.reset();
                    self.pending_user = None;
                    self.follow_tail();
                    self.view = View::Conversation;
                }
            }
            Event::TurnStarted { session, turn, model, user } => {
                if self.mine(session) {
                    if self.title.is_none() && !crate::bg::is_news(user) {
                        self.title = Some(first_line(user));
                    }
                    self.close_thinking(at_ms);
                    self.pending_user = None;
                    self.code.interrupting = false;
                    // A steer typed here is already on screen; do not show it twice.
                    let shown = matches!(self.entries.last(),
                        Some(Entry::User { text, .. }) if text == user);
                    if !shown {
                        self.entries.push(user_entry(user.clone()));
                    }
                    self.live = Some(LiveTurn { turn: *turn, model: model.clone(), started_ms: at_ms });
                    self.strip.turn_started(*turn);
                    self.follow_tail();
                }
            }
            Event::TextDelta { session, text, .. } if self.mine(session) => {
                self.strip.text(at_ms, text.chars().count());
                self.close_thinking(at_ms);
                match self.entries.last_mut() {
                    Some(Entry::Prose { text: t }) => t.push_str(text),
                    _ => self.entries.push(Entry::Prose { text: text.clone() }),
                }
            }
            // `tokens` counts this delta only. A provider may settle the count with an empty delta
            // after the answer began; that goes to this turn's last thinking line.
            Event::ReasoningDelta { session, text, tokens, .. } if self.mine(session) && text.is_empty() => {
                let open = self.entries.iter_mut().rev().take_while(|e| !matches!(e, Entry::User { .. } | Entry::News { .. }));
                for e in open {
                    if let Entry::Thinking { tokens: n, .. } = e {
                        *n += *tokens;
                        break;
                    }
                }
            }
            Event::ReasoningDelta { session, text, tokens, .. } if self.mine(session) => {
                self.strip.reasoning(at_ms, *tokens);
                match self.entries.last_mut() {
                    Some(Entry::Thinking { text: t, tokens: n, live: true, ms, .. }) => {
                        t.push_str(text);
                        *n += *tokens;
                        if let Some(since) = self.thinking_since {
                            *ms = at_ms.saturating_sub(since).max(0) as u64;
                        }
                    }
                    _ => {
                        self.thinking_since = Some(at_ms);
                        self.strip.call.entry = Some(self.entries.len());
                        let read = self.strip.call_read();
                        self.entries.push(Entry::Thinking { text: text.clone(), tokens: *tokens, live: true, ms: 0, read })
                    }
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
                        _ => self.entries.push(Entry::Thinking { text, tokens: *tokens, live: false, ms: 0, read: None }),
                    }
                }
            }
            Event::ToolStarted { session, call_id, tool, summary, .. } if self.mine(session) => {
                self.close_thinking(at_ms);
                self.strip.tool_started(call_id, summary, at_ms);
                self.record_tool_start(call_id, tool, summary);
                let summary = if summary.trim().is_empty() { tool.clone() } else { summary.clone() };
                self.entries.push(Entry::Tool {
                    call_id: call_id.clone(),
                    summary,
                    status: ToolStatus::Running,
                });
            }
            Event::ToolFinished { session, call_id, ok, summary, .. } if self.mine(session) => {
                // The first line is the tool line; any more is the call's output or its diff.
                let (first, rest) = split_summary(summary);
                self.strip.tool_finished(call_id);
                self.record_tool_finish(call_id, *ok, &rest);
                let status = if *ok {
                    ToolStatus::Done(first)
                } else {
                    ToolStatus::Failed(first)
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
                // A step boundary: what the user queued while the model worked goes in now.
                self.flush_queue(out);
            }
            Event::TurnFinished { session, turn, usage, stop } if self.mine(session) => {
                self.strip.finished(*turn, at_ms);
                self.close_thinking(at_ms);
                for e in &mut self.entries {
                    if let Entry::Tool { status: s @ ToolStatus::Running, .. } = e {
                        *s = ToolStatus::Failed("no result".into());
                    }
                }
                match stop {
                    TurnStop::Done if self.live.is_some() && !self.turn_said_anything() => {
                        self.note("ended without a reply", NoteTone::Dim)
                    }
                    TurnStop::Done | TurnStop::Error => {}
                    TurnStop::Interrupted => self.note("interrupted", NoteTone::Amber),
                    TurnStop::StepLimit => self.note("stopped at the step limit", NoteTone::Amber),
                    TurnStop::Budget => self.note("stopped at the budget", NoteTone::Amber),
                }
                if usage.total() > 0 {
                    self.entries.push(Entry::Usage { usage: *usage });
                }
                self.live = None;
                self.add_totals(usage);
                self.code.interrupting = false;
                if *stop == TurnStop::Interrupted {
                    // The user stopped the model: what was queued is theirs to resend, not ours.
                    self.queue_back_to_composer();
                } else {
                    // Nothing is left to steer: queued messages start the next turn.
                    self.flush_queue(out);
                }
                self.refresh_tool_details(out);
                // The runner may have loaded, reloaded or partly offloaded: refresh the meter.
                if self.conn.is_online() {
                    out.push(Request::ContextPolicies);
                }
            }
            Event::ApprovalRequested { approval, session, tool, summary, detail, run, proposed_rule } => {
                if !self.approvals.iter().any(|a| a.id == *approval) {
                    let first = self.approvals.is_empty();
                    let diff = serde_json::from_str::<serde_json::Value>(detail.trim())
                        .ok()
                        .and_then(|args| crate::diff::from_tool_call(tool, &args))
                        .or_else(|| crate::diff::looks_like_diff(detail).then(|| crate::diff::parse_unified(detail)).flatten());
                    self.approvals.push(Approval {
                        id: *approval,
                        session: session.clone(),
                        tool: tool.clone(),
                        summary: summary.clone(),
                        detail: detail.clone(),
                        run: run.clone(),
                        proposed_rule: proposed_rule.clone().filter(|r| !r.trim().is_empty()),
                        diff,
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
                let before = self.library.conductor();
                self.library.set_state(models.clone(), slots.clone(), providers.clone());
                if self.library.conductor() != before && before.is_some() {
                    out.push(Request::ContextPolicies);
                }
            }
            Event::ContextUsage { session, tokens, window, .. } if self.mine(session) => {
                self.context.usage = Some((session.clone(), *tokens, *window));
            }
            Event::ContextPlan(plan) => self.on_context_plan(plan),
            Event::ContextPolicies { models } => self.context.models = models.clone(),
            Event::ContextPolicySaved { model, policy, pending_restart, .. } => {
                if let Some(m) = self.context.models.iter_mut().find(|m| &m.model == model) {
                    m.max_ctx = policy.max_ctx;
                    m.policy = Some(*policy);
                }
                if let Some(p) = self.context.plan.as_mut().filter(|p| &p.model == model) {
                    p.policy = *policy;
                }
                if reply || self.overlay.is_none() {
                    let what = marlowe_proto::policy_label(policy);
                    if *pending_restart {
                        self.flash(format!("saved {what} · the KV/parallel change waits for an Ollama restart"), NoteTone::Amber);
                    } else {
                        self.flash(format!("saved {what}"), NoteTone::Dim);
                    }
                }
            }
            Event::ServerApplied { ok, message } => {
                self.flash(crate::text::clean(message), if *ok { NoteTone::Dim } else { NoteTone::Amber });
            }
            Event::Catalog { provider, models } => {
                self.library.set_catalog(*provider, models.clone());
            }
            Event::MemoryResults { hits } => self.memory_results(hits),
            Event::MemoryChanged { id, change } => self.memory_changed(id, change, out),
            Event::MemoryProfile { hits, tokens, omitted } => self.memory_profile(hits, *tokens, *omitted),
            Event::MemoryStats(report) => self.memory.stats = Some(report.clone()),
            Event::Recalled { session, count, summary, total_ms, stages, abstained, .. } if self.mine(session) => {
                self.strip.recalled();
                // A daemon that measures the stages says so even when nothing was found: the
                // line then reads `0 memories` and where the time went. An older one is silent.
                let timed = *total_ms > 0 || !stages.is_empty();
                if *count > 0 || timed {
                    let timing = timed.then(|| Box::new(RecallTiming { total_ms: *total_ms, stages: stages.clone(), abstained: *abstained }));
                    self.entries.push(Entry::Recall { count: *count, summary: crate::text::clean(summary), timing });
                }
            }
            Event::Phase(p) if p.session.as_ref().is_some_and(|s| self.mine(s)) => {
                if let Some(read) = self.strip.phase(p, at_ms) {
                    // The engine's own numbers replace the estimate on the reasoning header.
                    if let Some(Entry::Thinking { read: slot, .. }) = self.strip.call.entry.and_then(|i| self.entries.get_mut(i)) {
                        *slot = Some(read);
                    }
                }
            }
            Event::Phase(p) if crate::seq::BlockKind::of(p).is_some() => self.seq_phase(p),
            Event::TurnTimings { session, spans, numbers, .. } if self.mine(session) => {
                let screen_ms = self.strip.screen_us().map(|us| us.div_ceil(1000));
                self.entries.push(Entry::Timings(Box::new(Timings { spans: spans.clone(), numbers: numbers.clone(), screen_ms })));
            }
            Event::ImportProgress { read, imported } => {
                self.import = Some(ImportStatus { read: *read, imported: *imported });
            }
            Event::ImportDone { report } => {
                self.import = None;
                self.legacy = None;
                let dup = if report.skipped_duplicates > 0 {
                    format!(", {} already there", report.skipped_duplicates)
                } else {
                    String::new()
                };
                let msg = format!("imported {} of {} memories{dup}", report.imported, report.read);
                if report.errors.is_empty() {
                    self.flash(msg, NoteTone::Dim);
                } else {
                    self.flash(format!("{msg} · {} errors", report.errors.len()), NoteTone::Amber);
                }
            }
            Event::LegacyAvailable { path, approx_entries, already_imported } => {
                if *already_imported {
                    self.legacy = None;
                    if reply {
                        self.flash("the old memories are already imported", NoteTone::Dim);
                    }
                } else {
                    self.legacy = Some(LegacyOffer { path: path.clone(), approx_entries: *approx_entries });
                    // While onboarding is pending or up, its checklist carries the offer. The
                    // daemon re-sends it (as a reply) after every Welcome: a dismissal holds.
                    let free = self.overlay.is_none() && !self.onboard.in_progress();
                    if free && self.import.is_none() && !self.legacy_dismissed {
                        self.overlay = Some(Overlay::ImportOffer { sel: 0 });
                    }
                }
            }
            Event::Status(_) => {}
            Event::Interrupted { .. } | Event::KeepRunning { .. } | Event::Closing { .. } => self.closing_event(ev),
            Event::Brief { text, offers, .. } => self.on_brief(text, offers),
            Event::Notice(n) => self.on_notice(n),
            Event::Notices { list } => self.on_notices(list),
            Event::WatchState(ws) => self.on_watch_state(ws),
            Event::WatchCandidates { candidates } => {
                self.on_candidates(candidates);
                self.onboard_candidates();
            }
            Event::User { name, address, set } => {
                self.on_user(UserProfile { name: name.clone(), address: address.clone(), set: *set })
            }
            Event::VoiceState(v) => self.on_voice(v),
            Event::RunUpdated(info) => self.on_run_updated(info),
            Event::RunStep { run, step, action, text_delta } => {
                self.on_run_step(run, *step, action, text_delta.as_deref())
            }
            Event::RunFinished { run, dispatch } => self.on_run_finished(run, dispatch),
            Event::Runs { list } => self.on_runs(list),
            Event::RunTranscript { run, steps } => self.on_run_transcript(run, steps),
            Event::Roles { list } => self.on_roles(list),
            Event::ResearchProgress { run, progress } => self.crew.on_research(run, progress),
            Event::PlanProposed { run, plan, quote } => self.on_plan_proposed(run, plan, quote),
            Event::Error { message, .. } if reply && message.contains("is not available yet (M4.6") => self.unavailable(message),
            Event::Error { session: None, message } if self.absorb_absent(message) => {}
            // The daemon refused a trust whose digest no longer matches the project's files: the
            // message is shown, and the listing (with its new digest) is asked for again.
            Event::Error { session: None, message } if message.contains("changed since you looked") => {
                self.code.hooks_confirm = None;
                self.code.hooks_digest = None;
                self.flash(crate::text::clean(message), NoteTone::Amber);
                if let Some(ws) = self.shown_workspace().map(str::to_string) {
                    out.push(Request::ListHooks { workspace: Some(ws) });
                }
            }
            Event::Error { session: None, message } if self.context.loading.is_some() && message.starts_with("context plan") => {
                self.context.loading = None;
                self.context.error = Some(crate::text::clean(message));
            }
            Event::Error { session: None, message } if reply && message.starts_with("replay gap") => {
                // The daemon skipped the replay: reload what is open instead.
                self.check_gap = false;
                self.reload(out);
            }
            Event::Error { session, message } => {
                let text = crate::text::clean(message);
                if session.is_none() {
                    self.import = None;
                }
                if self.library.catalog_error(&text) {
                    return;
                }
                match session {
                    Some(s) if self.mine(s) => {
                        self.note(&text, NoteTone::Red);
                        if self.live.is_none() {
                            // The ask failed before it became a turn: nothing is in flight.
                            self.pending_user = None;
                            self.strip.reset();
                        }
                    }
                    Some(_) => {}
                    None => self.flash(text, NoteTone::Red),
                }
                if self.pending_user.is_some() && session.is_none() && self.session.is_none() {
                    self.strip.reset();
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
            // Session events for other sessions, and the M4.6 events.
            other => self.coding_event(other, reply, out),
        }
    }

    pub(crate) fn note(&mut self, text: &str, tone: NoteTone) {
        self.entries.push(Entry::Note { text: text.to_string(), tone });
    }

    /// Whether the turn in flight showed the user anything to read: prose, a note, a file or a
    /// job's output after the last thing they or a background source said.
    fn turn_said_anything(&self) -> bool {
        self.entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e, Entry::User { steer: false, .. } | Entry::News { .. }))
            .any(|e| match e {
                Entry::Prose { text } => !text.trim().is_empty(),
                Entry::Note { .. } | Entry::File { .. } | Entry::Job { .. } | Entry::Brief { .. } => true,
                _ => false,
            })
    }

    /// The last reasoning block stops streaming at `at_ms`, which settles how long it took.
    fn close_thinking(&mut self, at_ms: i64) {
        let since = self.thinking_since.take();
        for e in self.entries.iter_mut().rev() {
            if let Entry::Thinking { live, ms, .. } = e {
                if let (true, Some(since)) = (*live, since) {
                    *ms = at_ms.saturating_sub(since).max(0) as u64;
                }
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
            return if self.wake.speaking() { Activity::Speaking } else { Activity::Idle };
        }
        for e in self.entries.iter().rev() {
            match e {
                Entry::Tool { status: ToolStatus::Running, .. } => return Activity::Running,
                Entry::Thinking { live: true, .. } => return Activity::Thinking,
                Entry::Prose { .. } => return Activity::Writing,
                Entry::User { .. } | Entry::News { .. } => return Activity::Thinking,
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
    /// The voice is speaking; the meter follows its envelope.
    Speaking,
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
            Activity::Speaking => "speaking",
        }
    }

    pub fn moving(self) -> bool {
        matches!(self, Activity::Thinking | Activity::Writing | Activity::Running)
    }
}

/// A user-role text as an entry: the user's words, or news from background work.
pub fn user_entry(text: String) -> Entry {
    match crate::bg::news_label(&text) {
        Some(label) => Entry::News { label, text },
        None => Entry::User { text, steer: false },
    }
}

fn first_line(s: &str) -> String {
    crate::text::truncate(s.lines().next().unwrap_or("").trim(), 60)
}

/// Rebuilds conversation entries from a stored transcript.
pub fn entries_from_transcript(turns: &[StoredTurn]) -> Vec<Entry> {
    let mut out = Vec::new();
    // The measured phases of the turn being read, and what its first model call read.
    let mut timings: Option<Timings> = None;
    let mut read: Option<crate::live::Read> = None;
    for t in turns {
        if t.role == Role::User && (t.numbers.is_some() || !t.phases.is_empty()) {
            if let Some(done) = timings.take() {
                out.push(Entry::Timings(Box::new(done)));
            }
            let numbers = t.numbers.clone().unwrap_or_default();
            read = numbers.read.map(|r| crate::live::Read {
                new: r.new_tok,
                cached: r.cached_tok,
                ms: Some(r.ms),
                cached_est: true,
                ..Default::default()
            });
            timings = Some(Timings { spans: t.phases.clone(), numbers, screen_ms: None });
        }
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
                    out.push(user_entry(text));
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
                                ms: 0,
                                read: read.take(),
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
    if let Some(done) = timings.take() {
        out.push(Entry::Timings(Box::new(done)));
    }
    for e in &mut out {
        if let Entry::Tool { status: s @ ToolStatus::Running, .. } = e {
            *s = ToolStatus::Done(String::new());
        }
    }
    out
}

/// `read src/main.rs` from a stored call: the tool name and the argument that says what it did
/// (the command, the pattern or the path), else its first string argument.
fn describe_call(name: &str, args: &serde_json::Value) -> String {
    let obj = args.as_object();
    let pick = ["command", "pattern", "query", "url", "path", "file_path"]
        .iter()
        .find_map(|k| obj.and_then(|o| o.get(*k)).and_then(|v| v.as_str()))
        .or_else(|| obj.and_then(|o| o.values().find_map(|v| v.as_str())));
    match pick {
        Some(a) => format!("{name} {}", crate::text::truncate(a.lines().next().unwrap_or(""), 60)),
        None => name.to_string(),
    }
}

/// A tool summary as the tool line (its first line) and the rest (output or a diff).
pub fn split_summary(summary: &str) -> (String, String) {
    match summary.split_once('\n') {
        Some((first, rest)) => (first.trim_end().to_string(), rest.trim_matches('\n').to_string()),
        None => (summary.to_string(), String::new()),
    }
}

/// What a stored transcript says about each tool call beyond its line: the arguments of an edit as
/// a diff, and the result text for Ctrl+O.
pub fn transcript_details(turns: &[StoredTurn]) -> std::collections::HashMap<String, crate::coding::ToolDetail> {
    use crate::coding::ToolDetail;
    let mut map: std::collections::HashMap<String, ToolDetail> = std::collections::HashMap::new();
    for t in turns {
        for p in &t.content {
            match p {
                Part::ToolCall(c) => {
                    let args = serde_json::to_string_pretty(&c.args).unwrap_or_default();
                    map.insert(
                        c.id.clone(),
                        ToolDetail {
                            tool: c.name.clone(),
                            started: format!("{}\n{args}", describe_call(&c.name, &c.args)),
                            output: String::new(),
                            diff: crate::diff::from_tool_call(&c.name, &c.args),
                            ok: None,
                            full: true,
                        },
                    );
                }
                Part::ToolResult(r) => {
                    let d = map.entry(r.call_id.clone()).or_default();
                    d.full = true;
                    d.output = r.content.clone();
                    d.ok = Some(!r.is_error);
                    if d.diff.is_none() && crate::diff::looks_like_diff(&r.content) {
                        d.diff = crate::diff::parse_unified(&r.content);
                    }
                }
                _ => {}
            }
        }
    }
    map
}
