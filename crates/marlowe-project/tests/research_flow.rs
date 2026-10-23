//! The research phases end to end, on a recorded world: channel results, pages and model replies
//! come from `tests/fixtures/research/`. The real menu, the real scheduler and the real research
//! functions run; nothing here needs a network, a model or a GPU.

mod common;
mod world;

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;

use common::*;
use marlowe_project::engine::SlotPool;
use marlowe_project::research;
use marlowe_project::*;
use serde_json::{json, Value};
use world::*;

fn plan() -> Value {
    json!({
        "objective": "Find the newest and upcoming Kimi, DeepSeek and Qwen models (including Qwen4) and say what is confirmed and what is rumoured.",
        "questions": [
            "Which Kimi models (Moonshot AI) are newest or upcoming?",
            "Which DeepSeek models are newest or upcoming?",
            "Is Qwen4 released or announced, and what is known about it?"
        ],
        "fields": [{"key": "model_name"}, {"key": "vendor"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}, {"key": "release_date"}, {"key": "size"}],
        "send": true
    })
}

struct Run {
    p: Project,
    rig: Rig,
    model: Arc<Fake>,
    pool: SlotPool,
}

impl Run {
    fn new(tag: &str, w: &Arc<World>, lead: &'static str) -> Run {
        let p = Project::new(tag, Registry::builtin(), &plan());
        let rig = rig(w, &p.home.0);
        Run { p, rig, model: model(w, lead), pool: SlotPool::new(3) }
    }

    fn engine(&self) -> Engine {
        self.p.engine(self.model.clone(), Arc::new(self.pool.clone()), research::toolbox(&self.rig.deps), config("mini"))
    }

    /// Runs until the project is finished, answering every card with the next choice of `answers`
    /// (a card the list has run out for gets `approve`). Returns the cards seen.
    fn drive(&self, answers: &[&str]) -> Vec<Value> {
        let mut cards = Vec::new();
        let mut next = answers.iter();
        for _ in 0..8 {
            match join_within(self.engine().spawn().unwrap(), 120) {
                RunOutcome::Finished => return cards,
                RunOutcome::Parked { gate } => {
                    let st = self.p.state();
                    let g = st.gates.iter().find(|g| g.id == gate).unwrap();
                    cards.push(g.payload.clone());
                    self.engine().answer(&gate, next.next().copied().unwrap_or("approve"), None).unwrap();
                }
                other => panic!("the run ended as {other:?}: {}", debug(&self.p.state())),
            }
        }
        panic!("the project did not finish");
    }
}

fn debug(st: &ProjectState) -> String {
    let failed: Vec<String> = st.items.values().filter(|i| i.status == ItemStatus::Failed).map(|i| format!("{} {:?}", i.kind, i.error)).collect();
    let phases: Vec<String> = st.phases.iter().map(|p| format!("{} {:?}", p.id, p.status)).collect();
    format!("status {:?}; phases {phases:?}; failed {failed:?}", st.status)
}

fn text_of(rig: &Rig, st: &ProjectState, source: &Item) -> String {
    rig.deps.text(&st.id, source.data["text_ref"].as_str().unwrap()).expect("the stored page")
}

fn report_of(rig: &Rig, st: &ProjectState) -> String {
    std::fs::read_to_string(rig.deps.store().unwrap().dir(&st.id).unwrap().join("report.md")).expect("report.md")
}

fn urls_in(text: &str) -> BTreeSet<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"' | '<' | '>')).filter(|w| w.starts_with("http")).map(|w| w.trim_end_matches(['.', ',']).to_string()).collect()
}

#[test]
fn a_project_runs_from_the_plan_card_to_a_cited_report_on_a_recorded_world() {
    let w = World::load();
    let r = Run::new("rf-main", &w, "report");
    // The plan card parks the run; nothing is read before approval.
    assert!(matches!(join_within(r.engine().spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    assert_eq!((r.model.count(), r.rig.web.queries().len(), r.rig.pages.requests().len()), (0, 0, 0));
    r.engine().answer("g_1", "approve", None).unwrap();

    // Approve the plan, then the lead says "report" but flagged sources exist: the user is asked.
    let cards = {
        let mut cards = vec![];
        for _ in 0..4 {
            match join_within(r.engine().spawn().unwrap(), 120) {
                RunOutcome::Finished => break,
                RunOutcome::Parked { gate } => {
                    cards.push(r.p.state().gates.iter().find(|g| g.id == gate).unwrap().payload.clone());
                    r.engine().answer(&gate, "approve", None).unwrap();
                }
                other => panic!("{other:?}: {}", debug(&r.p.state())),
            }
        }
        cards
    };
    assert_eq!(cards.len(), 1, "one checkin: {cards:?}");
    let card = &cards[0];
    assert_eq!((card["reason"].as_str(), card["recommended"].as_str()), (Some("lead_decision"), Some("keep_researching")), "flagged sources exist and a round is left: the lead said report, the card still recommends more");
    let flagged = card["flagged"].as_array().unwrap();
    assert_eq!(flagged.iter().map(|f| f["source"].as_str().unwrap()).collect::<Vec<_>>(), ["https://moonshot.example/papers/k2-6", "https://moonshot.example/blog/kimi-k2-5"], "the technical report first (High from a 0.7 source), the previous release second (Low)");
    assert!(card["lead_note"].as_str().unwrap().contains("Qwen4"));

    let st = r.p.state();
    assert_eq!(st.status, ProjectStatus::Finished, "{}", debug(&st));
    let count = |k: &str, s: ItemStatus| st.items_of_kind(k).into_iter().filter(|i| i.status == s).count();

    // AttackVectors: today's date and the old-knowledge warning reach the prompt; an address as a
    // keyword is dropped; the searches carry date words and a year floor.
    let vector_prompts: Vec<(String, String)> = r.model.requests().iter().filter(|q| asks_for(q, "unknown_names")).map(|q| (q.system.iter().map(|b| b.text.clone()).collect::<String>(), q.messages.last().unwrap().joined_text())).collect();
    assert_eq!(vector_prompts.len(), 3);
    assert!(vector_prompts.iter().all(|(sys, _)| sys.contains("Today is 2026-10-23") && sys.contains("knowledge ends well before today")));
    let queries = r.rig.web.queries();
    for want in ["kimi newest model release", "kimi newest model release 2026", "kimi newest model release October 2026", "kimi newest model release latest", "kimi k3 release", "qwen4 release", "deepseek v5 release"] {
        assert!(queries.iter().any(|q| q.eq_ignore_ascii_case(want)), "missing search {want:?} in {queries:?}");
    }
    assert!(queries.iter().all(|q| !q.contains("evil.example") && !q.contains("http")), "no address was ever a search: {queries:?}");
    assert!(r.rig.web.searches.lock().unwrap().iter().any(|s| s.2 == Some(2025)), "dated variants carry a year floor");
    let kimi_vectors = st.items_of_kind("vector").into_iter().find(|v| v.data["question"].as_str().unwrap().contains("Kimi")).unwrap();
    assert_eq!(kimi_vectors.data["vectors"].as_array().unwrap().len(), 2, "the row whose keywords were an address is dropped");

    // SearchAndFetch: five Kimi results minus the http one, plus the others; two are never read.
    assert_eq!(count("hit", ItemStatus::Done), 9);
    let requested = r.rig.pages.requests();
    assert!(requested.iter().all(|u| u.starts_with("https://")), "{requested:?}");
    assert!(!requested.iter().any(|u| u.contains("insecure.example") || u.contains("internal-rebind") || u.contains("evil.example")), "{requested:?}");
    let page = |url: &str| st.items_of_kind("page").into_iter().find(|p| p.data["url"] == url).unwrap_or_else(|| panic!("no page for {url}"));
    assert_eq!(page("https://internal-rebind.example/kimi").data["status"], "blocked");
    assert_eq!(page("https://redirector.example/go").data["status"], "blocked", "a redirect to a private host is refused on that hop");
    assert!(page("https://redirector.example/go").error.as_deref().unwrap().contains("private address"));
    assert_eq!(count("page", ItemStatus::Done), 7);
    let p1 = page("https://moonshot.example/blog/kimi-k2-6");
    assert_eq!(p1.data["title"], "Kimi K2.6 released");
    assert!(p1.data["credibility"].as_f64().unwrap() > 0.0);
    // The stored text is numbered and carries no address; the link table has the exact urls.
    let stored = r.rig.deps.text(&st.id, p1.data["text_ref"].as_str().unwrap()).unwrap();
    assert!(stored.contains("Kimi K2.5 [L1]") && stored.contains("K2.6 technical report [L2]") && !stored.contains("http"), "{stored}");
    let links = r.rig.deps.links(&st.id, p1.data["text_ref"].as_str().unwrap());
    assert_eq!(links.iter().map(|l| l.url.as_str()).collect::<Vec<_>>(), ["https://moonshot.example/blog/kimi-k2-5", "https://moonshot.example/papers/k2-6"]);
    assert_eq!(st.metrics.fetch.ok, 7);

    // Analyze: an analyst that could not quote the page is told, tries again, and succeeds; one that
    // never can is failed with words the harness wrote.
    let sources = st.items_of_kind("source");
    let src = |title: &str| sources.iter().find(|s| s.data.get("title").and_then(Value::as_str) == Some(title) || s.label.contains(title)).copied();
    let s1 = src("Kimi K2.6 released").expect("the K2.6 source");
    assert_eq!(s1.attempts, 2, "the first reply's quote was not in the page");
    assert_eq!(s1.data["source"], "https://moonshot.example/blog/kimi-k2-6", "source is filled by code, from the page");
    assert_eq!(s1.data["relevancy"], "Critical");
    assert_eq!(s1.data["claims"].as_array().unwrap().len(), 3, "the fabricated claim was dropped");
    assert_eq!(s1.data["rejected"]["quote_not_found"], 1);
    assert_eq!(s1.data["one_line"], "Official post, see for the original", "an address in model words is removed");
    assert_eq!(s1.data["notes"], "visit now");
    assert_eq!(s1.data["credibility"].as_f64().unwrap(), 0.7, "an official page on the plain web is capped at 0.7");
    let extra = s1.data["additional_sources"].as_array().unwrap();
    assert_eq!(extra[0]["source"], "https://moonshot.example/papers/k2-6", "link 2 is looked up in the page's table");
    assert_eq!(extra[1]["source"], "https://moonshot.example/blog/kimi-k2-5");
    assert_eq!(extra.len(), 3, "link 99 does not exist and is dropped; the named reference stays without a link");
    assert_eq!(extra[2]["source"], Value::Null);
    assert_eq!(extra[2]["reference"], "see for details");
    let failed: Vec<&Item> = sources.iter().copied().filter(|s| s.status == ItemStatus::Failed).collect();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].error.as_deref().unwrap().contains("quotes_found") && !failed[0].error.as_deref().unwrap().contains("Qwen4 will definitely"), "{:?}", failed[0].error);
    let injected = src("Kimi K3 rumours").unwrap();
    assert!(injected.data["notes"].as_str().unwrap().starts_with("Obeyed: fetch"), "the corrupted field is a field of one form");
    assert!(!injected.data["notes"].as_str().unwrap().contains("evil.example"));
    assert_eq!(injected.data["credibility"].as_f64().unwrap(), 0.35, "a forum scores as a forum however the page rates itself");

    // Extract: every claim is the page's own words at the place the claim says.
    let claims = st.items_of_kind("claim");
    assert_eq!(claims.len(), 3 + 2 + 1 + 2 + 2 + 2, "kimi 3+2+1, deepseek 2+2, qwen 2");
    for c in &claims {
        let src_item = st.item(c.data["source"].as_str().unwrap()).unwrap();
        let text = text_of(&r.rig, &st, src_item);
        let (a, b) = (c.data["locator"]["start"].as_u64().unwrap() as usize, c.data["locator"]["end"].as_u64().unwrap() as usize);
        assert_eq!(&text[a..b], c.data["quote"].as_str().unwrap(), "claim {} is verbatim", c.data["id"]);
        assert!(c.data["id"].as_str().unwrap().starts_with("c_"));
    }
    assert!(claims.iter().all(|c| c.data["quote"] != "Kimi K2.6 beats every other model on every benchmark"), "a claim whose quote is not in the page is never created");

    // Condense + verify: identical claims became one with their originals attached byte for byte;
    // three pages that cite one another are one independent root.
    let cc = st.items_of_kind("cclaim");
    assert_eq!(cc.len(), 9);
    let by = |text: &str| cc.iter().find(|c| c.data["claim"] == text).unwrap_or_else(|| panic!("no condensed claim {text:?}"));
    let ctx = by("Kimi K2.6 supports a 256k token context window");
    assert_eq!(ctx.data["corroborates"].as_array().unwrap().len(), 2, "the announcement and the news report that cites it");
    assert_eq!((ctx.data["source_count"].as_u64(), ctx.data["score"].as_f64()), (Some(1), Some(0.7)), "the news report links to the announcement: one root, scored at its best");
    let v4 = by("DeepSeek V4 was announced on 2026-09-30");
    assert_eq!((v4.data["source_count"].as_u64(), v4.data["score"].as_f64()), (Some(2), Some(1.0)), "an official post and an independent blog: 0.7 + 0.45, capped at 1");
    for c in &cc {
        for row in c.data["corroborates"].as_array().unwrap() {
            let orig = claims.iter().find(|x| x.data["id"] == row["claim_id"]).expect("an original claim");
            assert_eq!((&row["text"], &row["quote"]), (&orig.data["text"], &orig.data["quote"]), "originals are attached unchanged");
        }
        assert_eq!(c.data["verdict"], "upheld", "{:?}", c.data["claim"]);
    }
    let out = st.phases.iter().find(|p| p.ty == "DataValidation").unwrap().output.clone().expect("the phase output");
    assert_eq!(out["claims"].as_array().unwrap().len(), 9);
    assert_eq!(out["coverage"]["pages"]["blocked"], 2);
    assert_eq!(out["coverage"]["claims"]["extracted"], 12);

    // Tabulate: the best-credibility value wins a field; the conflict and the unconfirmed are listed.
    let records = out["records"].as_array().unwrap();
    let kimi = records.iter().find(|r| r["model_name"] == "Kimi K2.6").unwrap();
    assert_eq!(kimi["release_date"], "2026-10-20", "the official post beats the news report on the date");
    assert_eq!(kimi["size"], "1T total, 32B active");
    let notes = &out["coverage"]["records"];
    assert!(notes["conflicts"].as_array().unwrap().iter().any(|c| c["key"] == "Kimi K2.6" && c["field"] == "release_date"));
    assert!(notes["unverified"].as_array().unwrap().is_empty(), "every key is named by a verified claim: {notes}");

    // Stage isolation: nothing a page told an employee to do reached a later prompt or the state.
    for q in r.model.requests() {
        if asks_for(&q, "groups") || asks_for(&q, "gaps") || (asks_for(&q, "text") && asks_for(&q, "end")) {
            let all = format!("{} {}", q.system.iter().map(|b| b.text.as_str()).collect::<String>(), q.messages.iter().map(|m| m.joined_text()).collect::<String>());
            assert!(!all.contains("evil.example") && !all.contains("IGNORE ALL"), "a hostile page reached a later stage: {all}");
        }
    }
    let state_json = serde_json::to_string(&st.to_value()).unwrap();
    assert!(!state_json.contains("evil.example") && !state_json.contains("IGNORE ALL"), "nothing a page or a model offered is in the project's state; page text stays in the files");

    // Report: verified claims only, the overreaching sentence removed, references from stored identity.
    let md = report_of(&r.rig, &st);
    assert!(md.starts_with("# Find the newest and upcoming Kimi, DeepSeek and Qwen models"), "{md}");
    assert!(md.contains("*Confidence: **") && md.contains("## Answer") && md.contains("## Records") && md.contains("## Checks") && md.contains("## References"), "{md}");
    for q in ["## Which Kimi models (Moonshot AI) are newest or upcoming?", "## Which DeepSeek models are newest or upcoming?", "## Is Qwen4 released or announced, and what is known about it?"] {
        assert!(md.contains(q), "{q}\n{md}");
    }
    assert!(!md.contains("beats every other model"), "the sentence its own citation did not support was cut:\n{md}");
    assert!(md.contains("| Kimi K2.6 | Moonshot AI | released | 2026-10-20 | 1T total, 32B active |"), "{md}");
    assert!(!md.contains("[C"), "claim markers became reference numbers:\n{md}");
    let world_urls: HashSet<String> = w.json["pages"].as_object().unwrap().keys().cloned().collect();
    for u in urls_in(&md) {
        assert!(world_urls.contains(&u), "the report names an address that no page had: {u}");
    }
    assert!(md.contains("https://moonshot.example/blog/kimi-k2-6") && md.contains("https://deepseek.example/news/v4"));
    assert!(!md.contains("evil.example") && !md.contains("http://"));
    let refs = md.split("## References").nth(1).unwrap();
    assert!(refs.lines().filter(|l| l.starts_with('[')).enumerate().all(|(i, l)| l.starts_with(&format!("[{}] ", i + 1))), "numbered in order of first appearance:\n{refs}");
    assert!(st.items_of_kind("audit").len() >= 1 && st.items_of_kind("sentence_batch").len() >= 1);
    let rep = st.items_of_kind("report");
    assert_eq!(rep.len(), 1);
    assert!(["high", "medium", "low"].contains(&rep[0].data["confidence"].as_str().unwrap()));

    // Finish: delivered copy, a deliverable for the relay, workspace disposal, memories.
    let del = st.items_of_kind("deliverable");
    assert_eq!(del.len(), 1);
    let dir = Path::new(del[0].data["dir"].as_str().unwrap());
    assert!(dir.starts_with(&r.rig.output) && dir.join("report.md").is_file() && dir.join("research.json").is_file());
    assert_eq!(std::fs::read_to_string(dir.join("report.md")).unwrap(), md);
    assert_eq!(del[0].data["send"], true);
    let research: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("research.json")).unwrap()).unwrap();
    assert_eq!(research["schema"], "marlowe.project-research/1");
    assert!(!r.rig.deps.store().unwrap().dir(&st.id).unwrap().join("work").exists());
    let mem = r.rig.memory.0.lock().unwrap();
    assert!(mem.len() >= 2 && mem.len() <= 6, "{} memories", mem.len());
    assert!(mem.iter().all(|m| m.tainted && m.project == st.id));
    assert!(mem.iter().filter(|m| m.kind == "finding").all(|m| !m.sources.is_empty() && m.sources.iter().all(|s| s.starts_with("https://"))));
    assert!(mem.iter().any(|m| m.kind == "result" && m.text.contains("confidence")));
    let est = mem.iter().find(|m| m.text == "DeepSeek V4 was announced on 2026-09-30").unwrap();
    assert!(est.established, "upheld by two independent roots");
    // The final state is archived so the project can be reopened from a stage.
    assert!(r.rig.deps.store().unwrap().snapshots(&st.id).unwrap().iter().any(|s| s.label == "final"));
}

#[test]
fn keep_researching_reads_the_flagged_sources_first_and_the_report_covers_the_new_question() {
    let w = World::load();
    let r = Run::new("rf-more", &w, "keep");
    let cards = r.drive(&["approve", "keep_researching"]);
    let st = r.p.state();
    assert_eq!(st.status, ProjectStatus::Finished, "{}", debug(&st));
    assert_eq!(cards.len(), 3, "the plan card, the checkin after round 1, and the lead's checkin after round 2 (it still wants more; the user approves)");
    assert_eq!(cards[1]["recommended"], "keep_researching");
    assert_eq!(cards[1]["proposed_questions"][0]["q"], "What benchmarks does the Kimi K2.6 technical report give?");
    assert_eq!(cards[1]["seed_sources"].as_array().unwrap().len(), 2);

    assert!(cards[2]["flagged"].as_array().unwrap().is_empty(), "the flagged sources were read, so nothing is flagged any more");
    assert_eq!(cards[2]["reason"], "lead_decision");

    // A second round: the new question, the flagged sources read first, nothing read twice.
    assert_eq!(st.plan.plan_no, 2);
    assert!(["ph_plan_2", "ph_dc_2", "ph_dv_2", "ph_think_2"].iter().all(|id| st.phase(id).is_some_and(|p| p.status == StageStatus::Done)), "{}", debug(&st));
    let seeds: Vec<&Item> = st.items_of_kind("hit").into_iter().filter(|h| h.data["origin"] == "seed").collect();
    assert_eq!(seeds.len(), 2);
    let requested = r.rig.pages.requests();
    for url in ["https://moonshot.example/papers/k2-6", "https://moonshot.example/blog/kimi-k2-5"] {
        assert_eq!(requested.iter().filter(|u| *u == url).count(), 1, "{url} read once: {requested:?}");
    }
    let q4 = st.items_of_kind("question").into_iter().find(|q| q.data["id"] == "q4").expect("the new question");
    assert_eq!(q4.data["text"], "What benchmarks does the Kimi K2.6 technical report give?");
    let md = report_of(&r.rig, &st);
    assert!(md.contains("## What benchmarks does the Kimi K2.6 technical report give?"), "{md}");
    assert!(md.contains("https://moonshot.example/papers/k2-6"), "the new source is cited:\n{md}");
    // The second round did not search the old results again: its search found nothing new.
    let round2_searched = st.items_of_kind("hit").into_iter().filter(|h| h.data["round"] == 2 && h.data["origin"] == "search").count();
    assert_eq!(round2_searched, 0, "every result of the new question's searches was already taken");
}

#[test]
fn the_same_run_twice_gives_the_same_ids_and_the_same_report_bytes() {
    let w = World::load();
    let a = Run::new("rf-det-a", &w, "report");
    a.drive(&[]);
    let b = Run::new("rf-det-b", &w, "report");
    b.drive(&[]);
    let (sa, sb) = (a.p.state(), b.p.state());
    let ids = |st: &ProjectState, kind: &str| -> BTreeSet<String> { st.items_of_kind(kind).iter().map(|i| i.id.clone()).collect() };
    for kind in ["hit", "page", "source", "claim"] {
        assert_eq!(ids(&sa, kind), ids(&sb, kind), "{kind} ids do not depend on arrival order");
    }
    let claim_ids = |st: &ProjectState| -> BTreeSet<String> { st.items_of_kind("claim").iter().map(|i| i.data["id"].as_str().unwrap().to_string()).collect() };
    assert_eq!(claim_ids(&sa), claim_ids(&sb));
    assert_eq!(report_of(&a.rig, &sa), report_of(&b.rig, &sb));
}

#[test]
fn a_restart_replays_the_journal_and_the_code_steps_that_lost_their_line_rebuild_the_same_report() {
    let w = World::load();
    let r = Run::new("rf-resume", &w, "report");
    // The state at the plan card, as a crash right after it would have left it on disk.
    assert!(matches!(join_within(r.engine().spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let id = r.p.id.clone();
    let proj_dir = r.rig.deps.store().unwrap().dir(&id).unwrap();
    let parked = std::fs::read(proj_dir.join("project.json")).unwrap();
    r.engine().answer("g_1", "approve", None).unwrap();
    r.drive(&[]);
    let original = report_of(&r.rig, &r.p.state());
    let Run { p, .. } = r;
    let home = p.stop();

    // Back to the plan card with every journal line kept, except those of the assembly and Finish:
    // they run again from the replayed state and must write the same bytes.
    let proj_dir = home.0.join("projects").join(&id);
    std::fs::write(proj_dir.join("project.json"), parked).unwrap();
    let _ = std::fs::remove_file(proj_dir.join("report.md"));
    let _ = std::fs::remove_file(proj_dir.join("research.json"));
    let _ = std::fs::remove_file(proj_dir.join("phases").join("ph_report").join("assemble.jsonl"));
    let _ = std::fs::remove_dir_all(proj_dir.join("phases").join("ph_finish"));
    let state = home.store().load(&id).unwrap();
    let p = Project::attach(home, state, Registry::builtin());
    let w2 = World::load();
    let rig2 = rig(&w2, &p.home.0);
    let model2 = model(&w2, "report");
    let run = Run { p, rig: rig2, model: model2.clone(), pool: SlotPool::new(3) };
    run.drive(&[]);
    assert_eq!(model2.count(), 0, "every model task came back from the journal");
    assert!(run.rig.pages.requests().is_empty() && run.rig.web.queries().is_empty(), "nothing was searched or fetched again");
    let st = run.p.state();
    assert_eq!(st.status, ProjectStatus::Finished, "{}", debug(&st));
    assert_eq!(report_of(&run.rig, &st), original, "the same bytes");
}

#[test]
fn a_channel_that_is_down_is_a_note_in_the_report_and_not_a_failure() {
    let w = World::load();
    let r = Run::new("rf-down", &w, "report");
    r.rig.web.down.lock().unwrap().push(marlowe_research::ChannelId::Arxiv);
    r.drive(&[]);
    let st = r.p.state();
    assert_eq!(st.status, ProjectStatus::Finished, "{}", debug(&st));
    let notes: Vec<String> = st.items_of_kind("note").iter().map(|n| n.data["text"].as_str().unwrap().to_string()).collect();
    assert!(notes.iter().any(|n| n.contains("arXiv search rate limited")), "{notes:?}");
    assert!(report_of(&r.rig, &st).contains("arXiv search rate limited"));
}

#[test]
fn a_claim_whose_page_text_changed_after_extraction_is_unsupported_and_costs_no_verify_call() {
    let w = World::load();
    let p = Project::new("rf-tamper", Registry::builtin(), &plan());
    let rig = rig(&w, &p.home.0);
    let (deps, pid) = (rig.deps.clone(), p.id.clone());
    let base = responder(&w, "report");
    let done = std::sync::atomic::AtomicBool::new(false);
    // When the rumour thread's claim reaches a condense pass its claim exists, so it was extracted;
    // then the stored page is altered, as a disk fault or an edit would.
    let model = Fake::with(move |req, n, c| {
        let text = req.messages.last().map(|m| m.joined_text()).unwrap_or_default();
        if asks_for(req, "groups") && text.contains("Kimi K3 could arrive in December") && !done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let dir = deps.store().unwrap().dir(&pid).unwrap().join("sources");
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "md") {
                    let t = std::fs::read_to_string(&path).unwrap();
                    if t.contains("Kimi K3 is being prepared") {
                        std::fs::write(&path, t.replace("Kimi K3 is being prepared", "Kimi K3 is being prepare d")).unwrap();
                    }
                }
            }
        }
        base(req, n, c)
    });
    let run = Run { p, rig, model: model.clone(), pool: SlotPool::new(3) };
    run.drive(&[]);
    let st = run.p.state();
    assert_eq!(st.status, ProjectStatus::Finished, "{}", debug(&st));
    let k3 = st.items_of_kind("cclaim").into_iter().find(|c| c.data["claim"] == "Kimi K3 could arrive in December").expect("the claim");
    assert_eq!(k3.data["verdict"], "unsupported");
    assert!(k3.data["reason"].as_str().unwrap().starts_with("None of the quoted passages"), "{}", k3.data["reason"]);
    assert!(k3.agents.iter().all(|a| st.agents[a].step != "verify"), "no verifier was asked: {:?}", k3.agents);
    let verify_calls = model.requests().iter().filter(|r| asks_for(r, "verdict") && asks_for(r, "used")).count();
    assert_eq!(verify_calls, 8, "nine condensed claims, one of them decided by code");
    let md = report_of(&run.rig, &st);
    assert!(!md.contains("could arrive in December"), "an unsupported claim is not in the report");
    assert!(md.contains("Not independently confirmed: Kimi K3"), "but its record row says it is unconfirmed:
{md}");
}
