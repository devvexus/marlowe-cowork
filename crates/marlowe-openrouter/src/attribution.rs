//! **Which model actually answered, and which upstream served it.**
//!
//! # This is the requirement that makes the adapter an instrument rather than a convenience
//!
//! OpenRouter routes one model *name* to several upstream providers, at different quantizations,
//! with different context limits and different tokenizers — and it may change that routing between
//! two requests without the name changing. Two benchmark runs can therefore differ materially
//! while **every label in the output reads identical**.
//!
//! That is this project's most-logged failure family stated in a new domain: *a measurement is
//! scoped to the system it was taken on*. CLAUDE.md's four instances are a graph, a split, a code
//! path and a machine. This is a fifth kind of boundary — the *upstream* — and it is worse than
//! the others in one specific way: it can move **between two calls in the same run**, with nobody
//! choosing anything.
//!
//! So the serving provider is recorded per call, and the run says what it saw.
//!
//! # `None` is rendered as NOT REPORTED, never as a plausible default
//!
//! If a response omits `provider`, this records that it was omitted. Filling it in with the
//! requested model name would produce a run record that reads exactly like one from a call whose
//! upstream *was* reported — which is the whole failure this type exists to prevent, committed by
//! the type that exists to prevent it.

use serde::{Deserialize, Serialize};

/// What one model call resolved to.
///
/// **No field can hold a credential.** That is asserted rather than assumed: this struct is
/// serialised into the run record, and the run record reaches the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallAttribution {
    /// The slug Marlowe asked for, e.g. `anthropic/claude-sonnet-4.5`.
    pub requested_model: String,
    /// The slug OpenRouter says it served. Usually equal to `requested_model`; **not always** —
    /// a `:floor`/`:nitro` variant or a fallback resolves to something else.
    pub served_model: Option<String>,
    /// The upstream that actually ran the tokens, e.g. `Anthropic`, `Amazon Bedrock`, `Fireworks`.
    /// This is the field that decides whether two runs are comparable.
    pub upstream_provider: Option<String>,
    /// OpenRouter's generation id. **The only way to get the exact cost afterwards** — the
    /// `/api/v1/generation?id=` endpoint returns native token counts and the settled price, which
    /// the streaming usage chunk rounds. Recorded so a benchmark can reconcile later.
    pub generation_id: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Cost in micros of USD, from OpenRouter's own `usage.cost`. **Reported, not estimated** —
    /// a per-token price table in this repo would be stale the week it was written.
    pub micros_usd: u64,
    /// How many HTTP attempts this call took. `> 1` means a retry happened, which is a latency
    /// number a benchmark should not silently absorb.
    pub attempts: u32,
    /// Whether the request pinned the upstream (`provider.order` with `allow_fallbacks: false`).
    /// A pinned run and an unpinned one are different experiments.
    pub upstream_pinned: bool,
}

/// What a field says when the response did not carry it.
pub const NOT_REPORTED: &str = "NOT REPORTED";

/// The longest an upstream-supplied identifier may be. A provider name is a word or three; a
/// generation id is thirty characters. Anything longer is not a name.
pub const MAX_FIELD_CHARS: usize = 120;

/// **Every value in [`CallAttribution`] except `requested_model` comes off the wire from a server,
/// and this is the one door it comes through.**
///
/// `provider`, `model` and `id` are strings openrouter.ai chooses. They are then printed to a
/// terminal (`marlowe: openrouter ...`), rendered by the classic CLI, and stored in the run
/// record — three places where an escape sequence or a right-to-left override would be acting on
/// the user's screen rather than describing a call.
///
/// This is not a claim that OpenRouter is hostile. It is
/// `no_rendered_event_carries_a_control_sequence_to_the_terminal`'s rule applied at the point of
/// entry, so that neither the daemon's `eprintln!` nor a future third renderer has to remember:
/// **sanitised where it arrives, not at each place it is shown.**
fn clean_field(raw: &str) -> Option<String> {
    let cleaned = marlowe_contract::text::sanitize_line(raw);
    let trimmed: String = cleaned.trim().chars().take(MAX_FIELD_CHARS).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

impl CallAttribution {
    /// Record the serving upstream, as the server named it.
    pub fn set_upstream(&mut self, raw: &str) {
        if let Some(v) = clean_field(raw) {
            self.upstream_provider = Some(v);
        }
    }

    /// Record the model the server says it served.
    pub fn set_served_model(&mut self, raw: &str) {
        if let Some(v) = clean_field(raw) {
            self.served_model = Some(v);
        }
    }

    /// Record the generation id, which is how the settled cost is reconciled later.
    pub fn set_generation_id(&mut self, raw: &str) {
        if let Some(v) = clean_field(raw) {
            self.generation_id = Some(v);
        }
    }
}

impl CallAttribution {
    pub fn requested(model: &str) -> Self {
        Self {
            requested_model: model.to_string(),
            served_model: None,
            upstream_provider: None,
            generation_id: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            micros_usd: 0,
            attempts: 1,
            upstream_pinned: false,
        }
    }

    /// The one line for `--dev` and for a benchmark's own log.
    ///
    /// **Never omits the upstream**, in either direction: a call whose provider was reported and
    /// one whose was not must not render the same, or the record is evidence about neither.
    pub fn disclosure(&self) -> String {
        format!(
            "model {} · served {} · upstream {}{} · {}+{} tok · {} µUSD · {} attempt(s) · gen {}",
            self.requested_model,
            self.served_model.as_deref().unwrap_or(NOT_REPORTED),
            self.upstream_provider.as_deref().unwrap_or(NOT_REPORTED),
            if self.upstream_pinned { " (PINNED)" } else { "" },
            self.prompt_tokens,
            self.completion_tokens,
            self.micros_usd,
            self.attempts,
            self.generation_id.as_deref().unwrap_or(NOT_REPORTED),
        )
    }

    /// Whether this call is reproducible enough to appear in a published table.
    ///
    /// **False when the upstream is unknown**, and that is the point of the method existing. A
    /// benchmark row whose backing provider was never recorded is not a row anyone can re-run, and
    /// this project treats an unreproducible number as not existing.
    pub fn upstream_is_recorded(&self) -> bool {
        self.upstream_provider.is_some()
    }
}

/// Every call in one run, folded.
///
/// A run is many model calls, and **the upstream can change between them**. A run-level record
/// that kept only the last one would be a plausible-looking lie about the earlier ones, so this
/// keeps the distinct set and says when it has more than one member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAttribution {
    pub calls: Vec<CallAttribution>,
}

impl RunAttribution {
    pub fn push(&mut self, call: CallAttribution) {
        self.calls.push(call);
    }

    pub fn total_micros_usd(&self) -> u64 {
        self.calls.iter().map(|c| c.micros_usd).sum()
    }

    pub fn total_tokens(&self) -> u64 {
        self.calls.iter().map(|c| c.prompt_tokens + c.completion_tokens).sum()
    }

    /// The distinct upstreams, sorted. `BTreeSet`-derived: iteration order reaches a run record,
    /// and `repro` compares run records.
    pub fn upstreams(&self) -> Vec<String> {
        let set: std::collections::BTreeSet<String> = self
            .calls
            .iter()
            .map(|c| c.upstream_provider.clone().unwrap_or_else(|| NOT_REPORTED.to_string()))
            .collect();
        set.into_iter().collect()
    }

    /// **True when one run was served by more than one upstream.**
    ///
    /// Not an error — OpenRouter is allowed to do this and it is often the reason a run finished
    /// at all. It is a fact the run record must carry, because a per-run average across two
    /// different quantizations is a number about no system.
    pub fn upstream_changed_mid_run(&self) -> bool {
        self.upstreams().len() > 1
    }

    /// The line a benchmark harness should print beside its score.
    pub fn disclosure(&self) -> String {
        if self.calls.is_empty() {
            return "no model calls".to_string();
        }
        let mut s = format!(
            "{} call(s) · {} tok · {} µUSD · upstream(s): {}",
            self.calls.len(),
            self.total_tokens(),
            self.total_micros_usd(),
            self.upstreams().join(", ")
        );
        if self.upstream_changed_mid_run() {
            s.push_str(
                " · WARNING: the serving upstream CHANGED during this run, so the calls in it \
                 were not all answered by the same system",
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreported_upstream_says_so_rather_than_defaulting_to_the_requested_model() {
        // Filling `upstream_provider` in from `requested_model` would make a call whose provider
        // was never reported render identically to one whose was — the exact confusion this type
        // exists to prevent, committed by the type that exists to prevent it.
        let a = CallAttribution::requested("anthropic/claude-sonnet-4.5");
        assert!(!a.upstream_is_recorded());
        let d = a.disclosure();
        assert!(d.contains(NOT_REPORTED), "{d}");
        assert!(
            d.matches(NOT_REPORTED).count() >= 3,
            "served model, upstream and generation id are each unknown and each must say so: {d}"
        );
    }

    #[test]
    fn a_run_served_by_two_upstreams_says_so() {
        // The failure this catches: a benchmark averaging across two quantizations and reporting
        // one number under one label.
        let mut run = RunAttribution::default();
        for up in ["Anthropic", "Amazon Bedrock"] {
            let mut c = CallAttribution::requested("anthropic/claude-sonnet-4.5");
            c.upstream_provider = Some(up.to_string());
            c.micros_usd = 1_500;
            c.prompt_tokens = 100;
            c.completion_tokens = 50;
            run.push(c);
        }
        assert!(run.upstream_changed_mid_run());
        assert_eq!(run.total_micros_usd(), 3_000);
        assert_eq!(run.total_tokens(), 300);
        assert_eq!(run.upstreams(), vec!["Amazon Bedrock", "Anthropic"]);
        assert!(run.disclosure().contains("CHANGED"), "{}", run.disclosure());
    }

    #[test]
    fn an_upstream_name_carrying_a_control_sequence_is_sanitised_where_it_arrives() {
        // These strings are chosen by a server and then printed to a terminal, rendered by the
        // classic CLI, and stored in the run record. Sanitising at each of those three sites is
        // three chances to forget; sanitising at the door is one.
        let mut c = CallAttribution::requested("m");
        c.set_upstream("Anthropic\u{1b}[2K\u{202e}exe.gnp");
        c.set_generation_id("gen-1\nmarlowe: everything is fine");
        let up = c.upstream_provider.clone().expect("an upstream was recorded");
        assert!(!up.contains('\u{1b}'), "an escape sequence survived: {up:?}");
        assert!(!up.contains('\u{202e}'), "a bidi override survived: {up:?}");
        assert!(up.starts_with("Anthropic"), "the name itself must survive: {up:?}");
        let id = c.generation_id.clone().expect("an id was recorded");
        assert!(!id.contains('\n'), "a newline let a server forge a Marlowe line: {id:?}");

        // Bounded, because a name is a name.
        c.set_upstream(&"A".repeat(10_000));
        assert!(c.upstream_provider.as_deref().unwrap().chars().count() <= MAX_FIELD_CHARS);

        // ...and an empty or whitespace-only value does NOT overwrite a real one with a blank,
        // which would read as "unreported" and is a different fact.
        c.set_upstream("   ");
        assert!(c.upstream_provider.is_some());
    }

    #[test]
    fn one_upstream_throughout_is_not_flagged() {
        // The control. A warning that fires on every run is a banner that means nothing — see
        // CLAUDE.md's fifteenth instance, where a latch fired on the first assemble of every run.
        let mut run = RunAttribution::default();
        for _ in 0..3 {
            let mut c = CallAttribution::requested("m");
            c.upstream_provider = Some("Anthropic".into());
            run.push(c);
        }
        assert!(!run.upstream_changed_mid_run());
        assert!(!run.disclosure().contains("CHANGED"), "{}", run.disclosure());
    }
}
