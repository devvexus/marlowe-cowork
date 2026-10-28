//! What wave 2 put in the daemon, as the client holds it (M4.6 track H3): the background task
//! registry, workflow runs, findings, sent files, notifications, the plan file, `/fast`, rule
//! suggestions, bench and eval reports, and the message threads to crew runs and agents.
//!
//! Plain state advanced by daemon events, like the rest of the client; the panels that show it are
//! in [`crate::bg_panels`] and the renderers in `render/bg.rs`.

use std::collections::{HashMap, VecDeque};

use marlowe_proto::{
    Event, FastState, Finding, FindingSeverity, NotifyState, Request, RuleSuggestion, RunId, SavedWorkflow, SentFile,
    SessionId, TaskInfo, TaskKind, TaskStatus, Urgency, WorkflowAgentStatus, WorkflowInfo,
};

use crate::state::{Entry, NoteTone, State, ToolStatus};

/// Output a task view keeps (the file has the rest).
pub const TASK_OUT_CAP: usize = 64 * 1024;
/// Lines of a workflow's `log()` the client keeps per run.
pub const WORKFLOW_LOG_CAP: usize = 200;
/// Finished tasks kept for the panel.
const FINISHED_KEPT: usize = 60;
/// The task output view asks for more this often while its task runs.
pub const TASK_POLL_MS: i64 = 1_000;

/// The output of one task, as the `/tasks` panel reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskOut {
    pub id: String,
    pub text: String,
    pub next_since: u64,
    pub done: bool,
    pub error: Option<String>,
    /// A request for more is in flight.
    pub loading: bool,
    pub polled_ms: i64,
}

/// One report of `report_findings`, kept for `/findings`.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingSet {
    pub session: Option<SessionId>,
    pub run: Option<RunId>,
    pub findings: Vec<Finding>,
}

/// A plan the conductor wrote and is waiting on (M13).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanAsk {
    pub session: SessionId,
    pub id: String,
    pub path: String,
    pub text: String,
}

/// Who said a thread message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadFrom {
    You,
    Conductor,
}

/// Where a message got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    Delivered(String),
    Refused(String),
}

/// One message to a crew run, a subagent or another session.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadMsg {
    /// A run id, an agent id or name, or `session:<id>`; empty until the call's arguments are known.
    pub target: String,
    pub from: ThreadFrom,
    pub text: String,
    pub call_id: Option<String>,
    pub at_ms: i64,
    pub delivery: Delivery,
}

/// A bench report card or a coding-eval report on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportFile {
    pub kind: ReportKind,
    pub path: String,
    pub name: String,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Bench,
    Eval,
}

/// Everything wave 2 adds to the client's state.
#[derive(Debug, Clone, Default)]
pub struct Bg {
    pub tasks: Vec<TaskInfo>,
    pub tasks_known: bool,
    pub task_out: Option<TaskOut>,
    pub workflows: Vec<WorkflowInfo>,
    pub saved: Vec<SavedWorkflow>,
    pub workflows_known: bool,
    pub findings: Vec<FindingSet>,
    pub files: Vec<(SessionId, SentFile)>,
    pub notify: Option<NotifyState>,
    pub fast: HashMap<SessionId, FastState>,
    /// Plans waiting for an answer, behind the one on screen.
    pub plans: VecDeque<PlanAsk>,
    pub suggestions: Option<Vec<RuleSuggestion>>,
    pub reports: Option<Vec<ReportFile>>,
    /// The report being read: its path and the text or why not.
    pub report_text: Option<(String, Result<String, String>)>,
    pub thread: Vec<ThreadMsg>,
    /// `/findings` or `/fewer-permission-prompts` was asked: the answer opens its panel.
    pub expect_findings: bool,
    pub expect_suggestions: bool,
    /// `/workflow <name>` was sent: the run that comes back opens in the progress view.
    pub expect_run: bool,
}

impl Bg {
    /// A new conversation: its messages, findings and waiting plans are not the old one's.
    pub fn reset_session(&mut self) {
        self.thread.clear();
        self.findings.clear();
        self.plans.clear();
        self.task_out = None;
    }

    /// Inserts or replaces a task by id.
    pub fn upsert_task(&mut self, t: TaskInfo) {
        match self.tasks.iter_mut().find(|x| x.id == t.id) {
            Some(x) => *x = t,
            None => self.tasks.push(t),
        }
        self.prune();
    }

    fn prune(&mut self) {
        let done = self.tasks.iter().filter(|t| t.status.is_terminal()).count();
        if done > FINISHED_KEPT {
            let mut extra = done - FINISHED_KEPT;
            // The oldest finished go first.
            let mut ended: Vec<(i64, String)> =
                self.tasks.iter().filter(|t| t.status.is_terminal()).map(|t| (t.ended_ms.unwrap_or(t.started_ms), t.id.clone())).collect();
            ended.sort();
            for (_, id) in ended {
                if extra == 0 {
                    break;
                }
                self.tasks.retain(|t| t.id != id);
                extra -= 1;
            }
        }
    }

    pub fn task(&self, id: &str) -> Option<&TaskInfo> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Running work first (newest first), then schedules, then the finished (newest first).
    pub fn tasks_sorted(&self) -> Vec<&TaskInfo> {
        let mut v: Vec<&TaskInfo> = self.tasks.iter().collect();
        v.sort_by_key(|t| {
            (t.status.is_terminal(), t.kind == TaskKind::Cron, std::cmp::Reverse(t.ended_ms.unwrap_or(t.started_ms).max(t.started_ms)))
        });
        v
    }

    /// What the strip under the conversation lists: work that is alive, in `session` or
    /// belonging to no session, oldest first. Schedules are not work.
    pub fn strip(&self, session: Option<&SessionId>) -> Vec<&TaskInfo> {
        let mut v: Vec<&TaskInfo> = self
            .tasks
            .iter()
            .filter(|t| !t.status.is_terminal() && t.kind != TaskKind::Cron)
            .filter(|t| t.session.is_none() || session.is_none() || t.session.as_ref() == session)
            .collect();
        v.sort_by_key(|t| t.started_ms);
        v
    }

    /// Background subagents (`agent_<session>.agent-n`) that are alive in `session`: the crew rail
    /// lists them beside the runs, as one-specialist team tasks. A crew run's own task id has no dot.
    pub fn subagents(&self, session: Option<&SessionId>) -> Vec<&TaskInfo> {
        self.strip(session)
            .into_iter()
            .filter(|t| t.kind == TaskKind::Agent && TaskKind::split(&t.id).is_some_and(|(_, raw)| raw.contains('.')))
            .collect()
    }

    pub fn workflow(&self, run: &RunId) -> Option<&WorkflowInfo> {
        self.workflows.iter().find(|w| &w.run == run)
    }

    pub fn fast_on(&self, session: Option<&SessionId>) -> Option<&FastState> {
        session.and_then(|s| self.fast.get(s)).filter(|f| f.on)
    }

    /// The thread to `target`, oldest first.
    pub fn thread_to(&self, target: &str) -> Vec<&ThreadMsg> {
        self.thread.iter().filter(|m| m.target == target).collect()
    }

    /// Every target that has a thread, in the order they were first written to.
    pub fn thread_targets(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in &self.thread {
            if !out.contains(&m.target) {
                out.push(m.target.clone());
            }
        }
        out
    }
}

/// `3 findings (1 high, 2 medium)`.
pub fn findings_summary(findings: &[Finding]) -> String {
    let n = findings.len();
    let mut parts = Vec::new();
    for sev in [FindingSeverity::Critical, FindingSeverity::High, FindingSeverity::Medium, FindingSeverity::Low, FindingSeverity::Info] {
        let c = findings.iter().filter(|f| f.severity == sev).count();
        if c > 0 {
            parts.push(format!("{c} {}", severity_word(sev)));
        }
    }
    let head = format!("{n} finding{}", if n == 1 { "" } else { "s" });
    if parts.is_empty() {
        head
    } else {
        format!("{head} ({})", parts.join(", "))
    }
}

pub fn severity_word(s: FindingSeverity) -> &'static str {
    match s {
        FindingSeverity::Critical => "critical",
        FindingSeverity::High => "high",
        FindingSeverity::Medium => "medium",
        FindingSeverity::Low => "low",
        FindingSeverity::Info => "info",
    }
}

/// How a news message heads: what it came from.
pub fn news_label(text: &str) -> Option<String> {
    let first = text.lines().next()?.trim();
    let inner = first.strip_prefix('[')?;
    // `[crew · run background · monitor]`, `[crew · run 01234567 · engineer · build · needs you]`.
    if let Some(rest) = inner.strip_prefix("crew · run ") {
        let head = rest.split(']').next().unwrap_or(rest);
        return Some(head.to_string());
    }
    for (prefix, word) in [
        ("monitor ", "monitor"),
        ("job ", "job"),
        ("wake-up ", "wake-up"),
        ("schedule ", "schedule"),
        ("reminder ", "reminder"),
        ("message from session ", "message"),
    ] {
        if inner.starts_with(prefix) {
            let id = inner[prefix.len()..].split([' ', ':', ']']).next().unwrap_or("");
            return Some(if word == "message" { format!("message · session {}", short_id(id)) } else { format!("{word} · {id}") });
        }
    }
    None
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Whether a user-role text is news from background work rather than the user's words.
pub fn is_news(text: &str) -> bool {
    news_label(text).is_some()
}

impl State {
    /// Daemon events wave 2 added. True when `ev` was one of them.
    pub(crate) fn bg_event(&mut self, ev: &Event, reply: bool, out: &mut Vec<Request>) -> bool {
        match ev {
            Event::Tasks { list } => {
                self.bg.tasks = list.clone();
                self.bg.tasks_known = true;
                self.clamp_panel();
            }
            Event::TaskUpdated(t) => {
                self.bg.upsert_task(t.clone());
                self.clamp_panel();
            }
            Event::TaskOutput { id, text, next_since, done } => {
                if let Some(o) = self.bg.task_out.as_mut().filter(|o| &o.id == id) {
                    o.loading = false;
                    o.error = None;
                    o.text.push_str(text);
                    if o.text.len() > TASK_OUT_CAP {
                        let cut = o.text.len() - TASK_OUT_CAP;
                        let at = (cut..o.text.len()).find(|i| o.text.is_char_boundary(*i)).unwrap_or(o.text.len());
                        o.text.drain(..at);
                    }
                    o.next_since = *next_since;
                    o.done = *done;
                }
            }
            Event::WorkflowList { saved, runs } => {
                self.bg.saved = saved.clone();
                self.bg.workflows = runs.clone();
                self.bg.workflows_known = true;
                self.clamp_panel();
            }
            Event::WorkflowUpdated(info) => {
                match self.bg.workflows.iter_mut().find(|w| w.run == info.run) {
                    Some(w) => *w = info.clone(),
                    None => self.bg.workflows.push(info.clone()),
                }
                self.bg.workflows_known = true;
                if std::mem::take(&mut self.bg.expect_run) {
                    self.open_workflow_run(info.run.clone());
                }
                self.clamp_panel();
            }
            Event::WorkflowLog { run, line } => {
                if let Some(w) = self.bg.workflows.iter_mut().find(|w| &w.run == run) {
                    w.log.push(line.clone());
                    if w.log.len() > WORKFLOW_LOG_CAP {
                        let extra = w.log.len() - WORKFLOW_LOG_CAP;
                        w.log.drain(..extra);
                    }
                }
            }
            Event::Findings { session, run, findings } => {
                let ours = session.as_ref().is_none_or(|s| self.mine(s));
                if ours {
                    self.on_findings(session.clone(), run.clone(), findings.clone(), reply);
                }
            }
            Event::Notified { session, title, body, urgency } => {
                let ours = session.as_ref().is_none_or(|s| self.mine(s));
                let line = crate::state::notification_line(title, body);
                let tone = if *urgency == Urgency::Urgent { NoteTone::Amber } else { NoteTone::Dim };
                if ours {
                    self.note(&line, tone);
                } else {
                    self.flash(line, tone);
                }
            }
            Event::NotifyState(ns) => self.bg.notify = Some(*ns),
            Event::FileSent { session, file } => {
                self.bg.files.push((session.clone(), file.clone()));
                if self.mine(session) {
                    // Sent by a `send_file` call that is still running: its line becomes the file's
                    // when the call ends. Sent by anything else, the file stands on its own.
                    let call = self.entries.iter().rev().find_map(|e| match e {
                        Entry::Tool { call_id, status: ToolStatus::Running, .. } if self.code.tool_detail.get(call_id).is_some_and(|d| d.tool == "send_file") => {
                            Some(call_id.clone())
                        }
                        _ => None,
                    });
                    match call {
                        Some(id) => {
                            self.code.sent.insert(id, file.clone());
                        }
                        None => self.entries.push(Entry::File { file: file.clone() }),
                    }
                    self.flash(format!("{} is ready \u{b7} /files", crate::text::clean(&file.name)), NoteTone::Dim);
                }
            }
            Event::PlanReady { session, id, path, text } => {
                if self.mine(session) {
                    self.plan_ready(PlanAsk { session: session.clone(), id: id.clone(), path: path.clone(), text: text.clone() });
                }
            }
            Event::FastChanged { session, state } => {
                self.bg.fast.insert(session.clone(), state.clone());
                if self.mine(session) {
                    let word = match (&state.on, &state.model) {
                        (true, Some(m)) => format!("fast on \u{b7} {}", crate::text::short_model(m, self.library.info(m))),
                        (true, None) => "fast on".to_string(),
                        (false, _) => "fast off \u{b7} back on the conductor's model".to_string(),
                    };
                    self.flash(word, NoteTone::Dim);
                    self.code.statusline.dirty = true;
                }
            }
            Event::RuleSuggestions { list } => {
                self.bg.suggestions = Some(list.clone());
                if std::mem::take(&mut self.bg.expect_suggestions) {
                    self.open_suggestions(list.clone());
                }
            }
            _ => {
                let _ = out;
                return false;
            }
        }
        true
    }

    fn on_findings(&mut self, session: Option<SessionId>, run: Option<RunId>, findings: Vec<Finding>, reply: bool) {
        let expected = std::mem::take(&mut self.bg.expect_findings);
        // A session-wide list is all of the session's findings: it replaces what was held. A
        // report on one run replaces the session-wide list (now out of date) and its own earlier one.
        if run.is_none() {
            self.bg.findings.retain(|s| s.session != session);
        } else {
            self.bg.findings.retain(|s| !(s.run.is_none() && s.session == session));
        }
        match self.bg.findings.iter_mut().find(|s| s.run == run && s.session == session) {
            Some(s) => s.findings = findings.clone(),
            None => self.bg.findings.push(FindingSet { session, run, findings: findings.clone() }),
        }
        // A broadcast report is news; the answer to `/findings` opens the panel instead.
        if !reply && !findings.is_empty() {
            self.note(&format!("review \u{b7} {} \u{b7} /findings", findings_summary(&findings)), NoteTone::Dim);
        }
        if expected {
            if findings.is_empty() {
                self.flash("no findings in this session yet", NoteTone::Dim);
            } else {
                self.open_findings_panel();
            }
        }
        self.clamp_panel();
    }

    // ---- the message threads

    /// A call to `send_message` started: its text, and its target once the arguments are known.
    pub(crate) fn bg_tool_started(&mut self, call_id: &str, tool: &str, summary: &str) {
        if tool != "send_message" {
            return;
        }
        let (head, rest) = crate::state::split_summary(summary);
        let text = head.strip_prefix("send_message").unwrap_or(&head).trim().to_string();
        let (target, full) = thread_args(&rest).unwrap_or_default();
        if self.bg.thread.iter().any(|m| m.call_id.as_deref() == Some(call_id)) {
            return;
        }
        self.bg.thread.push(ThreadMsg {
            target,
            from: ThreadFrom::Conductor,
            text: if full.is_empty() { text } else { full },
            call_id: Some(call_id.to_string()),
            at_ms: self.now_ms,
            delivery: Delivery::Sent,
        });
    }

    /// The call finished: delivered, or why not.
    pub(crate) fn bg_tool_finished(&mut self, call_id: &str, ok: bool, rest: &str) {
        if let Some(m) = self.bg.thread.iter_mut().find(|m| m.call_id.as_deref() == Some(call_id)) {
            let said = crate::text::clean(rest.lines().next().unwrap_or("")).trim().to_string();
            m.delivery = if ok { Delivery::Delivered(said) } else { Delivery::Refused(said) };
        }
    }

    /// The stored transcript's arguments arrived: targets and full texts.
    pub(crate) fn bg_refresh_thread(&mut self) {
        let details = &self.code.tool_detail;
        for m in self.bg.thread.iter_mut() {
            let Some(d) = m.call_id.as_ref().and_then(|c| details.get(c)).filter(|d| d.full && d.tool == "send_message") else { continue };
            let args = d.started.split_once('\n').map(|(_, a)| a).unwrap_or("");
            if let Some((target, text)) = thread_args(args) {
                if !target.is_empty() {
                    m.target = target;
                }
                if !text.is_empty() {
                    m.text = text;
                }
            }
            if let (Some(ok), true) = (d.ok, !d.output.is_empty() && matches!(m.delivery, Delivery::Sent)) {
                let said = crate::text::clean(d.output.lines().next().unwrap_or("")).trim().to_string();
                m.delivery = if ok { Delivery::Delivered(said) } else { Delivery::Refused(said) };
            }
        }
    }

    /// The user wrote to a crew run (the Run view's steer line, or a thread).
    pub(crate) fn thread_user(&mut self, run: &RunId, text: &str) {
        self.bg.thread.push(ThreadMsg {
            target: run.0.clone(),
            from: ThreadFrom::You,
            text: text.to_string(),
            call_id: None,
            at_ms: self.now_ms,
            delivery: Delivery::Sent,
        });
    }

    /// What the strip under the conversation says about a task.
    pub fn task_line(&self, t: &TaskInfo) -> String {
        let mut s = format!("{} {}", t.id, t.title);
        if let Some(d) = t.detail.as_deref().filter(|d| !d.trim().is_empty()) {
            s.push_str(&format!(" \u{b7} {d}"));
        }
        s
    }

    /// Whether the status is one that wants the user (a workflow on an approval, an agent that
    /// needs a decision).
    pub fn task_needs_you(t: &TaskInfo) -> bool {
        t.status == TaskStatus::Waiting && t.detail.as_deref().is_some_and(|d| d.starts_with("needs you") || d.starts_with("paused"))
    }
}

/// The `to` and `text` of a `send_message` call from its arguments (pretty JSON), or `None` when
/// they are not JSON.
fn thread_args(args: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(args.trim()).ok()?;
    let o = v.as_object()?;
    let target = ["to", "agent_id"].iter().find_map(|k| o.get(*k).and_then(|v| v.as_str())).unwrap_or("").to_string();
    let text = ["text", "message"].iter().find_map(|k| o.get(*k).and_then(|v| v.as_str())).unwrap_or("").to_string();
    Some((target, text))
}

/// A workflow agent that has not finished.
pub fn agent_alive(s: WorkflowAgentStatus) -> bool {
    matches!(s, WorkflowAgentStatus::Queued | WorkflowAgentStatus::Running)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_is_told_from_the_users_words() {
        assert_eq!(news_label("[crew \u{b7} run background \u{b7} monitor]\n[monitor mon_a] x").as_deref(), Some("background \u{b7} monitor"));
        assert_eq!(news_label("[monitor mon_a ended: `x` exited]").as_deref(), Some("monitor \u{b7} mon_a"));
        assert_eq!(news_label("[job job_j1 exited with code 0]").as_deref(), Some("job \u{b7} job_j1"));
        assert_eq!(news_label("[wake-up cron_2: you asked to be woken]").as_deref(), Some("wake-up \u{b7} cron_2"));
        assert_eq!(news_label("[message from session 5e55aabbcc \u{2014} another agent]").as_deref(), Some("message \u{b7} session 5e55aabb"));
        assert!(news_label("fix the gate").is_none());
        assert!(news_label("[todo] not news").is_none());
    }

    #[test]
    fn findings_are_counted_by_severity_worst_first() {
        let f = |s| Finding { lens: "l".into(), severity: s, file: "a.rs".into(), line: None, claim: "c".into(), evidence: String::new(), fix: None };
        let list = vec![f(FindingSeverity::Medium), f(FindingSeverity::High), f(FindingSeverity::Medium)];
        assert_eq!(findings_summary(&list), "3 findings (1 high, 2 medium)");
        assert_eq!(findings_summary(&list[..1]), "1 finding (1 medium)");
    }

    #[test]
    fn a_message_call_gives_its_target_and_text() {
        assert_eq!(thread_args("{\n \"to\": \"r1\",\n \"text\": \"stop at tests\"\n}"), Some(("r1".into(), "stop at tests".into())));
        assert_eq!(thread_args("{\"agent_id\":\"agent-2\",\"text\":\"hi\"}"), Some(("agent-2".into(), "hi".into())));
        assert_eq!(thread_args("not json"), None);
    }
}
