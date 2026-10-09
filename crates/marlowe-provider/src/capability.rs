//! ADR-028 requirement 2 — **record the capability difference, and keep recording it.**
//!
//! A local model is materially weaker than a frontier one, and §12 requires that be disclosed
//! honestly rather than papered over. It bites hardest on **tool-call reliability**, which is
//! exactly where a debugging session cannot tell a harness bug from a model that cannot follow a
//! schema — the symptom is identical: a tool that did not run.
//!
//! So the model in use travels with the run, and its tool-call success rate is a **measured
//! number with a denominator**, not a reputation. `tools/probe_tool_calls.rs` produces it.
//!
//! A rate quoted without the model and the trial count that produced it is the same defect as an
//! accuracy number quoted without its token cost (§5.7): a figure that cannot be checked.

use serde::{Deserialize, Serialize};

/// What one model was measured to do, on this machine, on a stated date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub model: String,
    /// Parameter count as the registry reports it, e.g. `9.0B`. Informational — the measured
    /// rate below is what a decision is made on.
    pub parameters: Option<String>,
    pub tool_calls: Option<ToolCallReport>,
}

impl ModelCapability {
    pub fn unmeasured(model: &str) -> Self {
        Self { model: model.to_string(), parameters: None, tool_calls: None }
    }

    /// The one line that goes in front of a user, and into `--dev`.
    ///
    /// **Never omits the denominator.** "92% reliable" invites a decision; "23/25" invites the
    /// question of whether 25 is enough, which is the question that should be asked.
    pub fn disclosure(&self) -> String {
        match &self.tool_calls {
            Some(r) => format!(
                "{} · tool calls {}/{} well-formed ({:.0}%) · measured {}",
                self.model, r.well_formed, r.trials, r.rate() * 100.0, r.measured_on
            ),
            None => format!(
                "{} · tool-call reliability NOT MEASURED on this machine — a failed tool call \
                 may be the model rather than the harness",
                self.model
            ),
        }
    }

    /// Whether this model is fit to be the default for a first run.
    ///
    /// The bar is deliberately about **tool calls**, not general quality: the first thing a user
    /// does is ask Marlowe to read a file, and a model that cannot emit a well-formed tool call
    /// looks exactly like a broken harness.
    pub fn fit_for_default(&self) -> bool {
        matches!(&self.tool_calls, Some(r) if r.trials >= MIN_TRIALS && r.rate() >= MIN_RATE)
    }
}

/// Below this many trials a rate is noise. Small, because each trial is a model call and the
/// probe has to stay runnable on a laptop — stated so the number is a choice, not a default.
pub const MIN_TRIALS: u32 = 12;

/// The bar a default model must clear. A model failing one call in five would make the product's
/// first impression indistinguishable from a harness bug.
pub const MIN_RATE: f32 = 0.80;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallReport {
    pub trials: u32,
    /// Emitted a tool call naming a registered tool, with arguments that parse.
    pub well_formed: u32,
    /// Emitted a tool call whose *target argument* was also correct. A model that calls `read`
    /// with the wrong path is well-formed and useless, and the two failures are worth telling
    /// apart when deciding whether the harness or the model is at fault.
    pub correct_target: u32,
    pub measured_on: String,
    pub median_ms: u64,
}

impl ToolCallReport {
    pub fn rate(&self) -> f32 {
        if self.trials == 0 {
            0.0
        } else {
            self.well_formed as f32 / self.trials as f32
        }
    }

    pub fn target_rate(&self) -> f32 {
        if self.trials == 0 {
            0.0
        } else {
            self.correct_target as f32 / self.trials as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(well_formed: u32, trials: u32) -> ToolCallReport {
        ToolCallReport {
            trials,
            well_formed,
            correct_target: well_formed,
            measured_on: "2026-08-08".into(),
            median_ms: 900,
        }
    }

    #[test]
    fn an_unmeasured_model_says_so_rather_than_implying_a_number() {
        let c = ModelCapability::unmeasured("qwen3.5:9b");
        assert!(c.disclosure().contains("NOT MEASURED"));
        assert!(
            c.disclosure().contains("may be the model rather than the harness"),
            "the disclosure has to name the confusion it exists to prevent"
        );
        assert!(!c.fit_for_default(), "an unmeasured model is never the default");
    }

    #[test]
    fn a_disclosure_always_carries_its_denominator() {
        let c = ModelCapability {
            model: "qwen3.5:9b".into(),
            parameters: Some("9.0B".into()),
            tool_calls: Some(report(23, 25)),
        };
        let d = c.disclosure();
        assert!(d.contains("23/25"), "{d}");
        assert!(d.contains("2026-08-08"), "a measurement without its date is not checkable: {d}");
    }

    #[test]
    fn the_default_bar_is_about_tool_calls_and_has_a_floor_on_trials() {
        // Too few trials: a rate of 1.0 over 3 attempts is noise.
        let noisy = ModelCapability {
            model: "m".into(),
            parameters: None,
            tool_calls: Some(report(3, 3)),
        };
        assert!(!noisy.fit_for_default());

        // Enough trials, under the bar.
        let weak = ModelCapability {
            model: "m".into(),
            parameters: None,
            tool_calls: Some(report(9, 12)),
        };
        assert!(weak.rate_below_bar());

        let good = ModelCapability {
            model: "m".into(),
            parameters: None,
            tool_calls: Some(report(11, 12)),
        };
        assert!(good.fit_for_default());
    }
}

#[cfg(test)]
impl ModelCapability {
    fn rate_below_bar(&self) -> bool {
        !self.fit_for_default()
    }
}
