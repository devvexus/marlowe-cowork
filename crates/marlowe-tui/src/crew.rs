//! The crew as the client knows it: runs for the CREW rail, the Run view (^r), the dispatch
//! prompt (^d), and toasts when a run needs the user or finishes.
//!
//! Plain state advanced by daemon events and keys, like the rest of the client. The one read from
//! disk is a research run's report (`report.md`, an artifact the daemon names), so the Run view
//! can render it; the daemon is always local.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{
    CostQuote, CrewRunInfo, CrewRunStatus, CrewStep, Dispatch, PlanDecision, Playbook, Request, ResearchCounts,
    ResearchPlan, ResearchProgress, ResearchStage, RoleInfo, RunId, SandboxKind, Severity, StageState,
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
    /// A research run's report, once it exists.
    pub report: Option<String>,
    /// Lines scrolled: from the top once the run has a result, from the bottom while it streams.
    pub scroll: usize,
    /// Largest useful scroll, written back by the renderer.
    pub scroll_max: Cell<usize>,
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
            report: None,
            scroll: 0,
            scroll_max: Cell::new(0),
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

    fn drop_plan(&mut self, id: &RunId) {
        self.plans.retain(|(r, _, _)| r != id);
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
        let fresh = self.crew.plan(run).is_none();
        self.crew.drop_plan(run);
        self.crew.plans.push((run.clone(), plan.clone(), quote.clone()));
        if fresh {
            if let Some(info) = self.crew.get(run).cloned() {
                let detail = format!("{} sub-questions · about {} tokens · ^r to answer", plan.sub_questions.len(), quote.model_tokens);
                self.crew_toast(&info, Severity::Attention, "plan ready", detail);
            }
        }
    }

    /// A live step: the rail's current action, and the Run view's transcript if it is open.
    pub(crate) fn on_run_step(&mut self, run: &RunId, step: u32, action: &str, delta: Option<&str>) {
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

    /// ^r: the Run view of the most urgent run, or the empty list.
    pub(crate) fn open_crew(&mut self, out: &mut Vec<Request>) {
        self.view = View::Crew;
        out.push(Request::ListRuns { limit: RUN_LIMIT });
        let top = self.crew.ordered().first().map(|r| r.id.clone());
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
        self.flash(format!("dispatched {role}{how} — ^r to watch"), NoteTone::Dim);
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
            KeyCode::Char('c') => {
                if !ended {
                    v.confirm = Some(RunConfirm::Cancel);
                }
            }
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
            KeyCode::Char('a') if plan_pending => {
                out.push(Request::ApprovePlan { run: run.clone(), decision: PlanDecision::Approve });
                self.crew.drop_plan(&run);
                self.flash("plan approved — the run is exploring", NoteTone::Dim);
            }
            KeyCode::Char('x') if plan_pending => v.confirm = Some(RunConfirm::RejectPlan),
            KeyCode::Char('r') => out.push(Request::WatchRun { run }),
            _ => {}
        }
    }
}
