//! The panels wave 2 added (M4.6 track H3): `/tasks`, `/workflows`, `/findings`, `/files`, the plan
//! file modal, `/bench`, `/fewer-permission-prompts` and the message threads. Each is
//! `Overlay::Panel` plus a [`Panel`] variant; the data is in [`crate::bg::Bg`], the panel keeps its
//! cursor. Keys come in through [`State::panel_key`] and leave as requests.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{
    Finding, PermissionMode, PlanFileDecision, Request, RuleAction, RunId, SessionId, SettingsScope, TaskInfo, TaskKind,
    WorkflowAgentInfo, WorkflowAgentStatus, WorkflowInfo,
};

use crate::bg::{agent_alive, Bg, FindingSet, PlanAsk, ReportFile, ReportKind, TASK_POLL_MS};
use crate::coding::{Effect, LocalEvent};
use crate::composer::Composer;
use crate::panel::{edit_key, scroll_up_key, Panel, Scroll};
use crate::state::{Focus, NoteTone, Overlay, State, APPROVAL_GUARD_MS};

// ---- /tasks ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TasksUi {
    pub sel: usize,
    /// Only this kind (Tab cycles).
    pub kind: Option<TaskKind>,
    pub mine_only: bool,
    pub hide_done: bool,
    /// The selected task's output is open.
    pub output: bool,
    pub scroll: Scroll,
}

/// The rows `/tasks` shows: running first, then finished, each newest first.
pub fn task_rows<'a>(bg: &'a Bg, session: Option<&SessionId>, ui: &TasksUi) -> Vec<&'a TaskInfo> {
    bg.tasks_sorted()
        .into_iter()
        .filter(|t| ui.kind.is_none_or(|k| t.kind == k))
        .filter(|t| !ui.mine_only || t.session.as_ref() == session)
        .filter(|t| !(ui.hide_done && t.status.is_terminal()))
        .collect()
}

const KINDS: [Option<TaskKind>; 6] =
    [None, Some(TaskKind::Job), Some(TaskKind::Agent), Some(TaskKind::Workflow), Some(TaskKind::Monitor), Some(TaskKind::Cron)];

pub fn kind_label(k: Option<TaskKind>) -> &'static str {
    k.map_or("all", TaskKind::as_str)
}

// ---- /workflows ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkflowsUi {
    /// Row of the list: runs, then saved workflows.
    pub sel: usize,
    /// The run whose progress is open.
    pub open: Option<RunId>,
    /// The selected agent, among the agents in the order the view shows them.
    pub agent: usize,
    /// `x` was pressed: a `y` stops the run.
    pub confirm_stop: bool,
}

/// A run's agents in the order the progress view lists them: by phase (the script's `phases`
/// first, in order, then any other), then in the order the script issued them.
pub fn ordered_agents(info: &WorkflowInfo) -> Vec<&WorkflowAgentInfo> {
    let rank = |a: &WorkflowAgentInfo| -> usize {
        match a.phase.as_deref() {
            Some(p) => info.phases.iter().position(|x| x == p).unwrap_or(info.phases.len()),
            None => info.phases.len() + 1,
        }
    };
    let mut v: Vec<&WorkflowAgentInfo> = info.agents.iter().collect();
    v.sort_by_key(|a| (rank(a), a.index));
    v
}

/// The runs the list shows, newest first (by when the crew made the run, else the order they came).
pub fn workflow_runs(state: &State) -> Vec<&WorkflowInfo> {
    let mut v: Vec<(usize, &WorkflowInfo)> = state.bg.workflows.iter().enumerate().collect();
    v.sort_by_key(|(i, w)| std::cmp::Reverse((state.crew.get(&w.run).map_or(0, |r| r.created_ms), *i)));
    v.into_iter().map(|(_, w)| w).collect()
}

/// `wf_<run>`: the run's id in the task registry.
pub fn workflow_task_id(run: &RunId) -> String {
    format!("wf_{}", run.0)
}

// ---- /findings -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FindingsUi {
    pub sel: usize,
    /// Which report, among those kept (the last by default).
    pub set: usize,
}

/// A report's findings, worst first, then by file and line.
pub fn sorted_findings(set: &FindingSet) -> Vec<&Finding> {
    let mut v: Vec<&Finding> = set.findings.iter().collect();
    v.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.file.cmp(&b.file)).then_with(|| a.line.cmp(&b.line)));
    v
}

// ---- the plan file ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct PlanUi {
    pub ask: PlanAsk,
    pub choice: usize,
    pub scroll: Scroll,
    /// Typing why the plan is not approved.
    pub feedback: Option<Composer>,
    pub since_ms: i64,
    /// The file was opened in the editor and read again.
    pub edited: bool,
}

/// The plan modal's choices: what a key, a click or Enter picks.
pub const PLAN_CHOICES: [(&str, &str); 3] =
    [("1", "Yes, and accept edits"), ("2", "Yes, and review each edit"), ("3", "No, keep planning")];

// ---- /bench ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BenchUi {
    pub sel: usize,
    /// The selected report is open.
    pub reading: bool,
    pub scroll: Scroll,
}

// ---- /fewer-permission-prompts ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SuggestUi {
    pub sel: usize,
    pub picked: Vec<bool>,
    pub scope: SettingsScope,
}

// ---- threads ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadUi {
    /// The run, agent or session whose thread is shown.
    pub target: String,
    pub typing: bool,
    pub input: Composer,
    pub scroll: Scroll,
}

impl State {
    // ---- openers

    pub fn open_tasks(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Tasks(TasksUi::default()), out);
    }

    pub fn open_workflows(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Workflows(WorkflowsUi::default()), out);
    }

    /// The progress view of `run`, opened by the run's first update.
    pub(crate) fn open_workflow_run(&mut self, run: RunId) {
        let ui = WorkflowsUi { open: Some(run), ..WorkflowsUi::default() };
        self.code.panel = Some(Panel::Workflows(ui));
        self.overlay = Some(Overlay::Panel);
    }

    pub fn open_files(&mut self, out: &mut Vec<Request>) {
        self.open_panel(Panel::Files { sel: 0 }, out);
    }

    pub fn open_bench(&mut self, out: &mut Vec<Request>) {
        self.bg.report_text = None;
        self.open_panel(Panel::Bench(BenchUi::default()), out);
    }

    pub fn open_threads(&mut self, target: Option<String>, out: &mut Vec<Request>) {
        let target = target.or_else(|| self.bg.thread_targets().into_iter().last()).unwrap_or_default();
        self.open_panel(Panel::Thread(ThreadUi { target, typing: false, input: Composer::default(), scroll: Scroll::default() }), out);
    }

    /// `/findings`: the last review's findings, asking the daemon when none are held.
    pub(crate) fn slash_findings(&mut self, out: &mut Vec<Request>) {
        if self.bg.findings.is_empty() {
            self.bg.expect_findings = true;
            match self.session.clone() {
                Some(session) => out.push(Request::ListFindings { run: None, session: Some(session) }),
                None => {
                    self.bg.expect_findings = false;
                    self.flash("no session yet \u{2014} /code-review makes findings", NoteTone::Dim);
                }
            }
        } else {
            self.open_findings_panel();
        }
    }

    pub(crate) fn open_findings_panel(&mut self) {
        let set = self.bg.findings.len().saturating_sub(1);
        self.code.panel = Some(Panel::Findings(FindingsUi { sel: 0, set }));
        self.overlay = Some(Overlay::Panel);
    }

    pub(crate) fn open_suggestions(&mut self, list: Vec<marlowe_proto::RuleSuggestion>) {
        if list.is_empty() {
            self.flash("nothing to suggest \u{2014} no approved read-only calls repeat in this project's sessions", NoteTone::Dim);
            return;
        }
        let n = list.len();
        self.code.panel = Some(Panel::Suggestions(SuggestUi { sel: 0, picked: vec![false; n], scope: SettingsScope::Project }));
        self.overlay = Some(Overlay::Panel);
    }

    /// A plan arrived: it takes the screen like a question does; later ones wait behind it.
    pub(crate) fn plan_ready(&mut self, ask: PlanAsk) {
        if matches!(self.code.panel, Some(Panel::Plan(_) | Panel::Question(_))) {
            self.bg.plans.push_back(ask);
        } else {
            self.show_plan(ask);
        }
    }

    pub(crate) fn show_plan(&mut self, ask: PlanAsk) {
        if let Some(p) = self.code.panel.take() {
            if !matches!(p, Panel::Question(_) | Panel::Plan(_)) {
                self.code.stashed_panel = Some(p);
            }
        }
        let ui = PlanUi { ask, choice: 0, scroll: Scroll::default(), feedback: None, since_ms: self.now_ms, edited: false };
        self.code.panel = Some(Panel::Plan(ui));
        self.overlay = Some(Overlay::Panel);
    }

    // ---- idle work

    /// The task output view asks for more while its task runs.
    pub(crate) fn bg_idle(&mut self, out: &mut Vec<Request>) {
        let Some(Panel::Tasks(ui)) = self.code.panel.as_ref() else { return };
        if !ui.output {
            return;
        }
        let now = self.now_ms;
        let Some(o) = self.bg.task_out.as_mut() else { return };
        if o.loading || o.done || now - o.polled_ms < TASK_POLL_MS {
            return;
        }
        o.loading = true;
        o.polled_ms = now;
        out.push(Request::TaskOutput { id: o.id.clone(), since: Some(o.next_since), filter: None });
    }

    // ---- answers to local work

    pub(crate) fn bg_local(&mut self, ev: LocalEvent) {
        match ev {
            LocalEvent::Reports(mut list) => {
                list.sort_by_key(|r| std::cmp::Reverse(r.modified_ms));
                self.bg.reports = Some(list);
                self.clamp_panel();
            }
            LocalEvent::ReportText { path, result } => self.bg.report_text = Some((path, result)),
            LocalEvent::FileText { path, result } => {
                if let (Some(Panel::Plan(p)), Ok(text)) = (self.code.panel.as_mut(), &result) {
                    if same_file(&p.ask.path, &path) {
                        p.ask.text = text.clone();
                        p.edited = true;
                        p.scroll = Scroll::default();
                    }
                }
                if let Err(e) = result {
                    self.flash(format!("could not read {}: {}", crate::text::short_path(&path), crate::text::clean(&e)), NoteTone::Amber);
                }
            }
            _ => {}
        }
    }

    /// The editor closed on `path`: true when it was the plan file, which is read again.
    pub(crate) fn plan_editor_closed(&mut self, path: &str, result: &Result<(), String>) -> bool {
        let is_plan = matches!(&self.code.panel, Some(Panel::Plan(p)) if same_file(&p.ask.path, path));
        if !is_plan {
            return false;
        }
        match result {
            Ok(()) => self.code.effects.push(Effect::ReadFile { path: path.to_string() }),
            Err(e) => self.flash(format!("the editor failed: {}", crate::text::clean(e)), NoteTone::Amber),
        }
        true
    }

    // ---- keys

    pub(crate) fn tasks_key(&mut self, ui: &mut TasksUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if ui.output {
            return match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                    ui.output = false;
                    self.bg.task_out = None;
                    true
                }
                KeyCode::Char('r') => {
                    self.more_task_output(out);
                    true
                }
                _ => {
                    scroll_up_key(k, &mut ui.scroll);
                    true
                }
            };
        }
        let rows: Vec<TaskInfo> = task_rows(&self.bg, self.session.as_ref(), ui).into_iter().cloned().collect();
        let n = rows.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Home => ui.sel = 0,
            KeyCode::End => ui.sel = n.saturating_sub(1),
            KeyCode::Tab => {
                let i = KINDS.iter().position(|x| *x == ui.kind).unwrap_or(0);
                ui.kind = KINDS[(i + 1) % KINDS.len()];
                ui.sel = 0;
            }
            KeyCode::Char('s') => {
                ui.mine_only = !ui.mine_only;
                ui.sel = 0;
            }
            KeyCode::Char('h') => {
                ui.hide_done = !ui.hide_done;
                ui.sel = 0;
            }
            KeyCode::Char('r') => out.push(Request::ListTasks { session: None }),
            KeyCode::Char('x') | KeyCode::Delete => match rows.get(ui.sel) {
                Some(t) if !t.status.is_terminal() => {
                    out.push(Request::StopTask { id: t.id.clone() });
                    self.flash(format!("stopping {}", t.id), NoteTone::Dim);
                }
                Some(_) => self.flash("that task has already finished", NoteTone::Dim),
                None => {}
            },
            KeyCode::Char('w') => {
                if let Some(t) = rows.get(ui.sel).filter(|t| t.kind == TaskKind::Workflow) {
                    let run = RunId(t.id.trim_start_matches(TaskKind::Workflow.prefix()).to_string());
                    out.push(Request::WatchWorkflow { run: run.clone() });
                    self.open_workflow_run(run);
                    return true;
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(t) = rows.get(ui.sel) {
                    ui.output = true;
                    ui.scroll = Scroll::default();
                    self.bg.task_out = Some(crate::bg::TaskOut { id: t.id.clone(), loading: true, polled_ms: self.now_ms, ..Default::default() });
                    out.push(Request::TaskOutput { id: t.id.clone(), since: None, filter: None });
                }
            }
            _ => {}
        }
        true
    }

    fn more_task_output(&mut self, out: &mut Vec<Request>) {
        let now = self.now_ms;
        if let Some(o) = self.bg.task_out.as_mut().filter(|o| !o.loading) {
            o.loading = true;
            o.polled_ms = now;
            out.push(Request::TaskOutput { id: o.id.clone(), since: Some(o.next_since), filter: None });
        }
    }

    pub(crate) fn workflows_key(&mut self, ui: &mut WorkflowsUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if let Some(run) = ui.open.clone() {
            return self.workflow_run_key(ui, &run, k, out);
        }
        let runs = workflow_runs(self).len();
        let saved = self.bg.saved.len();
        let n = runs + saved;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('r') => out.push(Request::ListWorkflows),
            KeyCode::Enter => {
                if ui.sel < runs {
                    let run = workflow_runs(self)[ui.sel].run.clone();
                    out.push(Request::WatchWorkflow { run: run.clone() });
                    ui.open = Some(run);
                    ui.agent = 0;
                } else if let Some(w) = self.bg.saved.get(ui.sel - runs) {
                    let line = format!("/workflow {} ", w.name);
                    self.composer.set(&line);
                    self.focus = Focus::Message;
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    fn workflow_run_key(&mut self, ui: &mut WorkflowsUi, run: &RunId, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if ui.confirm_stop {
            ui.confirm_stop = false;
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Enter) {
                out.push(Request::StopTask { id: workflow_task_id(run) });
                self.flash("stopping the workflow \u{b7} it can be resumed", NoteTone::Dim);
            }
            return true;
        }
        let agents: Vec<WorkflowAgentInfo> = self.bg.workflow(run).map(|w| ordered_agents(w).into_iter().cloned().collect()).unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => ui.open = None,
            KeyCode::Up | KeyCode::Char('k') => ui.agent = ui.agent.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.agent = (ui.agent + 1).min(agents.len().saturating_sub(1)),
            KeyCode::Home => ui.agent = 0,
            KeyCode::End => ui.agent = agents.len().saturating_sub(1),
            KeyCode::Char('x') => ui.confirm_stop = true,
            KeyCode::Char('m') => {
                self.open_threads(Some(run.0.clone()), out);
                return true;
            }
            KeyCode::Char('s') | KeyCode::Delete => match agents.get(ui.agent) {
                Some(a) if agent_alive(a.status) => {
                    out.push(Request::SkipWorkflowAgent { run: run.clone(), agent: a.index });
                    // The script sees `null` for it; the view shows it now.
                    if let Some(w) = self.bg.workflows.iter_mut().find(|w| &w.run == run) {
                        if let Some(x) = w.agents.iter_mut().find(|x| x.index == a.index) {
                            x.status = WorkflowAgentStatus::Skipped;
                        }
                    }
                    self.flash(format!("skipping {}", crate::text::clean(&a.label)), NoteTone::Dim);
                }
                Some(_) => self.flash("that agent has already finished", NoteTone::Dim),
                None => {}
            },
            _ => {}
        }
        true
    }

    pub(crate) fn findings_key(&mut self, ui: &mut FindingsUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let _ = out;
        let Some(set) = self.bg.findings.get(ui.set).cloned() else { return false };
        let rows = sorted_findings(&set);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.sel = (ui.sel + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Home => ui.sel = 0,
            KeyCode::End => ui.sel = rows.len().saturating_sub(1),
            KeyCode::Tab | KeyCode::Right if self.bg.findings.len() > 1 => {
                ui.set = (ui.set + 1) % self.bg.findings.len();
                ui.sel = 0;
            }
            KeyCode::BackTab | KeyCode::Left if self.bg.findings.len() > 1 => {
                ui.set = (ui.set + self.bg.findings.len() - 1) % self.bg.findings.len();
                ui.sel = 0;
            }
            // `f`: the finding as a request, in the Message box for the user to send.
            KeyCode::Char('f') | KeyCode::Enter => {
                if let Some(f) = rows.get(ui.sel) {
                    let at = match f.line {
                        Some(l) => format!("{}:{l}", f.file),
                        None => f.file.clone(),
                    };
                    let mut line = format!("Fix this review finding in {at}: {}", f.claim.trim());
                    if let Some(fix) = f.fix.as_deref().filter(|x| !x.trim().is_empty()) {
                        line.push_str(&format!(" Suggested fix: {}", fix.trim()));
                    }
                    self.composer.set(&line);
                    self.focus = Focus::Message;
                    return false;
                }
            }
            KeyCode::Char('o') => {
                if let Some(f) = rows.get(ui.sel) {
                    let path = match self.shown_workspace() {
                        Some(ws) if !std::path::Path::new(&f.file).is_absolute() => format!("{}/{}", ws.trim_end_matches(['/', '\\']), f.file),
                        _ => f.file.clone(),
                    };
                    self.code.effects.push(Effect::OpenEditor { path });
                }
            }
            _ => {}
        }
        true
    }

    pub(crate) fn files_key(&mut self, sel: &mut usize, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let files = self.shown_files();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(files.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(f) = files.get(*sel) {
                    out.push(Request::OpenFile { path: f.path.clone(), reveal: false });
                }
            }
            KeyCode::Char('r') => {
                if let Some(f) = files.get(*sel) {
                    out.push(Request::OpenFile { path: f.path.clone(), reveal: true });
                }
            }
            _ => {}
        }
        true
    }

    /// The files sent in this session (newest first), else every one when no session is open.
    pub fn shown_files(&self) -> Vec<marlowe_proto::SentFile> {
        self.bg
            .files
            .iter()
            .rev()
            .filter(|(s, _)| self.session.as_ref().is_none_or(|me| me == s))
            .map(|(_, f)| f.clone())
            .collect()
    }

    pub(crate) fn plan_key(&mut self, ui: &mut PlanUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(fb) = ui.feedback.as_mut() {
            match k.code {
                KeyCode::Esc => ui.feedback = None,
                KeyCode::Enter => {
                    let text = fb.text().trim().to_string();
                    self.answer_plan(ui, PlanFileDecision::Reject { feedback: Some(text).filter(|t| !t.is_empty()) }, out);
                    return false;
                }
                _ => edit_key(fb, k),
            }
            return true;
        }
        // What was being typed in the Message box as the plan arrived is not an answer.
        let ahead = self.now_ms.saturating_sub(ui.since_ms) < APPROVAL_GUARD_MS;
        let n = PLAN_CHOICES.len();
        match k.code {
            KeyCode::Esc => {
                self.answer_plan(ui, PlanFileDecision::Reject { feedback: None }, out);
                self.flash("plan not approved \u{b7} still in plan mode", NoteTone::Dim);
                return false;
            }
            KeyCode::Up if !ctrl => ui.choice = ui.choice.saturating_sub(1),
            KeyCode::Down if !ctrl => ui.choice = (ui.choice + 1).min(n - 1),
            KeyCode::Char('k') => ui.scroll.by(-1),
            KeyCode::Char('j') => ui.scroll.by(1),
            KeyCode::PageUp => ui.scroll.by(-10),
            KeyCode::PageDown => ui.scroll.by(10),
            KeyCode::Home => ui.scroll.to(0),
            KeyCode::End => ui.scroll.to(usize::MAX),
            KeyCode::Char('e') => {
                self.code.effects.push(Effect::OpenEditor { path: ui.ask.path.clone() });
            }
            KeyCode::Char(c @ '1'..='3') if !ahead => {
                ui.choice = c as usize - '1' as usize;
                return self.plan_pick(ui, out);
            }
            KeyCode::Char('n') if !ahead => {
                ui.choice = 2;
                return self.plan_pick(ui, out);
            }
            KeyCode::Enter if !ahead => return self.plan_pick(ui, out),
            _ => {}
        }
        true
    }

    /// The highlighted choice: approve into a mode, or ask why not. True when the modal stays.
    pub(crate) fn plan_pick(&mut self, ui: &mut PlanUi, out: &mut Vec<Request>) -> bool {
        match ui.choice {
            0 => {
                self.answer_plan(ui, PlanFileDecision::Approve { mode: PermissionMode::AcceptEdits }, out);
                false
            }
            1 => {
                self.answer_plan(ui, PlanFileDecision::Approve { mode: PermissionMode::Default }, out);
                false
            }
            _ => {
                ui.feedback = Some(Composer::default());
                true
            }
        }
    }

    fn answer_plan(&mut self, ui: &PlanUi, decision: PlanFileDecision, out: &mut Vec<Request>) {
        if let PlanFileDecision::Approve { mode } = &decision {
            self.code.mode = *mode;
            self.code.statusline.dirty = true;
            self.flash(format!("plan approved \u{b7} {}", mode.as_str().replace('_', " ")), NoteTone::Dim);
        }
        out.push(Request::AnswerPlan { session: ui.ask.session.clone(), id: ui.ask.id.clone(), decision });
    }

    pub(crate) fn bench_key(&mut self, ui: &mut BenchUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let _ = out;
        if ui.reading {
            return match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                    ui.reading = false;
                    self.bg.report_text = None;
                    true
                }
                _ => crate::panel::scroll_key(k, &mut ui.scroll),
            };
        }
        let list: Vec<ReportFile> = self.bg.reports.clone().unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.sel = (ui.sel + 1).min(list.len().saturating_sub(1)),
            KeyCode::Char('r') => self.code.effects.push(Effect::ListReports),
            KeyCode::Enter => {
                if let Some(f) = list.get(ui.sel) {
                    ui.reading = true;
                    ui.scroll = Scroll::default();
                    self.bg.report_text = None;
                    self.code.effects.push(Effect::ReadReport { path: f.path.clone() });
                }
            }
            _ => {}
        }
        true
    }

    pub(crate) fn suggestions_key(&mut self, ui: &mut SuggestUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let list = self.bg.suggestions.clone().unwrap_or_default();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Up | KeyCode::Char('k') => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ui.sel = (ui.sel + 1).min(list.len().saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(p) = ui.picked.get_mut(ui.sel) {
                    *p = !*p;
                }
            }
            KeyCode::Char('a') => {
                let all = ui.picked.iter().all(|p| *p);
                ui.picked.iter_mut().for_each(|p| *p = !all);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                ui.scope = match ui.scope {
                    SettingsScope::Project => SettingsScope::Local,
                    SettingsScope::Local => SettingsScope::User,
                    _ => SettingsScope::Project,
                };
            }
            KeyCode::Enter => {
                let mut chosen: Vec<&marlowe_proto::RuleSuggestion> =
                    list.iter().zip(&ui.picked).filter(|(_, p)| **p).map(|(s, _)| s).collect();
                if chosen.is_empty() {
                    chosen.extend(list.get(ui.sel));
                }
                let n = chosen.len();
                for s in chosen {
                    out.push(Request::AddRule { action: RuleAction::Allow, pattern: s.pattern.clone(), scope: ui.scope, workspace: self.ws() });
                }
                if n > 0 {
                    self.flash(format!("added {n} allow rule{} to the {} settings", if n == 1 { "" } else { "s" }, crate::panel::scope_label(ui.scope)), NoteTone::Dim);
                }
                return false;
            }
            _ => {}
        }
        true
    }

    pub(crate) fn thread_key(&mut self, ui: &mut ThreadUi, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        if ui.typing {
            match k.code {
                KeyCode::Esc => ui.typing = false,
                KeyCode::Enter => {
                    let text = ui.input.text().trim().to_string();
                    if !text.is_empty() {
                        let run = RunId(ui.target.clone());
                        out.push(Request::Steer { run: run.clone(), text: text.clone() });
                        self.thread_user(&run, &text);
                        ui.input.clear();
                        ui.scroll = Scroll::default();
                    }
                    ui.typing = false;
                }
                _ => edit_key(&mut ui.input, k),
            }
            return true;
        }
        let mut targets = self.bg.thread_targets();
        if !ui.target.is_empty() && !targets.contains(&ui.target) {
            targets.push(ui.target.clone());
        }
        let at = targets.iter().position(|t| *t == ui.target).unwrap_or(0);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Tab | KeyCode::Right if targets.len() > 1 => {
                ui.target = targets[(at + 1) % targets.len()].clone();
                ui.scroll = Scroll::default();
            }
            KeyCode::BackTab | KeyCode::Left if targets.len() > 1 => {
                ui.target = targets[(at + targets.len() - 1) % targets.len()].clone();
                ui.scroll = Scroll::default();
            }
            KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('s') => {
                let live = self.crew.get(&RunId(ui.target.clone())).is_some_and(|r| !r.status.is_terminal());
                if live {
                    ui.typing = true;
                } else {
                    self.flash("you can write to a running crew run; the conductor writes to the others", NoteTone::Dim);
                }
            }
            _ => {
                scroll_up_key(k, &mut ui.scroll);
            }
        }
        true
    }

    // ---- the mouse

    /// A click on row `i` of the open panel: the cursor goes there, and a second click on the row
    /// that already has it does what Enter does.
    pub(crate) fn panel_row_click(&mut self, i: usize, out: &mut Vec<Request>) {
        let Some(mut panel) = self.code.panel.take() else { return };
        let enter = KeyEvent::from(KeyCode::Enter);
        let was = match &mut panel {
            Panel::Tasks(u) if !u.output => std::mem::replace(&mut u.sel, i),
            Panel::Workflows(u) if u.open.is_none() => std::mem::replace(&mut u.sel, i),
            Panel::Workflows(u) => std::mem::replace(&mut u.agent, i),
            Panel::Findings(u) => std::mem::replace(&mut u.sel, i),
            Panel::Files { sel } | Panel::Hooks { sel } | Panel::Agents { sel, .. } | Panel::Jobs { sel } | Panel::Instructions { sel } => {
                std::mem::replace(sel, i)
            }
            Panel::Bench(u) if !u.reading => std::mem::replace(&mut u.sel, i),
            Panel::Suggestions(u) => {
                let was = std::mem::replace(&mut u.sel, i);
                if was == i {
                    if let Some(p) = u.picked.get_mut(i) {
                        *p = !*p;
                    }
                }
                usize::MAX
            }
            Panel::Permissions(u) => std::mem::replace(&mut u.sel, i),
            Panel::Mcp(u) => std::mem::replace(&mut u.sel, i),
            Panel::Config(u) if u.edit.is_none() => std::mem::replace(&mut u.sel, i),
            Panel::Rewind(u) if u.scope.is_none() => std::mem::replace(&mut u.sel, i),
            _ => usize::MAX,
        };
        self.code.panel = Some(panel);
        self.clamp_panel();
        if was == i && !matches!(self.code.panel, Some(Panel::Suggestions(_))) {
            // Re-enter the panel's key handler with Enter, as the keyboard would.
            self.overlay = Some(Overlay::Panel);
            self.panel_key(enter, out);
        }
    }

    /// A click on a plan modal button.
    pub(crate) fn plan_button(&mut self, i: usize, out: &mut Vec<Request>) {
        let Some(Panel::Plan(ui)) = self.code.panel.as_mut() else { return };
        if self.now_ms.saturating_sub(ui.since_ms) < APPROVAL_GUARD_MS {
            return;
        }
        ui.choice = i.min(PLAN_CHOICES.len() - 1);
        self.panel_key(KeyEvent::from(KeyCode::Enter), out);
    }
}

fn same_file(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('\\', "/").to_ascii_lowercase();
    norm(a) == norm(b)
}

pub fn report_kind_word(k: ReportKind) -> &'static str {
    match k {
        ReportKind::Bench => "bench",
        ReportKind::Eval => "eval",
    }
}
