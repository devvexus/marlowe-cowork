//! The Memory view's state and keys, and the legacy-import offer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{MemoryChange, MemoryHit, MemoryStatsReport, ModelsState, Request};

use crate::composer::Composer;
use crate::state::{NoteTone, Overlay, State, View};

/// Hits requested per search.
pub const SEARCH_LIMIT: u32 = 30;

/// What the memory models are doing, as one phrase for the memory line; `None` when they are
/// installed, or missing and still to be asked about.
pub fn models_phrase(m: &ModelsState) -> Option<String> {
    use crate::text::megabytes;
    match m {
        ModelsState::Installing { done, total } => {
            let pct = if *total == 0 { 0 } else { (done * 100 / total).min(100) };
            Some(format!("installing memory LMs {pct}% ({}/{} MB)", megabytes(*done), megabytes(*total)))
        }
        // The URL names the repo, which is what is missing; the file path after it only crowds the line.
        ModelsState::Failed { reason } => {
            let reason = reason.replace("https://", "");
            let reason = reason.split_once("/resolve/").map_or(reason.as_str(), |(repo, _)| repo);
            Some(format!("memory LMs {reason}: keyword only"))
        }
        ModelsState::Declined => Some("memory degraded · keyword only · marlowe models fetch installs the LMs".into()),
        ModelsState::Ready | ModelsState::Missing { .. } => None,
    }
}

/// What the Memory view is doing.
#[derive(Debug, Clone)]
pub enum MemMode {
    Browse,
    /// Waiting for y/n on forgetting this memory.
    ConfirmForget { id: String },
    /// Editing the replacement text of this memory.
    Correct { id: String, input: Composer },
}

/// Which list the Memory view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemFilter {
    /// Search results.
    #[default]
    Search,
    /// The profile: what is in the conductor's context every turn.
    Profile,
}

/// The profile as the daemon last reported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileView {
    pub hits: Vec<MemoryHit>,
    pub tokens: u32,
    pub omitted: u32,
}

/// The Memory view.
#[derive(Debug, Clone)]
pub struct MemoryState {
    pub query: String,
    pub hits: Vec<MemoryHit>,
    pub sel: usize,
    /// A search has come back for some query.
    pub searched: bool,
    pub stats: Option<MemoryStatsReport>,
    pub mode: MemMode,
    pub filter: MemFilter,
    /// `None` until the daemon answers a `MemoryProfile`.
    pub profile: Option<ProfileView>,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            query: String::new(),
            hits: Vec::new(),
            sel: 0,
            searched: false,
            stats: None,
            mode: MemMode::Browse,
            filter: MemFilter::Search,
            profile: None,
        }
    }
}

impl MemoryState {
    /// The list on screen: search results or the profile.
    pub fn shown(&self) -> &[MemoryHit] {
        match (self.filter, &self.profile) {
            (MemFilter::Profile, Some(p)) => &p.hits,
            (MemFilter::Profile, None) => &[],
            (MemFilter::Search, _) => &self.hits,
        }
    }

    pub fn selected(&self) -> Option<&MemoryHit> {
        self.shown().get(self.sel)
    }
}

/// The old harness profile the daemon found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyOffer {
    pub path: String,
    pub approx_entries: u64,
}

/// An import in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportStatus {
    pub read: u64,
    pub imported: u64,
}

impl State {
    pub(crate) fn open_memory(&mut self, out: &mut Vec<Request>) {
        self.view = View::Memory;
        self.memory.mode = MemMode::Browse;
        out.push(Request::MemoryStats);
    }

    pub(crate) fn start_import(&mut self, out: &mut Vec<Request>) {
        let path = self.legacy.as_ref().map(|l| l.path.clone());
        self.import = Some(ImportStatus { read: 0, imported: 0 });
        out.push(Request::ImportLegacy { path });
    }

    /// The offer panel's keys: Yes starts the import, Not now is remembered for this run.
    pub(crate) fn offer_key(&mut self, k: KeyEvent, sel: usize, out: &mut Vec<Request>) {
        match k.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.overlay = Some(Overlay::ImportOffer { sel: 1 - sel.min(1) });
            }
            KeyCode::Char('y') => self.accept_offer(out),
            KeyCode::Char('n') => self.decline_offer(),
            KeyCode::Enter if sel == 0 => self.accept_offer(out),
            KeyCode::Enter => self.decline_offer(),
            _ => {}
        }
    }

    fn accept_offer(&mut self, out: &mut Vec<Request>) {
        self.overlay = None;
        self.start_import(out);
    }

    pub(crate) fn decline_offer(&mut self) {
        self.overlay = None;
        self.legacy_dismissed = true;
    }

    pub(crate) fn memory_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.memory.mode {
            MemMode::ConfirmForget { id } => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        out.push(Request::MemoryForget { id: std::mem::take(id) });
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {}
                    _ => return,
                }
                self.memory.mode = MemMode::Browse;
            }
            MemMode::Correct { id, input } => match k.code {
                KeyCode::Esc => self.memory.mode = MemMode::Browse,
                KeyCode::Backspace if input.text().is_empty() => self.memory.mode = MemMode::Browse,
                KeyCode::Enter => {
                    if !input.is_blank() {
                        let text = input.take().trim().to_string();
                        out.push(Request::MemoryCorrect { id: std::mem::take(id), text });
                        self.memory.mode = MemMode::Browse;
                    }
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
            MemMode::Browse => {
                self.flash = None;
                let workspace = self.shown_workspace().map(str::to_string);
                let m = &mut self.memory;
                let last = m.shown().len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc => self.view = View::Conversation,
                    KeyCode::Enter => {
                        let q = m.query.trim().to_string();
                        if q.is_empty() {
                            self.flash("type something to search for", NoteTone::Dim);
                        } else {
                            if m.filter == MemFilter::Profile {
                                m.filter = MemFilter::Search;
                                m.sel = 0;
                            }
                            out.push(Request::MemorySearch { query: q, scope: None, limit: SEARCH_LIMIT });
                        }
                    }
                    // Toggles between search results and what is in context now.
                    KeyCode::Char('p') if ctrl => {
                        m.sel = 0;
                        if m.filter == MemFilter::Profile {
                            m.filter = MemFilter::Search;
                        } else {
                            m.filter = MemFilter::Profile;
                            out.push(Request::MemoryProfile { workspace });
                        }
                    }
                    KeyCode::Up => m.sel = m.sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Tab => m.sel = (m.sel + 1).min(last),
                    KeyCode::PageUp => m.sel = m.sel.saturating_sub(5),
                    KeyCode::PageDown => m.sel = (m.sel + 5).min(last),
                    KeyCode::Char('d') if ctrl => {
                        if let Some(h) = m.selected() {
                            m.mode = MemMode::ConfirmForget { id: h.id.clone() };
                        }
                    }
                    KeyCode::Char('e') if ctrl => {
                        if let Some(h) = m.selected() {
                            let mut input = Composer::default();
                            input.set(&h.text);
                            m.mode = MemMode::Correct { id: h.id.clone(), input };
                        }
                    }
                    KeyCode::Char('u') if ctrl => m.query.clear(),
                    // Typing searches; with nothing typed, Backspace goes back like Esc.
                    KeyCode::Backspace if m.query.is_empty() => self.view = View::Conversation,
                    KeyCode::Backspace => {
                        m.query.pop();
                    }
                    KeyCode::Char(c) if !ctrl => m.query.push(c),
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn memory_results(&mut self, hits: &[MemoryHit]) {
        self.memory.hits = hits.to_vec();
        self.memory.sel = self.memory.sel.min(hits.len().saturating_sub(1));
        self.memory.searched = true;
    }

    pub(crate) fn memory_profile(&mut self, hits: &[MemoryHit], tokens: u32, omitted: u32) {
        self.memory.profile = Some(ProfileView { hits: hits.to_vec(), tokens, omitted });
        if self.memory.filter == MemFilter::Profile {
            self.memory.sel = self.memory.sel.min(hits.len().saturating_sub(1));
        }
    }

    /// While the profile is on screen, any change re-reads it (a correction adds a line).
    pub(crate) fn memory_changed(&mut self, id: &str, change: &MemoryChange, out: &mut Vec<Request>) {
        match change {
            MemoryChange::Remembered => self.flash("remembered", NoteTone::Dim),
            MemoryChange::Corrected { .. } | MemoryChange::Forgotten => {
                self.memory.hits.retain(|h| h.id != id);
                if let Some(p) = &mut self.memory.profile {
                    p.hits.retain(|h| h.id != id);
                }
                self.memory.sel = self.memory.sel.min(self.memory.shown().len().saturating_sub(1));
                let word = if matches!(change, MemoryChange::Forgotten) { "forgotten" } else { "corrected" };
                self.flash(word, NoteTone::Dim);
            }
        }
        if self.view == View::Memory && self.memory.filter == MemFilter::Profile {
            out.push(Request::MemoryProfile { workspace: self.shown_workspace().map(str::to_string) });
        }
    }
}
