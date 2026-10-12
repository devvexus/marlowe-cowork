//! The 150-case edit fixture (design 2.1 T6) and the A5 harness.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::bench::{self, BenchCase, Format};
use super::*;

const CASES: &str = include_str!("fixtures/edit_cases.json");

/// Every case: the engine's outcome equals the fixture's (result text, tier and count, or the
/// error kind and the phrases a refusal must contain).
#[test]
fn parity_t6_edit_fixture_150_cases() {
    let fx: Value = serde_json::from_str(CASES).unwrap();
    let files = fx["files"].as_object().unwrap();
    let cases = fx["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 150);
    let mut by_tier: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures = Vec::new();
    for c in cases {
        let id = c["id"].as_str().unwrap();
        let file = files[c["file"].as_str().unwrap()].as_str().unwrap();
        let (old, new) = (c["old"].as_str().unwrap(), c["new"].as_str().unwrap());
        let all = c["replace_all"].as_bool().unwrap();
        let exp = &c["expect"];
        let got = apply(file, old, new, all);
        let ok = match (&got, exp["ok"].as_bool().unwrap()) {
            (Ok(a), true) => {
                *by_tier.entry(a.tier.as_str().into()).or_default() += 1;
                a.text == exp["result"].as_str().unwrap()
                    && a.tier.as_str() == exp["tier"].as_str().unwrap()
                    && a.count as u64 == exp["count"].as_u64().unwrap()
            }
            (Err(e), false) => {
                *by_tier.entry(e.kind().into()).or_default() += 1;
                let phrases = exp["contains"].as_array().cloned().unwrap_or_default();
                let count_ok = match (e, exp["count"].as_u64()) {
                    (EditError::Ambiguous { count, .. }, Some(n)) => *count as u64 == n,
                    _ => true,
                };
                e.kind() == exp["error"].as_str().unwrap()
                    && count_ok
                    && phrases.iter().all(|p| e.message().contains(p.as_str().unwrap()))
            }
            _ => false,
        };
        if !ok {
            failures.push(format!("{id}: got {got:?}\nwant {exp}"));
        }
    }
    assert!(failures.is_empty(), "{} of 150 failed:\n{}", failures.len(), failures.join("\n\n"));
    // The fixture exercises every tier and every refusal.
    for k in ["exact", "eol", "whitespace", "not_found", "ambiguous", "empty_old", "no_change"] {
        assert!(by_tier.get(k).copied().unwrap_or(0) > 0, "no {k} case: {by_tier:?}");
    }
}

#[test]
fn whitespace_tier_reindents_and_names_itself() {
    let file = "fn f() {\n    if x {\n        go();\n    }\n}\n";
    let a = apply(file, "if x {\n    go();\n}\n", "if y {\n    stop();\n}\n", false).unwrap();
    assert_eq!(a.tier, Tier::Whitespace);
    assert_eq!(a.text, "fn f() {\n    if y {\n        stop();\n    }\n}\n");
    assert!(Tier::Whitespace.note().contains("re-indented"));
    // Non-uniform: refused, not guessed.
    let e = apply(file, "if x {\ngo();\n}", "z", false).unwrap_err();
    assert_eq!(e.kind(), "not_found");
    assert!(e.message().contains("apart from whitespace"), "{}", e.message());
}

#[test]
fn a_miss_shows_the_closest_region() {
    let file = "alpha\nlet total = price * quantity + shipping_cost;\nomega\n";
    let e = apply(file, "let total = price * quantity + shiping_cost;", "x", false).unwrap_err();
    let m = e.message();
    assert!(m.contains("Did you mean lines 2-2") && m.contains("     2\tlet total"), "{m}");
}

#[test]
fn ambiguity_lists_sites_and_replace_all_takes_them() {
    let e = apply("a\nb\na\n", "a", "c", false).unwrap_err();
    assert!(matches!(e, EditError::Ambiguous { count: 2, .. }));
    assert!(e.message().contains("lines 1, 3") && e.message().contains("replace_all"));
    let a = apply("a\nb\na\n", "a", "c", true).unwrap();
    assert_eq!((a.text.as_str(), a.count), ("c\nb\nc\n", 2));
}

// ---------------------------------------------------------------- A5 harness

fn case(id: &str, format: Format, original: &str, intended: &str, attempts: Vec<Value>) -> BenchCase {
    BenchCase { id: id.into(), format, original: original.into(), intended: intended.into(), attempts }
}

/// The harness counts first-attempt applies per edit call and intended edits reached within two
/// retries, per format; a failure's message is the next attempt's feedback.
#[test]
fn a5_harness_scores_formats_from_recorded_attempts() {
    let orig = "one\ntwo\nthree\n";
    let want = "one\nTWO\nthree\n";
    let h2 = crate::editlines::line_hash("two");
    let cases = vec![
        // edit: first try applies.
        case("e1", Format::Edit, orig, want, vec![json!({"old_string": "two", "new_string": "TWO"})]),
        // edit: miss, then fixed on retry 1.
        case("e2", Format::Edit, orig, want, vec![json!({"old_string": "tow", "new_string": "TWO"}), json!({"old_string": "two", "new_string": "TWO"})]),
        // edit: three misses.
        case("e3", Format::Edit, orig, want, vec![json!({"old_string": "x"}), json!({"old_string": "y", "new_string": "z"}), json!({"old_string": "w", "new_string": "z"})]),
        // edit_lines: stale hash, then a fresh one.
        case("l1", Format::EditLines, orig, want, vec![json!({"anchor": "2:000", "op": "replace", "text": "TWO"}), json!({"anchor": format!("2:{h2}"), "op": "replace", "text": "TWO"})]),
        case("l2", Format::EditLines, orig, want, vec![json!({"anchor": format!("2:{h2}"), "op": "replace", "text": "TWO"})]),
        // write: applies but wrong, then right.
        case("w1", Format::Write, orig, want, vec![json!({"content": "one\nTWO\n"}), json!({"content": want})]),
    ];
    let mut feedback_seen = Vec::new();
    let results: Vec<_> = cases
        .iter()
        .map(|c| {
            bench::run_case(c, &mut |k, fb| {
                if let Some(f) = fb {
                    feedback_seen.push(f.to_string());
                }
                c.attempts.get(k).cloned()
            })
        })
        .collect();
    assert_eq!(results.iter().map(|r| r.calls).collect::<Vec<_>>(), [1, 2, 3, 2, 1, 2]);
    assert!(feedback_seen.iter().any(|f| f.contains("has changed since you read it")), "{feedback_seen:?}");
    assert!(feedback_seen.iter().any(|f| f.contains("was not found")));
    let rates = bench::score(&results);
    let edit = rates[&Format::Edit];
    assert_eq!((edit.cases, edit.first_applied, edit.succeeded), (3, 1, 2));
    let lines = rates[&Format::EditLines];
    assert_eq!((lines.cases, lines.first_applied, lines.succeeded), (2, 1, 2));
    let write = rates[&Format::Write];
    assert_eq!((write.cases, write.first_applied, write.succeeded), (1, 1, 1));
    assert!(!edit.meets(bench::GATE_LOCAL) && write.meets(bench::GATE_FRONTIER));
    let shown = bench::report(&rates);
    assert!(shown.contains("edit: first 33.3% (1/3), within 2 retries 66.7% (2/3)"), "{shown}");
    assert_eq!(bench::replay(&cases[1]), results[1]);
}

/// The fixture as a bench: every case whose expected outcome is a success, attempted once with
/// its own arguments, applies on the first attempt (the engine side of A5 is lossless).
#[test]
fn a5_harness_over_the_fixture_is_lossless_for_valid_edits() {
    let fx: Value = serde_json::from_str(CASES).unwrap();
    let files = fx["files"].as_object().unwrap();
    let mut results = Vec::new();
    for c in fx["cases"].as_array().unwrap().iter().filter(|c| c["expect"]["ok"] == true) {
        let file = files[c["file"].as_str().unwrap()].as_str().unwrap();
        let args = json!({"old_string": c["old"], "new_string": c["new"], "replace_all": c["replace_all"]});
        let bc = case(c["id"].as_str().unwrap(), Format::Edit, file, c["expect"]["result"].as_str().unwrap(), vec![args]);
        results.push(bench::replay(&bc));
    }
    let r = bench::score(&results)[&Format::Edit];
    assert_eq!(r.first_applied, r.cases);
    assert_eq!(r.succeeded, r.cases);
    assert!(r.cases >= 100);
}
