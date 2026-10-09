//! Keys, paste and mouse: the input half of the reducer.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{Request, Slot};
use ratatui::layout::{Position, Rect};

use crate::library::LibStep;
use crate::render::geometry;
use crate::state::{
    Command, NoteTone, Overlay, State, View, APPROVAL_CHOICES, APPROVAL_DETAIL_ROWS, APPROVAL_GUARD_MS,
};

/// Lines one wheel notch scrolls.
const WHEEL_LINES: usize = 3;

impl State {
    /// Handles a key press. Returns requests to send.
    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Request> {
        let mut out = Vec::new();
        if k.kind == KeyEventKind::Release {
            return out;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // Typing before onboarding shows means they are busy: it stays away this session.
        let printable = matches!(k.code, KeyCode::Char(_)) && !ctrl;
        if printable && self.onboard == crate::onboard::Onboard::Pending {
            self.close_onboarding();
        }
        // Any key ends the boot and then does what it would have done: typing is never delayed.
        self.wake.boot = None;
        // Barge-in: any key stops the voice.
        if self.wake.speaking() {
            if let Some(v) = &mut self.wake.voice {
                v.speaking = false;
            }
            out.push(Request::StopSpeaking);
        }

        // The approval modal takes every key while it is up, except quit (the daemon then times
        // the request out to Deny).
        if ctrl && k.code == KeyCode::Char('q') {
            self.quit = true;
            return out;
        }
        if !self.approvals.is_empty() {
            self.approval_key(k, &mut out);
            return out;
        }
        if self.overlay == Some(Overlay::Onboard) && self.onboard_typeahead(&k) {
            // Typed before the question could be read: the key goes where it was meant to.
            self.close_onboarding();
        } else if self.overlay.is_some() {
            self.overlay_key(k, &mut out);
            return out;
        }
        if ctrl {
            if let KeyCode::Char(c) = k.code {
                let cmd = match c.to_ascii_lowercase() {
                    'k' => {
                        self.overlay = Some(Overlay::Palette { query: String::new(), sel: 0 });
                        return out;
                    }
                    'q' => Some(Command::Quit),
                    'n' => Some(Command::NewSession),
                    'o' => Some(Command::Sessions),
                    'l' => Some(Command::Library),
                    'g' => Some(Command::Memory),
                    'r' => Some(Command::Crew),
                    // ^d is forget in the memory view.
                    'd' if self.view != View::Memory => Some(Command::Dispatch),
                    // ^w deletes a word while there is text in the composer, as ^c clears it.
                    'w' if !(self.view == View::Conversation && !self.composer.text().is_empty()) => {
                        Some(Command::Watch)
                    }
                    't' => Some(Command::ToggleThinking),
                    'c' if self.view == View::Conversation && !self.composer.text().is_empty() => {
                        self.composer.clear();
                        return out;
                    }
                    'c' => Some(Command::Quit),
                    _ => None,
                };
                if let Some(cmd) = cmd {
                    self.run_command(cmd, &mut out);
                    return out;
                }
            }
        }
        match self.view {
            View::Library => {
                let (stay, flash) = self.library.on_key(k, &mut out);
                if let Some(f) = flash {
                    self.flash(f, NoteTone::Dim);
                }
                if !stay {
                    self.view = View::Conversation;
                }
            }
            View::Memory => self.memory_key(k, &mut out),
            View::Watch => self.watch_key(k, &mut out),
            View::Crew => self.crew_key(k, &mut out),
            View::Conversation => self.conversation_key(k, &mut out),
        }
        out
    }

    /// A bracketed or detected paste.
    pub fn on_paste(&mut self, text: &str) {
        self.wake.boot = None;
        if !self.approvals.is_empty() {
            return;
        }
        match (&mut self.overlay, self.view) {
            (Some(Overlay::Palette { query, sel }), _) => {
                query.push_str(text.lines().next().unwrap_or(""));
                *sel = 0;
            }
            (Some(Overlay::Onboard), _) => self.onboard_paste(text),
            (Some(Overlay::Dispatch), _) => self.dispatch_paste(text),
            (Some(_), _) => {}
            (None, View::Library) => {
                for c in text.trim().chars() {
                    let mut ignore = Vec::new();
                    let _ = self.library.on_key(KeyEvent::from(KeyCode::Char(c)), &mut ignore);
                }
            }
            (None, View::Memory) => match &mut self.memory.mode {
                crate::memory::MemMode::Browse => self.memory.query.push_str(text.lines().next().unwrap_or("")),
                crate::memory::MemMode::Correct { input, .. } => input.insert_str(text),
                crate::memory::MemMode::ConfirmForget { .. } => {}
            },
            (None, View::Watch) => {
                if let crate::wake::WatchMode::Add { input } = &mut self.wake.mode {
                    input.insert_str(text.lines().next().unwrap_or("").trim());
                }
            }
            (None, View::Crew) => {
                if let Some(v) = self.crew.open.as_mut().filter(|v| v.steering) {
                    v.steer.insert_str(&text.replace(['\r', '\n'], " "));
                }
            }
            (None, View::Conversation) => self.composer.insert_str(text),
        }
    }

    /// Handles a mouse event against a frame of `area`.
    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) -> Vec<Request> {
        let mut out = Vec::new();
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.wake.boot = None;
        }
        let g = geometry(area, self);
        let at = Position { x: m.column, y: m.row };
        match m.kind {
            MouseEventKind::ScrollUp if self.view == View::Conversation && self.overlay.is_none() => {
                self.scroll = (self.scroll + WHEEL_LINES).min(self.scroll_max.get());
            }
            MouseEventKind::ScrollDown if self.view == View::Conversation && self.overlay.is_none() => {
                self.scroll = self.scroll.saturating_sub(WHEEL_LINES);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if !self.approvals.is_empty() {
                    return out;
                }
                if let Some(Overlay::ModelPicker { .. }) = self.overlay {
                    if let Some(r) = g.picker {
                        if r.contains(at) {
                            let row = at.y.saturating_sub(r.y + 1) as usize;
                            self.overlay = Some(Overlay::ModelPicker { sel: row });
                            self.overlay_key(KeyEvent::from(KeyCode::Enter), &mut out);
                            return out;
                        }
                    }
                    self.overlay = None;
                    return out;
                }
                if g.model.is_some_and(|r| r.contains(at)) && self.overlay.is_none() {
                    self.open_model_picker();
                }
            }
            _ => {}
        }
        out
    }

    fn open_model_picker(&mut self) {
        if !self.library.has_models() {
            self.view = View::Library;
            self.library.step = LibStep::Overview;
            return;
        }
        let current = self.library.conductor();
        let sel = self
            .library
            .models
            .iter()
            .position(|m| Some(&m.model) == current.as_ref())
            .unwrap_or(0);
        self.overlay = Some(Overlay::ModelPicker { sel });
    }

    pub(crate) fn run_command(&mut self, cmd: Command, out: &mut Vec<Request>) {
        self.overlay = None;
        match cmd {
            Command::NewSession => {
                self.reset_conversation();
                self.view = View::Conversation;
            }
            Command::Sessions => {
                self.sessions = None;
                self.overlay = Some(Overlay::Sessions { sel: 0 });
                out.push(Request::ListSessions { limit: 30 });
            }
            Command::Library => {
                if self.view == View::Library {
                    self.view = View::Conversation;
                } else {
                    self.view = View::Library;
                    self.library.step = LibStep::Overview;
                    out.push(Request::LibraryState);
                }
            }
            Command::Memory => {
                if self.view == View::Memory {
                    self.view = View::Conversation;
                } else {
                    self.open_memory(out);
                }
            }
            Command::Watch => {
                if self.view == View::Watch {
                    self.view = View::Conversation;
                } else {
                    self.open_watch(out);
                }
            }
            Command::Crew => {
                if self.view == View::Crew {
                    self.view = View::Conversation;
                } else {
                    self.open_crew(out);
                }
            }
            Command::Dispatch => self.open_dispatch(out),
            Command::Brief => {
                self.view = View::Conversation;
                out.push(Request::Brief);
            }
            Command::ToggleVoice => {
                let on = self.wake.voice.as_ref().is_some_and(|v| v.enabled);
                out.push(Request::VoiceSettings { enabled: Some(!on), voice: None });
            }
            Command::ImportMemories => {
                if self.import.is_none() {
                    self.start_import(out);
                }
            }
            Command::Introduce => {
                self.view = View::Conversation;
                self.open_onboarding(false, out);
            }
            Command::ToggleThinking => self.show_thinking = !self.show_thinking,
            Command::Interrupt => {
                if let (Some(s), true) = (self.session.clone(), self.is_busy()) {
                    out.push(Request::Interrupt { session: s });
                }
            }
            Command::Quit => self.quit = true,
        }
    }

    /// Up/Down/PgUp/PgDn/Home/End scroll the detail. Decision keys are ignored for a moment after
    /// the modal appears (type-ahead), and allow needs the whole detail to have been shown.
    fn approval_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(a) = self.approvals.first() else { return };
        let id = a.id;
        let max = a.detail_scroll_max();
        let scroll = match k.code {
            KeyCode::Up => Some(self.approval_scroll.saturating_sub(1)),
            KeyCode::Down => Some(self.approval_scroll + 1),
            KeyCode::PageUp => Some(self.approval_scroll.saturating_sub(APPROVAL_DETAIL_ROWS)),
            KeyCode::PageDown => Some(self.approval_scroll + APPROVAL_DETAIL_ROWS),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(max),
            _ => None,
        };
        if let Some(to) = scroll {
            self.approval_scroll = to.min(max);
            if self.approval_scroll >= max {
                self.approval_read = true;
            }
            return;
        }
        if self.now_ms.saturating_sub(self.approval_since_ms) < APPROVAL_GUARD_MS {
            return;
        }
        let decision = match k.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.approval_sel = self.approval_sel.saturating_sub(1);
                None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.approval_sel = (self.approval_sel + 1).min(APPROVAL_CHOICES.len() - 1);
                None
            }
            KeyCode::Enter => Some(APPROVAL_CHOICES[self.approval_sel]),
            KeyCode::Char('y') => Some(APPROVAL_CHOICES[0]),
            KeyCode::Char('a') => Some(APPROVAL_CHOICES[1]),
            KeyCode::Char('n') | KeyCode::Esc => Some(APPROVAL_CHOICES[2]),
            _ => None,
        };
        if let Some(decision) = decision {
            if decision.is_allowed() && !self.approval_read {
                return;
            }
            out.push(Request::Approve { approval: id, decision });
            // Resolve locally; the daemon's ApprovalResolved is then a no-op.
            self.approvals.remove(0);
            self.front_approval_changed();
        }
    }

    fn overlay_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        if self.overlay == Some(Overlay::Onboard) {
            self.onboard_key(k, out);
            return;
        }
        if self.overlay == Some(Overlay::Dispatch) {
            self.dispatch_key(k, out);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc || (ctrl && k.code == KeyCode::Char('k')) {
            if matches!(self.overlay, Some(Overlay::ImportOffer { .. })) {
                self.legacy_dismissed = true;
            }
            self.overlay = None;
            return;
        }
        let Some(overlay) = self.overlay.clone() else { return };
        match overlay {
            Overlay::Palette { mut query, sel } => {
                let n = palette_matches(&query).len();
                match k.code {
                    KeyCode::Up => self.overlay = Some(Overlay::Palette { query, sel: sel.saturating_sub(1) }),
                    KeyCode::Down | KeyCode::Tab => {
                        let sel = (sel + 1).min(n.saturating_sub(1));
                        self.overlay = Some(Overlay::Palette { query, sel });
                    }
                    KeyCode::Enter => {
                        if let Some(cmd) = palette_matches(&query).get(sel).copied() {
                            self.run_command(cmd, out);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.overlay = Some(Overlay::Palette { query, sel: 0 });
                    }
                    KeyCode::Char(c) if !ctrl => {
                        query.push(c);
                        self.overlay = Some(Overlay::Palette { query, sel: 0 });
                    }
                    _ => {}
                }
            }
            Overlay::ImportOffer { sel } => self.offer_key(k, sel, out),
            Overlay::Onboard | Overlay::Dispatch => {}
            Overlay::Sessions { sel } => {
                let n = self.sessions.as_ref().map_or(0, Vec::len);
                match k.code {
                    KeyCode::Up => self.overlay = Some(Overlay::Sessions { sel: sel.saturating_sub(1) }),
                    KeyCode::Down | KeyCode::Tab => {
                        self.overlay = Some(Overlay::Sessions { sel: (sel + 1).min(n.saturating_sub(1)) })
                    }
                    KeyCode::Enter => {
                        let pick = self.sessions.as_ref().and_then(|s| s.get(sel)).map(|s| s.id.clone());
                        if let Some(id) = pick {
                            self.overlay = None;
                            self.reset_conversation();
                            self.expect_open(id.clone());
                            self.view = View::Conversation;
                            out.push(Request::OpenSession { session: id });
                        }
                    }
                    _ => {}
                }
            }
            Overlay::ModelPicker { sel } => {
                let n = self.library.models.len();
                match k.code {
                    KeyCode::Up => self.overlay = Some(Overlay::ModelPicker { sel: sel.saturating_sub(1) }),
                    KeyCode::Down | KeyCode::Tab => {
                        // The last row opens the library.
                        self.overlay = Some(Overlay::ModelPicker { sel: (sel + 1).min(n) })
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        if sel >= n {
                            self.run_command(Command::Library, out);
                            return;
                        }
                        let model = self.library.models[sel].model.clone();
                        let effort = self.library.slot_rows()[0].effort;
                        out.push(Request::SetSlot {
                            slot: Slot::Conductor,
                            model: Some(model.clone()),
                            effort,
                        });
                        out.push(Request::LibraryState);
                        // Show the choice at once; the LibraryState reply confirms it.
                        let row = marlowe_proto::SlotAssignment {
                            slot: Slot::Conductor,
                            model: Some(model),
                            effort,
                            inherited: false,
                        };
                        match self.library.slots.iter_mut().find(|s| s.slot == Slot::Conductor) {
                            Some(s) => *s = row,
                            None => self.library.slots.push(row),
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn conversation_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        self.flash = None;
        match k.code {
            KeyCode::Enter if shift || alt => self.composer.insert('\n'),
            KeyCode::Char('j') if ctrl => self.composer.insert('\n'),
            KeyCode::Enter => self.send(out),
            KeyCode::Esc => {
                if self.is_busy() {
                    self.run_command(Command::Interrupt, out);
                } else if self.scroll > 0 {
                    self.scroll = 0;
                }
            }
            KeyCode::PageUp => {
                self.scroll = (self.scroll + self.page()).min(self.scroll_max.get());
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            KeyCode::Home if ctrl => self.scroll = self.scroll_max.get(),
            KeyCode::End if ctrl => self.scroll = 0,
            KeyCode::Backspace if ctrl || alt => self.composer.backspace_word(),
            KeyCode::Char('w') | KeyCode::Char('h') if ctrl => self.composer.backspace_word(),
            KeyCode::Char('u') if ctrl => self.composer.clear(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Up => {
                if !self.composer.up() {
                    self.scroll = (self.scroll + 1).min(self.scroll_max.get());
                }
            }
            KeyCode::Down => {
                if !self.composer.down() {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::Tab => self.composer.insert_str("    "),
            KeyCode::Char(c) if !ctrl => self.composer.insert(c),
            _ => {}
        }
    }

    fn page(&self) -> usize {
        10
    }

    fn send(&mut self, out: &mut Vec<Request>) {
        if self.composer.is_blank() {
            if self.library.known && !self.library.has_models() {
                self.view = View::Library;
                self.library.start_add();
            }
            return;
        }
        if !self.conn.is_online() {
            self.flash("offline — your message is still here; it will send once reconnected", NoteTone::Amber);
            return;
        }
        if self.library.known && !self.library.has_models() {
            self.flash("add a model first — opening the library", NoteTone::Amber);
            self.view = View::Library;
            self.library.start_add();
            return;
        }
        let text = self.composer.take().trim_end().to_string();
        match self.session.clone() {
            Some(s) => {
                if self.is_busy() {
                    self.entries.push(crate::state::Entry::User { text: text.clone(), steer: true });
                }
                out.push(Request::Ask { session: Some(s), text, workspace: None });
            }
            None => {
                self.expect_new_session(&text);
                out.push(Request::Ask {
                    session: None,
                    text,
                    workspace: self.default_workspace.clone(),
                });
            }
        }
        self.scroll = 0;
    }
}

/// Palette commands matching a query.
pub fn palette_matches(query: &str) -> Vec<Command> {
    let q = query.trim().to_lowercase();
    let mut v: Vec<Command> =
        Command::ALL.into_iter().filter(|c| c.label().to_lowercase().contains(&q)).collect();
    // Prefix matches first.
    v.sort_by_key(|c| !c.label().to_lowercase().starts_with(&q));
    v
}
