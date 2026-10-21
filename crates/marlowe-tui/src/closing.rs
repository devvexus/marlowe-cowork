//! Closing the window (track N, Matthew's decision of 2026-10-10): closing the terminal means
//! everything stops. This is the client's part of it.
//!
//! - **Quitting** (`^q`, `^c`, `/exit`, `/quit`) opens a modal (Matthew, 2026-10-10): "Stop and
//!   close agent", "Close window (keep agent running)" or Cancel, and "Remember my choice". The
//!   choice is stored as the user setting `quit.choice` (`stop` or `keep`) and as the daemon's
//!   `keep running` switch, so closing the window by its X does the same as the remembered answer:
//!   a safe shutdown that unloads the models first, or a background agent with a tray icon. With
//!   a remembered choice the modal does not open. Either way the client says goodbye to the
//!   daemon on its way out ([`State::exit_request`]): `Detach` with whether to keep running.
//! - **Closing the window** (the console close handler, `term.rs`) sends `WindowClosed` straight
//!   to the daemon: the process has a few seconds left and no event loop. A window that is simply
//!   killed is the daemon's job to notice (it treats the last screen disconnecting without a
//!   goodbye as a close).
//! - **The next launch** may be offered the work a close interrupted ("We were still working when
//!   you closed Marlowe last time (2 crew runs, 1 workflow). Resume?"); the answer goes back as
//!   `ResumeInterrupted`.
//!
//! The running work is tracked from the daemon's task events, observed without consuming them, so
//! the `/tasks` panel and this share one stream.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use marlowe_proto::{Event, Request, TaskInfo, TaskKind, TaskStatus};

use crate::state::{NoteTone, Overlay, State};

/// The quit dialog's buttons, top to bottom.
pub const QUIT_CHOICES: [(&str, &str); 3] =
    [("s", "Stop and close agent"), ("w", "Close window (keep agent running)"), ("c", "Cancel")];
/// What each quit button does, in a few words.
pub const QUIT_HELP: [&str; 3] = [
    "models unload, the GPU is freed, work is saved to resume later",
    "the agent carries on in the background, with a tray icon",
    "stay here",
];
/// The user setting that remembers the quit choice: `stop` or `keep`; absent means ask.
pub const QUIT_SETTING: &str = "quit.choice";
/// The resume offer's buttons.
pub const RESUME_CHOICES: [(&str, &str); 2] = [("y", "Yes"), ("n", "Not now")];
/// The quit dialog opens on Cancel: a stray Enter never ends background work.
pub const QUIT_DEFAULT: usize = 2;

/// What the choice overlay is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// "We were still working when you closed Marlowe last time (...). Resume?"
    Resume { summary: String },
    /// Closing Marlowe; `summary` is what is running, empty when nothing is.
    Quit { summary: String },
}

impl Dialog {
    /// The question, as the dialog words it.
    pub fn prompt(&self) -> String {
        match self {
            Dialog::Resume { summary } => format!("We were still working when you closed Marlowe last time ({summary}). Resume?"),
            Dialog::Quit { summary } => {
                let state = if summary.is_empty() { "Nothing is running right now.".to_string() } else { format!("Still running: {summary}.") };
                format!("{state} Stop the agent, or close the window and leave the agent running?")
            }
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Dialog::Resume { .. } => "resume",
            Dialog::Quit { .. } => "quit",
        }
    }

    /// The buttons: (key, label).
    pub fn choices(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Dialog::Resume { .. } => &RESUME_CHOICES,
            Dialog::Quit { .. } => &QUIT_CHOICES,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Closing {
    tasks: HashMap<String, TaskInfo>,
    /// The daemon's `keep running` setting.
    pub keep_setting: bool,
    pub dialog: Option<Dialog>,
    /// A resume offer waiting for the screen to be free.
    pending_offer: Option<String>,
    /// What the goodbye says: keep the daemon running.
    pub keep_on_exit: bool,
    /// "Remember my choice" is ticked in the quit dialog.
    pub remember: bool,
    /// The panel the quit modal covers (a plan waiting for an answer, say); Cancel brings it back.
    pub(crate) hidden_panel: Option<crate::panel::Panel>,
    /// What is running and whether the daemon keeps running were asked for since the Welcome.
    asked: bool,
}

/// A running task counts as work worth asking about unless it is only waiting for the user, or a
/// schedule (schedules end with the window, and say so when listed).
fn is_work(t: &TaskInfo) -> bool {
    if t.status.is_terminal() || t.kind == TaskKind::Cron {
        return false;
    }
    let waiting_on_user = t.status == TaskStatus::Waiting
        && t.detail.as_deref().is_some_and(|d| d.starts_with("paused") || d.starts_with("needs you"));
    !waiting_on_user
}

impl Closing {
    /// Takes note of the daemon's task events. Never consumes them.
    pub fn observe(&mut self, ev: &Event) {
        match ev {
            Event::Welcome { .. } => self.asked = false,
            Event::Tasks { list } => {
                self.tasks = list.iter().map(|t| (t.id.clone(), t.clone())).collect();
            }
            Event::TaskUpdated(t) => {
                self.tasks.insert(t.id.clone(), t.clone());
            }
            _ => {}
        }
    }

    /// The work that would stop with the window.
    pub fn running(&self) -> Vec<&TaskInfo> {
        let mut v: Vec<&TaskInfo> = self.tasks.values().filter(|t| is_work(t)).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// `2 agent runs, 1 workflow, 1 job, 1 monitor`; empty when nothing runs.
    pub fn running_summary(&self) -> String {
        let mut counts: Vec<(&str, &str, usize)> = vec![
            ("agent run", "agent runs", 0),
            ("workflow", "workflows", 0),
            ("job", "jobs", 0),
            ("monitor", "monitors", 0),
        ];
        for t in self.running() {
            let i = match t.kind {
                TaskKind::Agent => 0,
                TaskKind::Workflow => 1,
                TaskKind::Job => 2,
                TaskKind::Monitor => 3,
                TaskKind::Cron => continue,
            };
            counts[i].2 += 1;
        }
        counts
            .iter()
            .filter(|(_, _, n)| *n > 0)
            .map(|(one, many, n)| if *n == 1 { format!("1 {one}") } else { format!("{n} {many}") })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl State {
    /// Called for every daemon event before it is applied.
    pub(crate) fn closing_observe(&mut self, ev: &Event) {
        self.closing.observe(ev);
    }

    /// The events about closing and the next launch.
    pub(crate) fn closing_event(&mut self, ev: &Event) {
        match ev {
            Event::Interrupted { summary, .. } => {
                self.closing.pending_offer = Some(summary.clone());
                self.show_offer();
            }
            Event::KeepRunning { on } => {
                self.closing.keep_setting = *on;
                self.flash(if *on { "Marlowe keeps running after the window closes" } else { "closing the window stops everything" }, NoteTone::Dim);
            }
            Event::Closing { reason } => {
                self.note(&format!("Marlowe is stopping everything: {reason}"), NoteTone::Amber);
            }
            _ => {}
        }
    }

    /// Opens the resume offer when nothing else is in the way.
    fn show_offer(&mut self) {
        if self.closing.pending_offer.is_none() || self.overlay.is_some() || !self.approvals.is_empty() || self.onboard.in_progress() {
            return;
        }
        if let Some(summary) = self.closing.pending_offer.take() {
            self.closing.dialog = Some(Dialog::Resume { summary });
            self.overlay = Some(Overlay::Choice { sel: 0 });
        }
    }

    /// Per idle tick (online): once after each Welcome, what is running (a quit asks about it)
    /// and whether the daemon keeps running; an offer that had to wait for the screen opens now.
    pub(crate) fn closing_tick(&mut self, out: &mut Vec<Request>) {
        if !self.closing.asked {
            self.closing.asked = true;
            out.push(Request::ListTasks { session: None });
            out.push(Request::KeepRunning { on: None });
        }
        if self.closing.pending_offer.is_some() {
            self.show_offer();
        }
    }

    /// The quit choice the user asked to be remembered: `Some(true)` keeps the agent running. The
    /// daemon's `keep running` switch is the other half of it (the window's X follows it), so a
    /// switch that is on decides, and a remembered "keep" that the switch no longer agrees with
    /// is not a remembered choice any more.
    pub fn remembered_quit(&self) -> Option<bool> {
        if self.closing.keep_setting {
            return Some(true);
        }
        match self.code.settings.as_ref().and_then(|s| s.user_str(QUIT_SETTING)) {
            Some("stop") => Some(false),
            _ => None,
        }
    }

    /// `^q`, `^c`, `/exit`, `/quit`: the quit modal, unless a choice is remembered. A second `^q`
    /// with the modal up is the answer "stop and close agent".
    pub(crate) fn request_quit(&mut self) {
        let modal_up = matches!(self.overlay, Some(Overlay::Choice { .. }));
        match self.closing.dialog.clone() {
            Some(Dialog::Quit { .. }) if modal_up => {
                self.finish_quit(false);
                return;
            }
            // A dialog the screen no longer shows is stale: ask again.
            Some(Dialog::Quit { .. }) => self.closing.dialog = None,
            // The resume offer goes back in the queue, to be asked after the quit modal.
            Some(Dialog::Resume { summary }) => {
                self.closing.pending_offer = Some(summary);
                self.closing.dialog = None;
            }
            None => {}
        }
        // Nobody to ask the daemon's question of: the window just closes.
        if !self.conn.is_online() {
            self.finish_quit(false);
            return;
        }
        if let Some(keep) = self.remembered_quit() {
            self.finish_quit(keep);
            return;
        }
        let summary = self.closing.running_summary();
        self.closing.remember = false;
        if self.overlay == Some(Overlay::Panel) {
            self.closing.hidden_panel = self.code.panel.take();
        }
        self.closing.dialog = Some(Dialog::Quit { summary });
        self.overlay = Some(Overlay::Choice { sel: QUIT_DEFAULT });
    }

    fn finish_quit(&mut self, keep: bool) {
        self.closing.keep_on_exit = keep;
        self.closing.dialog = None;
        self.overlay = None;
        self.quit = true;
    }

    /// A quit that stops the daemon: asks it to, and starts showing its shutdown. `Some` the first
    /// time only (the request to send); `None` when nothing is to wait for (keeping it running, or
    /// offline) or the shutdown is already showing.
    pub fn begin_farewell(&mut self) -> Option<Request> {
        if self.farewell.is_some() || self.closing.keep_on_exit || !matches!(self.conn, crate::state::Conn::Online { .. }) {
            return None;
        }
        self.farewell = Some(crate::seq::Farewell::new(self.now_ms));
        Some(self.exit_request())
    }

    /// The shutdown has finished, or has nothing more to show: the terminal can be given back.
    pub fn farewell_done(&self) -> bool {
        self.farewell.as_ref().is_none_or(|f| f.done(self.now_ms))
    }

    /// The daemon finished closing (as opposed to a skip or a timeout).
    pub fn farewell_shut(&self) -> bool {
        self.farewell.as_ref().is_some_and(|f| f.shut())
    }

    /// `Goodnight, sir.`, from the profile's address.
    pub fn goodnight(&self) -> String {
        crate::seq::goodnight(self.user.as_ref().and_then(|u| u.address.as_deref()))
    }

    /// A research channel or crew spin-up phase: the run's sequence in the transcript, started on
    /// its first phase and updated in place after.
    pub(crate) fn seq_phase(&mut self, p: &marlowe_proto::PhaseEvent) {
        let Some(run) = p.run.as_ref().map(|r| r.as_str().to_string()) else { return };
        let at = self.entries.iter().rposition(|e| matches!(e, crate::state::Entry::Seq(b) if b.takes(&run, p)));
        match at {
            Some(i) => {
                if let crate::state::Entry::Seq(b) = &mut self.entries[i] {
                    b.apply(p);
                }
            }
            None => {
                if let Some(mut b) = crate::seq::Block::open(&run, p) {
                    b.apply(p);
                    self.entries.push(crate::state::Entry::Seq(Box::new(b)));
                }
            }
        }
    }

    /// What the client says to the daemon as it leaves.
    pub fn exit_request(&self) -> Request {
        Request::Detach { keep_running: self.closing.keep_on_exit }
    }

    /// Keys in the choice overlay.
    pub(crate) fn choice_key(&mut self, k: KeyEvent, sel: usize, out: &mut Vec<Request>) {
        let Some(dialog) = self.closing.dialog.clone() else {
            self.overlay = None;
            return;
        };
        let n = dialog.choices().len();
        let quit = matches!(dialog, Dialog::Quit { .. });
        let pick = match k.code {
            KeyCode::Char('r' | ' ') if quit => {
                self.closing.remember = !self.closing.remember;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') if quit => {
                self.overlay = Some(Overlay::Choice { sel: sel.saturating_sub(1) });
                return;
            }
            KeyCode::Down | KeyCode::Char('j') if quit => {
                self.overlay = Some(Overlay::Choice { sel: (sel + 1).min(n - 1) });
                return;
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.overlay = Some(Overlay::Choice { sel: (sel + n - 1) % n });
                return;
            }
            KeyCode::Right | KeyCode::Tab => {
                self.overlay = Some(Overlay::Choice { sel: (sel + 1) % n });
                return;
            }
            KeyCode::Enter => sel.min(n - 1),
            KeyCode::Char(c) => match dialog.choices().iter().position(|(key, _)| key.starts_with(c.to_ascii_lowercase())) {
                Some(i) => i,
                None => return,
            },
            _ => return,
        };
        self.choose(&dialog, pick, out);
    }

    /// Button `pick` of `dialog` was chosen (key or click).
    pub(crate) fn choose(&mut self, dialog: &Dialog, pick: usize, out: &mut Vec<Request>) {
        match (dialog, pick) {
            (Dialog::Resume { .. }, 0) => {
                out.push(Request::ResumeInterrupted { resume: true });
                self.flash("resuming where it stopped", NoteTone::Dim);
                self.close_choice();
            }
            (Dialog::Resume { .. }, _) => {
                out.push(Request::ResumeInterrupted { resume: false });
                self.close_choice();
            }
            (Dialog::Quit { .. }, 0) => {
                self.remember_quit(false, out);
                self.finish_quit(false);
            }
            (Dialog::Quit { .. }, 1) => {
                self.remember_quit(true, out);
                self.finish_quit(true);
            }
            (Dialog::Quit { .. }, _) => self.close_choice(),
        }
    }

    /// "Remember my choice": the user setting, and the daemon's `keep running` switch to match, so
    /// closing the window by its X does what was chosen here.
    fn remember_quit(&mut self, keep: bool, out: &mut Vec<Request>) {
        if !std::mem::take(&mut self.closing.remember) {
            return;
        }
        out.push(Request::SetSetting {
            scope: marlowe_proto::SettingsScope::User,
            key: QUIT_SETTING.into(),
            value: serde_json::Value::String(if keep { "keep" } else { "stop" }.into()),
            workspace: None,
        });
        out.push(Request::KeepRunning { on: Some(keep) });
    }

    pub(crate) fn close_choice(&mut self) {
        use crate::panel::Panel;
        self.closing.dialog = None;
        self.overlay = None;
        let waiting = !self.code.questions.is_empty() || !self.bg.plans.is_empty();
        match self.closing.hidden_panel.take() {
            // The covered plan or question is still unanswered: it comes back first.
            Some(p @ (Panel::Plan(_) | Panel::Question(_))) => self.restore_panel(p),
            Some(p) if !waiting => self.restore_panel(p),
            // A plan or question arrived under the modal: it is shown, the covered panel behind it.
            Some(p) => {
                if self.code.stashed_panel.is_none() {
                    self.code.stashed_panel = Some(p);
                }
                self.next_question();
            }
            None if waiting => self.next_question(),
            None => {}
        }
    }

    fn restore_panel(&mut self, p: crate::panel::Panel) {
        self.code.panel = Some(p);
        self.overlay = Some(Overlay::Panel);
    }

    /// Whether the quit or resume modal is on screen. Whatever arrives meanwhile waits behind it.
    pub(crate) fn choice_up(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Choice { .. }))
    }

    /// Puts `p` on screen, or behind the modal when one is up and nothing else is waiting there.
    pub(crate) fn place_panel(&mut self, p: crate::panel::Panel) {
        if self.choice_up() {
            if self.closing.hidden_panel.is_none() {
                self.closing.hidden_panel = Some(p);
            } else if self.code.stashed_panel.is_none() {
                self.code.stashed_panel = Some(p);
            }
            return;
        }
        self.code.panel = Some(p);
        self.overlay = Some(Overlay::Panel);
    }

    /// `/keep-running [on|off]`.
    pub(crate) fn slash_keep_running(&mut self, args: &str, out: &mut Vec<Request>) {
        match args.trim().to_ascii_lowercase().as_str() {
            "" => {
                let now = if self.closing.keep_setting { "on: Marlowe keeps running after the window closes" } else { "off: closing the window stops everything" };
                self.flash(format!("keep running is {now} · /keep-running on|off"), NoteTone::Dim);
                out.push(Request::KeepRunning { on: None });
            }
            "on" | "yes" | "true" => out.push(Request::KeepRunning { on: Some(true) }),
            "off" | "no" | "false" => out.push(Request::KeepRunning { on: Some(false) }),
            // Forget a remembered quit choice: the modal asks again.
            "ask" | "reset" => {
                out.push(Request::SetSetting { scope: marlowe_proto::SettingsScope::User, key: QUIT_SETTING.into(), value: serde_json::Value::Null, workspace: None });
                out.push(Request::KeepRunning { on: Some(false) });
                self.flash("closing Marlowe asks again", NoteTone::Dim);
            }
            other => self.flash(format!("/keep-running takes on, off or ask, not `{other}`"), NoteTone::Amber),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, kind: TaskKind, status: TaskStatus, detail: Option<&str>) -> TaskInfo {
        TaskInfo {
            id: id.into(),
            kind,
            session: None,
            title: id.into(),
            status,
            started_ms: 0,
            ended_ms: None,
            output_file: None,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn the_summary_counts_work_not_schedules_or_what_waits_on_the_user() {
        let mut c = Closing::default();
        c.observe(&Event::Tasks {
            list: vec![
                task("agent_r1", TaskKind::Agent, TaskStatus::Running, None),
                task("agent_r2", TaskKind::Agent, TaskStatus::Running, None),
                task("wf_r3", TaskKind::Workflow, TaskStatus::Running, None),
                task("job_j1", TaskKind::Job, TaskStatus::Running, None),
                task("mon_m1", TaskKind::Monitor, TaskStatus::Waiting, None),
                task("cron_1", TaskKind::Cron, TaskStatus::Waiting, None),
                task("agent_r4", TaskKind::Agent, TaskStatus::Waiting, Some("paused: interrupted")),
                task("job_j0", TaskKind::Job, TaskStatus::Done, None),
            ],
        });
        assert_eq!(c.running_summary(), "2 agent runs, 1 workflow, 1 job, 1 monitor");
        // An update replaces; a finished job is no longer work.
        c.observe(&Event::TaskUpdated(task("job_j1", TaskKind::Job, TaskStatus::Done, None)));
        assert_eq!(c.running_summary(), "2 agent runs, 1 workflow, 1 monitor");
    }

    #[test]
    fn the_prompts_read_as_the_dialogs_say_them() {
        let d = Dialog::Resume { summary: "2 crew runs, 1 workflow".into() };
        assert_eq!(d.prompt(), "We were still working when you closed Marlowe last time (2 crew runs, 1 workflow). Resume?");
        assert_eq!(d.choices().len(), 2);
        let q = Dialog::Quit { summary: "1 job".into() };
        assert!(q.prompt().contains("1 job") && q.prompt().contains("leave the agent running"));
        assert_eq!(
            q.choices().iter().map(|(_, l)| *l).collect::<Vec<_>>(),
            ["Stop and close agent", "Close window (keep agent running)", "Cancel"]
        );
        let idle = Dialog::Quit { summary: String::new() };
        assert!(idle.prompt().starts_with("Nothing is running right now."));
    }
}
