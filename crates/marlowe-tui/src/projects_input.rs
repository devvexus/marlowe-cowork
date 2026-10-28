//! Keys and clicks of a project's entry: the card's answers, `revise`, `checkin now`, `cancel`,
//! folding, and opening the report. Handlers return the requests to send, like every other
//! handler of the reducer.
//!
//! The entry never takes the keys by itself: `alt-p` (or a click on it, or on its row of the
//! board) gives it the Conversation box's focus, and `esc` gives it back. Until then the composer
//! types.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_project::{GateKind, ProjectStatus};
use marlowe_proto::Request;

use crate::panel::{Panel, ReportUi};
use crate::projects::{ProjTarget, Prompt};
use crate::state::{Entry, Focus, NoteTone, State, View};

impl State {
    /// `alt-p`: to the project that needs you, else the newest running one; its entry gets the
    /// keys and the view follows the tail, where a running project is drawn.
    pub fn projects_alt_p(&mut self) {
        let Some(id) = self.projects.attention().map(str::to_string) else {
            self.flash("no project is running", NoteTone::Dim);
            return;
        };
        self.focus_project(&id);
        self.follow_tail();
    }

    /// Gives the project's entry the keys.
    pub(crate) fn focus_project(&mut self, id: &str) {
        self.view = View::Conversation;
        self.focus = Focus::Conversation;
        self.projects.focus = Some(id.to_string());
        self.projects.sync_card(id);
    }

    /// Brings the project's entry into the pane and gives it the keys: a click on its board row.
    pub(crate) fn show_project(&mut self, id: &str) {
        self.focus_project(id);
        let live = self.projects.get(id).is_some_and(|l| !l.is_final());
        let at = self.entries.iter().position(|e| matches!(e, Entry::Project { id: p } if p == id));
        match (live, at) {
            (false, Some(i)) => {
                let end = self.layout.borrow().entry_end(i);
                let v = self.viewport.get();
                let h = v.size.1 as usize;
                if let Some(end) = end.filter(|_| h > 0 && v.total > 0) {
                    let max = v.total.saturating_sub(h);
                    let top = end.saturating_sub(h).min(max);
                    self.anchor.set(None);
                    self.scroll = max - top;
                }
            }
            _ => self.follow_tail(),
        }
    }

    /// A key while the Conversation box has the keys and a project's entry is focused. True when
    /// the entry took it; anything else goes on to the box (`↑↓` scroll, a letter types).
    pub(crate) fn projects_card_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let Some(id) = self.projects.focus.clone() else { return false };
        if self.projects.get(&id).is_none() {
            self.projects.focus = None;
            return false;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) || k.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        let now = self.now_ms;
        // The inline steer line takes every key first, then the cancel question.
        match self.projects.prompt.clone() {
            Some(Prompt::Steer { project, mut text }) if project == id => {
                match k.code {
                    KeyCode::Esc => self.projects.prompt = None,
                    KeyCode::Enter if !text.trim().is_empty() => {
                        self.projects.prompt = Some(Prompt::Steer { project, text });
                        self.projects.answer(&id, "steer", now, out);
                    }
                    KeyCode::Enter => {}
                    KeyCode::Backspace => {
                        text.pop();
                        self.projects.prompt = Some(Prompt::Steer { project, text });
                    }
                    KeyCode::Char(c) => {
                        text.push(c);
                        self.projects.prompt = Some(Prompt::Steer { project, text });
                    }
                    _ => {}
                }
                return true;
            }
            Some(Prompt::ConfirmCancel { project }) if project == id => {
                if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                    self.confirm_cancel(&id, out);
                } else {
                    self.projects.prompt = None;
                }
                return true;
            }
            _ => {}
        }
        if k.code == KeyCode::Esc {
            self.projects.focus = None;
            self.focus = Focus::Message;
            self.follow_tail();
            return true;
        }
        let Some((status, gate_kind, options)) = self.projects.get(&id).map(|l| {
            (l.state.status, l.open_gate().map(|g| g.kind), l.open_gate().map(|g| g.options.iter().map(|o| o.id.clone()).collect::<Vec<_>>()).unwrap_or_default())
        }) else {
            return false;
        };
        match status {
            ProjectStatus::AwaitingPlan | ProjectStatus::AwaitingCheckin if gate_kind.is_some() => match k.code {
                KeyCode::Char('r') if gate_kind == Some(GateKind::Plan) => {
                    self.proj_revise(&id);
                    true
                }
                KeyCode::Char(c) => match self.projects.option_for_key(&id, c) {
                    Some(opt) => {
                        if let Some(i) = options.iter().position(|o| *o == opt) {
                            self.projects.card_sel = i;
                        }
                        self.projects.answer(&id, &opt, now, out);
                        true
                    }
                    None => false,
                },
                KeyCode::Left => {
                    self.projects.card_sel = self.projects.card_sel.saturating_sub(1);
                    true
                }
                KeyCode::Right => {
                    self.projects.card_sel = (self.projects.card_sel + 1).min(options.len().saturating_sub(1));
                    true
                }
                KeyCode::Enter => {
                    if let Some(opt) = options.get(self.projects.card_sel).cloned() {
                        self.projects.answer(&id, &opt, now, out);
                    }
                    true
                }
                _ => false,
            },
            ProjectStatus::Running => match k.code {
                KeyCode::Char('c') => {
                    self.projects.checkin_now(&id, out);
                    true
                }
                KeyCode::Char('x') => {
                    self.projects.cancel_run(&id, out);
                    true
                }
                KeyCode::Char('-') => {
                    self.toggle_fold(&id);
                    true
                }
                _ => false,
            },
            _ if status.is_terminal() => match k.code {
                KeyCode::Enter => {
                    self.open_report(&id, out);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn toggle_fold(&mut self, id: &str) {
        if !self.projects.folded.remove(id) {
            self.projects.folded.insert(id.to_string());
        }
    }

    /// `y` on the cancel question: the card's own `cancel` when a card is open, else the project's.
    fn confirm_cancel(&mut self, id: &str, out: &mut Vec<Request>) {
        let now = self.now_ms;
        let gate_cancel = self.projects.get(id).and_then(|l| l.open_gate()).is_some_and(|g| g.options.iter().any(|o| o.id == "cancel"));
        if gate_cancel {
            self.projects.answer(id, "cancel", now, out);
        } else {
            self.projects.cancel_run(id, out);
        }
    }

    /// `revise`: the composer gets the words and the keys.
    pub(crate) fn proj_revise(&mut self, id: &str) {
        if let Some(t) = self.projects.revise_text(id) {
            self.composer.set(&t);
            self.view = View::Conversation;
            self.focus = Focus::Message;
            self.projects.focus = None;
            self.flash("revise: say what to change; the conductor revises the plan and a new card arrives", NoteTone::Dim);
        }
    }

    /// Opens the report in the reader panel.
    pub(crate) fn open_report(&mut self, id: &str, out: &mut Vec<Request>) {
        if self.projects.get(id).is_none_or(|l| l.report.is_none()) {
            self.flash("this project wrote no report file", NoteTone::Amber);
            return;
        }
        self.open_panel(Panel::Report(ReportUi { project: id.to_string(), scroll: Default::default() }), out);
    }

    /// A click on something a project drew.
    pub(crate) fn proj_click(&mut self, t: ProjTarget, out: &mut Vec<Request>) {
        let now = self.now_ms;
        match t {
            ProjTarget::Option(id, opt) => {
                self.focus_project(&id);
                if let Some(i) = self.projects.get(&id).and_then(|l| l.open_gate()).and_then(|g| g.options.iter().position(|o| o.id == opt)) {
                    self.projects.card_sel = i;
                }
                self.projects.answer(&id, &opt, now, out);
            }
            ProjTarget::Revise(id) => self.proj_revise(&id),
            ProjTarget::Cancel(id) => {
                self.focus_project(&id);
                self.projects.cancel_run(&id, out);
            }
            ProjTarget::CancelYes(id) => self.confirm_cancel(&id, out),
            ProjTarget::CancelNo => self.projects.prompt = None,
            ProjTarget::Checkin(id) => {
                self.projects.checkin_now(&id, out);
            }
            ProjTarget::Report(id) => self.open_report(&id, out),
            ProjTarget::Fold(id) => self.toggle_fold(&id),
            ProjTarget::Entry(id) => self.focus_project(&id),
            ProjTarget::Rail(id) => self.show_project(&id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_without_projects_has_nothing_to_focus() {
        let mut s = State::default();
        s.projects_alt_p();
        assert!(s.projects.focus.is_none());
    }
}
