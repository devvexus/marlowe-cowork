//! The model library view: state, onboarding steps, and its key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_model::{Effort, ModelInfo, ModelRef, ProviderKind};
use marlowe_proto::{LibraryModel, ProviderStatus, Request, Secret, Slot, SlotAssignment};

/// Where the library is in its flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibStep {
    /// Models and slots, or the "no models" invitation.
    Overview,
    /// Choosing a provider. `sel` indexes [`ProviderKind::ALL`].
    Providers { sel: usize },
    /// Entering an API key. Never rendered in clear.
    Key { provider: ProviderKind, input: KeyInput },
    /// Searching a provider's catalog. `sel` indexes the filtered list.
    Catalog { provider: ProviderKind, query: String, sel: usize },
}

/// A typed API key. `Debug` never prints it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KeyInput(String);

impl KeyInput {
    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for KeyInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyInput(***)")
    }
}

/// Which overview list has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibFocus {
    Slots,
    Models,
}

/// A provider's catalog as last received.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub provider: Option<ProviderKind>,
    pub models: Vec<ModelInfo>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    /// A LibraryState has arrived.
    pub known: bool,
    pub models: Vec<LibraryModel>,
    pub slots: Vec<SlotAssignment>,
    pub providers: Vec<ProviderStatus>,
    pub step: LibStep,
    pub focus: LibFocus,
    pub slot_sel: usize,
    pub model_sel: usize,
    pub catalog: Catalog,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            known: false,
            models: Vec::new(),
            slots: Vec::new(),
            providers: Vec::new(),
            step: LibStep::Overview,
            focus: LibFocus::Slots,
            slot_sel: 0,
            model_sel: 0,
            catalog: Catalog::default(),
        }
    }
}

/// The crew role shown when the daemon reports none.
pub const DEFAULT_CREW_ROLE: &str = "general";

impl Library {
    pub fn has_models(&self) -> bool {
        !self.models.is_empty()
    }

    pub(crate) fn set_state(
        &mut self,
        models: Vec<LibraryModel>,
        slots: Vec<SlotAssignment>,
        providers: Vec<ProviderStatus>,
    ) {
        self.known = true;
        self.models = models;
        self.slots = slots;
        self.providers = providers;
        self.model_sel = self.model_sel.min(self.models.len().saturating_sub(1));
        self.slot_sel = self.slot_sel.min(self.slot_rows().len().saturating_sub(1));
    }

    pub(crate) fn set_catalog(&mut self, provider: ProviderKind, models: Vec<ModelInfo>) {
        if self.catalog.provider == Some(provider) || self.catalog.provider.is_none() {
            self.catalog = Catalog { provider: Some(provider), models, loading: false, error: None };
        }
    }

    /// Routes an untargeted error to a loading catalog. True if consumed.
    pub(crate) fn catalog_error(&mut self, message: &str) -> bool {
        if self.catalog.loading && matches!(self.step, LibStep::Catalog { .. }) {
            self.catalog.loading = false;
            self.catalog.error = Some(message.to_string());
            return true;
        }
        false
    }

    pub fn provider_status(&self, kind: ProviderKind) -> Option<&ProviderStatus> {
        self.providers.iter().find(|p| p.kind == kind)
    }

    pub fn info(&self, r: &ModelRef) -> Option<&ModelInfo> {
        self.models.iter().find(|m| &m.model == r).map(|m| &m.info)
    }

    /// The first added model: every unset slot's model.
    pub fn default_model(&self) -> Option<&ModelRef> {
        self.models.first().map(|m| &m.model)
    }

    /// Slot rows in display order: Conductor, crew roles, Reflection, Quick. Missing rows are
    /// synthesised as inheriting the default.
    pub fn slot_rows(&self) -> Vec<SlotAssignment> {
        let find = |slot: &Slot| self.slots.iter().find(|s| &s.slot == slot).cloned();
        let inherit = |slot: Slot| SlotAssignment {
            slot,
            model: self.default_model().cloned(),
            effort: Effort::Medium,
            inherited: true,
        };
        let mut rows = vec![find(&Slot::Conductor).unwrap_or_else(|| inherit(Slot::Conductor))];
        let crew: Vec<_> =
            self.slots.iter().filter(|s| matches!(s.slot, Slot::Crew(_))).cloned().collect();
        if crew.is_empty() {
            rows.push(inherit(Slot::Crew(DEFAULT_CREW_ROLE.into())));
        } else {
            rows.extend(crew);
        }
        for s in [Slot::Reflection, Slot::Quick] {
            rows.push(find(&s).unwrap_or_else(|| inherit(s)));
        }
        rows
    }

    /// The conductor's model, for the title bar.
    pub fn conductor(&self) -> Option<ModelRef> {
        self.slot_rows().into_iter().next().and_then(|s| s.model)
    }

    /// Catalog rows matching the query: case-insensitive on id and display name, all terms.
    pub fn filtered_catalog(&self, query: &str) -> Vec<&ModelInfo> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.catalog
            .models
            .iter()
            .filter(|m| {
                let hay = format!("{} {}", m.id, m.display).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .collect()
    }

    pub fn in_library(&self, provider: ProviderKind, id: &str) -> bool {
        self.models.iter().any(|m| m.model.provider == provider && m.model.model_id == id)
    }

    fn provider_ready(&self, kind: ProviderKind) -> bool {
        match kind {
            ProviderKind::Ollama => true,
            _ => self.provider_status(kind).is_some_and(|p| p.configured || p.detected),
        }
    }

    /// Opens the provider list (the "add a model" flow).
    pub(crate) fn start_add(&mut self) {
        self.step = LibStep::Providers { sel: 0 };
    }

    fn open_catalog(&mut self, provider: ProviderKind, out: &mut Vec<Request>) {
        self.catalog = Catalog { provider: Some(provider), models: Vec::new(), loading: true, error: None };
        self.step = LibStep::Catalog { provider, query: String::new(), sel: 0 };
        out.push(Request::ListCatalog { provider });
    }

    /// Handles a key in the library view. Returns `false` when the library should close.
    pub(crate) fn on_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) -> (bool, Option<String>) {
        let mut flash = None;
        let step = self.step.clone();
        match step {
            LibStep::Overview => return (self.overview_key(k, out, &mut flash), flash),
            LibStep::Providers { sel } => match k.code {
                KeyCode::Esc => self.step = LibStep::Overview,
                KeyCode::Up => self.step = LibStep::Providers { sel: step_provider(sel, -1) },
                KeyCode::Down | KeyCode::Tab => {
                    self.step = LibStep::Providers { sel: step_provider(sel, 1) }
                }
                KeyCode::Enter => {
                    let kind = ProviderKind::ALL[sel];
                    if !kind.is_available_now() {
                        flash = Some(format!("{} is coming soon", kind.display_name()));
                    } else if self.provider_ready(kind) {
                        self.open_catalog(kind, out);
                    } else {
                        self.step = LibStep::Key { provider: kind, input: KeyInput::default() };
                    }
                }
                _ => {}
            },
            LibStep::Key { provider, mut input } => match k.code {
                KeyCode::Esc => self.step = LibStep::Providers { sel: provider_index(provider) },
                KeyCode::Enter => {
                    let key = input.0.trim().to_string();
                    if !key.is_empty() {
                        out.push(Request::SetCredential { provider, secret: Secret(key) });
                        out.push(Request::LibraryState);
                        self.open_catalog(provider, out);
                    }
                }
                KeyCode::Backspace => {
                    input.0.pop();
                    self.step = LibStep::Key { provider, input };
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.step = LibStep::Key { provider, input: KeyInput::default() };
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.0.push(c);
                    self.step = LibStep::Key { provider, input };
                }
                _ => {}
            },
            LibStep::Catalog { provider, mut query, sel } => {
                let n = self.filtered_catalog(&query).len();
                match k.code {
                    KeyCode::Esc => {
                        self.step = LibStep::Providers { sel: provider_index(provider) };
                    }
                    KeyCode::Up => {
                        self.step = LibStep::Catalog { provider, query, sel: sel.saturating_sub(1) }
                    }
                    KeyCode::Down => {
                        let sel = (sel + 1).min(n.saturating_sub(1));
                        self.step = LibStep::Catalog { provider, query, sel };
                    }
                    KeyCode::PageUp => {
                        self.step = LibStep::Catalog { provider, query, sel: sel.saturating_sub(10) }
                    }
                    KeyCode::PageDown => {
                        let sel = (sel + 10).min(n.saturating_sub(1));
                        self.step = LibStep::Catalog { provider, query, sel };
                    }
                    KeyCode::Enter => {
                        let picked = self.filtered_catalog(&query).get(sel).map(|m| m.id.clone());
                        if let Some(id) = picked {
                            if self.in_library(provider, &id) {
                                flash = Some(format!("{id} is already in the library"));
                            } else {
                                out.push(Request::AddModel { provider, model_id: id.clone() });
                                out.push(Request::LibraryState);
                                flash = Some(format!("added {id}"));
                                self.step = LibStep::Overview;
                                self.focus = LibFocus::Models;
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.step = LibStep::Catalog { provider, query, sel: 0 };
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        self.step = LibStep::Catalog { provider, query, sel: 0 };
                    }
                    _ => {}
                }
            }
        }
        (true, flash)
    }

    fn overview_key(
        &mut self,
        k: KeyEvent,
        out: &mut Vec<Request>,
        flash: &mut Option<String>,
    ) -> bool {
        if !self.has_models() {
            match k.code {
                KeyCode::Esc => return false,
                KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('y') => self.start_add(),
                _ => {}
            }
            return true;
        }
        let rows = self.slot_rows();
        match k.code {
            KeyCode::Esc => return false,
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    LibFocus::Slots => LibFocus::Models,
                    LibFocus::Models => LibFocus::Slots,
                }
            }
            KeyCode::Up => match self.focus {
                LibFocus::Slots => self.slot_sel = self.slot_sel.saturating_sub(1),
                LibFocus::Models => self.model_sel = self.model_sel.saturating_sub(1),
            },
            KeyCode::Down => match self.focus {
                LibFocus::Slots => self.slot_sel = (self.slot_sel + 1).min(rows.len() - 1),
                LibFocus::Models => {
                    self.model_sel = (self.model_sel + 1).min(self.models.len() - 1)
                }
            },
            KeyCode::Left | KeyCode::Right if self.focus == LibFocus::Slots => {
                let Some(row) = rows.get(self.slot_sel) else { return true };
                // Options: inherit the default, then each library model.
                let mut options: Vec<Option<ModelRef>> = vec![None];
                options.extend(self.models.iter().map(|m| Some(m.model.clone())));
                let current = if row.inherited { None } else { row.model.clone() };
                let i = options.iter().position(|o| *o == current).unwrap_or(0);
                let next = if k.code == KeyCode::Right {
                    (i + 1) % options.len()
                } else {
                    (i + options.len() - 1) % options.len()
                };
                let model = options[next].clone();
                self.apply_slot(&row.slot, model.clone(), row.effort);
                out.push(Request::SetSlot { slot: row.slot.clone(), model, effort: row.effort });
            }
            KeyCode::Char('e') | KeyCode::Enter if self.focus == LibFocus::Slots => {
                let Some(row) = rows.get(self.slot_sel) else { return true };
                let levels = row
                    .model
                    .as_ref()
                    .and_then(|m| self.info(m))
                    .map(|i| i.effort_levels.clone())
                    .unwrap_or_default();
                if levels.is_empty() {
                    *flash = Some("this model has no effort levels".into());
                    return true;
                }
                let effort = row.effort.cycle(&levels);
                let model = if row.inherited { None } else { row.model.clone() };
                self.apply_slot(&row.slot, model.clone(), effort);
                out.push(Request::SetSlot { slot: row.slot.clone(), model, effort });
            }
            KeyCode::Char('d') | KeyCode::Delete if self.focus == LibFocus::Models => {
                if let Some(m) = self.models.get(self.model_sel) {
                    out.push(Request::RemoveModel { model: m.model.clone() });
                    out.push(Request::LibraryState);
                    *flash = Some(format!("removed {}", m.model));
                }
            }
            _ => {}
        }
        true
    }

    /// Updates a slot locally so the table reacts before the daemon confirms.
    fn apply_slot(&mut self, slot: &Slot, model: Option<ModelRef>, effort: Effort) {
        let inherited = model.is_none();
        let resolved = model.or_else(|| self.default_model().cloned());
        let row = SlotAssignment { slot: slot.clone(), model: resolved, effort, inherited };
        match self.slots.iter_mut().find(|s| &s.slot == slot) {
            Some(s) => *s = row,
            None => self.slots.push(row),
        }
    }
}

fn provider_index(kind: ProviderKind) -> usize {
    ProviderKind::ALL.iter().position(|k| *k == kind).unwrap_or(0)
}

/// Moves the provider cursor, skipping providers that are not built yet.
fn step_provider(sel: usize, dir: isize) -> usize {
    let n = ProviderKind::ALL.len() as isize;
    let mut i = sel as isize;
    for _ in 0..n {
        i = (i + dir).rem_euclid(n);
        if ProviderKind::ALL[i as usize].is_available_now() {
            return i as usize;
        }
    }
    sel
}
