//! The crew's model slots, as a project's employees see them.
//!
//! A project's employees are not crew agents, but they share the crew's machine: one pool of
//! model slots (`ContextGate::crew_slots`, the planner's slots less the conductor's one) across
//! every run, the commit-memory check before a call to a local model, the "waiting for the GPU"
//! hold, and the run's budget. All of that already lives in [`StepHooks::before_call`] and
//! [`StepHooks::after_call`], so a project takes a slot by calling them and gives it back (booking
//! the spend) by calling `after_call`. The project engine's threads wait here; the conversation
//! never does.

use std::sync::Arc;

use marlowe_model::{CancelToken, ModelRef};
use marlowe_project::engine::{GateError, ModelGate, Permit};

use crate::playbook::{StepHooks, StepSpec};
use crate::CrewError;

/// How many calls of a model the crew can serve at once.
pub type Width = Arc<dyn Fn(&ModelRef) -> usize + Send + Sync>;

/// A [`ModelGate`] that takes its slots from a run's [`StepHooks`].
pub struct ProjectGate {
    hooks: Arc<dyn StepHooks>,
    models: Vec<ModelRef>,
    width: Width,
}

impl ProjectGate {
    /// A gate over `hooks` for the project's `models` (the employee and lead models, so the
    /// engine's native model ids can be mapped back to a provider); `width` says how many calls
    /// of a model the crew can serve at once.
    pub fn new(hooks: Arc<dyn StepHooks>, models: Vec<ModelRef>, width: Width) -> ProjectGate {
        ProjectGate { hooks, models, width }
    }

    /// The model the engine id (`qwen3:8b`) or a full reference (`ollama/qwen3:8b`) names.
    fn resolve(&self, model: &str) -> Option<&ModelRef> {
        self.models.iter().find(|m| m.model_id == model || m.to_string() == model)
    }

    fn spec(model: &str) -> StepSpec {
        StepSpec {
            key: format!("project:{model}"),
            role: "employee".into(),
            label: "project employee".into(),
            prompt: String::new(),
            tools: Some(Vec::new()),
            effort: None,
            max_output: None,
            sandbox: None,
        }
    }
}

impl ModelGate for ProjectGate {
    fn enter(&self, model: &str, cancel: &CancelToken) -> Result<Permit, GateError> {
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        let mref = self.resolve(model).cloned().ok_or_else(|| GateError::Refused(format!("`{model}` is not one of this project's models")))?;
        let local = mref.provider.is_local();
        let spec = ProjectGate::spec(model);
        match self.hooks.before_call(&spec, local.then_some(&mref)) {
            Ok(()) => {}
            Err(CrewError::Cancelled | CrewError::Stopped) => return Err(GateError::Cancelled),
            Err(e) => return Err(GateError::Refused(e.to_string())),
        }
        let hooks = self.hooks.clone();
        Ok(Permit::new(move |usage| hooks.after_call(&spec, local.then_some(&mref), usage)))
    }

    fn width(&self, model: &str) -> usize {
        self.resolve(model).map_or(1, |m| (self.width)(m)).max(1)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    use marlowe_model::{ProviderKind, Usage};

    use super::*;

    /// Hooks with a pool of `slots` for local models, like the run: `before_call` waits for a
    /// slot, `after_call` frees it and books the tokens.
    struct Pool {
        slots: u32,
        held: Mutex<u32>,
        freed: Condvar,
        booked: AtomicU32,
        refuse: bool,
    }

    impl StepHooks for Pool {
        fn before_call(&self, spec: &StepSpec, local: Option<&ModelRef>) -> Result<(), CrewError> {
            assert!(spec.tools.as_ref().is_some_and(Vec::is_empty), "a project employee has no tools");
            if self.refuse {
                return Err(CrewError::Step { step: spec.key.clone(), message: "Windows is out of memory".into() });
            }
            if local.is_some() {
                let mut held = self.held.lock().unwrap();
                while *held >= self.slots {
                    held = self.freed.wait_timeout(held, Duration::from_millis(20)).unwrap().0;
                }
                *held += 1;
            }
            Ok(())
        }

        fn after_call(&self, _spec: &StepSpec, local: Option<&ModelRef>, usage: Option<&Usage>) {
            if local.is_some() {
                *self.held.lock().unwrap() -= 1;
                self.freed.notify_all();
            }
            self.booked.fetch_add(usage.map_or(0, |u| u.total() as u32), Ordering::SeqCst);
        }
    }

    fn pool(slots: u32, refuse: bool) -> Arc<Pool> {
        Arc::new(Pool { slots, held: Mutex::new(0), freed: Condvar::new(), booked: AtomicU32::new(0), refuse })
    }

    fn gate(p: &Arc<Pool>) -> ProjectGate {
        let local = ModelRef { provider: ProviderKind::Ollama, model_id: "marlowe-mini:4b-super".into() };
        let cloud = ModelRef { provider: ProviderKind::OpenRouter, model_id: "x/y".into() };
        ProjectGate::new(p.clone(), vec![local, cloud], Arc::new(|m| if m.provider.is_local() { 2 } else { 5 }))
    }

    #[test]
    fn a_permit_takes_a_crew_slot_and_gives_it_back_booking_the_spend() {
        let p = pool(2, false);
        let g = gate(&p);
        let c = CancelToken::new();
        let a = g.enter("marlowe-mini:4b-super", &c).unwrap();
        let b = g.enter("ollama/marlowe-mini:4b-super", &c).unwrap();
        assert_eq!(*p.held.lock().unwrap(), 2);
        a.finish(Some(&Usage { input: 100, output: 20, ..Default::default() }));
        assert_eq!((*p.held.lock().unwrap(), p.booked.load(Ordering::SeqCst)), (1, 120));
        drop(b);
        assert_eq!(*p.held.lock().unwrap(), 0, "a dropped permit frees its slot too");
    }

    #[test]
    fn the_pool_is_the_crews_so_a_third_call_waits_for_a_slot() {
        let p = pool(2, false);
        let g = Arc::new(gate(&p));
        let c = CancelToken::new();
        let first = g.enter("marlowe-mini:4b-super", &c).unwrap();
        let _second = g.enter("marlowe-mini:4b-super", &c).unwrap();
        let (g2, c2) = (g.clone(), c.clone());
        let third = std::thread::spawn(move || g2.enter("marlowe-mini:4b-super", &c2).map(|p| p.finish(None)).is_ok());
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(*p.held.lock().unwrap(), 2, "the third caller is waiting on the crew pool");
        drop(first);
        assert!(third.join().unwrap());
    }

    #[test]
    fn width_follows_the_crews_slots_per_model_and_unknown_models_are_refused() {
        let p = pool(2, false);
        let g = gate(&p);
        assert_eq!((g.width("marlowe-mini:4b-super"), g.width("x/y"), g.width("nothing")), (2, 5, 1));
        assert_eq!(g.enter("nothing", &CancelToken::new()).err(), Some(GateError::Refused("`nothing` is not one of this project's models".into())));
    }

    #[test]
    fn a_cancelled_run_and_a_refusing_hook_are_told_apart() {
        let c = CancelToken::new();
        c.cancel();
        assert_eq!(gate(&pool(1, false)).enter("marlowe-mini:4b-super", &c).err(), Some(GateError::Cancelled));
        let refused = gate(&pool(1, true)).enter("marlowe-mini:4b-super", &CancelToken::new()).err();
        assert!(matches!(refused, Some(GateError::Refused(m)) if m.contains("out of memory")));
    }
}
