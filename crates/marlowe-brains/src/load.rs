//! Load admission: no model loads unless the planner says it fits beside what is resident.
//!
//! * A hosted model loads nothing here; a resident one is already paid for.
//! * A cold load is costed by the context planner at the window agents are allowed
//!   ([`ContextGate::allowed_ctx`](crate::ContextGate::allowed_ctx)) × the policy's agents,
//!   against the GPU memory free beside the runners already loaded.
//! * [`Priority::Foreground`] (the user's own turn) asks when it does not fit, with the cost;
//!   [`Priority::Crew`] is refused when it does not fit; [`Priority::Background`] never
//!   cold-loads at all: it runs on a resident (or hosted) model or not at all.
//! * Unknown is not "fits": a cost or free memory nobody measured asks the user's turn and
//!   refuses a crew step, and when the engine does not say what is loaded (a failed `/api/ps`)
//!   a crew step waits and background work does not run. Only a build with no hardware probe at
//!   all (tests) loads unmeasured.

use marlowe_hw::plan::{gib, HEADROOM_MIB};
use marlowe_model::{Effort, ModelRef};
use marlowe_proto::{ctx_label, KvType, Slot};

use crate::brains::Brains;
use crate::error::BrainsError;

/// Who wants the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// A turn the user is waiting on: may load, asks first when the load would not fit.
    Foreground,
    /// A crew run the user dispatched: may load what fits, never what does not.
    Crew,
    /// Housekeeping (briefs, anything idle-time): never triggers a load.
    Background,
}

/// What loading a model would cost on this machine now.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadCost {
    pub model: ModelRef,
    /// The window each agent gets, tokens.
    pub ctx: u32,
    pub agents: u16,
    pub kv: KvType,
    /// GPU memory the load needs (weights, compute and every agent's cache), MiB. `None`
    /// when the planner cannot see the model or the GPU.
    pub need_mib: Option<f64>,
    /// GPU memory free beside what is loaded, less the runner's headroom, MiB.
    pub free_mib: Option<f64>,
    /// Models the engine has loaded now.
    pub resident: Vec<String>,
    /// False when the engine did not say what is loaded: `resident` is then empty for want of an
    /// answer, and `free_mib` is unknown.
    pub resident_known: bool,
}

impl LoadCost {
    /// Whether the load fits beside what is resident. `None`: unknown.
    pub fn fits(&self) -> Option<bool> {
        Some(self.need_mib? <= self.free_mib?)
    }

    /// `marlowe-valiance:22b needs 15.2 GiB of GPU memory (32k × 1 agent, f16); 6.1 GiB is free
    /// beside marlowe-dawn:9b`.
    pub fn describe(&self) -> String {
        let shape = format!(
            "{} × {} agent{}, {}",
            ctx_label(self.ctx),
            self.agents,
            if self.agents == 1 { "" } else { "s" },
            self.kv
        );
        let need = match self.need_mib {
            Some(n) => format!("{} needs {} GiB of GPU memory ({shape})", self.model.model_id, gib(n)),
            None => format!("{} has no measured cost ({shape})", self.model.model_id),
        };
        let beside = if !self.resident_known {
            "(the engine did not say what is loaded)".to_string()
        } else if self.resident.is_empty() {
            "with nothing else loaded".to_string()
        } else {
            format!("beside {}", self.resident.join(", "))
        };
        match self.free_mib {
            Some(f) => format!("{need}; {} GiB is free {beside}", gib(f.max(0.0))),
            None => format!("{need}; free GPU memory is unknown {beside}"),
        }
    }
}

/// Whether a model may be used now.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// Hosted: nothing loads on this machine.
    Hosted,
    /// Already loaded by the engine.
    Resident,
    /// A cold load that fits (or that nothing measured says does not).
    Load(LoadCost),
    /// Foreground only: the load does not fit beside what is resident, or nobody can tell; ask
    /// with this cost.
    Ask(LoadCost),
    /// Crew only: the engine did not say what is loaded; try again shortly.
    Wait(String),
    /// Not allowed: the reason, with the cost when there is one.
    Refused(String),
}

impl Admission {
    /// The model may be called without asking anyone.
    pub fn may_run(&self) -> bool {
        matches!(self, Admission::Hosted | Admission::Resident | Admission::Load(_))
    }

    /// Whether calling the model now triggers a load.
    pub fn loads(&self) -> bool {
        matches!(self, Admission::Load(_) | Admission::Ask(_))
    }
}

/// `marlowe-dawn` and `marlowe-dawn:latest` are the same model to Ollama.
fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        if s.contains(':') {
            s
        } else {
            format!("{s}:latest")
        }
    };
    norm(a) == norm(b)
}

impl Brains {
    /// [`Brains::admit`], and when the engine did not say what is loaded (`Wait`) because it is
    /// not running, starts it and asks once more. For callers with no live line to show.
    pub fn admit_starting(&self, model: &ModelRef, who: Priority) -> Admission {
        let first = self.admit(model, who);
        if matches!(first, Admission::Wait(_)) && matches!(self.ensure_engine(model, &mut |_| {}), marlowe_hw::EngineStart::Started(_)) {
            return self.admit(model, who);
        }
        first
    }

    /// Whether `model` may be used now by `who`. See the module docs.
    pub fn admit(&self, model: &ModelRef, who: Priority) -> Admission {
        if !self.is_local_ollama(model) {
            return Admission::Hosted;
        }
        let Some(loaded) = self.resident_local_models() else {
            let why = format!("the engine did not say which models are loaded, so whether {} must load is unknown", model.model_id);
            return match who {
                Priority::Foreground => {
                    let mut cost = self.load_cost(model, Vec::new());
                    cost.resident_known = false;
                    cost.free_mib = None;
                    Admission::Ask(cost)
                }
                Priority::Crew => Admission::Wait(why),
                Priority::Background => Admission::Refused(why),
            };
        };
        if loaded.iter().any(|m| same_model(m, &model.model_id)) {
            return Admission::Resident;
        }
        if who == Priority::Background {
            return Admission::Refused(format!(
                "{} is not loaded, and background work never loads a model",
                model.model_id
            ));
        }
        let cost = self.load_cost(model, loaded);
        // Nothing to measure with (tests, a build without the probe): nothing is gated.
        if !self.has_hardware() {
            return Admission::Load(cost);
        }
        match (cost.fits(), who) {
            (Some(true), _) => Admission::Load(cost),
            // Does not fit, or its cost or the free memory is unknown.
            (_, Priority::Foreground) => Admission::Ask(cost),
            _ => Admission::Refused(cost.describe()),
        }
    }

    /// The planner's cost of loading `model` beside `resident`.
    fn load_cost(&self, model: &ModelRef, resident: Vec<String>) -> LoadCost {
        let gate = self.context();
        let policy = gate.policy(model).ok();
        let ctx = gate.allowed_ctx(model, None).ok().or(policy.as_ref().map(|p| p.max_ctx)).unwrap_or(crate::DEFAULT_WINDOW);
        // What the runner will serve, not only what is planned (a qwen35 model loads with one).
        let agents = gate.slots(model).map_or(1, |n| n.max(1));
        let kv = policy.as_ref().map_or(KvType::F16, |p| p.kv_type);
        let plan = self.context_plan(model).ok().and_then(|p| p.plan);
        let need_mib = plan.as_ref().map(|p| p.need_mib(ctx, kv, agents));
        // The engine's server gives the card back to the model it starts (it stops the one it
        // runs first), so it is measured as the planner measures it: free plus what Ollama's
        // runners hold, less the headroom. A model it would not fit never reaches the engine.
        let on_engine = self.engine_wanted(model);
        let free_mib = plan.as_ref().and_then(|p| p.gpu.as_ref()).map(|g| {
            if on_engine {
                g.free_for_marlowe_mib() - g.headroom_mib.max(HEADROOM_MIB)
            } else {
                g.free_mib - g.headroom_mib.max(HEADROOM_MIB)
            }
        });
        LoadCost { model: model.clone(), ctx, agents, kv, need_mib, free_mib, resident, resident_known: true }
    }

    /// The model background work on `slot` may use now: the slot's model, else the conductor's,
    /// whichever is hosted or already loaded. Never one that would have to load. The effort is
    /// the slot's.
    pub fn background_model(&self, slot: &Slot) -> Result<(ModelRef, Effort), BrainsError> {
        let (own, effort) = self.resolve(slot)?;
        let conductor = self.resolve(&Slot::Conductor).ok().map(|(m, _)| m);
        let mut why = Vec::new();
        for m in std::iter::once(own).chain(conductor) {
            match self.admit(&m, Priority::Background) {
                Admission::Hosted | Admission::Resident => return Ok((m, effort)),
                Admission::Refused(r) => why.push(r),
                other => why.push(format!("{}: {other:?}", m.model_id)),
            }
        }
        why.dedup();
        Err(BrainsError::Invalid(why.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_names_match_with_or_without_the_latest_tag() {
        assert!(same_model("marlowe-dawn", "marlowe-dawn:latest"));
        assert!(same_model("Marlowe-Dawn:9b", "marlowe-dawn:9b"));
        assert!(!same_model("marlowe-dawn:9b", "marlowe-valiance:22b"));
    }

    #[test]
    fn a_cost_says_what_it_needs_and_what_is_free_beside_what() {
        let c = LoadCost {
            model: "ollama/marlowe-valiance:22b".parse().unwrap(),
            ctx: 32_768,
            agents: 1,
            kv: KvType::F16,
            need_mib: Some(15.2 * 1024.0),
            free_mib: Some(6.1 * 1024.0),
            resident: vec!["marlowe-dawn:9b".into()],
            resident_known: true,
        };
        assert_eq!(c.fits(), Some(false));
        assert_eq!(
            c.describe(),
            "marlowe-valiance:22b needs 15.2 GiB of GPU memory (32k × 1 agent, f16); 6.1 GiB is free beside marlowe-dawn:9b"
        );
        assert_eq!(LoadCost { need_mib: None, ..c }.fits(), None);
    }
}
