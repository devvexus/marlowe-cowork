//! A4: the tool-call repair corpus (`tests/fixtures/repair_corpus.json`, 100+ cases).
//!
//! `args` cases go through `repair_args` against the named tool's schema. `text` cases go through
//! the text scanner, then the whole reply is streamed through the Ollama folder in pieces of 1, 3
//! and 17 characters: the calls must be the ones found whole, and no markup may reach the screen.
//! Fixture sources and licences are listed in the corpus itself and in THIRD_PARTY.md.

use marlowe_model::{Completion, StreamEvent, ToolSpec};
use marlowe_provider::ollama::repair::{repair_args, ArgsOutcome, RepairStats};
use marlowe_provider::ollama::scan::{scan, ToolSet};
use marlowe_provider::ollama::Folder;
use serde_json::{json, Value};

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/repair_corpus.json")).expect("the corpus is valid JSON")
}

fn tools(c: &Value) -> Vec<ToolSpec> {
    c["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| ToolSpec {
            name: t["name"].as_str().unwrap().to_string(),
            description: t["description"].as_str().unwrap().to_string(),
            input_schema: t["input_schema"].clone(),
        })
        .collect()
}

#[test]
fn the_corpus_is_at_least_fifty_cases_and_ids_are_unique() {
    let c = corpus();
    let (a, t) = (c["args"].as_array().unwrap(), c["text"].as_array().unwrap());
    assert!(a.len() + t.len() >= 50, "{} + {}", a.len(), t.len());
    let mut ids: Vec<&str> = a.iter().chain(t).map(|x| x["id"].as_str().unwrap()).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate case id");
    // Every donor-credited case names its source test.
    for case in a.iter().chain(t) {
        let src = case["source"].as_str().unwrap();
        assert!(src == "marlowe" || src.starts_with("flashagent@0b08afc "), "{src}");
    }
}

#[test]
fn every_args_case_repairs_to_what_it_expects() {
    let c = corpus();
    let list = tools(&c);
    let stats = RepairStats::new();
    let mut failures = Vec::new();
    for case in c["args"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let spec = list.iter().find(|t| t.name == case["tool"].as_str().unwrap());
        let out = repair_args(&case["raw"], spec);
        stats.record(&out, false);
        let want = &case["value"];
        let fixes: Vec<&str> = case["fixes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let ok = match (case["expect"].as_str().unwrap(), &out) {
            ("clean", ArgsOutcome::Clean(v)) => v == want,
            ("repaired", ArgsOutcome::Repaired(v, got)) => {
                let names: Vec<&str> = got.iter().map(|f| f.as_str()).collect();
                v == want && fixes.iter().all(|f| names.contains(f))
            }
            ("unusable", ArgsOutcome::Unusable { truncated: false, .. }) => true,
            ("truncated", ArgsOutcome::Unusable { truncated: true, .. }) => true,
            _ => false,
        };
        if !ok {
            failures.push(format!("{id}: expected {} {want} {fixes:?}, got {out:?}", case["expect"]));
        }
    }
    assert!(failures.is_empty(), "{} failed:\n{}", failures.len(), failures.join("\n"));

    // The A4 reading of the corpus: every call that was not cut off or garbage ran.
    let s = stats.snapshot();
    let cut_or_junk = c["args"].as_array().unwrap().iter().filter(|x| matches!(x["expect"].as_str(), Some("unusable" | "truncated"))).count();
    assert_eq!(s.unusable as usize, cut_or_junk, "only the cut-off and garbage cases are unusable");
    assert!(s.repaired > 0 && s.clean > 0);
    eprintln!("repair corpus: {} args cases, valid-after-repair {:.1}%, needing repair {:.1}%", s.calls, s.valid_rate() * 100.0, s.repair_rate() * 100.0);
}

fn fold(list: &[ToolSpec], pieces: &[String]) -> (Completion, String) {
    let mut shown = String::new();
    let completion = {
        let mut on = |e: StreamEvent| {
            if let StreamEvent::TextDelta(t) = e {
                shown.push_str(&t);
            }
        };
        let mut f = Folder::new(false, &mut on).with_tools(list);
        for p in pieces {
            f.frame(&json!({"message": {"content": p}})).unwrap();
        }
        f.frame(&json!({"done": true, "done_reason": "stop"})).unwrap();
        f.finish()
    };
    (completion, shown)
}

fn cut(text: &str, n: usize) -> Vec<String> {
    text.chars().collect::<Vec<_>>().chunks(n).map(|c| c.iter().collect()).collect()
}

#[test]
fn every_text_case_is_found_whole_and_in_pieces() {
    let c = corpus();
    let list = tools(&c);
    let mut failures = Vec::new();
    for case in c["text"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let text = case["text"].as_str().unwrap();
        let want: Vec<(String, Value)> = case["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| (x["name"].as_str().unwrap().to_string(), x["args"].clone()))
            .collect();
        let has: Vec<&str> = case["prose_has"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        let lacks: Vec<&str> = case["prose_lacks"].as_array().unwrap().iter().filter_map(Value::as_str).collect();

        let s = scan(text, ToolSet::Known(&list));
        let got: Vec<(String, Value)> = s.calls.iter().map(|c| (c.name.clone(), c.args.clone())).collect();
        if got != want {
            failures.push(format!("{id}: scan found {got:?}, expected {want:?}"));
            continue;
        }
        if let Some(r) = case.get("rejected").and_then(Value::as_u64) {
            if s.rejected as u64 != r {
                failures.push(format!("{id}: rejected {} expected {r}", s.rejected));
            }
        }
        for h in &has {
            if !s.prose.contains(h) {
                failures.push(format!("{id}: scan prose {:?} lacks {h:?}", s.prose));
            }
        }
        for l in &lacks {
            if s.prose.contains(l) {
                failures.push(format!("{id}: scan prose {:?} still has {l:?}", s.prose));
            }
        }

        for n in [text.chars().count().max(1), 1, 3, 17] {
            let (done, shown) = fold(&list, &cut(text, n));
            let calls: Vec<(String, Value)> = done.tool_calls.iter().map(|c| (c.name.clone(), c.args.clone())).collect();
            if calls != want {
                failures.push(format!("{id} in pieces of {n}: streamed calls {calls:?}, expected {want:?}"));
                continue;
            }
            for h in &has {
                if !shown.contains(h) {
                    failures.push(format!("{id} in pieces of {n}: screen {shown:?} lacks {h:?}"));
                }
            }
            for l in &lacks {
                if shown.contains(l) || done.text.contains(l) {
                    failures.push(format!("{id} in pieces of {n}: markup {l:?} reached the screen: {shown:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} failed:\n{}", failures.len(), failures.join("\n"));
}
