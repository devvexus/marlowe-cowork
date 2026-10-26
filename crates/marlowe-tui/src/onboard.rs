//! First-run onboarding: what to call the user, how to greet them, the offer to install the
//! memory models when they are missing, then a checklist of only the next steps that apply
//! (import old memories, watch suggested folders, add a model).
//!
//! It appears once the boot is over, only on an empty composer in the conversation view, and
//! never takes keys from someone who is already typing: a key during the boot defers it, and a
//! printable key in the first [`ONBOARD_GUARD_MS`] closes it and goes to the composer. Esc skips
//! the rest. Deferred or skipped, it stays away for the session; the palette can reopen it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{AddressForm, ModelsState, Request, WatchCandidate};

use crate::composer::Composer;
use crate::state::{NoteTone, Overlay, State, View};
use crate::wake::{norm, Presence, NO_SENTINEL};

/// A printable key this soon after the overlay appears was meant for the composer.
pub const ONBOARD_GUARD_MS: i64 = 600;
/// What the memory models are for, under the question that asks for them.
pub const MODELS_PURPOSE: &str = "They let me recall our past conversations by meaning, not just matching words. Without them, memory is keyword-only.";
/// Folders offered in the checklist at most.
pub const MAX_FOLDERS: usize = 4;

/// The user as the daemon reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserProfile {
    pub name: Option<String>,
    pub address: Option<String>,
    /// False until something has named the user.
    pub set: bool,
}

/// Where onboarding is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Onboard {
    /// Not needed, or the daemon has not said yet.
    #[default]
    Idle,
    /// The user is unset; waiting for the boot to end on an empty composer.
    Pending,
    /// On screen. `guard_until_ms`: printable keys before this are type-ahead.
    Active { step: Step, guard_until_ms: i64 },
    /// Finished, skipped or deferred: not shown again this session unless asked for.
    Done,
}

impl Onboard {
    /// Pending or on screen: it owns the first-run offers (the import offer waits for it).
    pub fn in_progress(&self) -> bool {
        matches!(self, Onboard::Pending | Onboard::Active { .. })
    }
}

/// A screen of the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Name { input: Composer },
    Address { name: Option<String>, sel: usize },
    /// "Install memory LMs?" `sel`: 0 yes, 1 no. Only when the daemon reports them missing.
    Models { bytes: u64, cascade: bool, sel: usize },
    Checklist { items: Vec<Item>, sel: usize },
}

/// One checklist row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    pub state: ItemState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    /// Import the old harness memories.
    Import { approx_entries: u64 },
    /// Watch one discovered folder (consent per folder).
    Watch(WatchCandidate),
    /// Open the library to add a model. Always last: it leaves the overlay.
    AddModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    Open,
    Done,
    Skipped,
}

/// The address choices for a name: "your name" only when there is one.
pub fn address_choices(name: Option<&str>) -> Vec<AddressForm> {
    AddressForm::ALL.into_iter().filter(|f| *f != AddressForm::Name || name.is_some()).collect()
}

impl State {
    /// Called by the run loop every frame: shows a pending onboarding once the boot is over.
    pub fn on_tick(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        // A checklist left waiting for discovery that never came (no Sentinel) has nothing to show.
        if let Onboard::Active { step: Step::Checklist { items, .. }, .. } = &self.onboard {
            if items.is_empty() && (self.wake.candidates.is_some() || self.wake.sentinel == Presence::Absent) {
                self.close_onboarding();
            }
        }
        if self.onboard != Onboard::Pending || self.booting() || !self.conn.is_online() {
            return out;
        }
        if !self.composer.is_blank() {
            // They started typing first: leave them to it.
            self.close_onboarding();
            return out;
        }
        if self.overlay.is_none() && self.approvals.is_empty() && self.view == View::Conversation {
            self.open_onboarding(true, &mut out);
        }
        out
    }

    /// Shows the overlay at the name step. `guard`: the user did not ask for it.
    pub(crate) fn open_onboarding(&mut self, guard: bool, out: &mut Vec<Request>) {
        let mut input = Composer::default();
        if let Some(n) = self.user.as_ref().and_then(|u| u.name.as_deref()) {
            input.set(n);
        }
        let guard_until_ms = if guard { self.now_ms + ONBOARD_GUARD_MS } else { i64::MIN };
        self.overlay = Some(Overlay::Onboard);
        // Named already (another client, `marlowe me`) and not asked for: only what still applies.
        if guard && self.user.as_ref().is_some_and(|u| u.set) {
            self.after_user(guard_until_ms);
        } else {
            self.onboard = Onboard::Active { step: Step::Name { input }, guard_until_ms };
        }
        // Discovery for the checklist, answered while the user types a name.
        if self.wake.candidates.is_none() && self.wake.sentinel != Presence::Absent {
            self.expect_error(NO_SENTINEL);
            out.push(Request::WatchCandidates);
        }
    }

    /// A key that should skip the overlay and go where it would have gone: a printable key while
    /// the guard is up and nothing has been typed into the name.
    pub(crate) fn onboard_typeahead(&self, k: &KeyEvent) -> bool {
        let Onboard::Active { step: Step::Name { input }, guard_until_ms } = &self.onboard else { return false };
        let printable = matches!(k.code, KeyCode::Char(_)) && !k.modifiers.contains(KeyModifiers::CONTROL);
        printable && self.now_ms < *guard_until_ms && input.text().is_empty()
    }

    /// Ends onboarding for this session. It held the import offer, so that is not raised again
    /// on a reconnect either; the palette still has it.
    pub(crate) fn close_onboarding(&mut self) {
        self.onboard = Onboard::Done;
        if self.legacy.is_some() {
            self.legacy_dismissed = true;
        }
        if self.overlay == Some(Overlay::Onboard) {
            self.overlay = None;
        }
    }

    pub(crate) fn onboard_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Onboard::Active { step, guard_until_ms } = &mut self.onboard else {
            self.overlay = None;
            return;
        };
        let guard_until_ms = *guard_until_ms;
        if k.code == KeyCode::Esc {
            self.close_onboarding();
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match step {
            Step::Name { input } => match k.code {
                KeyCode::Enter => {
                    let name = Some(input.text().trim().to_string()).filter(|n| !n.is_empty());
                    *step = Step::Address { name, sel: 0 };
                }
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Left => input.left(),
                KeyCode::Right => input.right(),
                KeyCode::Home => input.home(),
                KeyCode::End => input.end(),
                KeyCode::Char('u') if ctrl => input.clear(),
                KeyCode::Char(c) if !ctrl => input.insert(c),
                _ => {}
            },
            Step::Address { name, sel } => {
                let n = address_choices(name.as_deref()).len();
                match k.code {
                    KeyCode::Left | KeyCode::Up | KeyCode::BackTab => *sel = sel.saturating_sub(1),
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => *sel = (*sel + 1).min(n - 1),
                    KeyCode::Char(c @ '1'..='3') if (c as usize - '1' as usize) < n => {
                        *sel = c as usize - '1' as usize;
                        let (name, sel) = (name.clone(), *sel);
                        self.choose_address(name, sel, guard_until_ms, out);
                    }
                    KeyCode::Enter => {
                        let (name, sel) = (name.clone(), *sel);
                        self.choose_address(name, sel, guard_until_ms, out);
                    }
                    KeyCode::Backspace => {
                        let mut input = Composer::default();
                        input.set(name.as_deref().unwrap_or(""));
                        *step = Step::Name { input };
                    }
                    _ => {}
                }
            }
            Step::Models { bytes, sel, .. } => {
                let bytes = *bytes;
                match k.code {
                    KeyCode::Left | KeyCode::Up | KeyCode::BackTab => *sel = 0,
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => *sel = 1,
                    KeyCode::Char('y' | '1') => self.answer_models(true, bytes, guard_until_ms, out),
                    KeyCode::Char('n' | '2') => self.answer_models(false, bytes, guard_until_ms, out),
                    KeyCode::Enter => {
                        let yes = *sel == 0;
                        self.answer_models(yes, bytes, guard_until_ms, out);
                    }
                    _ => {}
                }
            }
            Step::Checklist { items, sel } => match k.code {
                KeyCode::Up | KeyCode::BackTab => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => *sel = (*sel + 1).min(items.len().saturating_sub(1)),
                KeyCode::Enter | KeyCode::Char('y') => {
                    let sel = *sel;
                    self.act(sel, out);
                }
                KeyCode::Char('s') | KeyCode::Char('n') | KeyCode::Delete => {
                    if let Some(it) = items.get_mut(*sel).filter(|it| it.state == ItemState::Open) {
                        it.state = ItemState::Skipped;
                    }
                    self.advance();
                }
                _ => {}
            },
        }
    }

    /// Saves the name and address, then moves to the checklist (or finishes when nothing applies).
    fn choose_address(&mut self, name: Option<String>, sel: usize, guard_until_ms: i64, out: &mut Vec<Request>) {
        let form = address_choices(name.as_deref())[sel];
        let address = form.resolve(name.as_deref());
        out.push(Request::SetUser { name: name.clone(), address: address.clone() });
        // Shown at once; the daemon's broadcast confirms it.
        self.user = Some(crate::onboard::UserProfile { name: name.clone(), address: address.clone(), set: true });
        let said = match (&name, form) {
            (Some(n), AddressForm::Sir) => format!("saved · {n} · \u{201c}sir\u{201d} in greetings"),
            (Some(n), AddressForm::Name) => format!("saved · {n} · your name in greetings"),
            (Some(n), AddressForm::Nothing) => format!("saved · {n} · no address in greetings"),
            (None, AddressForm::Sir) => "saved · \u{201c}sir\u{201d} in greetings".into(),
            (None, _) => "saved · no address in greetings".into(),
        };
        self.flash(said, NoteTone::Dim);
        self.after_user(guard_until_ms);
    }

    /// The memory models still to be offered: their size, and whether the cascade is in it.
    pub(crate) fn models_offer(&self) -> Option<(u64, bool)> {
        match self.memory.stats.as_ref()?.models {
            ModelsState::Missing { bytes, cascade } => Some((bytes, cascade)),
            _ => None,
        }
    }

    /// Raises onboarding for a user who is already named when the models are missing, so the
    /// offer is not only for a first launch with no name. Never over a finished or running one.
    pub(crate) fn offer_models_to_known_user(&mut self) {
        if self.onboard == Onboard::Idle && self.user.as_ref().is_some_and(|u| u.set) && self.models_offer().is_some() {
            self.onboard = Onboard::Pending;
        }
    }

    /// After the name and address: the memory-model offer if it applies, else the checklist.
    fn after_user(&mut self, guard_until_ms: i64) {
        match self.models_offer() {
            Some((bytes, cascade)) => {
                self.onboard = Onboard::Active { step: Step::Models { bytes, cascade, sel: 0 }, guard_until_ms };
            }
            None => self.enter_checklist(guard_until_ms),
        }
    }

    /// Yes asks the daemon to download on its own thread; no is remembered by it. Either way the
    /// memory line carries the rest, and the daemon's next stats confirm it.
    fn answer_models(&mut self, yes: bool, bytes: u64, guard_until_ms: i64, out: &mut Vec<Request>) {
        let (request, state, said) = if yes {
            (
                Request::ModelsInstall,
                ModelsState::Installing { done: 0, total: bytes },
                "installing memory LMs in the background \u{b7} progress is on the memory line",
            )
        } else {
            (
                Request::ModelsDecline,
                ModelsState::Declined,
                "memory stays keyword-only \u{b7} `marlowe models fetch` installs the LMs later",
            )
        };
        out.push(request);
        if let Some(s) = self.memory.stats.as_mut() {
            s.models = state;
        }
        self.flash(said, NoteTone::Dim);
        self.enter_checklist(guard_until_ms);
    }

    /// The checklist of what applies now, or the end when nothing does.
    fn enter_checklist(&mut self, guard_until_ms: i64) {
        let items = self.checklist_items();
        let waiting = self.wake.candidates.is_none() && self.wake.sentinel != Presence::Absent;
        if items.is_empty() && !waiting {
            self.close_onboarding();
            return;
        }
        self.onboard = Onboard::Active { step: Step::Checklist { items, sel: 0 }, guard_until_ms };
    }

    /// The next steps that apply now: import, folders, then a model.
    fn checklist_items(&self) -> Vec<Item> {
        let open = |kind| Item { kind, state: ItemState::Open };
        let mut v = Vec::new();
        if let (Some(l), None) = (&self.legacy, &self.import) {
            v.push(open(ItemKind::Import { approx_entries: l.approx_entries }));
        }
        for c in self.wake.open_offers().into_iter().take(MAX_FOLDERS) {
            v.push(open(ItemKind::Watch(c)));
        }
        if self.library.known && !self.library.has_models() {
            v.push(open(ItemKind::AddModel));
        }
        v
    }

    /// Discovery answered while the checklist is up: new folders join it (before Add a model).
    pub(crate) fn onboard_candidates(&mut self) {
        let fresh = self.wake.open_offers();
        let Onboard::Active { step: Step::Checklist { items, .. }, .. } = &mut self.onboard else { return };
        let mut folders = items.iter().filter(|i| matches!(i.kind, ItemKind::Watch(_))).count();
        for c in fresh {
            if folders >= MAX_FOLDERS {
                break;
            }
            let known = items.iter().any(|i| matches!(&i.kind, ItemKind::Watch(w) if norm(&w.path) == norm(&c.path)));
            if !known {
                let at = items.iter().position(|i| i.kind == ItemKind::AddModel).unwrap_or(items.len());
                items.insert(at, Item { kind: ItemKind::Watch(c), state: ItemState::Open });
                folders += 1;
            }
        }
        if items.is_empty() {
            self.close_onboarding();
        }
    }

    /// Does checklist item `sel`.
    fn act(&mut self, sel: usize, out: &mut Vec<Request>) {
        let Onboard::Active { step: Step::Checklist { items, .. }, .. } = &mut self.onboard else { return };
        let Some(item) = items.get_mut(sel).filter(|i| i.state == ItemState::Open) else { return };
        item.state = ItemState::Done;
        match item.kind.clone() {
            ItemKind::Import { .. } => {
                if self.import.is_none() {
                    self.start_import(out);
                }
            }
            ItemKind::Watch(c) => out.push(Request::WatchAdd { path: c.path }),
            ItemKind::AddModel => {
                self.close_onboarding();
                self.view = View::Library;
                self.library.start_add();
                return;
            }
        }
        self.advance();
    }

    /// Moves to the next open item, or finishes when none is left.
    fn advance(&mut self) {
        let Onboard::Active { step: Step::Checklist { items, sel }, .. } = &mut self.onboard else { return };
        let next = (*sel..items.len()).chain(0..*sel).find(|&i| items[i].state == ItemState::Open);
        match next {
            Some(i) => *sel = i,
            None => self.close_onboarding(),
        }
    }

    pub(crate) fn on_user(&mut self, u: UserProfile) {
        match (u.set, &self.onboard) {
            (false, Onboard::Idle) => self.onboard = Onboard::Pending,
            // Named elsewhere (another client, `marlowe me`) before it showed. The memory-model
            // offer, if there is one, still shows.
            (true, Onboard::Pending) if self.models_offer().is_none() => self.onboard = Onboard::Done,
            _ => {}
        }
        self.user = Some(u);
        self.offer_models_to_known_user();
    }

    /// A paste into the name field.
    pub(crate) fn onboard_paste(&mut self, text: &str) {
        if let Onboard::Active { step: Step::Name { input }, .. } = &mut self.onboard {
            input.insert_str(text.lines().next().unwrap_or("").trim());
        }
    }
}
