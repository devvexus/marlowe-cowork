//! The edit-reliability harness behind gate A5 (design 1): per edit format, the share of edit
//! calls that apply on the first attempt, and the share of intended edits reached within two
//! retries. A case is a file, the intended result and the model's attempts; [`run_case`] feeds
//! each failure back as the next attempt's prompt (`marlowe bench` drives a live model through
//! it), [`replay`] uses recorded attempts. Edits apply in memory with the same engines the
//! tools use, so the rates measure the formats, not the disk.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::engine;
use crate::editlines::{self, Anchor, Op};

/// Retries after a failed first attempt.
pub const MAX_RETRIES: usize = 2;

/// An edit format under test (design 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// `edit` / `multi_edit` (`old_string` -> `new_string`).
    Edit,
    /// `edit_lines` (hashed anchors).
    EditLines,
    /// `write` (whole file).
    Write,
}

/// One bench case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchCase {
    pub id: String,
    pub format: Format,
    pub original: String,
    pub intended: String,
    /// Recorded tool-call arguments, first attempt first (for [`replay`]).
    #[serde(default)]
    pub attempts: Vec<Value>,
}

/// How one case went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub format: Format,
    /// The first call applied (whether or not it produced the intended text).
    pub first_applied: bool,
    /// The intended text was reached within `1 + MAX_RETRIES` calls.
    pub succeeded: bool,
    /// Calls made.
    pub calls: usize,
    /// The feedback each failed call produced.
    pub errors: Vec<String>,
}

fn s<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str))
}

/// One call's arguments applied to `text` in `format`: the new text or the tool's error.
pub fn apply_attempt(format: Format, text: &str, args: &Value) -> Result<String, String> {
    match format {
        Format::Write => s(args, &["content"]).map(str::to_string).ok_or_else(|| "`content` is required".into()),
        Format::Edit => {
            let one = |t: &str, e: &Value| -> Result<String, String> {
                let old = s(e, &["old_string", "replacing"]).ok_or("`old_string` is required")?;
                let new = s(e, &["new_string", "content"]).ok_or("`new_string` is required")?;
                let all = e.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
                engine::apply(t, old, new, all).map(|a| a.text).map_err(|e| e.message())
            };
            match args.get("edits").and_then(Value::as_array) {
                Some(edits) => edits.iter().try_fold(text.to_string(), |t, e| one(&t, e)),
                None => one(text, args),
            }
        }
        Format::EditLines => {
            let anchor = Anchor::parse(s(args, &["anchor"]).unwrap_or("")).map_err(|e| e.message())?;
            let end = match s(args, &["end"]) {
                Some(e) if !e.trim().is_empty() => Some(Anchor::parse(e).map_err(|e| e.message())?),
                _ => None,
            };
            let op = Op::parse(s(args, &["op"]).unwrap_or("replace")).ok_or("`op` must be replace, insert_after, insert_before or delete")?;
            editlines::apply(text, &anchor, end.as_ref(), op, s(args, &["text"]).unwrap_or(""))
                .map(|a| a.text)
                .map_err(|e| e.message())
        }
    }
}

/// Runs one case. `next(k, feedback)` returns the k-th call's arguments (`feedback` is the
/// previous call's error, or a note that it applied but missed the intended text); `None` gives
/// up. A call that applies changes the working text, as it would on disk.
pub fn run_case(case: &BenchCase, next: &mut dyn FnMut(usize, Option<&str>) -> Option<Value>) -> CaseResult {
    let mut text = case.original.clone();
    let mut feedback: Option<String> = None;
    let mut r = CaseResult {
        id: case.id.clone(),
        format: case.format,
        first_applied: false,
        succeeded: false,
        calls: 0,
        errors: Vec::new(),
    };
    for k in 0..=MAX_RETRIES {
        let Some(args) = next(k, feedback.as_deref()) else { break };
        r.calls += 1;
        match apply_attempt(case.format, &text, &args) {
            Ok(t) => {
                if k == 0 {
                    r.first_applied = true;
                }
                text = t;
                if text == case.intended {
                    r.succeeded = true;
                    break;
                }
                feedback = Some("the edit applied, but the file does not yet match what was asked".into());
            }
            Err(e) => feedback = Some(e),
        }
        if let Some(f) = &feedback {
            r.errors.push(f.clone());
        }
    }
    r
}

/// [`run_case`] over the case's recorded attempts.
pub fn replay(case: &BenchCase) -> CaseResult {
    run_case(case, &mut |k, _| case.attempts.get(k).cloned())
}

/// A format's rates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Rates {
    /// Cases (one intended edit each; its first call is "the edit call").
    pub cases: usize,
    pub first_applied: usize,
    pub succeeded: usize,
}

impl Rates {
    pub fn first_attempt_rate(&self) -> f64 {
        ratio(self.first_applied, self.cases)
    }

    pub fn success_rate(&self) -> f64 {
        ratio(self.succeeded, self.cases)
    }

    /// Whether both rates meet `gate`.
    pub fn meets(&self, gate: Gate) -> bool {
        self.cases > 0 && self.first_attempt_rate() >= gate.first_attempt && self.success_rate() >= gate.success
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

/// Gate A5's thresholds for one model class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gate {
    pub first_attempt: f64,
    pub success: f64,
}

/// Local models (dusk): 92% first attempt, 98% within two retries.
pub const GATE_LOCAL: Gate = Gate { first_attempt: 0.92, success: 0.98 };
/// Frontier models: 98% and 99.5%.
pub const GATE_FRONTIER: Gate = Gate { first_attempt: 0.98, success: 0.995 };

/// Rates per format.
pub fn score(results: &[CaseResult]) -> BTreeMap<Format, Rates> {
    let mut out: BTreeMap<Format, Rates> = BTreeMap::new();
    for r in results {
        let e = out.entry(r.format).or_default();
        e.cases += 1;
        e.first_applied += usize::from(r.first_applied);
        e.succeeded += usize::from(r.succeeded);
    }
    out
}

/// One line per format: `edit: first 95.0% (19/20), within 2 retries 100.0% (20/20)`.
pub fn report(rates: &BTreeMap<Format, Rates>) -> String {
    rates
        .iter()
        .map(|(f, r)| {
            format!(
                "{}: first {:.1}% ({}/{}), within {MAX_RETRIES} retries {:.1}% ({}/{})",
                serde_json::to_value(f).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
                100.0 * r.first_attempt_rate(),
                r.first_applied,
                r.cases,
                100.0 * r.success_rate(),
                r.succeeded,
                r.cases
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
