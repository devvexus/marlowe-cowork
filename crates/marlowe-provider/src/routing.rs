//! ADR-008's tiered routing, as a table over **local** models.
//!
//! > Strong model for orchestration and synthesis; fast cheap models for subagent search,
//! > extraction, classification, consolidation. Routing is by task role, declared in
//! > `CapabilityProfile`, not by user preference.
//!
//! `ModelRoute` already names a **task role** — `Orchestrator | Worker | Summarizer` — and never
//! a model or a provider. That is what lets this table change when hosted models arrive without
//! anything upstream being reshaped: the table is the only place a model name appears.

use marlowe_loop::ModelRoute;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    #[error(
        "`{model}` is an Ollama CLOUD model. It proxies to a hosted service, needs an account, \
         and reaches the network — which would undo the one property ADR-028 exists to \
         establish: install, run, ask, answer, with no account and no key. Choose a local tag"
    )]
    CloudTag { model: String },

    #[error("no model is routed for {role:?}")]
    Unrouted { role: &'static str },
}

/// Whether a model tag is one of Ollama's hosted proxies.
///
/// Matched on the tag rather than probed at call time: a probe would need the network to tell
/// you that you are about to use the network.
pub fn is_cloud_tag(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.ends_with("-cloud") || m.ends_with(":cloud") || m.contains("-cloud:") || m.contains(":cloud-")
}

/// role → model. The only place a model name appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    orchestrator: String,
    worker: String,
    summarizer: String,
}

impl Routing {
    pub fn new(
        orchestrator: &str,
        worker: &str,
        summarizer: &str,
    ) -> Result<Self, RoutingError> {
        for m in [orchestrator, worker, summarizer] {
            if is_cloud_tag(m) {
                return Err(RoutingError::CloudTag { model: m.to_string() });
            }
        }
        Ok(Self {
            orchestrator: orchestrator.to_string(),
            worker: worker.to_string(),
            summarizer: summarizer.to_string(),
        })
    }

    /// One model for every role. What a machine with a single pulled model gets, and the shape
    /// the first run takes before anything has been measured.
    pub fn uniform(model: &str) -> Result<Self, RoutingError> {
        Self::new(model, model, model)
    }

    pub fn model_for(&self, route: ModelRoute) -> &str {
        match route {
            ModelRoute::Orchestrator => &self.orchestrator,
            ModelRoute::Worker => &self.worker,
            ModelRoute::Summarizer => &self.summarizer,
        }
    }

    /// Every distinct model this routing can reach. Used by the startup check, so a missing
    /// model is a named refusal at load rather than a failure on the turn that first needs it.
    pub fn models(&self) -> Vec<&str> {
        let mut v = vec![self.orchestrator.as_str(), self.worker.as_str(), self.summarizer.as_str()];
        v.sort_unstable();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_tags_are_refused_by_name() {
        // The failure this prevents is quiet: a cloud tag works, answers well, and turns the
        // local-first design into a hosted one that needs an account — with nothing observing
        // the change except a bill.
        for m in [
            "qwen3.5:397b-cloud",
            "deepseek-v3.1:671b-cloud",
            "glm-5:cloud",
            "kimi-k2.5:cloud",
        ] {
            assert!(is_cloud_tag(m), "`{m}` must be recognised as a cloud tag");
            assert_eq!(
                Routing::uniform(m),
                Err(RoutingError::CloudTag { model: m.to_string() })
            );
        }
    }

    #[test]
    fn ordinary_local_tags_are_accepted() {
        for m in ["qwen3.5:9b", "qwen2.5:14b", "gpt-oss:20b", "gemma3:12b"] {
            assert!(!is_cloud_tag(m), "`{m}` is local");
            assert!(Routing::uniform(m).is_ok());
        }
    }

    #[test]
    fn routing_is_by_role_and_the_table_is_the_only_place_a_model_is_named() {
        let r = Routing::new("qwen3.5:9b", "qwen3.5:4b", "qwen3.5:2b").unwrap();
        assert_eq!(r.model_for(ModelRoute::Orchestrator), "qwen3.5:9b");
        assert_eq!(r.model_for(ModelRoute::Worker), "qwen3.5:4b");
        assert_eq!(r.model_for(ModelRoute::Summarizer), "qwen3.5:2b");
        assert_eq!(r.models().len(), 3);

        // The shape hosted models slot into: same roles, different table.
        let uniform = Routing::uniform("qwen3.5:9b").unwrap();
        assert_eq!(uniform.models(), vec!["qwen3.5:9b"]);
    }
}
