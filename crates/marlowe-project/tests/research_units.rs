//! The research functions one at a time, called the way the engine calls them: a `TaskInput` over
//! a real project state, a stored page, and a crafted model reply. No network, no model.

mod common;
mod world;

use std::sync::{Arc, Mutex};

use common::*;
use marlowe_model::CancelToken;
use marlowe_project::engine::{Effect, Finished, Prompt, RuleFailure, TaskInput};
use marlowe_project::research::{self, Admit, Failure, Fetched, PageFetch, PageRef, Web};
use marlowe_project::*;
use marlowe_research::{ChannelError, ChannelId, ChannelQuery, Identity, SourceRef};
use serde_json::{json, Map, Value};
use world::*;

struct Lab {
    p: Project,
    rig: Rig,
    tools: Toolbox,
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}

fn lab(questions: usize) -> Lab {
    lab_for(&research_plan(questions))
}

fn lab_for(plan: &Value) -> Lab {
    let p = Project::new("lab", Registry::builtin(), plan);
    let rig = rig(&World::load(), &p.home.0);
    let tools = research::toolbox(&rig.deps);
    p.handle.mutate(Mutation::SetGrants(state::Grants { net: Some(state::NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 60, used: 0 }) })).unwrap();
    Lab { p, rig, tools }
}

impl Lab {
    fn put(&self, id: &str, kind: &str, form: &str, parent: Option<&str>, data: Value) -> Item {
        let mut it = Item::new(id.to_string(), kind, form);
        it.status = ItemStatus::Done;
        it.parent = parent.map(str::to_string);
        it.data = obj(data);
        self.p.handle.mutate(Mutation::PutItem(Box::new(it.clone()))).unwrap();
        it
    }

    fn question(&self, n: usize) -> String {
        self.p.state().items_of_kind("question")[n].id.clone()
    }

    /// Stores page text in the project's files and returns its hash.
    fn page(&self, text: &str) -> String {
        self.rig.deps.store().unwrap().put_source(&self.p.id, text).unwrap()
    }

    fn input(&self, step: &str, item: Option<Item>, members: Vec<Item>, existing: Vec<Item>, sub: &str) -> TaskInput {
        let reg = Registry::builtin();
        let spec = reg.menu().values().find_map(|ph| ph.step(step)).unwrap_or_else(|| panic!("no step {step}")).clone();
        let st = self.p.state();
        TaskInput {
            project: self.p.id.clone(),
            phase: "ph_x".into(),
            round: 1,
            step: Arc::new(spec),
            item,
            sub: sub.into(),
            plan: Arc::new(st.plan.clone()),
            members,
            existing,
            pass: 0,
            today: "2026-10-23".into(),
            project_handle: self.p.handle.clone(),
        }
    }

    fn finish(&self, name: &str, input: &TaskInput, reply: Value) -> Result<Finished, RuleFailure> {
        self.tools.get_finish(name).unwrap_or_else(|| panic!("no finish {name}"))(input, Some(obj(reply)))
    }

    fn prompt(&self, name: &str, input: &TaskInput) -> Prompt {
        self.tools.get_prompt(name).unwrap_or_else(|| panic!("no prompt {name}"))(input).expect("a prompt")
    }

    fn code(&self, name: &str, input: &TaskInput) -> Result<Finished, String> {
        self.tools.get_code(name).unwrap_or_else(|| panic!("no code fn {name}"))(input, &CancelToken::new())
    }
}

const PAGE: &str = "# Kimi K2.6 released\n\nMoonshot AI today released Kimi K2.6 on 2026-10-20. Compared with Kimi K2.5 [L1], the new release improves long-context retrieval. The details are in the technical report [L2].\n\nIt supports a context window of 256k tokens and is available as open weights.";

/// A page item whose text is stored, with a two-row link table.
fn page_item(l: &Lab, q: &str) -> (Item, String) {
    let hash = l.page(PAGE);
    let links = json!([
        {"index": 1, "url": "https://moonshot.example/blog/kimi-k2-5", "text": "Kimi K2.5", "host": "moonshot.example"},
        {"index": 2, "url": "https://moonshot.example/papers/k2-6", "text": "technical report", "host": "moonshot.example"}
    ]);
    l.rig.deps.store().unwrap().put_source_aux(&l.p.id, &hash, "links", &links.to_string()).unwrap();
    let it = l.put(
        "i_page1",
        "page",
        "page@1",
        Some(q),
        json!({"source_id": "s_1", "url": "https://moonshot.example/blog/kimi-k2-6", "title": "Kimi K2.6 released", "channel": "web", "status": "ok", "credibility": 0.7, "links": 2,
               "references": [{"doi": "10.1234/k26", "arxiv_id": null, "url": null, "year": 2026, "label": "12"}], "question": "q1", "text_ref": hash,
               "identity_confirmed": false, "retracted": false, "chars": 300}),
    );
    (it, hash)
}

// ---- analyze -----------------------------------------------------------------------------------

#[test]
fn no_model_field_can_carry_an_address_into_a_source() {
    let l = lab(2);
    let q = l.question(0);
    let (page, _) = page_item(&l, &q);
    let input = l.input("analyze", Some(page), vec![], vec![], "q1");
    let evil = "https://evil.example/x?leak=1";
    let reply = json!({
        "relevancy": "High", "source_kind": "official",
        "one_line": format!("see {evil}"), "notes": format!("go to www.evil.example/a then {evil}"),
        "claims": [{"text": format!("Kimi K2.6 was released {evil}"), "quote": "Moonshot AI today released Kimi K2.6 on 2026-10-20."}],
        "additional_sources": [
            {"link": 2, "reference": format!("also {evil}"), "opportunity": "High", "notes": format!("read {evil}")},
            {"link": null, "reference": evil, "opportunity": "Medium", "notes": "an address alone is nothing to follow"},
            {"link": 77, "reference": "evil.example/p", "opportunity": "High", "notes": "out of range"}
        ],
        "records": [{"model_name": format!("Kimi {evil}"), "vendor": "Moonshot AI", "status": "released", "release_date": evil, "size": "www.evil.example"}]
    });
    let fin = l.finish("analyze_finish", &input, reply).expect("accepted");
    let json = serde_json::to_string(&fin.data).unwrap();
    for bad in ["evil", "http", "www."] {
        assert!(!json.contains(bad.trim_start_matches("http")) || bad == "http" && json.matches("http").count() == json.matches("https://moonshot.example").count(), "{bad} survived: {json}");
    }
    assert!(!json.contains("evil.example") && !json.contains("www.evil"), "{json}");
    // The only addresses are the page's own link-table rows and the page itself.
    let urls: Vec<&str> = json.match_indices("https://").map(|(i, _)| &json[i..]).map(|s| s.split('"').next().unwrap()).collect();
    assert!(urls.iter().all(|u| u.starts_with("https://moonshot.example/")), "{urls:?}");
    let extra = fin.data["additional_sources"].as_array().unwrap();
    assert_eq!(extra.len(), 1, "an address as the only thing to follow is nothing, and link 77 does not exist: {extra:?}");
    assert_eq!(extra[0]["source"], "https://moonshot.example/papers/k2-6");
    assert_eq!(extra[0]["reference"], "also", "the address is cut out of the model's words");
    let rec = &fin.data["records"][0];
    assert_eq!((rec["release_date"].is_null(), rec["size"].is_null()), (true, true), "address-shaped values become null");
}

#[test]
fn a_named_reference_is_resolved_from_the_pages_own_list_and_a_page_that_cites_nothing_it_can_quote_is_told_so() {
    let l = lab(2);
    let q = l.question(0);
    let (page, _) = page_item(&l, &q);
    let input = l.input("analyze", Some(page), vec![], vec![], "q1");
    let ok = json!({"relevancy": "Medium", "source_kind": "news", "one_line": "x", "notes": "", "claims": [],
        "additional_sources": [{"link": null, "reference": "[12]", "opportunity": "High", "notes": "the report"}], "records": []});
    let fin = l.finish("analyze_finish", &input, ok).unwrap();
    assert_eq!(fin.data["additional_sources"][0]["source"], "https://doi.org/10.1234/k26");
    // Critical with a made-up quote: the employee is told what was wrong.
    let bad = json!({"relevancy": "Critical", "source_kind": "official", "one_line": "x", "notes": "",
        "claims": [{"text": "It beats everything", "quote": "Kimi K2.6 beats everything ever made"}], "additional_sources": [], "records": []});
    let err = l.finish("analyze_finish", &input, bad).unwrap_err();
    assert_eq!(err.rule, "quotes_found");
    assert!(err.tell.contains("word for word") && err.tell.contains("Kimi K2.6 beats everything"), "{}", err.tell);
    // The same claim under a lower rating is simply dropped.
    let low = json!({"relevancy": "Low", "source_kind": "news", "one_line": "x", "notes": "",
        "claims": [{"text": "It beats everything", "quote": "Kimi K2.6 beats everything ever made"}], "additional_sources": [], "records": []});
    let fin = l.finish("analyze_finish", &input, low).unwrap();
    assert!(fin.data["claims"].as_array().unwrap().is_empty() && fin.data["rejected"]["quote_not_found"] == 1);
}

#[test]
fn credibility_comes_from_the_channel_and_the_kind_and_a_skipped_page_keeps_no_claims() {
    let l = lab(2);
    let q = l.question(0);
    let (page, _) = page_item(&l, &q);
    let input = l.input("analyze", Some(page), vec![], vec![], "q1");
    let reply = |kind: &str, rel: &str| json!({"relevancy": rel, "source_kind": kind, "one_line": "x", "notes": "", "claims": [{"text": "t", "quote": "Moonshot AI today released Kimi K2.6 on 2026-10-20."}], "additional_sources": [], "records": []});
    assert_eq!(l.finish("analyze_finish", &input, reply("official", "High")).unwrap().data["credibility"], 0.7);
    assert_eq!(l.finish("analyze_finish", &input, reply("forum", "High")).unwrap().data["credibility"], 0.35);
    let skip = l.finish("analyze_finish", &input, reply("official", "Skip")).unwrap();
    assert!(skip.data["claims"].as_array().unwrap().is_empty());
    assert_eq!(skip.data["relevance"], 0.0);
    let idx: Vec<u64> = (0..3).map(|_| l.finish("analyze_finish", &input, reply("blog", "Low")).unwrap().data["index"].as_u64().unwrap()).collect();
    assert_eq!(idx[1], idx[0] + 1, "indexes count on");
    assert_eq!(idx[2], idx[0] + 2);
}

// ---- extract, condense ------------------------------------------------------------------------------

/// Two sources and their claims, as Extract leaves them. `links` says what the second source links to.
fn claims_world(l: &Lab, second_links_first: bool) -> (Vec<Item>, String) {
    let q = l.question(0);
    let t1 = l.page("Moonshot AI today released Kimi K2.6 on 2026-10-20. It supports a context window of 256k tokens.");
    let t2 = l.page("The lab said the model supports a context window of 256k tokens. Pricing is unchanged.");
    let link_rows = if second_links_first { json!([{"index": 1, "url": "https://a.example/post", "text": "post", "host": "a.example"}]) } else { json!([]) };
    l.rig.deps.store().unwrap().put_source_aux(&l.p.id, &t2, "links", &link_rows.to_string()).unwrap();
    l.put("i_s1", "source", "analyze@1", Some(&q), json!({"index": 1, "title": "A", "relevancy": "High", "credibility": 0.7, "source_kind": "official", "one_line": "x", "source": "https://a.example/post", "notes": "", "claims": [], "additional_sources": [], "records": [], "relevance": 0.7, "page": "i_p1", "text_ref": t1}));
    l.put("i_s2", "source", "analyze@1", Some(&q), json!({"index": 2, "title": "B", "relevancy": "High", "credibility": 0.5, "source_kind": "news", "one_line": "x", "source": "https://b.example/news", "notes": "", "claims": [], "additional_sources": [], "records": [], "relevance": 0.7, "page": "i_p2", "text_ref": t2}));
    let claim = |id: &str, src: &str, hash: &str, text: &str, quote: &str, cred: f64| -> Item {
        let page = if hash == t1 { "Moonshot AI today released Kimi K2.6 on 2026-10-20. It supports a context window of 256k tokens." } else { "The lab said the model supports a context window of 256k tokens. Pricing is unchanged." };
        let a = page.find(quote).unwrap();
        l.put(&format!("i_{id}"), "claim", "claim@1", Some(&q), json!({"id": id, "text": text, "quote": quote, "locator": {"doc": hash, "start": a, "end": a + quote.len()}, "source": src, "sub": "q1", "credibility": cred, "status": "new", "text_ref": hash}))
    };
    let c1 = claim("c_00000001", "i_s1", &t1, "Kimi K2.6 was released on 2026-10-20", "Moonshot AI today released Kimi K2.6 on 2026-10-20.", 0.7);
    let c2 = claim("c_00000002", "i_s1", &t1, "Kimi K2.6 has a 256k context window", "It supports a context window of 256k tokens.", 0.7);
    let c3 = claim("c_00000003", "i_s2", &t2, "Kimi K2.6 has a 256k context window", "The lab said the model supports a context window of 256k tokens.", 0.5);
    (vec![c1, c2, c3], t1)
}

#[test]
fn condense_loses_no_claim_drops_unknown_handles_and_keeps_a_claim_in_its_first_group() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    let input = l.input("condense", None, claims.clone(), vec![], "q1");
    // N order: highest credibility first, ties by claim id: N1 = c1, N2 = c2, N3 = c3.
    let reply = json!({"groups": [
        {"members": ["N2", "N3", "N9"], "claim": "Kimi K2.6 has a 256k context window", "contradicts": []},
        {"members": ["N2", "N1"], "claim": "Kimi K2.6 was released on 2026-10-20", "contradicts": []}
    ]});
    let fin = l.finish("condense_apply", &input, reply).unwrap();
    let cc: Vec<&Item> = fin.emits.iter().filter(|i| i.kind == "cclaim").collect();
    assert_eq!(cc.len(), 2);
    let ctx = cc.iter().find(|c| c.data["claim"] == "Kimi K2.6 has a 256k context window").unwrap();
    let ids: Vec<&str> = ctx.data["corroborates"].as_array().unwrap().iter().map(|r| r["claim_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["c_00000002", "c_00000003"], "N2 stayed in the first group; N9 does not exist");
    let rel = cc.iter().find(|c| c.data["claim"] == "Kimi K2.6 was released on 2026-10-20").unwrap();
    assert_eq!(rel.data["corroborates"].as_array().unwrap().len(), 1, "N2 was already taken, so the second group has N1 alone");
    // Every claim is in exactly one group, and the originals are the stored ones.
    let mut all: Vec<String> = cc.iter().flat_map(|c| c.data["corroborates"].as_array().unwrap().iter().map(|r| r["claim_id"].as_str().unwrap().to_string())).collect();
    all.sort();
    assert_eq!(all, ["c_00000001", "c_00000002", "c_00000003"]);
    for c in &cc {
        for row in c.data["corroborates"].as_array().unwrap() {
            let orig = claims.iter().find(|x| x.data["id"] == row["claim_id"]).unwrap();
            assert_eq!((&row["text"], &row["quote"], &row["text_ref"]), (&orig.data["text"], &orig.data["quote"], &orig.data["text_ref"]));
            assert_eq!((&row["start"], &row["end"]), (&orig.data["locator"]["start"], &orig.data["locator"]["end"]));
        }
        assert_eq!(c.status, ItemStatus::Done);
        assert_eq!(c.data["stance"], "supported");
    }
    assert_eq!(fin.rejected.as_ref().unwrap()["unknown_handle"], 1);
    assert_eq!(fin.rejected.as_ref().unwrap()["claim_in_two_groups"], 1);
}

#[test]
fn an_omitted_claim_becomes_a_group_of_its_own_and_a_failed_employee_still_loses_nothing() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    let input = l.input("condense", None, claims.clone(), vec![], "q1");
    let fin = l.finish("condense_apply", &input, json!({"groups": [{"members": ["N1"], "claim": "Kimi K2.6 was released on 2026-10-20", "contradicts": []}]})).unwrap();
    let texts: Vec<String> = fin.emits.iter().map(|c| c.data["claim"].as_str().unwrap().to_string()).collect();
    assert_eq!(texts.len(), 3, "N2 and N3 were left out and each became a group: {texts:?}");
    // The employee failed altogether: every claim is a singleton, in the claim's own words.
    let degraded = l.tools.get_finish("condense_apply").unwrap()(&input, None).unwrap();
    assert_eq!(degraded.emits.len(), 3);
    assert!(degraded.emits.iter().any(|c| c.data["claim"] == "Kimi K2.6 was released on 2026-10-20"));
}

#[test]
fn a_claim_is_contested_when_a_group_says_so_and_independence_counts_links() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, true);
    let input = l.input("condense", None, claims, vec![], "q1");
    let fin = l.finish("condense_apply", &input, json!({"groups": [
        {"members": ["N1"], "claim": "Kimi K2.6 was released on 2026-10-20", "contradicts": ["N3"]},
        {"members": ["N2", "N3"], "claim": "Kimi K2.6 has a 256k context window", "contradicts": []}]})).unwrap();
    let by = |t: &str| fin.emits.iter().find(|c| c.data["claim"] == t).unwrap();
    assert_eq!(by("Kimi K2.6 was released on 2026-10-20").data["stance"], "contested");
    assert_eq!(by("Kimi K2.6 has a 256k context window").data["stance"], "contested", "the group holding the contradicted claim is contested too");
    // The news page links to the announcement: one root, scored at the better credibility.
    let ctx = by("Kimi K2.6 has a 256k context window");
    assert_eq!((ctx.data["source_count"].as_u64(), ctx.data["score"].as_f64()), (Some(1), Some(0.7)));
}

#[test]
fn two_unlinked_sources_are_two_roots_and_a_later_pass_merges_into_the_group_it_extends() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    // Pass 1 sees the announcement's claim; pass 2 sees the news report's and the employee says it joins E1.
    let first = l.input("condense", None, vec![claims[1].clone()], vec![], "q1");
    let made = l.finish("condense_apply", &first, json!({"groups": [{"members": ["N1"], "claim": "Kimi K2.6 has a 256k context window", "contradicts": []}]})).unwrap();
    let host = made.emits[0].clone();
    assert_eq!(host.data["source_count"], 1);
    let second = l.input("condense", None, vec![claims[2].clone()], vec![host.clone()], "q1");
    let merged = l.finish("condense_apply", &second, json!({"groups": [{"members": ["E1", "N1"], "claim": "Kimi K2.6 has a 256k context window", "contradicts": []}]})).unwrap();
    assert_eq!(merged.emits.len(), 1);
    let again = &merged.emits[0];
    assert_eq!(again.id, host.id, "the group keeps its identity");
    assert_eq!(again.data["corroborates"].as_array().unwrap().len(), 2);
    assert_eq!((again.data["source_count"].as_u64(), again.data["score"].as_f64()), (Some(2), Some(1.0)), "0.7 and 0.5 are two roots: capped at 1");
    // An existing group the employee lists without any new claim is left alone.
    let third = l.input("condense", None, vec![], vec![host.clone()], "q1");
    let none = l.finish("condense_apply", &third, json!({"groups": [{"members": ["E1"], "claim": "x", "contradicts": []}]})).unwrap();
    assert!(none.emits.is_empty());
}

#[test]
fn extract_makes_claims_only_from_quotes_that_are_in_the_stored_page() {
    let l = lab(2);
    let q = l.question(0);
    let text = "Moonshot AI today released Kimi K2.6 on 2026-10-20. It supports a context window of 256k tokens.";
    let hash = l.page(text);
    let a = text.find("It supports").unwrap();
    let src = l.put("i_s1", "source", "analyze@1", Some(&q), json!({"index": 1, "title": "A", "relevancy": "High", "credibility": 0.7, "source_kind": "official", "one_line": "x", "source": "https://a.example/post", "notes": "", "relevance": 0.7, "page": "i_p1", "text_ref": hash,
        "claims": [
            {"text": "ok", "quote": "It supports a context window of 256k tokens.", "sub": "q1", "start": a, "end": a + 44},
            {"text": "stale span, same words", "quote": "Moonshot AI today released Kimi K2.6 on 2026-10-20.", "sub": "q1", "start": 3, "end": 9},
            {"text": "gone", "quote": "This sentence is not on the page at all.", "sub": "q1", "start": 0, "end": 5}
        ], "additional_sources": [], "records": []}));
    let input = l.input("extract", Some(src), vec![], vec![], "q1");
    let fin = l.code("extract_claims", &input).unwrap();
    assert_eq!(fin.emits.len(), 2);
    assert_eq!(fin.rejected.unwrap()["quote_not_found"], 1);
    for c in &fin.emits {
        let (s, e) = (c.data["locator"]["start"].as_u64().unwrap() as usize, c.data["locator"]["end"].as_u64().unwrap() as usize);
        assert_eq!(&text[s..e], c.data["quote"].as_str().unwrap());
    }
    // Ids come from the source and the offsets: the same input gives the same ids.
    let again = l.code("extract_claims", &input).unwrap();
    assert_eq!(fin.emits.iter().map(|c| c.data["id"].clone()).collect::<Vec<_>>(), again.emits.iter().map(|c| c.data["id"].clone()).collect::<Vec<_>>());
}

// ---- verify ----------------------------------------------------------------------------------------------

fn cclaim(l: &Lab, claims: &[Item], text: &str) -> Item {
    let q = l.question(0);
    let rows: Vec<Value> = claims
        .iter()
        .map(|c| json!({"claim_id": c.data["id"], "text": c.data["text"], "quote": c.data["quote"], "source": "https://a.example/post", "credibility": c.data["credibility"],
                        "text_ref": c.data["text_ref"], "start": c.data["locator"]["start"], "end": c.data["locator"]["end"], "source_item": c.data["source"]}))
        .collect();
    l.put("i_cc1", "cclaim", "cclaim@1", Some(&q), json!({"claim": text, "corroborates": rows, "source_count": 1, "score": 0.7, "stance": "supported", "sub": "q1"}))
}

#[test]
fn a_claim_whose_quotes_are_gone_from_the_stored_page_is_unsupported_without_a_call() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    let cc = cclaim(&l, &claims[..2], "Kimi K2.6 has a 256k context window");
    let input = l.input("verify", Some(cc.clone()), vec![], vec![], "q1");
    let p = l.prompt("verifier", &input);
    assert!(p.canned.is_none() && p.user.contains("[P1]"), "the passages are there: {}", p.user);
    assert!(p.system[0].text.contains("Use only the passages"));
    // The stored text changes under it.
    let stored = claims[0].data["text_ref"].as_str().unwrap();
    let path = l.rig.deps.store().unwrap().dir(&l.p.id).unwrap().join("sources").join(format!("{stored}.md"));
    std::fs::write(path, "Nothing of the sort is written here any more.").unwrap();
    let p = l.prompt("verifier", &input);
    let canned = p.canned.expect("no model is called");
    assert_eq!(canned["verdict"], "unsupported");
    let fin = l.finish("verify_finish", &input, canned).expect("the canned answer passes the rules");
    assert_eq!(fin.data["verdict"], "unsupported");
}

#[test]
fn verify_rules_make_a_verdict_name_its_passage_and_only_partial_may_correct() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    let cc = cclaim(&l, &claims[..2], "Kimi K2.6 has a 256k context window");
    let input = l.input("verify", Some(cc), vec![], vec![], "q1");
    let reply = |verdict: &str, used: Value, corrected: Value| json!({"reason": "because", "verdict": verdict, "corrected_claim": corrected, "used": used});
    // No passage named: told, for upheld and refuted alike.
    for v in ["upheld", "refuted", "partial"] {
        let e = l.finish("verify_finish", &input, reply(v, json!([]), Value::Null)).unwrap_err();
        assert_eq!(e.rule, "names_a_passage", "{v}");
        assert!(e.tell.contains("1 to 2"), "{}", e.tell);
    }
    // A passage that does not exist is the same as none.
    assert!(l.finish("verify_finish", &input, reply("upheld", json!([5]), Value::Null)).is_err());
    // Unsupported needs nothing.
    assert_eq!(l.finish("verify_finish", &input, reply("unsupported", json!([]), Value::Null)).unwrap().data["verdict"], "unsupported");
    // A correction on anything but partial is ignored and counted.
    let fin = l.finish("verify_finish", &input, reply("refuted", json!([1]), json!("Kimi K2.6 has a 32768 window"))).unwrap();
    assert!(fin.data.get("claim").is_none() && fin.rejected.unwrap()["corrected_claim_only_on_partial"] == 1);
    // Partial with a correction replaces the claim and keeps the original.
    let fin = l.finish("verify_finish", &input, reply("partial", json!([1, 1, 2]), json!("Kimi K2.6 has a 256k window, per the lab https://evil.example/x"))).unwrap();
    assert_eq!(fin.data["claim"], "Kimi K2.6 has a 256k window, per the lab");
    assert_eq!(fin.data["original_claim"], "Kimi K2.6 has a 256k context window");
}

#[test]
fn incomplete_verification_refuses_to_report_and_says_what_is_missing() {
    let l = lab(2);
    let (claims, _) = claims_world(&l, false);
    let _cc = cclaim(&l, &claims[..2], "Kimi K2.6 has a 256k context window");
    let input = l.input("collect", None, vec![], vec![], "");
    let err = l.code("validated_output", &input).unwrap_err();
    assert!(err.contains("verification did not finish") && err.contains("none of the 1 claims has a verdict"), "{err}");
    // One verdict is enough to go on; the missing one is counted.
    let q = l.question(0);
    l.put("i_cc2", "cclaim", "cclaim@1", Some(&q), json!({"claim": "Another", "corroborates": [], "source_count": 0, "score": 0.0, "stance": "supported", "sub": "q1", "verdict": "upheld", "reason": "ok"}));
    let fin = l.code("validated_output", &input).unwrap();
    let out = fin.output.unwrap();
    assert_eq!(out["claims"].as_array().unwrap().len(), 1);
    assert_eq!(out["coverage"]["claims"]["verdicts_missing"], 1);
    assert_eq!(out["coverage"]["blind_questions"], json!(["q2"]), "q1 has a verified claim, q2 has none");
}

// ---- thinking, checkin, report ------------------------------------------------------------------------

#[test]
fn the_lead_cannot_name_a_flagged_source_that_does_not_exist_and_must_say_what_to_research() {
    let l = lab(2);
    let input = l.input("decide", None, vec![], vec![], "");
    let ok = l.finish("lead_finish", &input, json!({"why": "x", "decision": "report", "gaps": [{"question": "short", "reason": "r"}, {"question": "What does the report say about long context?", "reason": "r"}], "focus": [1, 2, 9]})).unwrap();
    assert_eq!(ok.data["gaps"].as_array().unwrap().len(), 1, "a gap under ten characters is not a question");
    assert_eq!(ok.data["focus"], json!([]), "there are no flagged sources, so none can be chosen");
    let e = l.finish("lead_finish", &input, json!({"why": "x", "decision": "keep_researching", "gaps": [], "focus": []})).unwrap_err();
    assert_eq!(e.rule, "says_what_to_research");
}

#[test]
fn a_checkin_is_raised_while_a_round_is_left_and_not_after_the_last() {
    let l = lab(2);
    let q = l.question(0);
    l.put("i_d1", "decision", "lead_decision@1", Some(&q), json!({"why": "x", "decision": "keep_researching", "gaps": [], "focus": []}));
    let input = l.input("route", None, vec![], vec![], "");
    let fin = l.code("route_after_thinking", &input).unwrap();
    assert!(matches!(fin.effects.as_slice(), [Effect::InsertPhase { before: Some(b), phase }] if b == "ph_report" && phase.ty == "Checkin"), "{:?}", fin.effects);
    // The last round: no more research is offered, so no checkin.
    let mut last = input.clone();
    let mut plan = (*last.plan).clone();
    plan.limits.max_rounds = plan.plan_no;
    last.plan = Arc::new(plan);
    assert!(l.code("route_after_thinking", &last).unwrap().effects.is_empty());
    // A lead that says report with nothing flagged needs no checkin.
    l.put("i_d2", "decision", "lead_decision@1", Some(&q), json!({"why": "x", "decision": "report", "gaps": [], "focus": []}));
    assert!(l.code("route_after_thinking", &input).unwrap().effects.is_empty());
}

#[test]
fn a_section_writer_cites_only_claims_it_was_given_and_a_question_with_none_gets_the_fixed_line() {
    let l = lab(2);
    let q = l.question(0);
    let input = l.input("sections", Some(l.p.state().items_of_kind("question")[0].clone()), vec![], vec![], "q1");
    let p = l.prompt("section-writer", &input);
    assert_eq!(p.canned.as_ref().unwrap()["text"], research::report::NOTHING, "no verified claim, no call");
    let fin = l.finish("section_finish", &input, p.canned.unwrap()).expect("the fixed line passes");
    assert_eq!(fin.data["text"], research::report::NOTHING);
    l.put("i_cc1", "cclaim", "cclaim@1", Some(&q), json!({"claim": "Kimi K2.6 has a 256k window", "corroborates": [], "source_count": 2, "score": 0.9, "stance": "supported", "sub": "q1", "verdict": "upheld"}));
    let p = l.prompt("section-writer", &input);
    assert!(p.canned.is_none() && p.user.contains("[C1] Kimi K2.6 has a 256k window (score 0.90; 2 independent sources)"), "{}", p.user);
    let ok = l.finish("section_finish", &input, json!({"text": "It has 256k [C1]. Also see https://evil.example/x [C7].", "end": "END"})).unwrap();
    assert_eq!(ok.data["text"], "It has 256k [C1]. Also see.");
    assert_eq!(ok.data["sub"], "q1");
    let none = l.finish("section_finish", &input, json!({"text": "No markers here at all.", "end": "END"})).unwrap_err();
    assert_eq!(none.rule, "cites_claims");
    let only_bad = l.finish("section_finish", &input, json!({"text": "Claims [C9].", "end": "END"})).unwrap_err();
    assert_eq!(only_bad.rule, "cites_claims", "a marker that is not a given claim is removed, leaving nothing cited");
}

#[test]
fn an_auditor_that_forgets_a_sentence_or_a_sentence_with_no_passage_is_unsupported() {
    let l = lab(2);
    let hash = l.page("Moonshot AI today released Kimi K2.6 on 2026-10-20.");
    let batch = l.put("i_b1", "sentence_batch", "batch@1", None, json!({"batch": "1", "sentences": [
        {"n": 1, "where": "answer", "start": 0, "end": 10, "text": "One [C1].", "claims": [1], "refs": [format!("C1|{hash}|0|10")]},
        {"n": 2, "where": "answer", "start": 11, "end": 20, "text": "Two [C2].", "claims": [2], "refs": []},
        {"n": 3, "where": "answer", "start": 21, "end": 30, "text": "Three [C1].", "claims": [1], "refs": [format!("C1|{hash}|0|10")]}
    ]}));
    let input = l.input("audit", Some(batch), vec![], vec![], "");
    let p = l.prompt("auditor", &input);
    assert!(p.user.contains("passage [C1]") && p.user.contains("(no passage available)"), "{}", p.user);
    let fin = l.finish("audit_finish", &input, json!({"verdicts": [{"n": 1, "verdict": "supported", "reason": "yes"}, {"n": 2, "verdict": "supported", "reason": "the model is wrong to say so"}]})).unwrap();
    let v: Vec<(u64, &str)> = fin.data["verdicts"].as_array().unwrap().iter().map(|r| (r["n"].as_u64().unwrap(), r["verdict"].as_str().unwrap())).collect();
    assert_eq!(v, [(1, "supported"), (2, "unsupported"), (3, "unsupported")]);
}

// ---- search and fetch ----------------------------------------------------------------------------------------

/// A web whose answers the test writes.
struct Scripted(Mutex<Vec<(ChannelId, Result<Vec<SourceRef>, ChannelError>)>>);

impl Web for Scripted {
    fn channels(&self) -> Vec<ChannelId> {
        vec![ChannelId::Web, ChannelId::OpenAlex]
    }

    fn search(&self, channel: ChannelId, _q: &ChannelQuery, _c: &CancelToken) -> Result<Vec<SourceRef>, ChannelError> {
        let mut v = self.0.lock().unwrap();
        match v.iter().position(|(c, _)| *c == channel) {
            Some(i) => {
                let (_, r) = v.remove(i);
                r
            }
            None => Ok(Vec::new()),
        }
    }
}

fn lab_with_web(questions: usize, script: Vec<(ChannelId, Result<Vec<SourceRef>, ChannelError>)>, max_sources: u32) -> Lab {
    let mut plan = research_plan(questions);
    plan["limits"] = json!({"max_sources": max_sources});
    let p = Project::new("lab-web", Registry::builtin(), &plan);
    let w = World::load();
    let mut r = rig(&w, &p.home.0);
    let deps = Arc::new(research::ResearchDeps::new(p.home.0.join("projects"), p.home.0.join("output"), Arc::new(Scripted(Mutex::new(script))), r.pages.clone(), Arc::new(Dns(w)), r.memory.clone()));
    r.deps = deps;
    let tools = research::toolbox(&r.deps);
    p.handle.mutate(Mutation::SetGrants(state::Grants { net: Some(state::NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 60, used: 0 }) })).unwrap();
    Lab { p, rig: r, tools }
}

fn vector_with(l: &Lab, q: &str, queries: &[(&str, &str)]) -> Item {
    let rows: Vec<Value> = queries.iter().map(|(ch, text)| json!({"channel": ch, "query": text, "kind": "base", "since_year": null})).collect();
    l.put("i_v1", "vector", "attack_vectors@1", Some(q), json!({"question": "q", "today": "2026-10-23", "vectors": [], "unknown_names": [], "queries": rows}))
}

fn res(url: &str, title: &str) -> SourceRef {
    SourceRef::new(ChannelId::Web, url, title)
}

#[test]
fn a_search_drops_the_retracted_the_non_https_and_the_repeated_and_never_takes_one_source_twice() {
    let mut paper = SourceRef::new(ChannelId::OpenAlex, "https://doi.org/10.1/x", "A paper");
    paper.identity = Some(Identity::default().with_doi("10.1/x"));
    let mut same_paper = SourceRef::new(ChannelId::Web, "https://mirror.example/paper.pdf", "The same paper");
    same_paper.identity = Some(Identity::default().with_doi("10.1/X"));
    let mut retracted = res("https://r.example/a", "Retracted");
    retracted.retracted = true;
    let script = vec![
        (ChannelId::Web, Ok(vec![res("https://a.example/1?utm_source=x", "One"), retracted, res("http://plain.example/2", "Plain"), res("https://a.example/1", "One again"), same_paper])),
        (ChannelId::OpenAlex, Ok(vec![paper])),
    ];
    let l = lab_with_web(2, script, 40);
    let q = l.question(0);
    let v = vector_with(&l, &q, &[("web", "kimi"), ("open_alex", "kimi")]);
    let input = l.input("search", Some(v), vec![], vec![], "q1");
    let fin = l.code("search_sources", &input).unwrap();
    assert!(fin.tainted);
    let hits: Vec<&Item> = fin.emits.iter().filter(|i| i.kind == "hit").collect();
    let urls: Vec<&str> = hits.iter().map(|h| h.data["url"].as_str().unwrap()).collect();
    // Interleaved by rank: web #1, openalex #1, web #2...; the mirror of the paper is the same work.
    assert_eq!(urls, ["https://a.example/1?utm_source=x", "https://doi.org/10.1/x"], "{urls:?}");
    let rej = fin.rejected.unwrap();
    assert_eq!((rej["retracted"].as_u64(), rej["not_https"].as_u64(), rej["duplicate"].as_u64()), (Some(1), Some(1), Some(2)), "{rej}");
    let paper_hit = hits[1];
    assert_eq!((paper_hit.data["identity_confirmed"].as_bool(), paper_hit.data["identity"].as_str()), (Some(true), Some("doi:10.1/x")));
    // A second question that finds the same address does not take it again.
    let q2 = l.question(1);
    let v2 = l.put("i_v2", "vector", "attack_vectors@1", Some(&q2), json!({"question": "q", "today": "2026-10-23", "vectors": [], "unknown_names": [], "queries": [{"channel": "web", "query": "kimi", "kind": "base", "since_year": null}]}));
    for h in fin.emits.iter().filter(|i| i.kind == "hit") {
        l.p.handle.mutate(Mutation::PutItem(Box::new(h.clone()))).unwrap();
    }
    let l2 = lab_web_again(&l, vec![res("https://a.example/1", "One"), res("https://b.example/2", "Two")]);
    let input2 = l2.input("search", Some(v2), vec![], vec![], "q2");
    let fin2 = l2.code("search_sources", &input2).unwrap();
    let urls2: Vec<&str> = fin2.emits.iter().filter(|i| i.kind == "hit").map(|h| h.data["url"].as_str().unwrap()).collect();
    assert_eq!(urls2, ["https://b.example/2"], "the first question already has a.example/1 (tracking parameters ignored)");
}

/// The same project with a different scripted web (a fresh dependencies object, as after a restart).
fn lab_web_again(l: &Lab, results: Vec<SourceRef>) -> Lab {
    let w = World::load();
    let deps = Arc::new(research::ResearchDeps::new(l.p.home.0.join("projects"), l.p.home.0.join("output"), Arc::new(Scripted(Mutex::new(vec![(ChannelId::Web, Ok(results))]))), l.rig.pages.clone(), Arc::new(Dns(w)), l.rig.memory.clone()));
    let tools = research::toolbox(&deps);
    Lab { p: Project { home: Home(l.p.home.0.clone()), id: l.p.id.clone(), actor: None, handle: l.p.handle.clone(), events: l.p.events.clone(), registry: l.p.registry.clone() }, rig: Rig { deps, web: l.rig.web.clone(), pages: l.rig.pages.clone(), memory: l.rig.memory.clone(), output: l.rig.output.clone() }, tools }
}

#[test]
fn a_question_takes_its_share_of_the_quota_and_a_down_channel_is_a_note() {
    let many: Vec<SourceRef> = (1..=10).map(|i| res(&format!("https://s{i}.example/p"), &format!("S{i}"))).collect();
    let script = vec![(ChannelId::Web, Ok(many)), (ChannelId::OpenAlex, Err(ChannelError::RateLimited { channel: ChannelId::OpenAlex, retry_after: None }))];
    let l = lab_with_web(3, script, 6);
    let q = l.question(0);
    let v = vector_with(&l, &q, &[("web", "x"), ("open_alex", "x")]);
    let input = l.input("search", Some(v), vec![], vec![], "q1");
    let fin = l.code("search_sources", &input).unwrap();
    assert_eq!(fin.emits.iter().filter(|i| i.kind == "hit").count(), 2, "6 sources over 3 questions: 2 each");
    let notes: Vec<&str> = fin.emits.iter().filter(|i| i.kind == "note").map(|n| n.data["text"].as_str().unwrap()).collect();
    assert_eq!(notes, ["OpenAlex search rate limited for \"x\"."]);
}

#[test]
fn seeds_from_a_continuation_are_read_first_and_only_once_per_round() {
    let l = lab_with_web(1, vec![(ChannelId::Web, Ok(vec![res("https://s.example/found", "Found")]))], 10);
    let q = l.question(0);
    let mut plan = l.p.state().plan.clone();
    plan.seed_sources = vec!["https://seed.example/a".into(), "http://seed.example/plain".into(), "https://seed.example/b".into()];
    l.p.handle.mutate(Mutation::SetPlan(Box::new(plan))).unwrap();
    let v = vector_with(&l, &q, &[("web", "x")]);
    let input = l.input("search", Some(v.clone()), vec![], vec![], "q1");
    let fin = l.code("search_sources", &input).unwrap();
    let hits: Vec<(&str, &str)> = fin.emits.iter().filter(|i| i.kind == "hit").map(|h| (h.data["origin"].as_str().unwrap(), h.data["url"].as_str().unwrap())).collect();
    assert_eq!(hits, [("seed", "https://seed.example/a"), ("seed", "https://seed.example/b"), ("search", "https://s.example/found")]);
    assert_eq!(fin.rejected.unwrap()["seed_not_https"], 1);
    // A second task of the same round finds the seeds claimed.
    let again = l.code("search_sources", &input).unwrap();
    assert!(again.emits.iter().filter(|i| i.kind == "hit").all(|h| h.data["origin"] == "search"));
}

/// A reader that returns what the test says.
struct One(Mutex<Option<Fetched>>);

impl PageFetch for One {
    fn read(&self, url: &str, admit: Admit<'_>, _c: &CancelToken) -> Fetched {
        if let Err(w) = admit(url) {
            return Fetched::failed(url, Failure::Blocked, w);
        }
        self.0.lock().unwrap().take().unwrap_or_else(|| Fetched::failed(url, Failure::Unreachable, "the host could not be reached"))
    }
}

fn fetch_lab(f: Fetched) -> (Lab, Item) {
    let p = Project::new("lab-fetch", Registry::builtin(), &research_plan(1));
    let w = World::load();
    let r = rig(&w, &p.home.0);
    let deps = Arc::new(research::ResearchDeps::new(p.home.0.join("projects"), p.home.0.join("output"), r.web.clone(), Arc::new(One(Mutex::new(Some(f)))), Arc::new(Dns(w)), r.memory.clone()));
    let tools = research::toolbox(&deps);
    let l = Lab { p, rig: Rig { deps, ..r }, tools };
    l.p.handle.mutate(Mutation::SetGrants(state::Grants { net: Some(state::NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 60, used: 0 }) })).unwrap();
    let q = l.question(0);
    let hit = l.put("i_h1", "hit", "hit@1", Some(&q), json!({"url": "https://a.example/post", "title": "Hit title", "channel": "web", "origin": "search", "identity_confirmed": false, "retracted": false, "rank": 1, "question": "q1", "round": 1}));
    (l, hit)
}

fn read_page(f: Fetched) -> (Lab, Item) {
    let (l, hit) = fetch_lab(f);
    let input = l.input("fetch", Some(hit), vec![], vec![], "q1");
    let fin = l.code("fetch_page", &input).unwrap();
    assert_eq!(fin.emits.len(), 1);
    let page = fin.emits.into_iter().next().unwrap();
    (l, page)
}

fn fetched(text: &str) -> Fetched {
    Fetched { url: "https://a.example/post".into(), final_url: "https://a.example/post".into(), title: Some("View PDF".into()), text: text.into(), ..Default::default() }
}

#[test]
fn a_page_is_stored_numbered_and_titled_from_the_hit_when_the_extractor_kept_a_placeholder() {
    let body = format!("A [link](https://other.example/x) in a page. {}", "Words that make a page long enough to count. ".repeat(20));
    let mut f = fetched(&body);
    f.references = vec![PageRef { label: Some("1".into()), text: "A reference".into(), doi: Some("10.1/x".into()), arxiv_id: None, url: Some("http://plain.example/p".into()), year: Some(2025) }, PageRef { text: "no identity".into(), ..Default::default() }];
    f.meta = vec![("citation_publication_date".into(), "2026-10-01".into())];
    let (l, page) = read_page(f);
    assert_eq!((page.status, page.data["status"].as_str()), (ItemStatus::Done, Some("ok")));
    assert_eq!(page.data["title"], "Hit title", "'View PDF' is not a title");
    assert_eq!(page.data["date"], "2026-10-01");
    assert_eq!(page.data["credibility"].as_f64(), Some(0.7), "the ceiling of an official plain-web page");
    let stored = l.rig.deps.text(&l.p.id, page.data["text_ref"].as_str().unwrap()).unwrap();
    assert!(stored.starts_with("A link [L1] in a page.") && !stored.contains("http"));
    assert_eq!(l.rig.deps.links(&l.p.id, page.data["text_ref"].as_str().unwrap())[0].url, "https://other.example/x");
    let refs = page.data["references"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "a reference with no identity is not kept");
    assert_eq!((refs[0]["doi"].as_str(), refs[0]["url"].clone()), (Some("10.1/x"), Value::Null), "an http address is not kept");
    let st = l.p.state();
    l.p.handle.flush().unwrap();
    assert_eq!(st.metrics.fetch.ok, 1);
}

#[test]
fn what_cannot_be_read_is_an_item_with_a_reason_and_never_an_error_that_stops_the_run() {
    let long = "Words that make a page long enough to count. ".repeat(20);
    let cases: Vec<(Fetched, &str, ItemStatus)> = vec![
        (Fetched::failed("u", Failure::Robots, "robots.txt does not allow it"), "robots", ItemStatus::Skipped),
        (Fetched::failed("u", Failure::Http, "the server answered with an error"), "http", ItemStatus::Failed),
        (Fetched::failed("u", Failure::Unreachable, "the host could not be reached"), "unreachable", ItemStatus::Failed),
        (Fetched::failed("u", Failure::Unreadable, "no text"), "unreadable", ItemStatus::Skipped),
        (fetched("short"), "unreadable", ItemStatus::Skipped),
        (fetched(&"A thin page. ".repeat(20)), "thin", ItemStatus::Done),
        (fetched(&long), "ok", ItemStatus::Done),
    ];
    for (f, status, item) in cases {
        let (_, page) = read_page(f);
        assert_eq!((page.data["status"].as_str().unwrap(), page.status), (status, item));
        if item != ItemStatus::Done {
            assert!(page.error.is_some(), "{status}");
        }
    }
}

#[test]
fn a_hit_is_not_read_without_the_grant_or_past_the_quota() {
    let (l, hit) = fetch_lab(fetched(&"Words that make a page long enough to count. ".repeat(20)));
    l.p.handle.mutate(Mutation::SetGrants(state::Grants { net: None })).unwrap();
    let input = l.input("fetch", Some(hit.clone()), vec![], vec![], "q1");
    let p = l.code("fetch_page", &input).unwrap().emits.remove(0);
    assert_eq!((p.data["status"].as_str(), p.error.as_deref()), (Some("blocked"), Some("the plan was not approved, so no page is read")));
    l.p.handle.mutate(Mutation::SetGrants(state::Grants { net: Some(state::NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 0, used: 0 }) })).unwrap();
    let p = l.code("fetch_page", &input).unwrap().emits.remove(0);
    assert_eq!(p.data["status"], "over_quota");
    assert_eq!(l.rig.pages.requests().len(), 0, "nothing was requested");
}

// ---- subjects: a claim about one thing never lands under a question about another ---------------------------------
//
// Run m6 (the prompt about Kimi, DeepSeek and Qwen 4), recorded in tests/fixtures/research/m6_subjects.json:
// a DeepSeek "V3.2" fact was read for the Qwen 4 question, condensed with Qwen 4 claims, and the report called the
// model "V3.2 (Qwen 4)".

const M6: &str = include_str!("fixtures/research/m6_subjects.json");

fn m6() -> Value {
    serde_json::from_str(M6).expect("the recorded fixture")
}

fn m6_lab() -> Lab {
    let m = m6();
    lab_for(&json!({"objective": m["objective"], "questions": m["questions"], "fields": [{"key": "model_name"}]}))
}

fn m6_question(m: &Value, sub: &str) -> String {
    let n: usize = sub.trim_start_matches('q').parse().unwrap();
    m["questions"][n - 1].as_str().unwrap().to_string()
}

const OFF_SUBJECT: usize = 12;

#[test]
fn the_recorded_m6_claims_that_leaked_across_subjects_are_held_back_and_the_rest_stay() {
    use research::subject::Subjects;
    let m = m6();
    let subjects = Subjects::of(m["objective"].as_str().unwrap());
    let mut off: Vec<(String, String)> = Vec::new();
    let mut kept = 0;
    for c in m["claims"].as_array().unwrap() {
        let sub = c["sub"].as_str().unwrap();
        let scope = subjects.scope(&m6_question(&m, sub));
        let page = subjects.page(c["title"].as_str().unwrap(), c["url"].as_str().unwrap(), "");
        if subjects.claim_fits(&scope, c["text"].as_str().unwrap(), c["quote"].as_str().unwrap(), &page) {
            kept += 1;
        } else {
            off.push((sub.to_string(), c["text"].as_str().unwrap().to_string()));
        }
    }
    // The two DeepSeek facts read off api-docs.deepseek.com for the Qwen question (q5) are the leak.
    for text in ["V3.2 level performance [GPT-5].", "V3.2-Speciale attains gold-level results in IMO, CMO, ICPC World Finals & IOI 2025."] {
        assert!(off.contains(&("q5".to_string(), text.to_string())), "{text} must not stay under q5: {off:?}");
    }
    // DeepSeek pages read for the Kimi question (q6) are held back the same way.
    assert!(off.iter().any(|(s, t)| s == "q6" && t.contains("DeepSeek-V3")), "{off:?}");
    // A Qwen claim under the Qwen question and a Kimi comparison on a Qwen page stay.
    assert!(!off.iter().any(|(_, t)| t.contains("Qwen 4 is in training")), "{off:?}");
    assert!(!off.iter().any(|(_, t)| t.contains("behind Kimi K2.5")), "{off:?}");
    assert_eq!((off.len(), kept), (OFF_SUBJECT, 104 - OFF_SUBJECT), "{off:?}");
}

#[test]
fn extract_drops_a_claim_read_off_another_subjects_page_before_it_can_be_condensed() {
    let l = m6_lab();
    let m = m6();
    let rows = m["mixed"]["rows"].as_array().unwrap();
    let (deepseek, qwen) = (&rows[0], &rows[2]);
    assert!(deepseek["source"].as_str().unwrap().contains("deepseek.com"), "the recorded DeepSeek row");
    let text = format!("{} {}", deepseek["quote"].as_str().unwrap(), qwen["quote"].as_str().unwrap());
    let hash = l.page(&text);
    let at = |q: &str| text.find(q).unwrap();
    let claim = |row: &Value| {
        let q = row["quote"].as_str().unwrap();
        json!({"text": row["text"], "quote": q, "sub": "q5", "start": at(q), "end": at(q) + q.len()})
    };
    let q5 = l.question(4);
    let mk = |id: &str, url: &str, title: &str, row: &Value| {
        l.put(id, "source", "analyze@1", Some(&q5), json!({"index": 1, "title": title, "relevancy": "High", "credibility": 0.7, "source_kind": "official", "one_line": "x", "source": url, "notes": "",
            "relevance": 0.7, "page": "i_p", "text_ref": hash, "claims": [claim(row)], "additional_sources": [], "records": []}))
    };
    let from_deepseek = mk("i_sd", deepseek["source"].as_str().unwrap(), "DeepSeek-V3.2 release", deepseek);
    let from_qwen = mk("i_sq", qwen["source"].as_str().unwrap(), "How to deploy Qwen 4", qwen);
    let fin = l.code("extract_claims", &l.input("extract", Some(from_deepseek.clone()), vec![], vec![], "q5")).unwrap();
    assert!(fin.emits.is_empty(), "a DeepSeek fact has no place under the Qwen 4 question");
    assert_eq!(fin.rejected.unwrap()["claim_other_subject"], 1);
    let fin = l.code("extract_claims", &l.input("extract", Some(from_qwen), vec![], vec![], "q5")).unwrap();
    assert_eq!(fin.emits.len(), 1, "the Qwen 4 claim stays");
    // A question that names no subject holds nothing apart.
    let fin = l.code("extract_claims", &l.input("extract", Some(from_deepseek), vec![], vec![], "q4")).unwrap();
    assert_eq!(fin.emits.len(), 1);
}

#[test]
fn condense_never_merges_claims_of_different_subjects_whatever_the_employee_grouped() {
    let l = m6_lab();
    let m = m6();
    let rows = m["mixed"]["rows"].as_array().unwrap();
    let q5 = l.question(4);
    let hash = l.page("x");
    let mut claims = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let sid = format!("i_s{i}");
        l.put(&sid, "source", "analyze@1", Some(&q5), json!({"index": i + 1, "title": "t", "relevancy": "High", "credibility": r["credibility"], "source_kind": "news", "one_line": "x", "source": r["source"], "notes": "",
            "claims": [], "additional_sources": [], "records": [], "relevance": 0.7, "page": "i_p", "text_ref": hash}));
        claims.push(l.put(&format!("i_c{i}"), "claim", "claim@1", Some(&q5), json!({"id": format!("c_{i}"), "text": r["text"], "quote": r["quote"], "locator": {"doc": hash, "start": 0, "end": 0},
            "source": sid, "sub": "q5", "credibility": r["credibility"], "status": "new", "text_ref": hash})));
    }
    let input = l.input("condense", None, claims, vec![], "q5");
    // The recorded group: the DeepSeek row and the two Qwen rows as one claim, in the employee's mixed words.
    let mixed = m["mixed"]["claim"].as_str().unwrap();
    let reply = json!({"groups": [{"members": ["N1", "N2", "N3"], "claim": mixed, "contradicts": []}]});
    let fin = l.finish("condense_apply", &input, reply).unwrap();
    let cc: Vec<&Item> = fin.emits.iter().filter(|i| i.kind == "cclaim").collect();
    assert_eq!(cc.len(), 2, "one claim per subject");
    for c in &cc {
        let hosts: std::collections::BTreeSet<bool> = c.data["corroborates"].as_array().unwrap().iter().map(|r| r["source"].as_str().unwrap().contains("deepseek")).collect();
        assert_eq!(hosts.len(), 1, "a group holds one subject's claims: {:?}", c.data["corroborates"]);
        assert_ne!(c.data["claim"], mixed, "the mixed sentence names a subject its first group does not hold");
    }
    let rej = fin.rejected.unwrap();
    assert_eq!((rej["group_split_by_subject"].as_u64(), rej["sentence_other_subject"].as_u64()), (Some(1), Some(1)));
}

#[test]
fn a_verifiers_correction_cannot_bring_in_another_subject() {
    let l = m6_lab();
    let q5 = l.question(4);
    let text = "Gold-Medal Performance: V3.2-Speciale attains gold-level results in IMO.";
    let hash = l.page(text);
    let row = json!({"claim_id": "c_1", "text": "V3.2-Speciale attains gold-level results.", "quote": text, "source": "https://api-docs.deepseek.com/news/news251201/", "credibility": 0.7,
        "text_ref": hash, "start": 0, "end": text.len(), "source_item": "i_s1"});
    let cc = l.put("i_cc1", "cclaim", "cclaim@1", Some(&q5), json!({"claim": "V3.2-Speciale attains gold-level results.", "corroborates": [row], "source_count": 1, "score": 0.7, "stance": "supported", "sub": "q5"}));
    let input = l.input("verify", Some(cc), vec![], vec![], "q5");
    let reply = |corrected: &str| json!({"reason": "r", "verdict": "partial", "corrected_claim": corrected, "used": [1]});
    let fin = l.finish("verify_finish", &input, reply("Qwen 4 attains gold-level results in IMO.")).unwrap();
    assert!(fin.data.get("claim").is_none(), "the claim stays as it was");
    assert_eq!(fin.rejected.unwrap()["correction_other_subject"], 1);
    let fin = l.finish("verify_finish", &input, reply("V3.2-Speciale attains gold-level results in IMO only.")).unwrap();
    assert_eq!(fin.data["claim"], "V3.2-Speciale attains gold-level results in IMO only.", "a narrower claim about the same thing is fine");
}

#[test]
fn a_section_writer_sees_only_claims_that_do_not_belong_to_another_subject() {
    let l = m6_lab();
    let q5 = l.question(4);
    let put = |id: &str, claim: &str| {
        l.put(id, "cclaim", "cclaim@1", Some(&q5), json!({"claim": claim, "corroborates": [], "source_count": 2, "score": 0.9, "stance": "supported", "sub": "q5", "verdict": "upheld"}))
    };
    put("i_a", "Qwen 4 is in training and has no release date.");
    put("i_b", "DeepSeek-V3.2 attains gold-level results in IMO.");
    put("i_c", "No benchmarks have been published yet.");
    let input = l.input("sections", Some(l.p.state().items_of_kind("question")[4].clone()), vec![], vec![], "q5");
    let p = l.prompt("section-writer", &input);
    assert!(p.user.contains("Qwen 4 is in training") && p.user.contains("No benchmarks have been published"), "{}", p.user);
    assert!(!p.user.contains("DeepSeek-V3.2 attains"), "{}", p.user);
    // Nor can the section cite it: the claim number was never given.
    let n_b = research::report::verified_claims(&l.p.state()).iter().find(|c| c.id == "i_b").unwrap().n;
    let reply = json!({"text": format!("DeepSeek attains gold [C{n_b}]."), "end": "END"});
    assert_eq!(l.finish("section_finish", &input, reply).unwrap_err().rule, "cites_claims");
}
