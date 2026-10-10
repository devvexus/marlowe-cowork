//! Deep research v2 wire round-trips: plan approval, plan proposal, stage progress.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

fn rid() -> RunId {
    RunId("0123456789abcdef0123456789abcdef".into())
}

fn plan() -> ResearchPlan {
    ResearchPlan {
        goal: "What does the literature say about LoRA rank?".into(),
        sub_questions: vec![
            SubQuestion {
                id: 1,
                question: "Which ablations vary LoRA rank on LLMs?".into(),
                channels: vec![ChannelId::Arxiv, ChannelId::SemanticScholar, ChannelId::OpenAlex],
                rationale: Some("published research".into()),
            },
            SubQuestion {
                id: 2,
                question: "What do practitioners report for rank 8 vs 64?".into(),
                channels: vec![ChannelId::Github, ChannelId::HackerNews, ChannelId::Web],
                rationale: None,
            },
        ],
        breadth: 4,
        depth: 2,
        source_budget: 60,
        recalled: vec!["mem-41".into()],
        notes: Some("Excludes vision models.".into()),
    }
}

fn quote() -> CostQuote {
    CostQuote { model_tokens: 420_000, fetches: 84, minutes: 18, usd: None, basis: "2 sub-questions x b4 x d2".into() }
}

fn progress(stage: ResearchStage, state: StageState) -> ResearchProgress {
    ResearchProgress {
        stage,
        state,
        done: 3,
        total: 8,
        detail: Some("scout 2 · fetched 14 sources".into()),
        counts: ResearchCounts { queries: 9, sources_found: 31, sources_fetched: 14, claims: 40, ..Default::default() },
    }
}

#[test]
fn approve_plan_requests_round_trip_for_every_decision() {
    let reqs = [
        Request::ApprovePlan { run: rid(), decision: PlanDecision::Approve },
        Request::ApprovePlan { run: rid(), decision: PlanDecision::Edit { plan: plan() } },
        Request::ApprovePlan { run: rid(), decision: PlanDecision::Reject { reason: Some("too broad".into()) } },
        Request::ApprovePlan { run: rid(), decision: PlanDecision::Reject { reason: None } },
    ];
    for r in &reqs {
        let s = round_trip(r);
        assert!(s.starts_with(r#"{"op":"approve_plan","#), "{s}");
    }
    let s = serde_json::to_string(&reqs[0]).unwrap();
    assert!(s.contains(r#""decision":{"kind":"approve"}"#), "{s}");
    let r: Request = serde_json::from_str(
        r#"{"op":"approve_plan","run":"0123456789abcdef0123456789abcdef","decision":{"kind":"reject"}}"#,
    )
    .unwrap();
    assert_eq!(r, Request::ApprovePlan { run: rid(), decision: PlanDecision::Reject { reason: None } });
    // An edited plan naming an unknown channel does not parse.
    let bad = serde_json::to_string(&reqs[1]).unwrap().replace("\"hacker_news\"", "\"twitter\"");
    assert!(serde_json::from_str::<Request>(&bad).is_err());
}

#[test]
fn plan_proposed_and_progress_events_round_trip_inside_an_envelope() {
    let mut events = vec![Event::PlanProposed { run: rid(), plan: plan(), quote: quote() }];
    for (i, stage) in ResearchStage::ALL.into_iter().enumerate() {
        let state = [StageState::Started, StageState::Running, StageState::Done, StageState::NeedsYou][i % 4];
        events.push(Event::ResearchProgress { run: rid(), progress: progress(stage, state) });
    }
    events.push(Event::ResearchProgress { run: rid(), progress: progress(ResearchStage::Verify, StageState::Failed) });
    for (i, event) in events.into_iter().enumerate() {
        let env = Envelope { seq: i as u64 + 1, at_ms: 9, event };
        round_trip(&env);
    }
    let s = serde_json::to_string(&Event::PlanProposed { run: rid(), plan: plan(), quote: quote() }).unwrap();
    assert!(s.starts_with(r#"{"ev":"plan_proposed","#), "{s}");
    assert!(s.contains(r#""channels":["arxiv","semantic_scholar","open_alex"]"#), "{s}");
    let s = serde_json::to_string(&Event::ResearchProgress {
        run: rid(),
        progress: progress(ResearchStage::Synthesise, StageState::Running),
    })
    .unwrap();
    assert!(s.starts_with(r#"{"ev":"research_progress","#) && s.contains(r#""stage":"synthesise""#), "{s}");
    assert_eq!(Event::PlanProposed { run: rid(), plan: plan(), quote: quote() }.session(), None);
}

#[test]
fn sparse_progress_and_quote_parse_with_defaults() {
    let p: ResearchProgress = serde_json::from_str(r#"{"stage":"explore","state":"running"}"#).unwrap();
    assert_eq!(p.done, 0);
    assert_eq!(p.counts, ResearchCounts::default());
    let q: CostQuote = serde_json::from_str(r#"{"model_tokens":1,"fetches":2,"minutes":3}"#).unwrap();
    assert_eq!(q.usd, None);
    assert!(q.basis.is_empty());
    let s = round_trip(&ResearchPlan { recalled: vec![], notes: None, ..plan() });
    assert!(!s.contains("recalled") && !s.contains("notes"), "{s}");
}

#[test]
fn plans_validate_their_bounds() {
    assert_eq!(plan().validate(), Ok(()));
    assert_eq!(plan().channels().len(), 6);
    let mut p = plan();
    p.sub_questions[1].id = 1;
    assert_eq!(p.validate(), Err(PlanError::DuplicateId(1)));
    let mut p = plan();
    p.sub_questions[0].channels.clear();
    assert_eq!(p.validate(), Err(PlanError::NoChannels(1)));
    assert_eq!(ResearchPlan { breadth: 0, ..plan() }.validate(), Err(PlanError::Breadth(0)));
    assert_eq!(ResearchPlan { depth: 9, ..plan() }.validate(), Err(PlanError::Depth(9)));
    assert_eq!(ResearchPlan { source_budget: 0, ..plan() }.validate(), Err(PlanError::SourceBudget(0)));
    assert_eq!(ResearchPlan { sub_questions: vec![], ..plan() }.validate(), Err(PlanError::NoSubQuestions));
    assert_eq!(ResearchPlan { goal: " ".into(), ..plan() }.validate(), Err(PlanError::NoGoal));
}

#[test]
fn channels_and_stages_have_stable_names() {
    for c in ChannelId::ALL {
        assert_eq!(round_trip(&c), format!("\"{}\"", c.as_str()));
        assert_eq!(c.as_str().parse::<ChannelId>().unwrap(), c);
    }
    assert_eq!("S2".parse::<ChannelId>().unwrap(), ChannelId::SemanticScholar);
    assert_eq!("searxng".parse::<ChannelId>().unwrap(), ChannelId::Web);
    assert_eq!("Hacker-News".parse::<ChannelId>().unwrap(), ChannelId::HackerNews);
    assert!("twitter".parse::<ChannelId>().is_err());
    assert!(!ChannelId::Memory.is_network() && ChannelId::Web.is_network());
    assert_eq!(ResearchStage::Plan.ordinal(), 1);
    assert_eq!(ResearchStage::Remember.ordinal(), 7);
    assert_eq!(round_trip(&StageState::NeedsYou), r#""needs_you""#);
}
