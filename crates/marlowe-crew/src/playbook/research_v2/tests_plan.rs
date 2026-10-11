//! The user's plan decisions and the one source budget shared by parallel scouts.

use super::*;

#[test]
fn the_user_decides_the_plan_reject_edit_and_a_bad_edit_asked_again() {
    // Rejected: nothing is searched, the run ends cancelled.
    let dir = temp_dir("reject");
    let world = Arc::new(imagenet_world());
    let host = Host::new(world.clone());
    host.decisions.lock().unwrap().push_back(PlanDecision::Reject { reason: Some("too broad".into()) });
    let crew = imagenet_crew("[]", VerifierScript::default());
    let r = deep_research_v2(
        &runner(&crew, &dir, Arc::new(Checkpoints::default())),
        &host,
        QM,
        &ResearchV2Config::default(),
    );
    assert!(matches!(r, Err(CrewError::Cancelled)), "{r:?}");
    assert!(world.searches.lock().unwrap().is_empty() && world.fetches.lock().unwrap().is_empty());
    assert!(host.stages().contains(&(ResearchStage::Plan, StageState::Failed)));
    assert!(host.approved.lock().unwrap().is_empty(), "a rejected plan approves no budget");

    // A bad edit is asked again; the valid edit is the plan that runs.
    let dir2 = temp_dir("edit");
    let world = Arc::new(imagenet_world());
    let host = Host::new(world.clone());
    let edited = ResearchPlan {
        goal: String::new(),
        sub_questions: vec![SubQuestion {
            id: 1,
            question: QM.into(),
            channels: vec![ChannelId::Web],
            rationale: None,
            queries: Vec::new(),
        }],
        breadth: 1,
        depth: 0,
        source_budget: 1,
        recalled: vec![],
        notes: Some("one source is enough".into()),
    };
    let bad = ResearchPlan { breadth: 0, ..edited.clone() };
    host.decisions.lock().unwrap().extend([PlanDecision::Edit { plan: bad }, PlanDecision::Edit { plan: edited }]);
    let crew = imagenet_crew("[]", VerifierScript::default());
    deep_research_v2(&runner(&crew, &dir2, Arc::new(Checkpoints::default())), &host, QM, &ResearchV2Config::default())
        .unwrap();
    assert_eq!(host.proposals.lock().unwrap().len(), 2, "the bad edit was not run; the plan was proposed again");
    let approved: ResearchPlan =
        serde_json::from_str(&host.store.lock().unwrap()[&checkpoint_key(ResearchStage::Plan, Some("approved"))])
            .unwrap();
    assert_eq!((approved.goal.as_str(), approved.breadth, approved.source_budget), (QM, 1, 1));
    assert_eq!(world.fetched_urls().len(), 1, "the edited budget of one source holds");
    // The edit is priced again, and that price is what the approval covers.
    let (plan, quote, steps) = host.approved.lock().unwrap()[0].clone();
    assert_eq!(plan, approved);
    let proposed = host.proposals.lock().unwrap()[0].1.clone();
    assert!(quote.model_tokens < proposed.model_tokens, "{quote:?} vs {proposed:?}");
    assert_eq!(steps, step_estimate(&approved, &ResearchV2Config::default()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn one_source_budget_holds_across_parallel_scouts() {
    let dir = temp_dir("budget");
    let mut w = World::default();
    let urls: Vec<String> = (1..=6).map(|i| format!("https://s{i}.example/page")).collect();
    w.results.insert(ChannelId::Github, urls[..3].iter().map(|u| web(u, "github result")).collect());
    w.results.insert(ChannelId::StackExchange, urls[3..].iter().map(|u| web(u, "stack exchange result")).collect());
    for u in &urls {
        w.pages.insert(u.clone(), page(u, "A page", "Nothing in particular is claimed on this page.", vec![]));
    }
    let world = Arc::new(w);
    let host = Host::new(world.clone());
    let plan = r#"{"sub_questions": [{"question": "How do I pin a crate version?", "channels": ["github"]},
                                     {"question": "Why does cargo pick a newer patch release?", "channels": ["stack_exchange"]}],
                   "breadth": 3, "depth": 0, "source_budget": 4}"#;
    let crew = Crew {
        researcher: Scripted::new(move |p| if p.contains("Draft") { plan.into() } else { "Nothing found.".into() }),
        scout: Scripted::new(|_| "PICK: 1, 2, 3".into()),
        compressor: Scripted::new(|_| "[]".into()),
        verifier: verifier(VerifierScript::default()),
    };
    let r = deep_research_v2(
        &runner(&crew, &dir, Arc::new(Checkpoints::default())),
        &host,
        "Pin a crate",
        &ResearchV2Config::default(),
    );
    // Two scouts wanted three each; the run may read four.
    assert_eq!(world.fetched_urls().len(), 4, "{:?}", world.fetched_urls());
    assert_eq!(world.fetches.lock().unwrap().len(), 1, "one fetch for the wave");
    assert_eq!(host.last_counts().sources_fetched, 4);
    // Nothing claimed: there is nothing to verify, so there is no report either.
    assert!(matches!(r, Err(CrewError::Step { ref step, .. }) if step == "verify"), "{r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
