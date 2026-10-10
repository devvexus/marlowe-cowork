//! The seam's stage-type tests: quote, gate, decisions, tree bounds, the shared budget, the
//! verification rule and checkpoint keys.

use marlowe_proto::{ChannelId, Confidence, PlanDecision, PlanError, ResearchPlan, SubQuestion};
use marlowe_research::{Claim, ClaimId, Corroboration, Quote, SourceId, Standing};

use super::*;

fn plan() -> ResearchPlan {
    ResearchPlan {
        goal: "LoRA rank".into(),
        sub_questions: vec![
            SubQuestion {
                id: 1,
                question: "ablations?".into(),
                channels: vec![ChannelId::Arxiv, ChannelId::Memory],
                rationale: None,
            },
            SubQuestion {
                id: 2,
                question: "practice?".into(),
                channels: vec![ChannelId::Github, ChannelId::Web],
                rationale: None,
            },
        ],
        breadth: 3,
        depth: 1,
        source_budget: 10,
        recalled: vec![],
        notes: None,
    }
}

#[test]
fn quotes_scale_with_the_plan_and_auto_approval_is_off_by_default() {
    let cfg = ResearchV2Config::default();
    let q = quote(&plan(), &cfg, &QuoteModel::default());
    // 2 roots x (1 + 2 follow-ups) = 6 nodes; 3 network channels x 3 = 9 queries; min(18, 10) sources.
    assert!(
        q.basis.contains("6 nodes") && q.basis.contains("9 queries") && q.basis.contains("10 sources"),
        "{}",
        q.basis
    );
    assert_eq!(q.fetches, 19);
    assert_eq!(q.model_tokens, 6_000 + 9 * 3_000 + 10 * 11_000 + 30_000);
    // 1 plan + 6 scouts + 10 x 2 chunks + 1 grouping + 16 verify (11 + retry headroom)
    // + 2 sections + 1 answer + 4 report audits.
    assert_eq!(step_estimate(&plan(), &cfg), 51);
    assert_eq!(gate(&q, &cfg), PlanGate::AwaitUser);
    let auto = ResearchV2Config { auto_approve_under_tokens: Some(1_000_000), ..cfg };
    assert_eq!(gate(&q, &auto), PlanGate::AutoApproved);
}

#[test]
fn decisions_validate_edits_and_reject_cleanly() {
    assert_eq!(apply_decision(plan(), PlanDecision::Approve).unwrap(), Some(plan()));
    assert_eq!(apply_decision(plan(), PlanDecision::Reject { reason: None }).unwrap(), None);
    let edited = ResearchPlan { goal: String::new(), breadth: 5, ..plan() };
    assert_eq!(apply_decision(plan(), PlanDecision::Edit { plan: edited }).unwrap().unwrap().goal, "LoRA rank");
    let bad = ResearchPlan { breadth: 0, ..plan() };
    assert_eq!(apply_decision(plan(), PlanDecision::Edit { plan: bad }), Err(PlanError::Breadth(0)));
}

#[test]
fn the_tree_bounds_depth_followups_and_repeats() {
    let mut t = ExploreTree::from_plan(&plan(), &ResearchV2Config::default());
    assert_eq!(t.wave(0), [NodeId(0), NodeId(1)]);
    let a = t.add_followup(NodeId(0), "rank vs data size").unwrap();
    assert_eq!(t.get(a).unwrap().depth, 1);
    assert_eq!(t.add_followup(a, "deeper"), Err(ExploreError::TooDeep(1)));
    assert_eq!(t.add_followup(NodeId(0), "Rank  VS data size"), Err(ExploreError::Duplicate));
    t.add_followup(NodeId(0), "rank vs model size").unwrap();
    assert_eq!(t.add_followup(NodeId(0), "third"), Err(ExploreError::FollowupsSpent(NodeId(0))));
    assert_eq!(t.wave(1).len(), 2);
}

#[test]
fn the_source_budget_holds_across_threads() {
    let b = std::sync::Arc::new(SourceBudget::new(100));
    let granted: u32 = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let b = b.clone();
                s.spawn(move || (0..10).map(|_| b.take(3)).sum::<u32>())
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert_eq!(granted, 100);
    assert_eq!((b.used(), b.remaining(), b.take(1)), (100, 0, 0));
}

fn cit(support: CitationSupport) -> CitationCheck {
    CitationCheck { sentence: "s".into(), claim: ClaimId(1), support, note: None }
}

#[test]
fn no_verification_means_no_report_and_confidence_follows_it() {
    let e = require_verification(None, 3).unwrap_err();
    assert!(e.to_string().contains("verify stage did not run"), "{e}");
    let partial =
        Verification { reviews: vec![ClaimReview { group: 1, verdict: ClaimVerdict::Upheld }], citations: vec![] };
    let e = require_verification(Some(&partial), 2).unwrap_err();
    assert_eq!(e.missing.len(), 2);

    let est = |g| Corroboration {
        group: g,
        supporting_roots: 2,
        contradicting_roots: 0,
        memory_support: 0,
        standing: Standing::Established,
    };
    let v = Verification {
        reviews: vec![
            ClaimReview { group: 1, verdict: ClaimVerdict::Upheld },
            ClaimReview { group: 2, verdict: ClaimVerdict::Upheld },
        ],
        citations: vec![cit(CitationSupport::Supported); 20],
    };
    assert!(require_verification(Some(&v), 2).is_ok());
    assert_eq!(derive_confidence(&[est(1), est(2)], &v), Confidence::High);
    let mut weak = v.clone();
    weak.citations.extend([cit(CitationSupport::Partial), cit(CitationSupport::Unsupported)]);
    assert_eq!(weak.citation_accuracy(), Some(20.5 / 22.0));
    assert_eq!(derive_confidence(&[est(1), est(2)], &weak), Confidence::Medium);
    weak.citations.extend(vec![cit(CitationSupport::Unsupported); 10]);
    assert_eq!(derive_confidence(&[est(1), est(2)], &weak), Confidence::Low);
    let json = serde_json::to_string(&ClaimVerdict::Refuted("v3 retracts it".into())).unwrap();
    assert_eq!(json, r#"{"verdict":"refuted","reason":"v3 retracts it"}"#);
}

#[test]
fn checkpoint_keys_and_stage_types_serialise() {
    assert_eq!(checkpoint_key(ResearchStage::Explore, Some("3")), "v2:explore:3");
    assert_eq!(checkpoint_key(ResearchStage::Verify, None), "v2:verify");
    let doc = DocRef::of(b"d");
    let c = CompressedSource {
        source: SourceId(1),
        doc: doc.clone(),
        claims: vec![Claim {
            id: ClaimId(1),
            source: SourceId(1),
            sub_question: 1,
            text: "t".into(),
            quote: Quote::locate(&doc, "abc text def", "text").unwrap(),
        }],
        rejected_quotes: 2,
    };
    let back: CompressedSource = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(back, c);
}
