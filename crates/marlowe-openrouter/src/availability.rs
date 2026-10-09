//! Degrading honestly. **Invariant 4: degrade with a named remedy, never silently.**
//!
//! `Availability::probe` on the Ollama path answers three distinguishable questions — is anything
//! listening, does it speak Ollama, is the routed model present — because the remedies differ and
//! a user cannot act on "unavailable". This is the same discipline over a different set of
//! failures, and the set is larger: a hosted endpoint can also refuse the credential, refuse the
//! account, and be out of money.
//!
//! **Nothing here reaches the network.** A startup probe would put a round trip in front of every
//! `--serve`, and the failures worth catching early — no key, no model named — are answerable
//! without one. What cannot be answered without one (is the key valid, does the account have
//! credit) is answered by the first call, which reports it through [`explain_status`].

use marlowe_loop::DegradedPath;

/// What is wrong, specifically enough to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready { model: String },
    /// No credential. The one this session's interim makes most likely.
    KeyMissing { message: String },
    /// The provider was selected with no model named.
    ModelUnnamed,
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready { .. })
    }

    /// The line the user sees. **Names the remedy**, because a degraded state a user cannot act
    /// on is a crash with better manners.
    pub fn remedy(&self) -> String {
        match self {
            Availability::Ready { model } => format!("ready · {model} via openrouter.ai"),
            Availability::KeyMissing { message } => {
                format!("no model available — {message}")
            }
            Availability::ModelUnnamed => format!(
                "no model available — `--provider openrouter` was given with no model. There is \
                 no default: nothing on OpenRouter has been measured by this project, and a \
                 built-in slug would read as a recommendation. Name one, e.g. \
                 `--openrouter-model anthropic/claude-sonnet-4.5`. Browse them at \
                 https://openrouter.ai/models"
            ),
        }
    }

    pub fn degraded_path(&self) -> Option<DegradedPath> {
        (!self.is_ready()).then_some(DegradedPath::ModelUnavailable)
    }

    /// Everything answerable **without touching the network**.
    ///
    /// A hosted provider tempts a startup probe. It would cost a round trip on every launch, it
    /// would make `--serve` fail when the network is briefly down even though the first turn
    /// might be minutes away, and it would answer a question the first call answers anyway.
    pub fn check(model: &str, key: &Result<crate::secret::ApiKey, crate::secret::KeyMissing>) -> Self {
        if model.trim().is_empty() {
            return Availability::ModelUnnamed;
        }
        match key {
            Err(e) => Availability::KeyMissing { message: e.to_string() },
            Ok(_) => Availability::Ready { model: model.to_string() },
        }
    }
}

/// Turn a non-retriable HTTP status into a sentence with a remedy in it.
///
/// **Each one names a different action**, because collapsing them into "the request failed" is a
/// message a user cannot act on — and because on this path the difference between *your key is
/// wrong* and *your account is empty* is the difference between a two-second fix and a purchase.
pub fn explain_status(status: u16, model: &str, body: &str) -> String {
    let tail = first_line(body);
    match status {
        400 => format!(
            "openrouter.ai rejected the request as malformed (HTTP 400). This is a harness bug, \
             not a configuration one — the model `{model}` may not accept a field Marlowe sent. \
             {tail}"
        ),
        401 => format!(
            "openrouter.ai refused the credential (HTTP 401). The key in `{}` is not valid — \
             check it at https://openrouter.ai/keys. {tail}",
            crate::secret::KEY_VAR
        ),
        402 => format!(
            "openrouter.ai reports no credit (HTTP 402). Add credit at \
             https://openrouter.ai/credits, or choose a `:free` model. {tail}"
        ),
        403 => format!(
            "openrouter.ai refused the request (HTTP 403). This is usually upstream moderation or \
             a model your account is not permitted to use. {tail}"
        ),
        404 => format!(
            "openrouter.ai has no model called `{model}` (HTTP 404). The slug is \
             `vendor/model-name`, e.g. `anthropic/claude-sonnet-4.5`; the full list is at \
             https://openrouter.ai/models. {tail}"
        ),
        413 => format!(
            "the request was larger than `{model}` accepts (HTTP 413). Lower `--context`. {tail}"
        ),
        _ => format!("openrouter.ai returned HTTP {status} for `{model}`. {tail}"),
    }
}

/// The first line of an error body, bounded. A provider's error body can be a page.
fn first_line(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let line: String = trimmed.lines().next().unwrap_or("").chars().take(300).collect();
    format!("Upstream said: {line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::{ApiKey, KeyMissing, KEY_VAR};

    #[test]
    fn each_failure_names_a_different_remedy() {
        // Three distinguishable failures on the *local* half, because the fixes differ. This is
        // the Ollama adapter's `each_unavailability_names_a_different_remedy` for a path where
        // there are more ways to be wrong.
        let no_key = Availability::check(
            "anthropic/claude-sonnet-4.5",
            &Err(KeyMissing { var: KEY_VAR, empty: false }),
        );
        let no_model = Availability::check("  ", &Ok(ApiKey::from_secret("k")));
        let ready = Availability::check("m", &Ok(ApiKey::from_secret("k")));

        assert!(no_key.remedy().contains(KEY_VAR), "{}", no_key.remedy());
        assert!(no_model.remedy().contains("--openrouter-model"), "{}", no_model.remedy());
        assert!(ready.is_ready());
        for a in [&no_key, &no_model] {
            assert_eq!(a.degraded_path(), Some(DegradedPath::ModelUnavailable));
        }
        assert_eq!(ready.degraded_path(), None, "a ready run must not be flagged degraded");
    }

    #[test]
    fn a_missing_model_is_checked_before_the_key() {
        // Ordering matters for the message a user gets first. Naming a model is free; finding a
        // key is not, and telling someone to go and get one before mentioning that they also did
        // not say which model wastes the trip.
        let a = Availability::check("", &Err(KeyMissing { var: KEY_VAR, empty: false }));
        assert_eq!(a, Availability::ModelUnnamed);
    }

    #[test]
    fn a_status_explanation_names_the_action_not_the_number() {
        // "HTTP 402" is not a remedy. Each of these must name what to DO, and they must differ —
        // a bad key and an empty account are a two-second fix and a purchase.
        let bad_key = explain_status(401, "m", "");
        let no_credit = explain_status(402, "m", "");
        let no_model = explain_status(404, "vendor/typo", "");
        assert!(bad_key.contains(KEY_VAR), "{bad_key}");
        assert!(no_credit.contains("credits"), "{no_credit}");
        assert!(no_model.contains("vendor/typo") && no_model.contains("openrouter.ai/models"));
        assert_ne!(bad_key, no_credit);
    }

    #[test]
    fn an_error_body_is_bounded_and_attributed() {
        // A provider's error body can be an HTML page. It is quoted so a user can act on it, and
        // bounded so it cannot become the whole screen — and it says WHO said it, because
        // harness prose and upstream prose must never be confusable (ADR-030).
        let long = "x".repeat(5_000);
        let out = explain_status(500, "m", &long);
        assert!(out.contains("Upstream said:"), "{out}");
        assert!(out.len() < 600, "the body was not bounded: {} chars", out.len());
    }
}
