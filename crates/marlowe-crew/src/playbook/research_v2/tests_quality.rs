//! The live run fe527eba's failures, each as a scripted run: a verifier that could rubber-stamp,
//! a cut-off section, one paper counted as two sources, an off-topic paper in the report, a
//! memory-only finding stored at user trust, a run that went in half-blind, findings that could
//! not corroborate across sub-questions, and retrieval that never reached the known methods. No
//! network, no inference.

use std::collections::BTreeSet;

use marlowe_model::ModelInfo;
use marlowe_proto::MemoryTrust;
use marlowe_research::{ChannelError, Claim, ClaimId, Quote, SourceId, Standing};

use super::*;

fn auto() -> ResearchV2Config {
    ResearchV2Config { auto_approve_under_tokens: Some(10_000_000), ..Default::default() }
}

fn report_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(REPORT_FILE)).unwrap()
}

fn accuracy(rec: &Value) -> f64 {
    let cites = rec["verification"]["citations"].as_array().unwrap();
    let score: f64 = cites
        .iter()
        .map(|c| match c["support"].as_str() {
            Some("supported") => 1.0,
            Some("partial") => 0.5,
            _ => 0.0,
        })
        .sum();
    score / cites.len() as f64
}

/// Two independent benchmark pages agree on model M: the high-confidence run.
fn agreeing() -> (World, Crew) {
    let (x, y) = ("https://one.example/m", "https://two.example/m");
    let mut w = World::default();
    w.results.insert(ChannelId::Web, vec![web(x, "Benchmark one"), web(y, "Benchmark two")]);
    w.pages.insert(x.into(), page(x, "Benchmark one", "Model M reaches 91.2% top-1 accuracy on ImageNet.", vec![]));
    w.pages.insert(
        y.into(),
        page(y, "Benchmark two", "We measure model M at 91.2% top-1 accuracy on ImageNet as well.", vec![]),
    );
    let mut crew = imagenet_crew(
        r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1, 2]}]"#,
        VerifierScript::default(),
    );
    crew.compressor = Scripted::new(|p| {
        if p.contains("Group the claims") {
            r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1, 2]}]"#.into()
        } else {
            let q = if p.contains("Benchmark one") {
                "Model M reaches 91.2% top-1 accuracy"
            } else {
                "model M at 91.2% top-1 accuracy"
            };
            format!("[{}]", claim("Model M reaches 91.2% top-1 on ImageNet", q, ""))
        }
    });
    (w, crew)
}

// ---------------------------------------------------------------- the verifier

/// Answers "supported" to everything when asked without reasoning (as dusk did live, 18 of 18),
/// and passes every other call to the scripted verifier.
struct Stamp {
    inner: Arc<Scripted>,
    off_calls: AtomicUsize,
}

impl Provider for Stamp {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        if req.effort != Effort::Off {
            return self.inner.stream(req, cancel, on);
        }
        self.off_calls.fetch_add(1, Ordering::SeqCst);
        let prompt = req.messages.iter().find(|m| m.role == Role::User).map(Message::joined_text).unwrap_or_default();
        let lines: Vec<String> = ['G', 'C', 'S']
            .iter()
            .flat_map(|t| ids_after(&prompt, *t).into_iter().map(move |i| format!("{t}{i}: supported")))
            .collect();
        let evs = vec![StreamEvent::TextDelta(lines.join("\n")), StreamEvent::Stop(StopReason::EndTurn)];
        for ev in &evs {
            on(ev.clone());
        }
        Ok(fold_events(&evs))
    }
}

fn runner_with_verifier(crew: &Crew, verifier: Arc<dyn Provider>, root: &Path) -> AgentRunner {
    let inner = crew.resolver();
    let resolver: ModelResolver = Arc::new(move |card: &RoleCard| {
        if card.name == "verifier" {
            Ok(StepModel { provider: verifier.clone(), model: model() })
        } else {
            inner(card)
        }
    });
    let sandbox =
        Sandbox { kind: SandboxKind::Scratch, root: root.to_path_buf(), origin: None, branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None };
    let mut r = AgentRunner::new(RunId::new_random(), Roles::builtin(), resolver, Arc::new(NoTools), sandbox, None);
    r.hooks = Arc::new(Checkpoints::default());
    r
}

/// Found live: verification calls that failed with thinking on were re-asked without it, and
/// every such verdict was "supported". A retry now keeps the card's effort and gets a larger
/// budget, so a rubber-stamping direct path is never reached and accuracy cannot rise from it.
#[test]
fn a_verifier_retry_keeps_its_reasoning_so_it_cannot_rubber_stamp() {
    let base_dir = temp_dir("stamp-base");
    let host = lora_host(Arc::new(lora_world()));
    let crew = lora_crew(lora_verifier());
    deep_research_v2(&runner(&crew, &base_dir, Arc::new(Checkpoints::default())), &host, GOAL, &auto()).unwrap();
    let baseline = accuracy(&record(&base_dir));

    // Every call with more than one item fails (the model spent its budget thinking).
    let dir = temp_dir("stamp");
    let host = lora_host(Arc::new(lora_world()));
    let crew = lora_crew(VerifierScript { empty_over: Some(1), ..lora_verifier() });
    let stamp = Arc::new(Stamp { inner: crew.verifier.clone(), off_calls: AtomicUsize::new(0) });
    deep_research_v2(&runner_with_verifier(&crew, stamp.clone(), &dir), &host, GOAL, &auto()).unwrap();
    assert_eq!(stamp.off_calls.load(Ordering::SeqCst), 0, "no verification call is made without reasoning");
    let calls = crew.verifier.calls_with("");
    assert!(calls.iter().all(|c| c.1 == Effort::High), "the verifier card's effort, every time");
    let budgets: BTreeSet<Option<u32>> = calls.iter().map(|c| c.2).collect();
    assert!(
        budgets.contains(&Some(2 * FIRST_OUTPUT)) && budgets.contains(&Some(3 * FIRST_OUTPUT)),
        "each smaller retry gets a larger budget: {budgets:?}"
    );
    assert!(accuracy(&record(&dir)) <= baseline + 1e-9, "retries cannot raise accuracy");
    let _ = std::fs::remove_dir_all(&base_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- truncation

/// Found live: section 5 ended mid-sentence ("sign-preserving score"). Cut off on both tries,
/// a section is cut back to its last full sentence, says so, and caps confidence.
#[test]
fn a_section_cut_off_twice_is_cut_back_marked_and_caps_confidence() {
    let dir = temp_dir("cut-twice");
    let (w, mut crew) = agreeing();
    let host = Host::new(Arc::new(w));
    let inner = crew.researcher.clone();
    crew.researcher = Scripted::new(move |p| {
        if p.contains("Write the report's section") {
            return format!(
                "{CUT}Model M reaches 91.2% top-1 on ImageNet [C1, C2]. It was then tuned with power transformation, sign"
            );
        }
        (inner.respond)(p)
    });
    let d = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, QM, &auto()).unwrap();
    let report = report_of(&dir);
    assert!(!report.contains("power transformation"), "{report}");
    assert!(report.contains("Model M reaches 91.2% top-1 on ImageNet [1, 2].") && report.contains(CUT_NOTE), "{report}");
    let warn = report.find("Coverage warning").expect("a warning");
    assert!(warn < report.find("## Answer").unwrap() && report.contains("section 1 cut off"), "{report}");
    assert_eq!(d.confidence, Confidence::Low, "the agreeing run is High uncut; cut, it is Low");
    assert!(d.summary.starts_with("**Coverage warning.**"), "{}", d.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resumed run replays a stored cut-off step as cut off, and retries it.
#[test]
fn a_resumed_run_retries_a_step_stored_cut_off() {
    let dir = temp_dir("cut-resume");
    let (w, crew) = agreeing();
    let host = Host::new(Arc::new(w));
    let hooks = Arc::new(Checkpoints::default());
    hooks.done.lock().unwrap().insert(
        checkpoint_key(ResearchStage::Synthesise, Some("q1")),
        StepResult { text: "Model M reaches 91.2% top-1 [C1]. Then it was".into(), truncated: true, ..Default::default() },
    );
    let d = deep_research_v2(&runner(&crew, &dir, hooks), &host, QM, &auto()).unwrap();
    let calls = crew.researcher.calls_with("Write the report's section");
    assert_eq!(calls.len(), 1, "only the retry ran");
    assert_eq!(calls[0].2, Some(2 * FIRST_OUTPUT));
    assert!(!report_of(&dir).contains("Then it was"));
    assert_eq!(d.confidence, Confidence::High);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- relevance

const NOT_URL: &str = "https://transplant.example/not";

/// Found live: a layer-transplant adaptation paper filled a section of a report on pruning.
#[test]
fn an_off_goal_group_never_reaches_a_section_or_memory() {
    let dir = temp_dir("off-goal");
    let (mut w, mut crew) = agreeing();
    w.results.get_mut(&ChannelId::Web).unwrap().push(web(NOT_URL, "Neural Organ Transplantation"));
    w.pages.insert(
        NOT_URL.into(),
        page(NOT_URL, "Neural Organ Transplantation", "NOT transplants trained layers and trains 2.8x faster.", vec![]),
    );
    let host = Host::new(Arc::new(w));
    let plan = format!(r#"{{"sub_questions": [{{"question": "{QM}", "channels": ["web"]}}], "breadth": 3, "depth": 0, "source_budget": 4}}"#);
    let sections = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = sections.clone();
    crew.researcher = Scripted::new(move |p| {
        if p.contains("Draft the research plan") {
            plan.clone()
        } else {
            if p.contains("Write the report's section") {
                seen.lock().unwrap().push(p.to_string());
            }
            "Model M reaches 91.2% top-1 on ImageNet [C1, C2].".into()
        }
    });
    crew.compressor = Scripted::new(|p| {
        if p.contains("Group the claims") {
            return r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1, 2]},
                       {"statement": "NOT trains 2.8x faster", "supports": [3]}]"#
                .into();
        }
        let (c, q) = if p.contains("Benchmark one") {
            ("Model M reaches 91.2% top-1 on ImageNet", "Model M reaches 91.2% top-1 accuracy")
        } else if p.contains("Benchmark two") {
            ("Model M reaches 91.2% top-1 on ImageNet", "model M at 91.2% top-1 accuracy")
        } else {
            ("NOT trains 2.8x faster", "trains 2.8x faster")
        };
        format!("[{}]", claim(c, q, ""))
    });
    crew.verifier = verifier(VerifierScript { off_goal: vec![2], ..Default::default() });
    deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, QM, &auto()).unwrap();
    let prompts = sections.lock().unwrap().clone();
    assert!(!prompts.is_empty() && prompts.iter().all(|p| !p.contains("NOT trains")), "{prompts:?}");
    let rec = record(&dir);
    assert!(rec["off_goal"]["2"].as_str().unwrap().contains("not pruning"), "{}", rec["off_goal"]);
    assert!(rec["corroboration"].as_array().unwrap().iter().all(|c| c["group"] != 2));
    assert!(host.remembered.lock().unwrap().iter().all(|(f, _)| !f.text.contains("NOT")));
    assert!(report_of(&dir).contains("1 judged off the goal"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- one paper, two copies

const DARWIN: &str = "2502.07780";
const REPOST: &str = "https://s-rsa.example/index.php/agi/article/view/123";

/// Found live: one arXiv paper's abs and html pages were "two independent sources", the
/// answer said so, and five "established" findings about it were remembered. A copy that says
/// what it is (`citation_arxiv_id`) is one root with the paper, the grading is brought down,
/// and nothing is remembered as established.
#[test]
fn one_paper_read_twice_is_one_root_and_its_grading_is_corrected() {
    let dir = temp_dir("one-root");
    let a_url = format!("https://arxiv.org/abs/{DARWIN}");
    let title = "DarwinLM: Evolutionary Structured Pruning of Large Language Models";
    let mut w = World::default();
    w.results.insert(ChannelId::Arxiv, vec![arxiv(DARWIN, title, "Shengkun Tang", "2025")]);
    w.results.insert(ChannelId::Web, vec![web(REPOST, "View article")]);
    w.pages.insert(a_url.clone(), page(&a_url, title, "DarwinLM keeps 95% of dense accuracy at 50% sparsity.", vec![]));
    let mut repost = page(REPOST, "AGI journal", "DarwinLM keeps 95% of dense accuracy at 50% sparsity on Llama.", vec![]);
    repost.meta = vec![("citation_title".into(), title.into()), ("citation_arxiv_id".into(), DARWIN.into())];
    w.pages.insert(REPOST.into(), repost);
    let host = Host::new(Arc::new(w));
    let sentence = "Two independent sources report that DarwinLM keeps 95% of dense accuracy at 50% sparsity [C1, C2].";
    let plan = r#"{"sub_questions": [{"question": "How much accuracy does DarwinLM keep?", "channels": ["arxiv", "web"]}],
                   "breadth": 2, "depth": 0, "source_budget": 2}"#;
    let crew = Crew {
        researcher: Scripted::new(move |p| if p.contains("Draft") { plan.into() } else { sentence.into() }),
        scout: Scripted::new(|_| "PICK: 1, 2".into()),
        compressor: Scripted::new(|p| {
            if p.contains("Group the claims") {
                return r#"[{"statement": "DarwinLM keeps 95% of dense accuracy at 50% sparsity", "supports": [1, 2]}]"#.into();
            }
            format!("[{}]", claim("DarwinLM keeps 95% of dense accuracy at 50% sparsity", "DarwinLM keeps 95% of dense accuracy at 50% sparsity", ""))
        }),
        verifier: verifier(VerifierScript::default()),
    };
    let d = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, "DarwinLM accuracy", &auto())
        .unwrap();
    let rec = record(&dir);
    let g = &rec["corroboration"][0];
    assert_eq!((g["supporting_roots"].as_u64(), g["standing"].as_str()), (Some(1), Some("suggested")), "{rec}");
    let report = report_of(&dir);
    assert!(report.contains("One source reports that DarwinLM keeps 95%"), "{report}");
    assert!(!report.split("## How this was checked").next().unwrap().contains("Two independent"), "{report}");
    assert!(report.contains("Grading words brought down"), "{report}");
    assert!(report.contains(&format!("[2] {title}")) && report.matches(&format!("arXiv:{DARWIN}")).count() == 2, "{report}");
    let mem = host.remembered.lock().unwrap().clone();
    assert!(mem.iter().all(|(f, _)| !f.text.contains("established by")), "nothing stored as established");
    assert_ne!(d.confidence, Confidence::High);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- trust of what is remembered

const MEM: &str = "Matthew pruned Qwen 32B to 25% sparsity and lost 1.3 points of MMLU.";

fn memory_run(channels: &str, trust: MemoryTrust) -> (Host, Arc<World>) {
    let dir = temp_dir("mem-taint");
    let mut w = World::default();
    w.results.insert(ChannelId::Web, vec![web("https://blog.example/prune", "Pruning notes")]);
    let world = Arc::new(w);
    let mut host = Host::new(world.clone());
    host.recalled = vec![Recalled { id: "m-7".into(), text: MEM.into(), provenance: "you".into(), trust }];
    let plan = format!(
        r#"{{"sub_questions": [{{"question": "What did Matthew see pruning Qwen 32B?", "channels": {channels}}}], "breadth": 1, "depth": 0, "source_budget": 2}}"#
    );
    let crew = Crew {
        researcher: Scripted::new(move |p| {
            if p.contains("Draft") {
                plan.clone()
            } else {
                "Pruning Qwen 32B to 25% cost 1.3 points of MMLU [C1].".into()
            }
        }),
        scout: Scripted::new(|_| "PICK: 1".into()),
        compressor: Scripted::new(|p| {
            if p.contains("Group the claims") {
                return "[]".into();
            }
            format!("[{}]", claim("Pruning Qwen 32B to 25% cost 1.3 points of MMLU", "lost 1.3 points of MMLU", ""))
        }),
        verifier: verifier(VerifierScript::default()),
    };
    deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, "Qwen 32B pruning", &auto()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (host, world)
}

/// Found in review: a run that searched the web but read only memory stored its findings at
/// user trust, and a finding resting on an untrusted memory was trusted.
#[test]
fn what_a_run_remembers_is_tainted_by_its_latch_and_its_memories() {
    let (host, world) = memory_run(r#"["web", "memory"]"#, MemoryTrust::User);
    assert!(world.fetched_urls().is_empty(), "only the memory was read");
    let mem = host.remembered.lock().unwrap().clone();
    assert!(!mem.is_empty() && mem.iter().all(|(_, tainted)| *tainted), "the web results tainted the run");
    assert_eq!(mem[0].0.derived_from, ["m-7"], "the memory is its lineage");
    assert_eq!(mem[0].0.sources, ["memory:m-7"]);

    let (host, _) = memory_run(r#"["memory"]"#, MemoryTrust::Untrusted);
    assert!(!host.tainted.load(Ordering::SeqCst), "nothing from the network");
    assert!(host.remembered.lock().unwrap().iter().all(|(_, t)| *t), "an untrusted memory taints what rests on it");

    let (host, _) = memory_run(r#"["memory"]"#, MemoryTrust::User);
    assert!(host.remembered.lock().unwrap().iter().all(|(_, t)| !*t), "the user's own note stays trusted");
}

// ---------------------------------------------------------------- coverage

/// Found live: the web had no backend and Semantic Scholar answered 429 to everything; 29
/// queries failed, and the report said "nothing settles it" with only a footer line about it.
#[test]
fn a_dead_web_is_left_out_of_the_plan_a_blind_node_is_widened_and_thin_coverage_comes_first() {
    let dir = temp_dir("thin");
    let p_url = "https://arxiv.org/abs/2306.11695";
    let mut w = World::default();
    w.results.insert(ChannelId::Arxiv, vec![arxiv("2306.11695", "Wanda: A Simple and Effective Pruning Approach", "Mingjie Sun", "2023")]);
    w.results.insert(ChannelId::Web, vec![]);
    w.failing.insert(ChannelId::SemanticScholar, ChannelError::RateLimited { channel: ChannelId::SemanticScholar, retry_after: None });
    w.down = vec![(ChannelId::Web, "no web search backend answers: SearXNG at http://127.0.0.1:8080 is not reachable".into())];
    w.pages.insert(p_url.into(), page(p_url, "Wanda", "Wanda prunes LLaMA-65B to 50% sparsity with a 0.6 perplexity loss.", vec![]));
    let world = Arc::new(w);
    let host = Host::new(world.clone());
    let q2 = "Which pruning methods hold perplexity on 30B models?";
    let plan = format!(
        r#"{{"sub_questions": [{{"question": "Which pruning methods scale to 65B?", "channels": ["web", "arxiv"]}},
                              {{"question": "{q2}", "channels": ["semantic_scholar"]}}],
            "breadth": 2, "depth": 0, "source_budget": 10}}"#
    );
    let crew = Crew {
        researcher: Scripted::new(move |p| {
            if p.contains("Draft") {
                plan.clone()
            } else {
                "Wanda prunes LLaMA-65B to 50% sparsity with a 0.6 perplexity loss [C1].".into()
            }
        }),
        scout: Scripted::new(|_| "PICK: 1".into()),
        compressor: Scripted::new(|p| {
            if p.contains("Group the claims") {
                return "[]".into();
            }
            format!("[{}]", claim("Wanda prunes LLaMA-65B to 50% sparsity with a 0.6 perplexity loss", "Wanda prunes LLaMA-65B to 50% sparsity", ""))
        }),
        verifier: verifier(VerifierScript::default()),
    };
    let d = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, "LLM pruning at scale", &auto())
        .unwrap();
    let (plan, _) = host.proposals.lock().unwrap().first().cloned().unwrap_or_else(|| {
        let p: ResearchPlan = serde_json::from_str(&host.store.lock().unwrap()[&checkpoint_key(ResearchStage::Plan, Some("approved"))]).unwrap();
        (p, CostQuote { model_tokens: 0, fetches: 0, minutes: 0, usd: None, basis: String::new() })
    });
    assert_eq!(plan.sub_questions[0].channels, [ChannelId::Arxiv], "the dead web is never routed to");
    assert!(plan.notes.as_deref().unwrap_or("").contains("Not searched: web (no web search backend answers"), "{plan:?}");
    let queries = world.queries.lock().unwrap().clone();
    assert!(queries.iter().any(|(c, q)| *c == ChannelId::Arxiv && q.text == search_text(q2)), "q2 widened to arXiv: {queries:?}");
    let report = report_of(&dir);
    let warn = report.find("Coverage warning").expect("a coverage warning");
    assert!(warn < report.find("## Answer").unwrap(), "{report}");
    assert!(report.contains("1 of 4 planned sources were read") && report.contains("not searched: web"), "{report}");
    assert!(report.contains("1 of 3 channel queries failed") || report.contains("widened to arxiv"), "{report}");
    assert!(report.contains("widened to arxiv"), "{report}");
    assert_eq!(d.confidence, Confidence::Low);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- grouping across the run

/// Found live: with more than 100 claims grouping ran per sub-question, so a finding read under
/// q1 could not corroborate the same finding read under q2. Chunks are now run-wide and a merge
/// pass joins their groups.
#[test]
fn a_finding_read_under_two_sub_questions_corroborates_across_chunks() {
    let dir = temp_dir("merge");
    let world = Arc::new(World::default());
    let host = Host::new(world);
    let compressor = Scripted::new(|p| {
        if p.contains("Each line below is a finding") {
            let ids: Vec<u32> =
                p.lines().filter(|l| l.contains("Wanda keeps")).filter_map(|l| l.strip_prefix('G')?.split(':').next()?.parse().ok()).collect();
            return format!(r#"[{{"statement": "Wanda keeps 95% at 50% sparsity", "groups": {ids:?}}}]"#);
        }
        if p.contains("Group the claims") {
            let first = ids_after(p, 'C').into_iter().find(|i| p.contains(&format!("C{i} (q")) && *i % 100 == 1);
            return match first {
                Some(i) => format!(r#"[{{"statement": "Wanda keeps 95% at 50% sparsity", "supports": [{i}]}}]"#),
                None => "[]".into(),
            };
        }
        "[]".into()
    });
    let crew = Crew {
        researcher: Scripted::new(|_| String::new()),
        scout: Scripted::new(|_| String::new()),
        compressor,
        verifier: verifier(VerifierScript::default()),
    };
    let r = runner(&crew, &dir, Arc::new(Checkpoints::default()));
    let cfg = ResearchV2Config::default();
    let plan = parse_plan(
        "pruning",
        r#"{"sub_questions": [{"question": "q one", "channels": ["arxiv"]}, {"question": "q two", "channels": ["arxiv"]}]}"#,
        &[],
        &[ChannelId::Arxiv],
    );
    let ctx = Ctx::new(&r, &host, "pruning", &cfg);
    let doc = marlowe_research::DocRef::of(b"d");
    let text = "Wanda keeps 95% at 50% sparsity.";
    // 120 claims, two per source: sources 0..50 under q1, 50..60 under q2.
    let claims: Vec<Claim> = (1..=120u32)
        .map(|i| Claim {
            id: ClaimId(i),
            source: SourceId((i - 1) / 2),
            sub_question: if i <= 100 { 1 } else { 2 },
            text: if i % 100 == 1 { text.into() } else { format!("unrelated claim {i}") },
            quote: Quote::locate(&doc, text, text).unwrap(),
        })
        .collect();
    let comp = Compressed { claims, ..Default::default() };
    let mut ex = explore::Explored {
        state: ExploreState {
            tree: ExploreTree::from_plan(&plan, &cfg),
            routes: Vec::new(),
            fetched: Vec::new(),
            tables: Vec::new(),
            memory: Default::default(),
            memory_untrusted: Vec::new(),
            budget_used: 0,
            queries: 0,
            failures: Vec::new(),
            node_queries: Default::default(),
            widened: Vec::new(),
            waves_done: 1,
        },
        dedup: marlowe_research::Deduper::default(),
        navigator: marlowe_research::Navigator::new(),
    };
    let cor = compress::corroborate_stage(&ctx, &plan, &mut ex, &comp).unwrap();
    let wanda = cor.groups.iter().find(|g| g.statement.starts_with("Wanda")).expect("the merged group");
    let members: Vec<u32> = wanda.members.iter().map(|(c, _)| c.0).collect();
    assert_eq!(members, [1, 101], "one finding, read under q1 and q2");
    assert_eq!(cor.standing(wanda.id).unwrap().standing, Standing::Established, "two sources, two roots");
    assert_eq!(cor.groups.len(), 119, "the other 118 claims stay apart");
    let grouping = crew.compressor.calls_with("Group the claims");
    assert_eq!(grouping.len(), 2, "two run-wide chunks");
    assert!(grouping.iter().all(|(p, _, _)| p.contains("(q1") || p.contains("(q2")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- retrieval

/// Found live: one 12-word query per channel, no date range, no method names, no citation
/// chasing; Wanda, SparseGPT and OWL were never found. The plan's keyword queries go out with
/// arXiv's field syntax and the goal's years, and a paper read leads to the work citing it.
#[test]
fn keyword_queries_carry_the_goals_years_and_citation_chasing_reads_a_citing_paper() {
    let dir = temp_dir("queries");
    let (a, b) = ("2301.00774", "2310.05175");
    let mut w = World::default();
    w.results.insert(ChannelId::Arxiv, vec![arxiv(a, "SparseGPT", "Elias Frantar", "2023")]);
    let mut owl = arxiv(b, "Outlier Weighed Layerwise Sparsity (OWL)", "Lu Yin", "2025");
    owl.channel = ChannelId::SemanticScholar;
    owl.url = format!("https://www.semanticscholar.org/arxiv/{b}");
    w.citing.insert(ChannelId::SemanticScholar, vec![owl]);
    for (id, t) in [(a, "SparseGPT"), (b, "OWL")] {
        let u = format!("https://arxiv.org/abs/{id}");
        w.pages.insert(u.clone(), page(&u, t, &format!("{t} prunes LLMs in one shot without retraining."), vec![]));
    }
    let owl_page = format!("https://www.semanticscholar.org/arxiv/{b}");
    w.pages.insert(owl_page.clone(), page(&owl_page, "OWL", "OWL prunes LLMs with outlier-weighed layerwise sparsity.", vec![]));
    let world = Arc::new(w);
    let host = Host::new(world.clone());
    let goal = "Which 2025–2026 pruning methods work best for 30B LLMs?";
    let plan = r#"{"sub_questions": [{"question": "Which one-shot pruning methods scale to 30B?", "channels": ["arxiv"],
                     "queries": ["SparseGPT one-shot pruning", "Wanda pruning LLaMA", "https://evil.example/x OWL sparsity"]}],
                   "breadth": 2, "depth": 1, "source_budget": 6}"#;
    let crew = Crew {
        researcher: Scripted::new(move |p| if p.contains("Draft") { plan.into() } else { "Nothing.".into() }),
        scout: Scripted::new(|_| "PICK: 1".into()),
        compressor: Scripted::new(|_| "[]".into()),
        verifier: verifier(VerifierScript::default()),
    };
    let _ = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, goal, &auto());
    let queries = world.queries.lock().unwrap().clone();
    let arxiv_texts: Vec<&str> = queries.iter().filter(|(c, _)| *c == ChannelId::Arxiv).map(|(_, q)| q.text.as_str()).collect();
    assert_eq!(
        arxiv_texts,
        ["all:SparseGPT AND all:one-shot AND all:pruning", "all:Wanda AND all:pruning AND all:LLaMA", "all:OWL AND all:sparsity"],
        "keyword queries, arXiv syntax, the URL dropped"
    );
    assert!(queries.iter().filter(|(c, _)| *c == ChannelId::Arxiv).all(|(_, q)| q.since_year == Some(2025)));
    let chased: Vec<_> = queries.iter().filter_map(|(c, q)| q.citations.as_ref().map(|x| (*c, x.clone()))).collect();
    assert_eq!(chased.len(), 2, "forward and backward from the paper read: {chased:?}");
    assert!(chased.iter().all(|(c, x)| *c == ChannelId::SemanticScholar && x.work.arxiv_id.as_deref() == Some(a)));
    assert!(world.fetched_urls().contains(&owl_page), "the citing paper was read: {:?}", world.fetched_urls());
    let _ = std::fs::remove_dir_all(&dir);
}
