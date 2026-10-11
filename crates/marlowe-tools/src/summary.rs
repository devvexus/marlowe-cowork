//! CONTRACTS.md §8 — the tool-result summary contract.
//!
//! > Every tool declares how its result renders as **one line**. `done` is not acceptable.
//!
//! One contract serves three things at once, which is why it is pinned rather than left to
//! each tool: terminal craft (§B6's one-line footprint), the token budget (§6 — this summary
//! is the loop's *default view* of a result above `inline_threshold_bytes`), and containment
//! (§8.2 — raw untrusted bytes stay out of attention behind a reference).
//!
//! [`Metric`] has no free-text variant and must not gain one. The type is the enforcement:
//! a tool cannot report `done` without adding a metric kind and defending it in review.

use serde::Serialize;

/// CONTRACTS.md §8. Typed, never generic prose.
///
/// **`Serialize` only, and that is a consequence of the `&'static str` fields rather than an
/// oversight.** A `Deserialize` impl would need `String` or `Cow`, which reintroduces free
/// text at exactly the boundary that reads outside input — the escape hatch this type exists
/// to close. A resumed run (M3) reconstructs summaries from the journal's typed
/// `ToolCompleted` payload; it does not deserialize this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Count { n: u64, unit: &'static str },
    Diff { added: u32, removed: u32 },
    /// Deciseconds rather than `f32`, so a summary is `Eq` and comparable in a test. A
    /// floating-point field here would make two identical results unequal.
    Test { passed: u32, failed: u32, decis: u32 },
    Duration { ms: u64 },
    Bytes { n: u64 },
    /// A non-zero process exit. Distinct from `Count` because it renders in the failure
    /// colour and because "exit 1" counts nothing.
    Exit { code: i32 },
    /// A one-word outcome for a call with nothing to count — `spawned`, `queued`.
    /// `&'static str` on purpose: it cannot become a free-text escape hatch for `done`.
    State(&'static str),
}

impl Metric {
    pub fn render(&self) -> String {
        match self {
            Metric::Count { n, unit } => {
                if *n == 1 {
                    // "1 lines" is the tell of a system that never read its own output.
                    format!("1 {}", unit.trim_end_matches('s'))
                } else {
                    format!("{n} {unit}")
                }
            }
            // U+2212 MINUS SIGN. A hyphen beside a plus reads as a dash.
            Metric::Diff { added, removed } => format!("+{added} \u{2212}{removed}"),
            Metric::Test { passed, failed, decis } => {
                let secs = format!("{}.{}s", decis / 10, decis % 10);
                if *failed == 0 {
                    format!("{passed} passed · {secs}")
                } else {
                    format!("{passed} passed · {failed} failed · {secs}")
                }
            }
            Metric::Duration { ms } => format!("{ms} ms"),
            Metric::Bytes { n } => format!("{n} B"),
            Metric::Exit { code } => format!("exit {code}"),
            Metric::State(s) => (*s).to_string(),
        }
    }
}

/// CONTRACTS.md §8. `detail` is the expansion payload, held by reference so expanding a
/// result is a content-store read rather than a thing the loop was already carrying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultSummary {
    pub metrics: Vec<Metric>,
    pub detail: Option<String>,
}

impl ResultSummary {
    pub fn new(metrics: Vec<Metric>) -> Self {
        Self { metrics, detail: None }
    }

    pub fn with_detail(metrics: Vec<Metric>, detail: impl Into<String>) -> Self {
        Self { metrics, detail: Some(detail.into()) }
    }

    /// The right-hand side of a §B6 tool line: `48 lines`, `+3 −0`, `12 passed · 1.4s`.
    pub fn render(&self) -> String {
        self.metrics.iter().map(Metric::render).collect::<Vec<_>>().join(" · ")
    }
}

/// How one tool's result becomes one line, and when its bytes stop being inlined.
///
/// Function pointers rather than a trait object: a `SummarySpec` is data on a registration,
/// and a tool that cannot describe its own result in this shape has not finished declaring
/// itself.
#[derive(Debug, Clone, Copy)]
pub struct SummarySpec {
    pub verb: &'static str,
    /// Above this, the loop receives a reference and the summary — never the bytes.
    /// §6's "sandbox tool output before it reaches context", expressed as a number a tool
    /// must choose rather than inherit.
    pub inline_threshold_bytes: u64,
}

impl SummarySpec {
    pub const fn new(verb: &'static str, inline_threshold_bytes: u64) -> Self {
        Self { verb, inline_threshold_bytes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_the_way_b6_specifies() {
        assert_eq!(Metric::Count { n: 1, unit: "lines" }.render(), "1 line");
        assert_eq!(Metric::Count { n: 48, unit: "lines" }.render(), "48 lines");
        assert_eq!(Metric::Diff { added: 3, removed: 0 }.render(), "+3 \u{2212}0");
        assert_eq!(
            Metric::Test { passed: 12, failed: 0, decis: 14 }.render(),
            "12 passed · 1.4s"
        );
        assert_eq!(Metric::Exit { code: 1 }.render(), "exit 1");
    }

    #[test]
    fn there_is_no_free_text_metric() {
        // Executable documentation for §8's "`done` is not acceptable". `State` takes
        // `&'static str`, so a runtime string cannot reach it; if someone widens that to
        // `String`, this test is what has to be deleted first.
        let s = ResultSummary::new(vec![Metric::State("spawned")]);
        assert_eq!(s.render(), "spawned");
    }
}
