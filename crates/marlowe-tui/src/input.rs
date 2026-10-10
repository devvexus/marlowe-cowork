//! Keys, paste and mouse: the input half of the reducer.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{Request, Slot};
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
        // The footer's one Alt key. Ctrl+Alt is AltGr on many layouts and is a character.
        if alt && !ctrl && matches!(k.code, KeyCode::Char('v' | 'V')) {
            self.run_command(Command::ToggleVoice, &mut out);
            return out;
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
            return;
        }
        match (&mut self.overlay, self.view) {
            (Some(Overlay::Palette { query, sel }), _) | (Some(Overlay::WorkspacePicker { query, sel }), _) => {
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
            (None, View::Conversation) => {
                self.focus = Focus::Message;
                self.composer.insert_str(text);
            }
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
        if !self.approvals.is_empty() {
            if let Some(Target::Button(i)) = self.target_at(at) {
                if self.now_ms.saturating_sub(self.approval_since_ms) >= APPROVAL_GUARD_MS {
                    self.approval_sel = i.min(APPROVAL_CHOICES.len() - 1);
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
            Target::ContextMeter => self.open_context_for_conductor(out),
            Target::ModelCtx(i) => {
                if let Some(m) = self.library.models.get(i).map(|m| m.model.clone()) {
                    self.open_context(m, out);
                }
            }
            // A fold marker opens or closes its block where it is; the keys stay where they were,
            // so Enter in the Message box still sends.
            Target::Thought(entry) => self.toggle_thought(entry),
            Target::Button(_) | Target::PlanCell(..) | Target::PlanField(_) => {}
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

    fn open_model_picker(&mut self) {
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
    /// while a session is open starts a new session (the old one stays under ^o).
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
            self.flash(format!("new session in {shown} · the last one is under ^o"), NoteTone::Dim);
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
        // A server restart is allowed once or denied: never "for this session".
        let once_only = self.approvals.first().is_some_and(|a| a.tool == marlowe_proto::SERVER_RESTART_TOOL);
        let decision = match k.code {
            KeyCode::Left | KeyCode::BackTab if once_only => {
                self.approval_sel = 0;
                None
            }
            KeyCode::Right | KeyCode::Tab if once_only => {
                self.approval_sel = APPROVAL_CHOICES.len() - 1;
                None
            }
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
            KeyCode::Char('a') if once_only => None,
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
        if matches!(self.overlay, Some(Overlay::ContextPlan { .. })) {
            self.context_key(k, out);
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
            Overlay::Onboard | Overlay::Dispatch | Overlay::ContextPlan { .. } => {}
            Overlay::Sessions { sel } => {
                let n = self.sessions.as_ref().map_or(0, Vec::len);
                match k.code {
                    KeyCode::Backspace => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::Sessions { sel: sel.saturating_sub(1) })
                    }
                    KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
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
        // Focus moves the same way from every box.
        match k.code {
            KeyCode::Tab if !ctrl && !alt => {
                self.focus = self.focus.next();
                return;
            }
            KeyCode::BackTab => {
                self.focus = self.focus.prev();
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
            KeyCode::Char(c) if !ctrl => self.composer.insert(c),
            _ => {}
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
            }
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
        self.follow_tail();
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
