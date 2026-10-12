//! The crew as the client knows it: runs for the CREW rail, the Run view (alt-r), the dispatch
//! prompt (^d), and toasts when a run needs the user or finishes.
//!
//! Plain state advanced by daemon events and keys, like the rest of the client. The one read from
//! disk is a research run's report (`report.md`, an artifact the daemon names), so the Run view
//! can render it; the daemon is always local.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{
    ChannelId, CostQuote, CrewRunInfo, CrewRunStatus, CrewStep, Dispatch, PlanDecision, Playbook, Request,
    ResearchCounts, ResearchPlan, ResearchProgress, ResearchStage, RoleInfo, RunId, SandboxKind, Severity, StageState,
    SubQuestion, PLAN_MAX_BREADTH, PLAN_MAX_DEPTH, PLAN_MAX_SOURCES, PLAN_MAX_SUBQUESTIONS,
};
use marlowe_surface::chrome::Ink;

use crate::composer::Composer;
use crate::state::{NoteTone, Overlay, State, View};
use crate::wake::{Presence, MAX_TOASTS, TOAST_MS};

/// Runs asked for on connect and when the crew view opens.
pub const RUN_LIMIT: u32 = 30;
/// The daemon's answer to crew requests when no supervisor runs (see `marlowe-daemon`).
pub const NO_CREW: &str = "the crew is not running";
/// Runs the rail lists before `+N more`.
pub const RAIL_RUNS: usize = 3;
/// Largest report the Run view reads.
pub const REPORT_CAP: u64 = 512 * 1024;
/// Transcript lines shown per step (its tail).
pub const STEP_TAIL: usize = 6;
/// The playbook choices in the dispatch prompt, cycled by Tab.
pub const PLAYBOOKS: [Option<Playbook>; 3] = [None, Some(Playbook::DeepResearch), Some(Playbook::Build)];

/// A pending y/n in the Run view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunConfirm {
    Cancel,
    Discard,
    RejectPlan,
}

/// What a control on a waiting research plan does. Keys and clicks both end in
/// [`State::plan_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    /// Open the run's Run view, where the plan is shown in full.
    Review,
    Approve,
    /// Open the plan editor.
    Edit,
    /// Ask to reject (y/n), in the Run view.
    Reject,
    /// Send the edited plan: approved as edited.
    Send,
    /// Leave the editor, dropping the edits.
    Discard,
}

/// A row of the plan editor, below its sub-questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRow {
    Question(usize),
    Add,
    Breadth,
    Depth,
    Sources,
}

/// The Run view's plan editor: a working copy of the plan, edited row by row and sent as
/// `PlanDecision::Edit`.
#[derive(Debug, Clone)]
pub struct PlanEdit {
    pub plan: ResearchPlan,
    /// Selected row: a sub-question, then add, breadth, depth, sources.
    pub sel: usize,
    /// The selected question's text while it is being typed.
    pub typing: Option<Composer>,
}

impl PlanEdit {
    pub fn new(plan: &ResearchPlan) -> Self {
        Self { plan: plan.clone(), sel: 0, typing: None }
    }

    pub fn rows(&self) -> usize {
        self.plan.sub_questions.len() + 4
    }

    pub fn row(&self, i: usize) -> PlanRow {
        let n = self.plan.sub_questions.len();
        match i.checked_sub(n) {
            None => PlanRow::Question(i),
            Some(0) => PlanRow::Add,
            Some(1) => PlanRow::Breadth,
            Some(2) => PlanRow::Depth,
            Some(_) => PlanRow::Sources,
        }
    }

    /// Starts typing on the selected row: a question's text, or a new question from the add row.
    fn start_typing(&mut self) {
        match self.row(self.sel) {
            PlanRow::Question(i) => {
                let mut c = Composer::default();
                c.set(&self.plan.sub_questions[i].question);
                self.typing = Some(c);
            }
            PlanRow::Add if self.plan.sub_questions.len() < PLAN_MAX_SUBQUESTIONS => {
                let channels = self.default_channels();
                let id = self.plan.sub_questions.iter().map(|q| q.id).max().unwrap_or(0) + 1;
                self.plan.sub_questions.push(SubQuestion { id, question: String::new(), channels, rationale: None, queries: Vec::new() });
                self.sel = self.plan.sub_questions.len() - 1;
                self.typing = Some(Composer::default());
            }
            _ => {}
        }
    }

    /// A new question searches where the plan already does.
    fn default_channels(&self) -> Vec<ChannelId> {
        let c = self.plan.channels();
        if c.is_empty() {
            vec![ChannelId::Web]
        } else {
            c
        }
    }

    /// Ends typing: a changed question drops the model's queries for it (they were for the old
    /// wording); an empty one is removed.
    fn commit(&mut self) {
        let Some(c) = self.typing.take() else { return };
        let PlanRow::Question(i) = self.row(self.sel) else { return };
        let text = c.text().trim().to_string();
        if text.is_empty() {
            self.remove(i);
            return;
        }
        let q = &mut self.plan.sub_questions[i];
        if q.question != text {
            q.question = text;
            q.queries.clear();
            q.rationale = None;
        }
    }

    /// Typing abandoned: a question added and never written goes again.
    fn cancel_typing(&mut self) {
        self.typing = None;
        if let PlanRow::Question(i) = self.row(self.sel) {
            if self.plan.sub_questions[i].question.trim().is_empty() {
                self.remove(i);
            }
        }
    }

    fn remove(&mut self, i: usize) {
        if i < self.plan.sub_questions.len() {
            self.plan.sub_questions.remove(i);
            self.sel = self.sel.min(self.rows() - 1);
        }
    }

    /// Steps breadth, depth or the source budget, within the plan's bounds.
    fn adjust(&mut self, up: bool) {
        let step = |v: u32, lo: u32, hi: u32, by: u32| if up { (v + by).min(hi) } else { v.saturating_sub(by).max(lo) };
        let row = self.row(self.sel);
        let p = &mut self.plan;
        match row {
            PlanRow::Breadth => p.breadth = step(p.breadth, 1, PLAN_MAX_BREADTH, 1),
            PlanRow::Depth => p.depth = step(p.depth, 0, PLAN_MAX_DEPTH, 1),
            PlanRow::Sources => p.source_budget = step(p.source_budget, 1, PLAN_MAX_SOURCES, 4),
            _ => {}
        }
    }

    /// The plan to send: questions numbered from 1, as the user sees them.
    pub fn finished(&self) -> ResearchPlan {
        let mut p = self.plan.clone();
        for (i, q) in p.sub_questions.iter_mut().enumerate() {
            q.id = i as u32 + 1;
        }
        p
    }
}

/// `1 sub-question · ~204k tokens · 24 fetches · 38 min`: a plan and its quote in a line.
pub fn plan_summary(plan: &ResearchPlan, quote: &CostQuote) -> String {
    let n = plan.sub_questions.len();
    let usd = quote.usd.filter(|u| *u > 0.0).map(|u| format!(" · {}", crate::text::cost(u))).unwrap_or_default();
    format!(
        "{n} sub-question{} · ~{} tokens · {} fetches · {} min{usd}",
        if n == 1 { "" } else { "s" },
        crate::text::compact(quote.model_tokens),
        quote.fetches,
        quote.minutes
    )
}

/// The open Run view.
#[derive(Debug, Clone)]
pub struct RunView {
    pub run: RunId,
    /// Stored steps, then live ones as `RunStep` arrives.
    pub steps: Vec<CrewStep>,
    /// The `RunTranscript` reply has arrived.
    pub loaded: bool,
    pub steer: Composer,
    /// Keys go to the steer box.
    pub steering: bool,
    pub confirm: Option<RunConfirm>,
    /// The plan editor, while the waiting plan is being edited.
    pub plan_edit: Option<PlanEdit>,
    /// A research run's report, once it exists.
    pub report: Option<String>,
    /// Lines scrolled: from the top once the run has a result, from the bottom while it streams.
    pub scroll: usize,
    /// Largest useful scroll, written back by the renderer.
    pub scroll_max: Cell<usize>,
    /// Where the plan editor's typing cursor was drawn, written back by the renderer.
    pub plan_cursor: Cell<Option<(u16, u16)>>,
}

impl RunView {
    fn new(run: RunId) -> Self {
        Self {
            run,
            steps: Vec::new(),
            loaded: false,
            steer: Composer::default(),
            steering: false,
            confirm: None,
            plan_edit: None,
            report: None,
            scroll: 0,
            scroll_max: Cell::new(0),
            plan_cursor: Cell::new(None),
        }
    }
}

/// The research stages the Run view's strip shows, in order. `Remember` happens after the report
/// and is not worth a cell.
pub const STRIP: [ResearchStage; 6] = [
    ResearchStage::Plan,
    ResearchStage::Explore,
    ResearchStage::Compress,
    ResearchStage::Corroborate,
    ResearchStage::Verify,
    ResearchStage::Synthesise,
];

/// A deep research run's progress, from its `ResearchProgress` events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResearchTrack {
    /// Each stage's last report: state, done, total. `None` until the stage has reported.
    pub stages: [Option<(StageState, u32, u32)>; 7],
    /// The stage that reported last.
    pub current: Option<ResearchStage>,
    /// The last report's one-line detail.
    pub detail: Option<String>,
    pub counts: ResearchCounts,
}

impl ResearchTrack {
    pub fn stage(&self, s: ResearchStage) -> Option<(StageState, u32, u32)> {
        self.stages[s.ordinal() as usize - 1]
    }
}

/// The ^d prompt: a role, a goal, an optional playbook.
#[derive(Debug, Clone, Default)]
pub struct DispatchPrompt {
    /// Index into the roles list.
    pub role: usize,
    /// Index into [`PLAYBOOKS`].
    pub playbook: usize,
    pub goal: Composer,
}

impl DispatchPrompt {
    pub fn playbook(&self) -> Option<Playbook> {
        PLAYBOOKS[self.playbook % PLAYBOOKS.len()]
    }
}

/// A run that needs the user, failed, or finished, on screen for a while.
#[derive(Debug, Clone, PartialEq)]
pub struct CrewToast {
    pub run: RunId,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub until_ms: i64,
}

/// The crew, as the client knows it.
#[derive(Debug, Clone, Default)]
pub struct Crew {
    pub presence: Presence,
    /// As the daemon sent them; [`Crew::ordered`] is the display order.
    pub runs: Vec<CrewRunInfo>,
    /// `None` until `Roles` arrives.
    pub roles: Option<Vec<RoleInfo>>,
    /// Selected row in the run list.
    pub sel: usize,
    /// The Run view, when one is open; otherwise the crew view is the run list.
    pub open: Option<RunView>,
    pub prompt: Option<DispatchPrompt>,
    pub toasts: Vec<CrewToast>,
    /// Research runs' stage progress, by run. Absent for runs that never reported (older crews).
    pub research: Vec<(RunId, ResearchTrack)>,
    /// Research plans waiting for the user's answer, by run.
    pub plans: Vec<(RunId, ResearchPlan, CostQuote)>,
    /// Build runs' task graphs, by run: the latest `dag` report each sent.
    pub dags: Vec<(RunId, crate::dag::BuildDag)>,
}

impl Crew {
    pub fn get(&self, id: &RunId) -> Option<&CrewRunInfo> {
        self.runs.iter().find(|r| &r.id == id)
    }

    /// Display order: needs you, running, paused, queued, then the ended, newest first.
    pub fn ordered(&self) -> Vec<&CrewRunInfo> {
        let mut v: Vec<&CrewRunInfo> = self.runs.iter().collect();
        v.sort_by_key(|r| (status_rank(r.status), std::cmp::Reverse(r.updated_ms)));
        v
    }

    /// The open run's info.
    pub fn open_run(&self) -> Option<&CrewRunInfo> {
        self.open.as_ref().and_then(|v| self.get(&v.run))
    }

    pub fn count(&self, s: CrewRunStatus) -> usize {
        self.runs.iter().filter(|r| r.status == s).count()
    }

    /// Toasts still up at `now_ms`, oldest first.
    pub fn live_toasts(&self, now_ms: i64) -> impl Iterator<Item = &CrewToast> {
        self.toasts.iter().filter(move |t| t.until_ms > now_ms)
    }

    fn upsert(&mut self, info: &CrewRunInfo) -> Option<CrewRunStatus> {
        match self.runs.iter_mut().find(|r| r.id == info.id) {
            Some(r) => {
                let prev = r.status;
                // A result already in hand is kept if this update does not carry one.
                let result = info.result.clone().or_else(|| r.result.take());
                *r = info.clone();
                r.result = result;
                Some(prev)
            }
            None => {
                self.runs.insert(0, info.clone());
                None
            }
        }
    }

    /// A research run's progress, if it has reported any.
    pub fn dag(&self, id: &RunId) -> Option<&crate::dag::BuildDag> {
        self.dags.iter().find(|(r, _)| r == id).map(|(_, d)| d)
    }

    pub fn research(&self, id: &RunId) -> Option<&ResearchTrack> {
        self.research.iter().find(|(r, _)| r == id).map(|(_, t)| t)
    }

    pub(crate) fn on_research(&mut self, run: &RunId, p: &ResearchProgress) {
        let i = match self.research.iter().position(|(r, _)| r == run) {
            Some(i) => i,
            None => {
                self.research.push((run.clone(), ResearchTrack::default()));
                self.research.len() - 1
            }
        };
        let t = &mut self.research[i].1;
        t.stages[p.stage.ordinal() as usize - 1] = Some((p.state, p.done, p.total));
        // A new stage's detail replaces the last; a report without one keeps its stage's.
        if t.current != Some(p.stage) || p.detail.is_some() {
            t.detail = p.detail.clone();
        }
        t.current = Some(p.stage);
        t.counts = p.counts;
    }

    /// The plan a research run is waiting on, if any.
    pub fn plan(&self, id: &RunId) -> Option<(&ResearchPlan, &CostQuote)> {
        self.plans.iter().find(|(r, _, _)| r == id).map(|(_, p, q)| (p, q))
    }

    /// The plan the main window's banner shows: the oldest still waiting.
    pub fn waiting_plan(&self) -> Option<(&RunId, &ResearchPlan, &CostQuote)> {
        self.plans.first().map(|(r, p, q)| (r, p, q))
    }

    /// The plan is answered (here or elsewhere) or gone: so is its editor and its y/n.
    fn drop_plan(&mut self, id: &RunId) {
        self.plans.retain(|(r, _, _)| r != id);
        if let Some(v) = self.open.as_mut().filter(|v| &v.run == id) {
            v.plan_edit = None;
            if v.confirm == Some(RunConfirm::RejectPlan) {
                v.confirm = None;
            }
        }
    }

    fn clamp_sel(&mut self) {
        self.sel = self.sel.min(self.runs.len().saturating_sub(1));
    }
}

fn status_rank(s: CrewRunStatus) -> u8 {
    match s {
        CrewRunStatus::NeedsYou => 0,
        CrewRunStatus::Running => 1,
        CrewRunStatus::Paused => 2,
        CrewRunStatus::Queued => 3,
        CrewRunStatus::Done | CrewRunStatus::Failed | CrewRunStatus::Cancelled => 4,
    }
}

/// Glyph and ink for a run's status: ● running · ◐ needs you · ✓ done · ✗ failed · ○ queued.
pub fn status_glyph(s: CrewRunStatus) -> (&'static str, Ink) {
    match s {
        CrewRunStatus::Running => ("●", Ink::Green),
        CrewRunStatus::NeedsYou => ("◐", Ink::Amber),
        CrewRunStatus::Done => ("✓", Ink::Green),
        CrewRunStatus::Failed => ("✗", Ink::Red),
        CrewRunStatus::Queued => ("○", Ink::Dim),
        CrewRunStatus::Paused => ("‖", Ink::Dim),
        CrewRunStatus::Cancelled => ("✗", Ink::Dimmer),
    }
}

/// The status as a word for the Run view and the list.
pub fn status_word(s: CrewRunStatus) -> &'static str {
    match s {
        CrewRunStatus::NeedsYou => "needs you",
        other => other.as_str(),
    }
}

/// `$0.0213` once there is a cost, else `41.2k tok`, else empty.
pub fn spend(r: &CrewRunInfo) -> String {
    if r.spent.usd > 0.0 {
        crate::text::cost(r.spent.usd)
    } else if r.spent.tokens > 0 {
        format!("{} tok", crate::text::compact(r.spent.tokens))
    } else {
        String::new()
    }
}

fn first_line(s: &str) -> String {
    crate::text::clean(s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(""))
}

/// The rail's second line for a run: `step 4 · $0.0213 · scout 2/4 · web_search …`.
pub fn run_detail(r: &CrewRunInfo, now_ms: i64) -> String {
    let progress = || {
        let mut parts = vec![format!("step {}", r.step)];
        let s = spend(r);
        if !s.is_empty() {
            parts.push(s);
        }
        parts
    };
    match r.status {
        CrewRunStatus::Running => {
            let mut parts = progress();
            if let Some(a) = r.current_action.as_deref().map(first_line).filter(|a| !a.is_empty()) {
                parts.push(a);
            }
            parts.join(" · ")
        }
        CrewRunStatus::Queued => match r.playbook {
            Some(p) => format!("queued · {}", p.as_str()),
            None => "queued".into(),
        },
        CrewRunStatus::Paused => {
            let mut parts = vec!["paused".to_string()];
            parts.extend(progress());
            parts.join(" · ")
        }
        CrewRunStatus::NeedsYou => match r.note.as_deref().map(first_line).filter(|n| !n.is_empty()) {
            Some(n) => n,
            None if mergeable(r) => "ready to merge".into(),
            None => "waiting on you".into(),
        },
        CrewRunStatus::Done => match r.result.as_ref().map(|d| first_line(&d.summary)).filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => format!("done {}", crate::text::ago(r.updated_ms, now_ms)),
        },
        CrewRunStatus::Failed => match r.note.as_deref().map(first_line).filter(|n| !n.is_empty()) {
            Some(n) => n,
            None => format!("failed {}", crate::text::ago(r.updated_ms, now_ms)),
        },
        CrewRunStatus::Cancelled => format!("cancelled {}", crate::text::ago(r.updated_ms, now_ms)),
    }
}

/// A finished run with something to merge: a Build diff, or a research report to accept.
pub fn mergeable(r: &CrewRunInfo) -> bool {
    let ready = matches!(r.status, CrewRunStatus::NeedsYou | CrewRunStatus::Done);
    let has = r.result.as_ref().is_some_and(|d| {
        d.diff.as_ref().is_some_and(|x| !x.files.is_empty()) || report_artifact(d).is_some()
    });
    ready && has
}

/// A run with a sandbox that is not running can be thrown away.
pub fn discardable(r: &CrewRunInfo) -> bool {
    let sandbox = r.sandbox.as_ref().is_some_and(|s| s.kind != SandboxKind::None);
    sandbox && !matches!(r.status, CrewRunStatus::Running | CrewRunStatus::Queued)
}

/// The research report among a result's artifacts.
pub fn report_artifact(d: &Dispatch) -> Option<&str> {
    d.artifacts
        .iter()
        .map(String::as_str)
        .find(|a| a.replace('\\', "/").to_ascii_lowercase().ends_with("/report.md") || a.eq_ignore_ascii_case("report.md"))
}

/// A research run's report: the `report.md` artifact (capped), else the text of the synthesize
/// step. `None` for other runs or before it exists.
pub fn load_report(r: &CrewRunInfo, steps: &[CrewStep]) -> Option<String> {
    if r.playbook != Some(Playbook::DeepResearch) {
        return None;
    }
    if let Some(path) = r.result.as_ref().and_then(report_artifact) {
        use std::io::Read;
        if let Ok(f) = std::fs::File::open(path) {
            let mut s = String::new();
            if f.take(REPORT_CAP).read_to_string(&mut s).is_ok() && !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    steps
        .iter()
        .rev()
        .find(|s| s.action.to_ascii_lowercase().contains("synth") && !s.text.trim().is_empty())
        .map(|s| s.text.clone())
}

impl State {
    /// A run was created or changed (broadcast) or answered a Dispatch (reply). Toasts when a run
    /// newly needs the user or failed.
    pub(crate) fn on_run_updated(&mut self, info: &CrewRunInfo) {
        self.crew.presence = Presence::Online;
        let prev = self.crew.upsert(info);
        let changed = prev != Some(info.status);
        if info.status != CrewRunStatus::NeedsYou {
            // Answered here or elsewhere; the run moved on.
            self.crew.drop_plan(&info.id);
        }
        if changed && info.status == CrewRunStatus::NeedsYou {
            let detail = info.note.as_deref().map(first_line).unwrap_or_default();
            self.crew_toast(info, Severity::Attention, "needs you", detail);
        }
        if changed && info.status == CrewRunStatus::Failed {
            let detail = info.note.as_deref().map(first_line).unwrap_or_default();
            self.crew_toast(info, Severity::Alarm, "failed", detail);
        }
        self.refresh_report(&info.id);
    }

    /// A research run drafted its plan and waits for the user (also re-sent when its Run view
    /// opens).
    pub(crate) fn on_plan_proposed(&mut self, run: &RunId, plan: &ResearchPlan, quote: &CostQuote) {
        // A re-send (the Run view opening, a reattach) replaces the held plan in place: an editor
        // or a reject y/n already open on it stays.
        match self.crew.plans.iter_mut().find(|(r, _, _)| r == run) {
            Some(held) => {
                held.1 = plan.clone();
                held.2 = quote.clone();
            }
            None => {
                self.crew.plans.push((run.clone(), plan.clone(), quote.clone()));
                if let Some(info) = self.crew.get(run).cloned() {
                    let detail = format!("{} · alt-r to review", plan_summary(plan, quote));
                    self.crew_toast(&info, Severity::Attention, "research plan waiting", detail);
                }
            }
        }
    }

    /// A live step: the rail's current action, and the Run view's transcript if it is open.
    pub(crate) fn on_run_step(&mut self, run: &RunId, step: u32, action: &str, delta: Option<&str>) {
        // A build run's progress report is the strip's, not a line of the transcript.
        if let Some(d) = crate::dag::parse(action) {
            let now = format!("build · {} · {}", d.stage.as_deref().unwrap_or("working"), d.summary());
            if let Some(r) = self.crew.runs.iter_mut().find(|r| &r.id == run) {
                r.current_action = Some(now);
                r.step = r.step.max(step);
            }
            match self.crew.dags.iter_mut().find(|(r, _)| r == run) {
                Some((_, slot)) => *slot = d,
                None => self.crew.dags.push((run.clone(), d)),
            }
            return;
        }
        let role = match self.crew.runs.iter_mut().find(|r| &r.id == run) {
            Some(r) => {
                r.current_action = Some(action.to_string()).filter(|a| !a.is_empty());
                r.step = r.step.max(step);
                r.role.clone()
            }
            None => String::new(),
        };
        let at_ms = self.now_ms;
        let Some(v) = self.crew.open.as_mut().filter(|v| &v.run == run) else { return };
        match v.steps.iter_mut().rev().find(|s| s.step == step && s.role != "you") {
            Some(s) => {
                if !action.is_empty() {
                    s.action = action.to_string();
                }
                if let Some(d) = delta {
                    s.text.push_str(d);
                }
            }
            None => v.steps.push(CrewStep {
                step,
                role,
                action: action.to_string(),
                text: delta.unwrap_or("").to_string(),
                at_ms,
            }),
        }
    }

    /// A run finished with a result.
    pub(crate) fn on_run_finished(&mut self, run: &RunId, dispatch: &Dispatch) {
        self.crew.presence = Presence::Online;
        let Some(r) = self.crew.runs.iter_mut().find(|r| &r.id == run) else { return };
        r.result = Some(dispatch.clone());
        if !r.status.is_terminal() && r.status != CrewRunStatus::NeedsYou {
            r.status = CrewRunStatus::Done;
        }
        let info = r.clone();
        self.crew_toast(&info, Severity::Good, "finished", first_line(&dispatch.summary));
        self.refresh_report(run);
    }

    pub(crate) fn on_runs(&mut self, list: &[CrewRunInfo]) {
        self.crew.presence = Presence::Online;
        // Results arrive separately from some updates; keep any already in hand.
        let old = std::mem::take(&mut self.crew.runs);
        self.crew.runs = list
            .iter()
            .map(|r| {
                let mut r = r.clone();
                if r.result.is_none() {
                    r.result = old.iter().find(|o| o.id == r.id).and_then(|o| o.result.clone());
                }
                r
            })
            .collect();
        self.crew.clamp_sel();
    }

    pub(crate) fn on_run_transcript(&mut self, run: &CrewRunInfo, steps: &[CrewStep]) {
        self.crew.presence = Presence::Online;
        self.crew.upsert(run);
        if let Some(v) = self.crew.open.as_mut().filter(|v| v.run == run.id) {
            // Steers typed before the reply stay at the end.
            let local: Vec<CrewStep> = v.steps.drain(..).filter(|s| s.role == "you").collect();
            v.steps = steps.to_vec();
            v.steps.extend(local);
            v.loaded = true;
        }
        self.refresh_report(&run.id);
    }

    pub(crate) fn on_roles(&mut self, list: &[RoleInfo]) {
        self.crew.roles = Some(list.to_vec());
        if let Some(p) = &mut self.crew.prompt {
            p.role = p.role.min(list.len().saturating_sub(1));
        }
    }

    /// Re-reads the open run's report when it might have appeared.
    fn refresh_report(&mut self, run: &RunId) {
        let Some(info) = self.crew.get(run).cloned() else { return };
        if let Some(v) = self.crew.open.as_mut().filter(|v| &v.run == run) {
            if v.report.is_none() {
                v.report = load_report(&info, &v.steps);
            }
        }
    }

    /// No toast for the run the user is looking at.
    fn crew_toast(&mut self, r: &CrewRunInfo, severity: Severity, what: &str, detail: String) {
        let watching = self.view == View::Crew && self.crew.open.as_ref().is_some_and(|v| v.run == r.id);
        if watching {
            return;
        }
        let now = self.now_ms;
        let title = format!("{} {what} · {}", r.role, crate::text::clean_maths(&r.title));
        self.crew.toasts.retain(|t| t.until_ms > now && t.run != r.id);
        self.crew.toasts.push(CrewToast { run: r.id.clone(), severity, title, detail, until_ms: now + TOAST_MS });
        let extra = self.crew.toasts.len().saturating_sub(MAX_TOASTS);
        self.crew.toasts.drain(..extra);
    }

    /// alt-r: the Run view of the most urgent run, or the empty list.
    pub(crate) fn open_crew(&mut self, out: &mut Vec<Request>) {
        self.view = View::Crew;
        out.push(Request::ListRuns { limit: RUN_LIMIT });
        // A plan waiting on the user first: alt-r is how the banner says to review it.
        let top = self
            .crew
            .waiting_plan()
            .map(|(r, _, _)| r.clone())
            .filter(|r| self.crew.get(r).is_some())
            .or_else(|| self.crew.ordered().first().map(|r| r.id.clone()));
        match top {
            Some(id) => self.open_run(id, out),
            None => self.crew.open = None,
        }
    }

    pub(crate) fn open_run(&mut self, id: RunId, out: &mut Vec<Request>) {
        self.view = View::Crew;
        if let Some(i) = self.crew.ordered().iter().position(|r| r.id == id) {
            self.crew.sel = i;
        }
        self.crew.toasts.retain(|t| t.run != id);
        out.push(Request::WatchRun { run: id.clone() });
        self.crew.open = Some(RunView::new(id.clone()));
        self.refresh_report(&id);
    }

    /// ^d: the dispatch prompt. Roles are re-read each time so edited cards show.
    pub(crate) fn open_dispatch(&mut self, out: &mut Vec<Request>) {
        out.push(Request::ListRoles);
        let role = self
            .crew
            .roles
            .as_ref()
            .and_then(|l| l.iter().position(|r| r.name == "researcher"))
            .unwrap_or(0);
        self.crew.prompt = Some(DispatchPrompt { role, ..Default::default() });
        self.overlay = Some(Overlay::Dispatch);
    }

    pub(crate) fn dispatch_paste(&mut self, text: &str) {
        if let Some(p) = &mut self.crew.prompt {
            p.goal.insert_str(&text.replace(['\r', '\n'], " "));
        }
    }

    /// Keys in the dispatch prompt: typing edits the goal, ↑↓ pick the role, Tab the playbook.
    pub(crate) fn dispatch_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n_roles = self.crew.roles.as_ref().map_or(0, Vec::len);
        let Some(p) = &mut self.crew.prompt else {
            self.overlay = None;
            return;
        };
        match k.code {
            KeyCode::Esc => {
                self.crew.prompt = None;
                self.overlay = None;
            }
            KeyCode::Char('k') | KeyCode::Char('d') if ctrl => {
                self.crew.prompt = None;
                self.overlay = None;
            }
            KeyCode::Up => p.role = p.role.saturating_sub(1),
            KeyCode::Down => p.role = (p.role + 1).min(n_roles.saturating_sub(1)),
            KeyCode::Tab => p.playbook = (p.playbook + 1) % PLAYBOOKS.len(),
            KeyCode::BackTab => p.playbook = (p.playbook + PLAYBOOKS.len() - 1) % PLAYBOOKS.len(),
            KeyCode::Enter => self.submit_dispatch(out),
            KeyCode::Backspace if ctrl => p.goal.backspace_word(),
            KeyCode::Char('w') if ctrl => p.goal.backspace_word(),
            KeyCode::Char('u') if ctrl => p.goal.clear(),
            KeyCode::Backspace if p.goal.text().is_empty() => {
                self.crew.prompt = None;
                self.overlay = None;
            }
            KeyCode::Backspace => p.goal.backspace(),
            KeyCode::Delete => p.goal.delete(),
            KeyCode::Left => p.goal.left(),
            KeyCode::Right => p.goal.right(),
            KeyCode::Home => p.goal.home(),
            KeyCode::End => p.goal.end(),
            KeyCode::Char(c) if !ctrl => p.goal.insert(c),
            _ => {}
        }
    }

    fn submit_dispatch(&mut self, out: &mut Vec<Request>) {
        let Some(p) = &self.crew.prompt else { return };
        let Some(role) = self.crew.roles.as_ref().and_then(|l| l.get(p.role)).map(|r| r.name.clone()) else {
            self.flash("no roles yet — the daemon has not sent them", NoteTone::Amber);
            return;
        };
        if p.goal.is_blank() {
            self.flash("give the run a goal first", NoteTone::Amber);
            return;
        }
        if !self.conn.is_online() {
            self.flash("offline — the dispatch is still here", NoteTone::Amber);
            return;
        }
        let goal = p.goal.text().trim().to_string();
        let playbook = p.playbook();
        out.push(Request::Dispatch {
            session: self.session.clone(),
            role: role.clone(),
            goal,
            playbook,
            workspace: self.shown_workspace().map(str::to_string),
            budget: None,
        });
        self.crew.prompt = None;
        self.overlay = None;
        let how = playbook.map(|p| format!(" · {}", p.as_str())).unwrap_or_default();
        self.flash(format!("dispatched {role}{how} — alt-r to watch"), NoteTone::Dim);
    }

    /// Keys in the crew view: the run list, or the open Run view.
    pub(crate) fn crew_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        if self.crew.open.is_some() {
            self.run_key(k, out);
            return;
        }
        let order: Vec<RunId> = self.crew.ordered().iter().map(|r| r.id.clone()).collect();
        match k.code {
            KeyCode::Esc | KeyCode::Backspace => self.view = View::Conversation,
            KeyCode::Up | KeyCode::Char('k') => self.crew.sel = self.crew.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.crew.sel = (self.crew.sel + 1).min(order.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(id) = order.get(self.crew.sel).cloned() {
                    self.open_run(id, out);
                }
            }
            KeyCode::Char('r') => out.push(Request::ListRuns { limit: RUN_LIMIT }),
            _ => {}
        }
    }

    /// `x`, `c` or a click on the header's stop control: ask whether to stop the open run.
    pub(crate) fn ask_stop_run(&mut self) {
        let ended = self.crew.open_run().is_none_or(|r| r.status.is_terminal());
        if let Some(v) = self.crew.open.as_mut().filter(|_| !ended) {
            v.confirm = Some(RunConfirm::Cancel);
        }
    }

    fn run_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let info = self.crew.open_run().cloned();
        let plan_pending = self.crew.open.as_ref().is_some_and(|v| self.crew.plan(&v.run).is_some());
        let Some(v) = self.crew.open.as_mut() else { return };
        let run = v.run.clone();

        if let Some(c) = v.confirm {
            match k.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    v.confirm = None;
                    match c {
                        RunConfirm::Cancel => out.push(Request::CancelRun { run }),
                        RunConfirm::Discard => out.push(Request::DiscardRun { run }),
                        RunConfirm::RejectPlan => {
                            out.push(Request::ApprovePlan { run: run.clone(), decision: PlanDecision::Reject { reason: None } });
                            self.crew.drop_plan(&run);
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => v.confirm = None,
                _ => {}
            }
            return;
        }

        if v.plan_edit.is_some() {
            self.plan_edit_key(k, out);
            return;
        }

        if v.steering {
            match k.code {
                KeyCode::Esc => v.steering = false,
                KeyCode::Backspace if v.steer.text().is_empty() => v.steering = false,
                KeyCode::Enter => {
                    if !v.steer.is_blank() {
                        let text = v.steer.take().trim().to_string();
                        let step = info.as_ref().map_or(0, |r| r.step);
                        v.steps.push(CrewStep { step, role: "you".into(), action: "steer".into(), text: text.clone(), at_ms: self.now_ms });
                        v.steering = false;
                        v.scroll = 0;
                        self.thread_user(&run, &text);
                        out.push(Request::Steer { run, text });
                    }
                }
                KeyCode::Backspace if ctrl => v.steer.backspace_word(),
                KeyCode::Char('w') if ctrl => v.steer.backspace_word(),
                KeyCode::Char('u') if ctrl => v.steer.clear(),
                KeyCode::Backspace => v.steer.backspace(),
                KeyCode::Delete => v.steer.delete(),
                KeyCode::Left => v.steer.left(),
                KeyCode::Right => v.steer.right(),
                KeyCode::Home => v.steer.home(),
                KeyCode::End => v.steer.end(),
                KeyCode::Char(c) if !ctrl => v.steer.insert(c),
                _ => {}
            }
            return;
        }

        let max = v.scroll_max.get();
        let ended = info.as_ref().is_none_or(|r| r.status.is_terminal());
        match k.code {
            // Back one level: the run list.
            KeyCode::Esc | KeyCode::Backspace => {
                self.crew.open = None;
            }
            KeyCode::Up | KeyCode::Char('k') => v.scroll = (v.scroll + 1).min(max),
            KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageUp => v.scroll = (v.scroll + 10).min(max),
            KeyCode::PageDown => v.scroll = v.scroll.saturating_sub(10),
            KeyCode::Home => v.scroll = max,
            KeyCode::End => v.scroll = 0,
            KeyCode::Char('s') | KeyCode::Tab => {
                if ended {
                    self.flash("the run has ended; there is nothing to steer", NoteTone::Dim);
                } else {
                    v.steering = true;
                }
            }
            KeyCode::Char('p') => match info.map(|r| r.status) {
                Some(CrewRunStatus::Paused) => out.push(Request::ResumeRun { run }),
                Some(CrewRunStatus::Running | CrewRunStatus::Queued | CrewRunStatus::NeedsYou) => {
                    out.push(Request::PauseRun { run })
                }
                _ => {}
            },
            KeyCode::Char('c') => self.ask_stop_run(),
            KeyCode::Char('x') if !plan_pending => self.ask_stop_run(),
            KeyCode::Char('m') => match info {
                Some(r) if mergeable(&r) => {
                    out.push(Request::MergeRun { run });
                    let what = if r.result.as_ref().is_some_and(|d| d.diff.is_some()) { "merge" } else { "accepting the report" };
                    self.flash(format!("{what} asked — approve it to apply"), NoteTone::Dim);
                }
                _ => self.flash("nothing to merge yet", NoteTone::Dim),
            },
            KeyCode::Char('d') => match info {
                Some(r) if discardable(&r) => v.confirm = Some(RunConfirm::Discard),
                Some(_) => self.flash("nothing to discard while the run is working", NoteTone::Dim),
                None => {}
            },
            KeyCode::Char('a') if plan_pending => self.plan_action(run, PlanAction::Approve, out),
            KeyCode::Char('e') if plan_pending => self.plan_action(run, PlanAction::Edit, out),
            KeyCode::Char('x') if plan_pending => self.plan_action(run, PlanAction::Reject, out),
            KeyCode::Char('r') => out.push(Request::WatchRun { run }),
            // The messages sent to this run, yours and the conductor's.
            KeyCode::Char('t') => self.open_threads(Some(run.0.clone()), out),
            _ => {}
        }
    }
    /// A control on a waiting plan, from a key or a click: in the main window's banner or the Run
    /// view's plan panel. Approving or sending answers the run at once (`ApprovePlan`); edit and
    /// reject happen in the Run view, which opens if it is not already on that run.
    pub(crate) fn plan_action(&mut self, run: RunId, action: PlanAction, out: &mut Vec<Request>) {
        let Some(plan) = self.crew.plan(&run).map(|(p, _)| p.clone()) else {
            self.flash("that plan has already been answered", NoteTone::Dim);
            return;
        };
        let needs_view = matches!(action, PlanAction::Review | PlanAction::Edit | PlanAction::Reject);
        let open_here = self.view == View::Crew && self.crew.open.as_ref().is_some_and(|v| v.run == run);
        if needs_view && !open_here {
            self.open_run(run.clone(), out);
        }
        match action {
            PlanAction::Review => {}
            PlanAction::Approve => {
                out.push(Request::ApprovePlan { run: run.clone(), decision: PlanDecision::Approve });
                self.crew.drop_plan(&run);
                self.flash("plan approved — the run is exploring", NoteTone::Dim);
            }
            PlanAction::Edit => {
                if let Some(v) = self.crew.open.as_mut() {
                    v.confirm = None;
                    v.steering = false;
                    v.plan_edit = Some(PlanEdit::new(&plan));
                }
            }
            PlanAction::Reject => {
                if let Some(v) = self.crew.open.as_mut() {
                    v.plan_edit = None;
                    v.confirm = Some(RunConfirm::RejectPlan);
                }
            }
            PlanAction::Send => {
                let Some(edit) = self.crew.open.as_mut().filter(|v| v.run == run).and_then(|v| v.plan_edit.as_mut()) else {
                    return;
                };
                edit.commit();
                let edited = edit.finished();
                if let Err(e) = edited.validate() {
                    self.flash(format!("the edited plan is not runnable: {e}"), NoteTone::Red);
                    return;
                }
                let (decision, what) = if edited == plan {
                    (PlanDecision::Approve, "plan approved")
                } else {
                    (PlanDecision::Edit { plan: edited }, "plan approved as edited")
                };
                out.push(Request::ApprovePlan { run: run.clone(), decision });
                self.crew.drop_plan(&run);
                self.flash(format!("{what} — the run is exploring"), NoteTone::Dim);
            }
            PlanAction::Discard => {
                if let Some(v) = self.crew.open.as_mut() {
                    v.plan_edit = None;
                }
            }
        }
    }

    /// A click on a plan editor row: selects it, and a second click on a question types in it.
    pub(crate) fn plan_row_click(&mut self, i: usize) {
        let Some(e) = self.crew.open.as_mut().and_then(|v| v.plan_edit.as_mut()) else { return };
        if i >= e.rows() {
            return;
        }
        if e.typing.is_some() {
            e.commit();
        }
        let again = e.sel == i;
        e.sel = i.min(e.rows() - 1);
        if again && matches!(e.row(e.sel), PlanRow::Question(_) | PlanRow::Add) {
            e.start_typing();
        }
    }

    /// Keys in the plan editor: ↑↓ pick a row; ↵ types a question (or adds one); ←→ step
    /// breadth, depth and sources; d drops a question; s sends; esc leaves without sending.
    fn plan_edit_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(v) = self.crew.open.as_mut() else { return };
        let run = v.run.clone();
        let Some(e) = v.plan_edit.as_mut() else { return };
        if let Some(c) = e.typing.as_mut() {
            match k.code {
                KeyCode::Enter => e.commit(),
                KeyCode::Esc => e.cancel_typing(),
                KeyCode::Up => {
                    e.commit();
                    e.sel = e.sel.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    e.commit();
                    e.sel = (e.sel + 1).min(e.rows() - 1);
                }
                KeyCode::Backspace if ctrl => c.backspace_word(),
                KeyCode::Char('w') if ctrl => c.backspace_word(),
                KeyCode::Char('u') if ctrl => c.clear(),
                KeyCode::Backspace => c.backspace(),
                KeyCode::Delete => c.delete(),
                KeyCode::Left => c.left(),
                KeyCode::Right => c.right(),
                KeyCode::Home => c.home(),
                KeyCode::End => c.end(),
                KeyCode::Char(ch) if !ctrl => c.insert(ch),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc => self.plan_action(run, PlanAction::Discard, out),
            KeyCode::Char('s') => self.plan_action(run, PlanAction::Send, out),
            KeyCode::Up | KeyCode::Char('k') => e.sel = e.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => e.sel = (e.sel + 1).min(e.rows() - 1),
            KeyCode::BackTab => e.sel = e.sel.saturating_sub(1),
            KeyCode::Enter => e.start_typing(),
            KeyCode::Left | KeyCode::Char('-') => e.adjust(false),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => e.adjust(true),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let PlanRow::Question(i) = e.row(e.sel) {
                    e.remove(i);
                }
            }
            _ => {}
        }
    }
}
