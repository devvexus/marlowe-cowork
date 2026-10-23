//! The context meter and the "Context & hardware" planner: state and keys.
//!
//! The daemon sends a [`ContextPlan`] (the model's architecture, the GPU budget and a linear cost
//! model); the popup moves the sliders against that model locally, so every number on screen is
//! the same arithmetic the daemon will check on save. Nothing is applied until the review screen
//! says what changes and the user saves; a server restart goes through the approval modal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_model::ModelRef;
use marlowe_proto::{
    planned_changes, server_change, ContextPlan, ContextPolicy, Engine, KvType, ModelContext, PlannedChange, PolicySource,
    Request, SessionId, MAX_AGENTS, MIN_CTX,
};

use crate::state::{NoteTone, Overlay, State};

/// One step of ←/→ on the context field.
pub const CTX_STEP: u32 = 4096;

/// The planner field that ←/→ adjust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanField {
    #[default]
    Ctx,
    Kv,
    Parallel,
}

impl PlanField {
    pub const ALL: [PlanField; 3] = [PlanField::Ctx, PlanField::Kv, PlanField::Parallel];

    pub fn next(self) -> PlanField {
        match self {
            PlanField::Ctx => PlanField::Kv,
            PlanField::Kv => PlanField::Parallel,
            PlanField::Parallel => PlanField::Ctx,
        }
    }

    pub fn prev(self) -> PlanField {
        self.next().next()
    }
}

/// What the client knows about context: every model's window, the live usage of this session,
/// and the plan the popup shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextUi {
    pub models: Vec<ModelContext>,
    /// The open session's context now: (session, tokens, window).
    pub usage: Option<(SessionId, u64, u32)>,
    pub plan: Option<ContextPlan>,
    /// A plan was asked for and has not arrived.
    pub loading: Option<ModelRef>,
    pub error: Option<String>,
}

impl ContextUi {
    pub fn model(&self, m: &ModelRef) -> Option<&ModelContext> {
        self.models.iter().find(|c| &c.model == m)
    }

    /// The plan for `m`, when it has arrived.
    pub fn plan_for(&self, m: &ModelRef) -> Option<&ContextPlan> {
        self.plan.as_ref().filter(|p| &p.model == m)
    }
}

/// The meter's numbers for the conductor: used tokens, the window, and its notes.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub used: u64,
    pub window: u32,
    pub kv: Option<KvType>,
    pub slots: Option<u16>,
    /// Amber `57/65 on GPU`.
    pub partial: Option<(u32, u32)>,
    /// A saved KV type the server does not use yet.
    pub pending: Option<KvType>,
    pub loaded: bool,
    pub cloud: bool,
}

impl State {
    /// The meter for the conductor's model, once the library is known.
    pub fn meter(&self) -> Option<Meter> {
        let model = self.library.conductor()?;
        let mc = self.context.model(&model);
        let info_window = self.library.info(&model).and_then(|i| i.context_window);
        let window = mc.map(|c| c.max_ctx).or(info_window)?;
        let used = match (&self.context.usage, &self.session) {
            (Some((s, t, _)), Some(open)) if s == open => *t,
            _ => 0,
        };
        let cloud = model.provider != marlowe_model::ProviderKind::Ollama
            || mc.is_some_and(|c| c.in_effect.engine == Engine::Cloud)
            || self.library.info(&model).is_some_and(|i| !i.local);
        let e = mc.map(|c| &c.in_effect);
        // The llama.cpp engine runs the policy: its slots and cache type are the planner's, loaded or not.
        let llama = e.is_some_and(|e| e.engine == Engine::LlamaCpp);
        let policy = mc.and_then(|c| c.policy).filter(|_| llama);
        Some(Meter {
            used,
            window,
            kv: if cloud { None } else { policy.map(|p| p.kv_type).or_else(|| e.and_then(|e| e.kv_type)) },
            slots: if cloud { None } else { policy.map(|p| p.parallel).or_else(|| e.and_then(|e| e.parallel)) },
            partial: e.and_then(|e| e.gpu_layers).filter(|(n, m)| n < m),
            pending: mc.and_then(ModelContext::pending),
            loaded: e.is_some_and(|e| e.loaded),
            cloud,
        })
    }

    /// `llama.cpp` when the conductor's model runs on Marlowe's own engine: the Model box says so.
    pub fn engine_label(&self) -> Option<&'static str> {
        let model = self.library.conductor()?;
        let e = &self.context.model(&model)?.in_effect;
        (e.engine == Engine::LlamaCpp).then(|| e.engine.label())
    }

    /// Opens the planner for `model` and asks the daemon for its plan.
    pub(crate) fn open_context(&mut self, model: ModelRef, out: &mut Vec<Request>) {
        self.context.error = None;
        let draft = self.context.plan_for(&model).map(|p| p.policy);
        self.overlay = Some(Overlay::ContextPlan { model: model.clone(), field: PlanField::Ctx, draft, review: false });
        if self.conn.is_online() {
            self.context.loading = Some(model.clone());
            out.push(Request::ContextPlan { model });
        }
    }

    /// The planner for the conductor's model (the meter, the palette).
    pub(crate) fn open_context_for_conductor(&mut self, out: &mut Vec<Request>) {
        match self.library.conductor() {
            Some(m) => self.open_context(m, out),
            None => self.flash("add a model first — the planner plans a model", NoteTone::Amber),
        }
    }

    pub(crate) fn on_context_plan(&mut self, plan: &ContextPlan) {
        if self.context.loading.as_ref() == Some(&plan.model) {
            self.context.loading = None;
        }
        if let Some(Overlay::ContextPlan { model, draft, .. }) = &mut self.overlay {
            if *model == plan.model && draft.is_none() {
                *draft = Some(plan.policy);
            }
        }
        self.context.plan = Some(plan.clone());
    }

    /// The contexts the planner's table shows: the standard rows up to the model's maximum, plus
    /// the one being edited.
    pub fn plan_rows(&self) -> Vec<u32> {
        let Some(Overlay::ContextPlan { model, draft, .. }) = &self.overlay else { return Vec::new() };
        let Some(p) = self.context.plan_for(model) else { return Vec::new() };
        match &p.plan {
            Some(m) => m.row_contexts(draft.map(|d| d.max_ctx)),
            None => p.rows.iter().map(|r| r.ctx).collect(),
        }
    }

    /// The review lines for the open planner: exactly what saving changes.
    pub fn context_review(&self) -> Vec<PlannedChange> {
        let Some(Overlay::ContextPlan { model, draft: Some(d), .. }) = &self.overlay else { return Vec::new() };
        let Some(p) = self.context.plan_for(model) else { return Vec::new() };
        planned_changes(p.policy.max_ctx, &p.in_effect, d)
    }

    /// Keys while the planner is up.
    pub(crate) fn context_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(Overlay::ContextPlan { model, mut field, draft, review }) = self.overlay.clone() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if k.code == KeyCode::Esc || (ctrl && k.code == KeyCode::Char('k')) {
            self.overlay = if review { Some(Overlay::ContextPlan { model, field, draft, review: false }) } else { None };
            return;
        }
        let Some(mut d) = draft else { return };
        let Some(plan) = self.context.plan_for(&model).cloned() else { return };
        if review {
            match k.code {
                KeyCode::Char('s') | KeyCode::Enter => self.save_context(&model, d, false, out),
                KeyCode::Char('r') => self.save_context(&model, d, true, out),
                KeyCode::Backspace => self.overlay = Some(Overlay::ContextPlan { model, field, draft, review: false }),
                _ => {}
            }
            return;
        }
        let before = d;
        let local = plan.in_effect.engine.is_local() && plan.plan.is_some();
        let rows: Vec<u32> = self.plan_rows().into_iter().filter(|c| *c <= plan.ctx_limit).collect();
        match k.code {
            KeyCode::Tab | KeyCode::BackTab if !local => field = PlanField::Ctx,
            KeyCode::Tab => field = field.next(),
            KeyCode::BackTab => field = field.prev(),
            KeyCode::Left | KeyCode::Right => {
                let up = k.code == KeyCode::Right;
                match field {
                    PlanField::Ctx => {
                        let step = if shift { 1024 } else { CTX_STEP };
                        d.max_ctx = if up {
                            (d.max_ctx / step * step + step).min(plan.ctx_limit)
                        } else {
                            (d.max_ctx.div_ceil(step) * step).saturating_sub(step).max(MIN_CTX)
                        };
                    }
                    PlanField::Kv if local => {
                        let i = d.kv_type.index();
                        d.kv_type = KvType::ALL[if up { (i + 1).min(2) } else { i.saturating_sub(1) }];
                    }
                    PlanField::Parallel if local => {
                        d.parallel = if up { (d.parallel + 1).min(MAX_AGENTS) } else { d.parallel.saturating_sub(1).max(1) };
                    }
                    _ => {}
                }
            }
            // ↑/↓ walk the table's contexts.
            KeyCode::Up | KeyCode::Down if !rows.is_empty() => {
                let i = rows.iter().position(|c| *c >= d.max_ctx).unwrap_or(rows.len());
                let at = rows.get(i) == Some(&d.max_ctx);
                let j = match (k.code, at) {
                    (KeyCode::Up, _) => i.saturating_sub(1),
                    (_, true) => (i + 1).min(rows.len() - 1),
                    (_, false) => i.min(rows.len() - 1),
                };
                d.max_ctx = rows[j];
                field = PlanField::Ctx;
            }
            KeyCode::Char('a') => match plan.auto {
                Some(a) => d = ContextPolicy { source: PolicySource::Auto, ..a },
                None => self.flash("no automatic choice without a GPU measurement", NoteTone::Amber),
            },
            KeyCode::Char('f') if local => d.kv_type = KvType::F16,
            // The engine switch for this model: llama.cpp (Marlowe's server) or Ollama.
            KeyCode::Char('e') => {
                let e = &plan.in_effect;
                match (e.engine, e.engine_user_off) {
                    (Engine::LlamaCpp, _) => {
                        out.push(Request::SetModelEngine { model: model.clone(), llama: false });
                        self.flash("llama.cpp off for this model · Ollama serves it", NoteTone::Dim);
                    }
                    (_, true) => {
                        out.push(Request::SetModelEngine { model: model.clone(), llama: true });
                        self.flash("llama.cpp on for this model", NoteTone::Dim);
                    }
                    _ => self.flash(e.engine_note.as_deref().unwrap_or("Ollama serves this model"), NoteTone::Amber),
                }
            }
            KeyCode::Enter => {
                self.overlay = Some(Overlay::ContextPlan { model, field, draft: Some(d), review: true });
                return;
            }
            _ => {}
        }
        if d != before && k.code != KeyCode::Char('a') {
            d.source = PolicySource::User;
        }
        self.overlay = Some(Overlay::ContextPlan { model, field, draft: Some(d), review: false });
    }

    /// Selects a context from the table (a click on its row).
    pub(crate) fn context_pick(&mut self, ctx: u32, kv: Option<KvType>) {
        if let Some(Overlay::ContextPlan { draft: Some(d), field, review: false, .. }) = &mut self.overlay {
            d.max_ctx = ctx;
            if let Some(k) = kv {
                d.kv_type = k;
            }
            d.source = PolicySource::User;
            *field = PlanField::Ctx;
        }
    }

    fn save_context(&mut self, model: &ModelRef, d: ContextPolicy, restart: bool, out: &mut Vec<Request>) {
        let in_effect = self.context.plan_for(model).map(|p| p.in_effect.clone()).unwrap_or_default();
        out.push(Request::SetContextPolicy { model: model.clone(), policy: d });
        let (kv_type, parallel, flash_attn) = server_change(&in_effect, &d);
        if restart && (kv_type.is_some() || parallel.is_some() || flash_attn.is_some()) {
            out.push(Request::ApplyOllamaServer { kv_type, parallel, flash_attn });
            self.flash("saved · asking before restarting Ollama", NoteTone::Dim);
        }
        self.overlay = None;
    }
}
