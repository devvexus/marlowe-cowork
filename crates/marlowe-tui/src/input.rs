//! Keys, paste and mouse: the input half of the reducer.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::Request;
use ratatui::layout::{Position, Rect};

use crate::library::LibStep;
use crate::render::geometry;
use crate::render::BoardMode;
use crate::state::{
    pick_thought, Anchor, BoardTab, Command, Entry, Focus, NoteTone, Overlay, State, Target, View, APPROVAL_CHOICES,
    APPROVAL_DETAIL_ROWS, APPROVAL_GUARD_MS,
};

/// Lines one wheel notch scrolls.
const WHEEL_LINES: usize = 3;

impl State {
    /// Handles a key press. Returns requests to send.
    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Request> {
        self.settle_scroll();
        let mut out = Vec::new();
        if k.kind == KeyEventKind::Release {
            return out;
        }
        // While Marlowe shuts down any key leaves at once.
        if let Some(f) = &mut self.farewell {
            f.skip();
            return out;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);

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
            self.request_quit();
            return out;
        }
        if !self.approvals.is_empty() && !matches!(self.overlay, Some(Overlay::Choice { .. })) {
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
                    // ^o: every tool call in full, as Claude Code's transcript view.
                    'o' => Some(Command::Transcript),
                    'l' => Some(Command::Library),
                    'g' => Some(Command::Memory),
                    // ^r searches the prompt history in the conversation; the other views keep
                    // it as the way to the crew and back.
                    'r' if self.view == View::Conversation => Some(Command::HistorySearch),
                    'r' => Some(Command::Crew),
                    'b' => Some(Command::Background),
                    'v' if self.view == View::Conversation => {
                        self.paste_clipboard_image();
                        return out;
                    }
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
        // Alt chords. Ctrl+Alt is AltGr on many layouts and is a character.
        if alt && !ctrl {
            let cmd = match k.code {
                KeyCode::Char('v' | 'V') => Some(Command::ToggleVoice),
                KeyCode::Char('s' | 'S') => Some(Command::Sessions),
                KeyCode::Char('r' | 'R') => Some(Command::Crew),
                KeyCode::Char('t' | 'T') => Some(Command::CycleEffort),
                // Windows Terminal keeps ^v for itself: the other way to paste an image.
                KeyCode::Char('p' | 'P') if self.view == View::Conversation => {
                    self.paste_clipboard_image();
                    return out;
                }
                _ => None,
            };
            if let Some(cmd) = cmd {
                self.run_command(cmd, &mut out);
                return out;
            }
        }
        match self.view {
            View::Library if k.code == KeyCode::Char('c') && self.library_model_row().is_some() => {
                if let Some(m) = self.library_model_row() {
                    self.open_context(m, &mut out);
                }
            }
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
            if let Some(r) = self.approval_reason.as_mut() {
                r.insert_str(&text.replace(['\r', '\n'], " "));
            }
            return;
        }
        match (&mut self.overlay, self.view) {
            (Some(Overlay::Palette { query, sel }), _) | (Some(Overlay::WorkspacePicker { query, sel }), _) => {
                query.push_str(text.lines().next().unwrap_or(""));
                *sel = 0;
            }
            (Some(Overlay::Onboard), _) => self.onboard_paste(text),
            (Some(Overlay::Dispatch), _) => self.dispatch_paste(text),
            (Some(Overlay::Panel), _) => self.panel_paste(text),
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
            (None, View::Conversation) => self.paste_into_composer(text),
        }
    }

    /// Handles a mouse event against a frame of `area`. Motion only moves [`State::hover`]; the
    /// run loop redraws when that changes.
    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) -> Vec<Request> {
        let mut out = Vec::new();
        let at = Position { x: m.column, y: m.row };
        match m.kind {
            MouseEventKind::Moved => self.hover = self.pointer_target(area, at),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                self.wheel(up);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.wake.boot = None;
                self.click(area, at, &mut out);
                // What is under the pointer may have changed with the screen.
                self.hover = None;
            }
            _ => {}
        }
        out
    }

    /// The wheel scrolls whatever is reading: the conversation, a run's transcript, an approval.
    fn wheel(&mut self, up: bool) {
        self.settle_scroll();
        if let Some(a) = self.approvals.first() {
            let max = a.detail_scroll_max();
            self.approval_scroll =
                if up { self.approval_scroll.saturating_sub(WHEEL_LINES) } else { (self.approval_scroll + WHEEL_LINES).min(max) };
            if self.approval_scroll >= max {
                self.approval_read = true;
            }
            return;
        }
        if self.overlay == Some(Overlay::Panel) {
            self.panel_wheel(up);
            return;
        }
        if self.overlay.is_some() {
            return;
        }
        match self.view {
            View::Conversation if up => self.scroll = (self.scroll + WHEEL_LINES).min(self.scroll_max.get()),
            View::Conversation => self.scroll = self.scroll.saturating_sub(WHEEL_LINES),
            View::Crew => {
                if let Some(v) = self.crew.open.as_mut() {
                    let max = v.scroll_max.get();
                    v.scroll = if up { (v.scroll + WHEEL_LINES).min(max) } else { v.scroll.saturating_sub(WHEEL_LINES) };
                }
            }
            _ => {}
        }
    }

    /// What the pointer is over: whatever the last frame recorded there, else (on the conversation
    /// screen with nothing floating) the box under it.
    pub fn pointer_target(&self, area: Rect, at: Position) -> Option<Target> {
        if let Some(t) = self.target_at(at) {
            return Some(t);
        }
        if !self.approvals.is_empty() || self.overlay.is_some() || self.view != View::Conversation {
            return None;
        }
        let g = geometry(area, self);
        if g.model.is_some_and(|r| r.contains(at)) {
            Some(Target::Box(Focus::Model))
        } else if g.workspace.is_some_and(|r| r.contains(at)) {
            Some(Target::Box(Focus::Workspace))
        } else if let Some(t) = g.tab_at(at) {
            Some(Target::Tab(t))
        } else if g.board.contains(at) {
            // The tabbed board's body is the section on show.
            Some(Target::Tab(self.board_tab))
        } else if g.composer.is_some_and(|r| r.contains(at)) {
            Some(Target::Box(Focus::Message))
        } else if g.conversation_box.contains(at) {
            Some(Target::Box(Focus::Conversation))
        } else {
            None
        }
    }

    fn click(&mut self, area: Rect, at: Position, out: &mut Vec<Request>) {
        if !self.approvals.is_empty() && !matches!(self.overlay, Some(Overlay::Choice { .. })) {
            if let Some(Target::Button(i)) = self.target_at(at) {
                if self.now_ms.saturating_sub(self.approval_since_ms) >= APPROVAL_GUARD_MS {
                    // One past the buttons is "deny, and tell Marlowe why".
                    let n = self.approvals.first().map_or(APPROVAL_CHOICES.len(), |a| a.choices().len()) + 1;
                    self.approval_sel = i.min(n - 1);
                    self.approval_key(KeyEvent::from(KeyCode::Enter), out);
                }
            }
            return;
        }
        let g = geometry(area, self);
        let enter = KeyEvent::from(KeyCode::Enter);
        match self.overlay.clone() {
            Some(Overlay::ContextPlan { review, draft, .. }) => {
                match self.target_at(at) {
                    Some(Target::PlanCell(i, kv)) => {
                        let ctx = self.plan_rows().get(i).copied();
                        if let Some(ctx) = ctx {
                            self.context_pick(ctx, kv.and_then(|k| marlowe_proto::KvType::ALL.get(k).copied()));
                        }
                    }
                    Some(Target::PlanField(f)) => {
                        if let Some(Overlay::ContextPlan { field, .. }) = &mut self.overlay {
                            *field = f;
                        }
                    }
                    Some(Target::Button(0)) => self.context_key(KeyEvent::from(if review { KeyCode::Char('s') } else { KeyCode::Enter }), out),
                    Some(Target::Button(1)) if review => self.context_key(KeyEvent::from(KeyCode::Char('r')), out),
                    Some(Target::Button(_)) => self.context_key(KeyEvent::from(KeyCode::Esc), out),
                    Some(_) => {}
                    None if !review && draft.is_some() => {}
                    None => self.overlay = None,
                }
                return;
            }
            Some(Overlay::ModelPicker { .. }) => {
                if let Some(r) = g.picker.filter(|r| r.contains(at)) {
                    let row = at.y.saturating_sub(r.y + 1) as usize;
                    if at.y > r.y && row <= self.library.models.len() {
                        self.overlay = Some(Overlay::ModelPicker { sel: row });
                        self.overlay_key(enter, out);
                    }
                    return;
                }
                self.overlay = None;
                self.focus = Focus::Message;
                return;
            }
            Some(Overlay::WorkspacePicker { query, .. }) => {
                if let Some(r) = g.picker.filter(|r| r.contains(at)) {
                    // Rows start under the query line and a blank.
                    let row = at.y.saturating_sub(r.y + 3) as usize;
                    if at.y >= r.y + 3 && row < self.workspace_choices(&query).len() {
                        self.overlay = Some(Overlay::WorkspacePicker { query, sel: row });
                        self.overlay_key(enter, out);
                    }
                    return;
                }
                self.overlay = None;
                self.focus = Focus::Message;
                return;
            }
            Some(Overlay::Palette { query, .. }) => {
                match self.target_at(at) {
                    Some(Target::Row(i)) => {
                        self.overlay = Some(Overlay::Palette { query, sel: i });
                        self.overlay_key(enter, out);
                    }
                    Some(_) => {}
                    None => self.overlay = None,
                }
                return;
            }
            Some(Overlay::Sessions { .. }) => {
                match self.target_at(at) {
                    Some(Target::Row(i)) => {
                        self.overlay = Some(Overlay::Sessions { sel: i });
                        self.overlay_key(enter, out);
                    }
                    Some(_) => {}
                    None => self.overlay = None,
                }
                return;
            }
            Some(Overlay::ImportOffer { .. }) => {
                if let Some(Target::Button(i)) = self.target_at(at) {
                    self.overlay = Some(Overlay::ImportOffer { sel: i.min(1) });
                    self.overlay_key(enter, out);
                }
                return;
            }
            Some(Overlay::Choice { .. }) => {
                match (self.target_at(at), self.closing.dialog.clone()) {
                    (Some(Target::ChoiceButton(i)), Some(dialog)) => self.choose(&dialog, i.min(dialog.choices().len() - 1), out),
                    (Some(Target::Remember), Some(_)) => self.closing.remember = !self.closing.remember,
                    _ => {}
                }
                return;
            }
            Some(Overlay::Panel) => {
                match self.target_at(at) {
                    Some(Target::Row(i)) => self.panel_row_click(i, out),
                    Some(Target::Button(i)) => self.plan_button(i, out),
                    Some(Target::StopRun) => {
                        if let Some(crate::panel::Panel::Workflows(u)) = self.code.panel.as_mut() {
                            u.confirm_stop = u.open.is_some();
                        }
                    }
                    _ => {}
                }
                return;
            }
            Some(_) => return,
            None => {}
        }
        let Some(target) = self.pointer_target(area, at) else { return };
        match target {
            Target::Box(Focus::Model) => {
                self.focus = Focus::Model;
                self.open_model_picker();
            }
            Target::Box(Focus::Workspace) => self.open_workspace_picker(out),
            Target::Box(f) => self.focus = f,
            Target::Tab(t) => {
                // A stacked section is a box of its own: a click opens it. A tab is selected by the
                // first click and opened by a second.
                let open = g.board_mode == BoardMode::Stacked
                    || g.tab_at(at).is_none()
                    || (self.board_tab == t && self.focus == Focus::Board);
                self.select_tab(t);
                if open {
                    self.open_board_view(out);
                }
            }
            Target::Run(id) => {
                self.flash = None;
                self.open_run(id, out);
            }
            Target::Watched(path) => {
                self.open_watch(out);
                let p = crate::wake::norm(&path);
                let rows = self.wake.rows();
                if let Some(i) = rows.iter().position(|r| {
                    matches!(r, crate::wake::WatchRow::Root(root) if p.starts_with(&crate::wake::norm(root)))
                }) {
                    self.wake.sel = i;
                }
            }
            Target::Chord(cmd) => self.run_command(cmd, out),
            Target::Palette => self.overlay = Some(Overlay::Palette { query: String::new(), sel: 0 }),
            Target::Summary => {
                if self.view != View::Crew {
                    self.open_crew(out);
                }
            }
            Target::Slot(_) | Target::Model(_) | Target::AddModel => self.library_click(&target, out),
            Target::Row(i) => match self.view {
                View::Library => self.library_click(&target, out),
                View::Watch if matches!(self.wake.mode, crate::wake::WatchMode::Browse) => {
                    if self.wake.sel == i {
                        self.watch_key(enter, out);
                    } else {
                        self.wake.sel = i;
                    }
                }
                View::Memory if matches!(self.memory.mode, crate::memory::MemMode::Browse) => self.memory.sel = i,
                _ => {}
            },
            Target::Steer => {
                if let Some(v) = self.crew.open.as_mut() {
                    v.steering = true;
                }
            }
            Target::StopRun => self.ask_stop_run(),
            Target::ContextMeter => self.open_context_for_conductor(out),
            Target::ModelCtx(i) => {
                if let Some(m) = self.library.models.get(i).map(|m| m.model.clone()) {
                    self.open_context(m, out);
                }
            }
            // A fold marker opens or closes its block where it is; the keys stay where they were,
            // so Enter in the Message box still sends.
            Target::Thought(entry) => self.toggle_thought(entry),
            Target::Plan(run, action) => {
                self.flash = None;
                self.plan_action(run, action, out);
            }
            Target::PlanRow(i) => self.plan_row_click(i),
            Target::Button(_) | Target::ChoiceButton(_) | Target::PlanCell(..) | Target::PlanField(_) | Target::Remember => {}
        }
    }

    /// Opens or closes reasoning block `entry`. The next frame keeps its marker on the row it was
    /// drawn on, so only the lines under it move.
    pub(crate) fn toggle_thought(&mut self, entry: usize) {
        if !matches!(self.entries.get(entry), Some(Entry::Thinking { .. })) {
            return;
        }
        let open = self.thought_open(entry);
        self.folds.insert(entry, !open);
        self.thought = Some(entry);
        let v = self.viewport.get();
        let rows = v.size.1 as usize;
        let line = self.marks.borrow().iter().find(|m| m.entry == entry).map(|m| m.line);
        if let Some(line) = line.filter(|l| v.epoch == self.epoch && (v.top..v.top + rows).contains(l)) {
            self.anchor.set(Some(Anchor { entry, row: line - v.top, epoch: self.epoch }));
        }
    }

    /// Shift+Up/Down in the Conversation box: the cursor to the reasoning block above or below,
    /// with the pane brought to it when it is off screen. With no reasoning to step between, a line.
    fn step_thought(&mut self, up: bool) {
        let marks = self.marks.borrow().clone();
        let v = self.viewport.get();
        let rows = v.size.1 as usize;
        if marks.is_empty() || rows == 0 {
            self.scroll = if up { (self.scroll + 1).min(self.scroll_max.get()) } else { self.scroll.saturating_sub(1) };
            return;
        }
        let at = pick_thought(&marks, v.top, rows, self.thought).and_then(|e| marks.iter().position(|m| m.entry == e));
        let next = match (at, up) {
            (Some(i), true) => i.saturating_sub(1),
            (Some(i), false) => (i + 1).min(marks.len() - 1),
            (None, true) => marks.iter().rposition(|m| m.line < v.top).unwrap_or(0),
            (None, false) => marks.iter().position(|m| m.line >= v.top + rows).unwrap_or(marks.len() - 1),
        };
        let m = marks[next];
        self.thought = Some(m.entry);
        if !(v.top..v.top + rows).contains(&m.line) {
            // A third of the way down the pane, so what follows the marker shows too.
            let top = m.line.saturating_sub(rows / 3);
            self.scroll = self.scroll_max.get().saturating_sub(top);
        }
    }

    /// The Library model row under the cursor, on the overview.
    fn library_model_row(&self) -> Option<marlowe_model::ModelRef> {
        let lib = &self.library;
        if lib.step != LibStep::Overview || lib.focus != crate::library::LibFocus::Models {
            return None;
        }
        lib.models.get(lib.model_sel).map(|m| m.model.clone())
    }

    fn library_click(&mut self, t: &Target, out: &mut Vec<Request>) {
        let (stay, flash) = self.library.click(t, out);
        if let Some(f) = flash {
            self.flash(f, NoteTone::Dim);
        }
        if !stay {
            self.view = View::Conversation;
        }
    }

    pub(crate) fn open_model_picker(&mut self) {
        if !self.library.has_models() {
            self.focus = Focus::Message;
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

    /// Opens the workspace picker, asking for recent sessions so their directories are offered.
    fn open_workspace_picker(&mut self, out: &mut Vec<Request>) {
        self.view = View::Conversation;
        self.focus = Focus::Workspace;
        self.overlay = Some(Overlay::WorkspacePicker { query: String::new(), sel: 0 });
        if self.conn.is_online() {
            out.push(Request::ListSessions { limit: 30 });
        }
    }

    /// New sessions start in `path`. A session is tied to its workspace, so choosing another one
    /// while a session is open starts a new session (the old one stays under alt-s).
    fn choose_workspace(&mut self, path: String) {
        let path = path.trim().trim_matches('"').trim().to_string();
        let same = self.shown_workspace().map(crate::wake::norm) == Some(crate::wake::norm(&path));
        self.default_workspace = Some(path.clone());
        if same {
            return;
        }
        let shown = crate::text::short_path(&path);
        if self.session.is_some() {
            self.reset_conversation();
            self.flash(format!("new session in {shown} · the last one is under alt-s"), NoteTone::Dim);
        } else {
            self.workspace = None;
            self.flash(format!("new sessions start in {shown}"), NoteTone::Dim);
        }
    }

    /// Selects a board section and gives the board the keys.
    fn select_tab(&mut self, t: BoardTab) {
        self.board_tab = t;
        self.focus = Focus::Board;
    }

    /// Enter on the board: the section's full view.
    fn open_board_view(&mut self, out: &mut Vec<Request>) {
        match self.board_tab {
            BoardTab::Crew => self.open_crew(out),
            BoardTab::Watch => self.open_watch(out),
            BoardTab::Memory => self.open_memory(out),
            BoardTab::Threads => self.flash("no open threads yet — reflection opens them", NoteTone::Dim),
        }
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
                self.code.sessions_query.clear();
                self.overlay = Some(Overlay::Sessions { sel: 0 });
                out.push(Request::ListSessions { limit: 30 });
            }
            Command::Model => {
                self.view = View::Conversation;
                self.focus = Focus::Model;
                self.open_model_picker();
            }
            Command::Workspace => self.open_workspace_picker(out),
            Command::Context => self.open_context_for_conductor(out),
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
            Command::ToggleThinking => {
                self.show_thinking = !self.show_thinking;
                // Every block follows ^t again, whatever was opened or closed on its own.
                self.folds.clear();
                // Every reasoning block opens or closes: the pane starts again from the tail, or
                // from the same distance above it, rather than hold a line index from before.
                self.epoch = self.epoch.wrapping_add(1);
            }
            Command::Interrupt => {
                if let (Some(s), true) = (self.session.clone(), self.is_busy()) {
                    out.push(Request::Interrupt { session: s });
                    self.code.interrupting = true;
                }
            }
            Command::Quit => self.request_quit(),
            Command::Transcript => self.open_transcript(out),
            Command::HistorySearch => {
                self.view = View::Conversation;
                self.focus = Focus::Message;
                self.open_search();
            }
            Command::CycleMode => self.cycle_mode(out),
            Command::CycleEffort => self.slash_effort_cycle(out),
            Command::Background => self.background_running(out),
            Command::Config => self.open_config(out),
            Command::Permissions => self.open_permissions(out),
            Command::Hooks => self.open_hooks(out),
            Command::Agents => self.open_agents(out),
            Command::Mcp => self.open_mcp(out),
            Command::Instructions => self.open_instructions(out),
            Command::ContextUsage => self.open_context_view(out),
            Command::Cost => self.open_cost(out),
            Command::Diff => self.open_diff(None, out),
            Command::Rewind => self.open_rewind(out),
            Command::Jobs => self.open_jobs(out),
            Command::Todos => {
                self.code.todos_hidden = !self.code.todos_hidden;
            }
            Command::Help => self.open_help(out),
            Command::Tasks => self.open_tasks(out),
            Command::Workflows => self.open_workflows(out),
            Command::Findings => self.slash_findings(out),
            Command::Files => self.open_files(out),
            Command::Messages => self.open_threads(None, out),
            Command::Bench => self.open_bench(out),
            Command::Fast => {
                let _ = self.run_slash("/fast", out);
            }
        }
    }

    /// Up/Down/PgUp/PgDn/Home/End scroll the detail. Decision keys are ignored for a moment after
    /// the modal appears (type-ahead), and allow needs the whole detail to have been shown.
    fn approval_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(a) = self.approvals.first() else { return };
        let id = a.id;
        let max = a.detail_scroll_max();
        if self.approval_reason.is_some() {
            self.approval_reason_key(k, id, out);
            return;
        }
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
        // A server restart is allowed once or denied: never "for this session".
        let once_only = self.approvals.first().is_some_and(|a| a.tool == marlowe_proto::SERVER_RESTART_TOOL);
        let choices = self.approvals.first().map(|a| a.choices()).unwrap_or_else(|| APPROVAL_CHOICES.to_vec());
        let last = choices.len() - 1;
        // One past the buttons: "deny, and tell Marlowe why".
        let why = choices.len();
        let decision = match k.code {
            KeyCode::Left | KeyCode::BackTab if once_only => {
                self.approval_sel = 0;
                None
            }
            KeyCode::Right | KeyCode::Tab if once_only => {
                self.approval_sel = last;
                None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.approval_sel = self.approval_sel.saturating_sub(1);
                None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.approval_sel = (self.approval_sel + 1).min(why);
                None
            }
            KeyCode::Enter if self.approval_sel >= why => {
                self.approval_reason = Some(crate::composer::Composer::default());
                None
            }
            KeyCode::Char('w') => {
                self.approval_sel = why;
                self.approval_reason = Some(crate::composer::Composer::default());
                None
            }
            KeyCode::Enter => Some(choices[self.approval_sel.min(last)]),
            KeyCode::Char('y') => Some(choices[0]),
            KeyCode::Char('a') if once_only || choices.len() == 2 => None,
            KeyCode::Char('a') => Some(choices[1]),
            // "Always for this project": the rule goes to settings.local.toml.
            KeyCode::Char('p') if choices.len() == 4 => Some(choices[2]),
            KeyCode::Char('n') | KeyCode::Esc => Some(choices[last]),
            _ => None,
        };
        if let Some(decision) = decision {
            if decision.is_allowed() && !self.approval_read {
                return;
            }
            out.push(Request::Approve { approval: id, decision, reason: None });
            // Resolve locally; the daemon's ApprovalResolved is then a no-op.
            self.approvals.remove(0);
            self.front_approval_changed();
        }
    }

    /// Keys while the reason line is open: Enter denies with it (blank is a plain denial), Esc
    /// goes back to the buttons.
    fn approval_reason_key(&mut self, k: KeyEvent, id: marlowe_proto::ApprovalId, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(r) = self.approval_reason.as_mut() else { return };
        match k.code {
            KeyCode::Esc => {
                self.approval_reason = None;
                self.approval_sel = self.approvals.first().map_or(crate::state::APPROVAL_DENY, |a| a.why_index());
            }
            KeyCode::Enter => {
                let said = r.take().trim().to_string();
                let reason = (!said.is_empty()).then_some(said);
                if let Some(said) = &reason {
                    // Under the denied tool line: what the model is about to be told.
                    self.note(&format!("you told Marlowe why: {said}"), NoteTone::Dim);
                }
                out.push(Request::Approve { approval: id, decision: marlowe_proto::ApprovalDecision::Deny, reason });
                self.approvals.remove(0);
                self.front_approval_changed();
            }
            KeyCode::Backspace if ctrl => r.backspace_word(),
            KeyCode::Char('w') if ctrl => r.backspace_word(),
            KeyCode::Char('u') if ctrl => r.clear(),
            KeyCode::Backspace => r.backspace(),
            KeyCode::Delete => r.delete(),
            KeyCode::Left => r.left(),
            KeyCode::Right => r.right(),
            KeyCode::Home => r.home(),
            KeyCode::End => r.end(),
            KeyCode::Char(c) if !ctrl => r.insert(c),
            _ => {}
        }
    }

    fn overlay_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        if matches!(self.overlay, Some(Overlay::ContextPlan { .. })) {
            self.context_key(k, out);
            return;
        }
        if self.overlay == Some(Overlay::Panel) {
            self.panel_key(k, out);
            return;
        }
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
            if matches!(self.overlay, Some(Overlay::Choice { .. })) {
                self.close_choice();
                return;
            }
            if matches!(self.overlay, Some(Overlay::ModelPicker { .. } | Overlay::WorkspacePicker { .. })) {
                self.focus = Focus::Message;
            }
            self.overlay = None;
            return;
        }
        let Some(overlay) = self.overlay.clone() else { return };
        match overlay {
            Overlay::Palette { mut query, sel } => {
                let n = palette_matches(&query).len();
                match k.code {
                    KeyCode::Backspace if query.is_empty() => self.overlay = None,
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
            Overlay::WorkspacePicker { mut query, sel } => {
                let n = self.workspace_choices(&query).len();
                match k.code {
                    KeyCode::Backspace if query.is_empty() => {
                        self.overlay = None;
                        self.focus = Focus::Message;
                    }
                    KeyCode::Up => {
                        self.overlay = Some(Overlay::WorkspacePicker { query, sel: sel.saturating_sub(1) })
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        let sel = (sel + 1).min(n.saturating_sub(1));
                        self.overlay = Some(Overlay::WorkspacePicker { query, sel });
                    }
                    KeyCode::Enter => {
                        if let Some(p) = self.workspace_choices(&query).get(sel).cloned() {
                            self.overlay = None;
                            self.focus = Focus::Message;
                            self.choose_workspace(p);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.overlay = Some(Overlay::WorkspacePicker { query, sel: 0 });
                    }
                    KeyCode::Char(c) if !ctrl => {
                        query.push(c);
                        self.overlay = Some(Overlay::WorkspacePicker { query, sel: 0 });
                    }
                    _ => {}
                }
            }
            Overlay::ImportOffer { sel } => self.offer_key(k, sel, out),
            Overlay::Choice { sel } => self.choice_key(k, sel, out),
            Overlay::Onboard | Overlay::Dispatch | Overlay::Panel | Overlay::ContextPlan { .. } => {}
            Overlay::Sessions { sel } => {
                let n = self.session_rows().len();
                match k.code {
                    KeyCode::Backspace if self.code.sessions_query.is_empty() => self.overlay = None,
                    KeyCode::Backspace => {
                        self.code.sessions_query.pop();
                        self.overlay = Some(Overlay::Sessions { sel: 0 });
                    }
                    KeyCode::Up => self.overlay = Some(Overlay::Sessions { sel: sel.saturating_sub(1) }),
                    KeyCode::Down | KeyCode::Tab => {
                        self.overlay = Some(Overlay::Sessions { sel: (sel + 1).min(n.saturating_sub(1)) })
                    }
                    KeyCode::Enter => {
                        let pick = self.session_rows().get(sel).map(|s| s.id.clone());
                        if let Some(id) = pick {
                            self.overlay = None;
                            self.reset_conversation();
                            self.expect_open(id.clone());
                            self.view = View::Conversation;
                            out.push(Request::OpenSession { session: id });
                        }
                    }
                    // Typing narrows the list: the picker's search, as `/resume` has in Claude Code.
                    KeyCode::Char(c) if !ctrl => {
                        self.code.sessions_query.push(c);
                        self.overlay = Some(Overlay::Sessions { sel: 0 });
                    }
                    _ => {}
                }
            }
            Overlay::ModelPicker { sel } => {
                let n = self.library.models.len();
                match k.code {
                    KeyCode::Backspace => {
                        self.overlay = None;
                        self.focus = Focus::Message;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::ModelPicker { sel: sel.saturating_sub(1) })
                    }
                    KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                        // The last row opens the library.
                        self.overlay = Some(Overlay::ModelPicker { sel: (sel + 1).min(n) })
                    }
                    // `c`: that model's context and hardware plan.
                    KeyCode::Char('c') if sel < n => {
                        let m = self.library.models[sel].model.clone();
                        self.focus = Focus::Message;
                        self.open_context(m, out);
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.focus = Focus::Message;
                        if sel >= n {
                            self.run_command(Command::Library, out);
                            return;
                        }
                        // The slot, and in an open session the conversation's model too: the history
                        // is kept. The choice shows at once; the LibraryState reply confirms it.
                        let model = self.library.models[sel].model.clone();
                        self.switch_model(model, out);
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
        // Ctrl+R search owns every key until it ends.
        if self.code.search.is_some() {
            if let Some(p) = self.popup() {
                self.popup_key(&p, k, out);
            }
            return;
        }
        // Focus moves the same way from every box.
        match k.code {
            KeyCode::Tab if !ctrl && !alt && self.popup().is_none_or(|p| p.is_empty()) => {
                self.focus = self.focus.next();
                return;
            }
            // Shift+Tab cycles the permission mode from the Message box, as Claude Code does;
            // from the other boxes it still walks the focus back.
            KeyCode::BackTab => {
                if self.focus == Focus::Message {
                    self.cycle_mode(out);
                } else {
                    self.focus = self.focus.prev();
                }
                return;
            }
            // Alt-1..4 picks a board section without leaving the Message box.
            KeyCode::Char(c) if alt && !ctrl => {
                if let Some(t) = BoardTab::from_key(c) {
                    self.board_tab = t;
                    return;
                }
            }
            _ => {}
        }
        if self.focus != Focus::Message {
            self.region_key(k, out);
            return;
        }
        if let Some(p) = self.popup() {
            if self.popup_key(&p, k, out) {
                return;
            }
        }
        // History and the queue come first: they replace the buffer without counting as typing.
        match k.code {
            KeyCode::Up if !ctrl && !alt && !self.composer.up() => {
                if self.unqueue() {
                    return;
                }
                let cur = self.composer.text().to_string();
                match self.code.history.older(&cur).map(str::to_string) {
                    Some(t) => self.composer.set(&t),
                    None => self.scroll = (self.scroll + 1).min(self.scroll_max.get()),
                }
                return;
            }
            KeyCode::Down if !ctrl && !alt && !self.composer.down() => {
                match self.code.history.newer().map(str::to_string) {
                    Some(t) => self.composer.set(&t),
                    None => self.scroll = self.scroll.saturating_sub(1),
                }
                return;
            }
            KeyCode::Up | KeyCode::Down if !ctrl && !alt => return,
            _ => {}
        }
        let before = self.composer.text().to_string();
        match k.code {
            KeyCode::Enter if shift || alt => self.composer.insert('\n'),
            KeyCode::Char('j') if ctrl => self.composer.insert('\n'),
            KeyCode::Enter => self.send(out),
            // Esc interrupts a running turn; otherwise it hands the keys to the boxes.
            KeyCode::Esc => {
                if self.is_busy() {
                    self.run_command(Command::Interrupt, out);
                } else {
                    self.focus = Focus::Conversation;
                }
            }
            KeyCode::PageUp => {
                self.scroll = (self.scroll + self.page()).min(self.scroll_max.get());
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            KeyCode::Home if ctrl => self.scroll = self.scroll_max.get(),
            KeyCode::End if ctrl => self.follow_tail(),
            KeyCode::Backspace if ctrl || alt => self.composer.backspace_word(),
            KeyCode::Char('w') | KeyCode::Char('h') if ctrl => self.composer.backspace_word(),
            KeyCode::Char('u') if ctrl => self.composer.clear(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Char(c) if !ctrl => self.composer.insert(c),
            _ => {}
        }
        if self.composer.text() != before {
            self.composer_edited();
        }
    }

    /// Keys while a box other than Message has focus: the boxes' hotkeys, the focused box's own
    /// keys, and any other character goes back to the Message box and is typed there.
    fn region_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let focus = self.focus;
        match k.code {
            KeyCode::Esc => {
                self.focus = Focus::Message;
                self.follow_tail();
            }
            // Enter on the Conversation box folds the reasoning block its cursor is on, as Enter
            // on the old harness's conversation region did; with none on screen it goes to type.
            KeyCode::Enter if focus == Focus::Conversation => {
                let v = self.viewport.get();
                let marks = self.marks.borrow().clone();
                match pick_thought(&marks, v.top, v.size.1 as usize, self.thought) {
                    Some(entry) => self.toggle_thought(entry),
                    None => self.focus = Focus::Message,
                }
            }
            KeyCode::Enter => match focus {
                Focus::Model => self.open_model_picker(),
                Focus::Workspace => self.open_workspace_picker(out),
                Focus::Board => self.open_board_view(out),
                Focus::Conversation | Focus::Message => self.focus = Focus::Message,
            },
            KeyCode::Up | KeyCode::Down if shift && focus == Focus::Conversation => {
                self.step_thought(k.code == KeyCode::Up)
            }
            KeyCode::Char('i') if !ctrl => self.focus = Focus::Message,
            KeyCode::Char('c') if !ctrl => self.focus = Focus::Conversation,
            KeyCode::Char('m') if !ctrl => {
                self.focus = Focus::Model;
                self.open_model_picker();
            }
            KeyCode::Char('w') if !ctrl => self.open_workspace_picker(out),
            KeyCode::Char(c) if !ctrl && BoardTab::from_key(c).is_some() => {
                if let Some(t) = BoardTab::from_key(c) {
                    self.select_tab(t);
                }
            }
            KeyCode::Left if focus == Focus::Board => {
                let i = self.board_tab.index();
                self.board_tab = BoardTab::ALL[(i + BoardTab::ALL.len() - 1) % BoardTab::ALL.len()];
            }
            KeyCode::Right if focus == Focus::Board => {
                let i = self.board_tab.index();
                self.board_tab = BoardTab::ALL[(i + 1) % BoardTab::ALL.len()];
            }
            KeyCode::Up => self.scroll = (self.scroll + 1).min(self.scroll_max.get()),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll = (self.scroll + self.page()).min(self.scroll_max.get()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page()),
            KeyCode::Home => self.scroll = self.scroll_max.get(),
            KeyCode::End => self.follow_tail(),
            KeyCode::Char(c) if !ctrl => {
                self.focus = Focus::Message;
                self.composer.insert(c);
                self.composer_edited();
            }
            _ => {}
        }
    }

    fn page(&self) -> usize {
        10
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
