//! Deep research v2 stage tests. Every model is scripted per role (researcher, scout, compressor,
//! verifier) through the real `AgentRunner`; channels, pages and the run host are in-memory. No
//! network, no inference.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_extract::Link;
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Effort, Message, ModelInfo, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec,
};
use marlowe_proto::{
    ChannelId, Confidence, CostQuote, PlanDecision, ResearchPlan, ResearchProgress, ResearchStage, RunId, SandboxKind,
    StageState, SubQuestion,
};
use marlowe_research::{ChannelError, ChannelQuery, Identity, SourceRef};
use serde_json::Value;

use super::*;
use crate::playbook::{AgentRunner, ModelResolver, StepHooks, StepModel, StepResult, StepSpec, REPORT_FILE};
use crate::role::{RoleCard, Roles};
use crate::sandbox::Sandbox;
use crate::CrewError;

// ---------------------------------------------------------------- scripted models

type Respond = dyn Fn(&str) -> String + Send + Sync;

/// Answers each step prompt with `respond(prompt)`; records prompts and tool counts.
struct Scripted {
    respond: Box<Respond>,
    prompts: Mutex<Vec<String>>,
    tools: Mutex<Vec<usize>>,
    efforts: Mutex<Vec<Effort>>,
}

impl Scripted {
    fn new(respond: impl Fn(&str) -> String + Send + Sync + 'static) -> Arc<Scripted> {
        Arc::new(Scripted {
            respond: Box::new(respond),
            prompts: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            efforts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

fn model() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::OpenRouter, "scripted");
    m.supports_tools = true;
    m
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(
        &self,
        req: &ChatRequest,
        _cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let prompt = req.messages.iter().find(|m| m.role == Role::User).map(Message::joined_text).unwrap_or_default();
        self.prompts.lock().unwrap().push(prompt.clone());
        self.tools.lock().unwrap().push(req.tools.len());
        self.efforts.lock().unwrap().push(req.effort);
        let evs = vec![StreamEvent::TextDelta((self.respond)(&prompt)), StreamEvent::Stop(StopReason::EndTurn)];
        for ev in &evs {
            on(ev.clone());
        }
        Ok(fold_events(&evs))
    }
}

struct Crew {
    researcher: Arc<Scripted>,
    scout: Arc<Scripted>,
    compressor: Arc<Scripted>,
    verifier: Arc<Scripted>,
}

impl Crew {
    fn resolver(&self) -> ModelResolver {
        let map: Vec<(String, Arc<Scripted>)> = vec![
            ("researcher".into(), self.researcher.clone()),
            ("scout".into(), self.scout.clone()),
            ("compressor".into(), self.compressor.clone()),
            ("verifier".into(), self.verifier.clone()),
        ];
        Arc::new(move |card: &RoleCard| {
            let p = map
                .iter()
                .find(|(r, _)| *r == card.name)
                .map(|(_, p)| p.clone())
                .ok_or_else(|| CrewError::UnknownRole(card.name.clone()))?;
            Ok(StepModel { provider: p, model: model() })
        })
    }

    fn total_calls(&self) -> usize {
        self.researcher.calls() + self.scout.calls() + self.compressor.calls() + self.verifier.calls()
    }
}

/// No tools: every v2 step runs without them.
struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error(format!("{} is not available", call.name))
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

/// Completed agent steps by key, shared across a run and its resume.
#[derive(Default)]
struct Checkpoints {
    done: Mutex<HashMap<String, StepResult>>,
}

impl StepHooks for Checkpoints {
    fn completed(&self, key: &str) -> Option<StepResult> {
        self.done.lock().unwrap().get(key).cloned()
    }

    fn finished(&self, spec: &StepSpec, _messages: &[Message], result: &StepResult) {
        self.done.lock().unwrap().insert(spec.key.clone(), result.clone());
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-v2-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn runner(crew: &Crew, root: &Path, hooks: Arc<Checkpoints>) -> AgentRunner {
    let sandbox = Sandbox {
        kind: SandboxKind::Scratch,
        root: root.to_path_buf(),
        origin: None,
        branch: None,
        base: None,
        repo_digest: None,
    };
    let mut r =
        AgentRunner::new(RunId::new_random(), Roles::builtin(), crew.resolver(), Arc::new(NoTools), sandbox, None);
    r.hooks = hooks;
    r
}

// ---------------------------------------------------------------- channels, pages, host

#[derive(Default)]
struct World {
    results: HashMap<ChannelId, Vec<SourceRef>>,
    pages: HashMap<String, FetchedPage>,
    searches: Mutex<Vec<(ChannelId, String)>>,
    fetches: Mutex<Vec<(Vec<String>, usize)>>,
}

impl ResearchIo for World {
    fn channels(&self) -> Vec<ChannelId> {
        let mut v: Vec<ChannelId> = self.results.keys().copied().collect();
        v.sort();
        v
    }

    fn search(
        &self,
        channel: ChannelId,
        query: &ChannelQuery,
        _cancel: &CancelToken,
    ) -> Result<Vec<SourceRef>, ChannelError> {
        self.searches.lock().unwrap().push((channel, query.text.clone()));
        Ok(self.results.get(&channel).cloned().unwrap_or_default())
    }

    fn fetch(&self, urls: &[String], concurrency: usize, _cancel: &CancelToken) -> Vec<FetchedPage> {
        self.fetches.lock().unwrap().push((urls.to_vec(), concurrency));
        urls.iter()
            .map(|u| {
                self.pages.get(u).cloned().unwrap_or(FetchedPage {
                    url: u.clone(),
                    final_url: u.clone(),
                    error: Some("HTTP 404".into()),
                    ..Default::default()
                })
            })
            .collect()
    }
}

impl World {
    fn fetched_urls(&self) -> Vec<String> {
        self.fetches.lock().unwrap().iter().flat_map(|(u, _)| u.clone()).collect()
    }
}

struct Host {
    run: RunId,
    world: Arc<World>,
    cancel: CancelToken,
    decisions: Mutex<VecDeque<PlanDecision>>,
    proposals: Mutex<Vec<(ResearchPlan, CostQuote)>>,
    approved: Mutex<Vec<(ResearchPlan, CostQuote, u32)>>,
    progress: Mutex<Vec<ResearchProgress>>,
    store: Arc<Mutex<HashMap<String, String>>>,
    recalled: Vec<Recalled>,
    profile: Option<String>,
    tainted: AtomicBool,
    remembered: Mutex<Vec<(Finding, bool)>>,
}

impl Host {
    fn new(world: Arc<World>) -> Host {
        Host {
            run: RunId::new_random(),
            world,
            cancel: CancelToken::new(),
            decisions: Mutex::new(VecDeque::new()),
            proposals: Mutex::new(Vec::new()),
            approved: Mutex::new(Vec::new()),
            progress: Mutex::new(Vec::new()),
            store: Arc::new(Mutex::new(HashMap::new())),
            recalled: Vec::new(),
            profile: None,
            tainted: AtomicBool::new(false),
            remembered: Mutex::new(Vec::new()),
        }
    }

    fn stages(&self) -> Vec<(ResearchStage, StageState)> {
        self.progress.lock().unwrap().iter().map(|p| (p.stage, p.state)).collect()
    }

    fn last_counts(&self) -> marlowe_proto::ResearchCounts {
        self.progress.lock().unwrap().last().map(|p| p.counts).unwrap_or_default()
    }
}

impl ResearchHost for Host {
    fn run_id(&self) -> RunId {
        self.run.clone()
    }

    fn io(&self) -> &dyn ResearchIo {
        self.world.as_ref()
    }

    fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    fn progress(&self, progress: &ResearchProgress) {
        self.progress.lock().unwrap().push(progress.clone());
    }

    fn propose_plan(&self, plan: &ResearchPlan, quote: &CostQuote) -> Result<PlanDecision, CrewError> {
        self.proposals.lock().unwrap().push((plan.clone(), quote.clone()));
        Ok(self.decisions.lock().unwrap().pop_front().unwrap_or(PlanDecision::Approve))
    }

    fn plan_approved(&self, plan: &ResearchPlan, quote: &CostQuote, steps: u32) {
        self.approved.lock().unwrap().push((plan.clone(), quote.clone(), steps));
    }

    fn load(&self, key: &str) -> Option<String> {
        self.store.lock().unwrap().get(key).cloned()
    }

    fn save(&self, key: &str, _label: &str, data: &str) {
        self.store.lock().unwrap().insert(key.to_string(), data.to_string());
    }

    fn profile(&self) -> Option<String> {
        self.profile.clone()
    }

    fn recall(&self, _query: &str, limit: usize) -> Vec<Recalled> {
        self.recalled.iter().take(limit).cloned().collect()
    }

    fn taint(&self) {
        self.tainted.store(true, Ordering::SeqCst);
    }

    fn remember(&self, finding: &Finding, tainted: bool) -> Result<String, String> {
        let mut r = self.remembered.lock().unwrap();
        r.push((finding.clone(), tainted));
        Ok(format!("mem-{}", r.len()))
    }
}

// ---------------------------------------------------------------- fixtures

const FILLER: &str = "The rest of this page describes the experimental setup, the datasets and the hardware in \
                      detail; none of it bears on the question at hand, but it is long enough to read.";

fn page(url: &str, title: &str, body: &str, links: Vec<Link>) -> FetchedPage {
    FetchedPage {
        url: url.into(),
        final_url: url.into(),
        title: Some(title.into()),
        text: format!("# {title}\n\n{body}\n\n{FILLER}\n"),
        links,
        error: None,
    }
}

fn arxiv(id: &str, title: &str, author: &str, year: &str) -> SourceRef {
    let mut r = SourceRef::new(ChannelId::Arxiv, &format!("https://arxiv.org/abs/{id}"), title);
    r.identity = Some(Identity::default().with_arxiv(id));
    r.authors = vec![author.into()];
    r.date = Some(year.into());
    r.snippet = format!("{title}: abstract.");
    r
}

fn web(url: &str, title: &str) -> SourceRef {
    let mut r = SourceRef::new(ChannelId::Web, url, title);
    r.snippet = format!("{title} — snippet");
    r
}

/// Verifier behaviour for a scenario.
#[derive(Clone, Default)]
struct VerifierScript {
    /// Leave this group out of the first review reply (it is asked for again).
    omit_first: Option<u32>,
    /// Claims whose passage does not support them.
    unsupported: Vec<u32>,
    /// Report sentences (by substring) judged unsupported / partial.
    audit_unsupported: Vec<&'static str>,
    audit_partial: Vec<&'static str>,
    /// Weakened groups with the reason.
    weakened: Vec<(u32, &'static str)>,
    /// Reply with prose and no verdicts.
    garbage: bool,
    /// Give no answer at all (the step fails) when a call carries more than this many items.
    empty_over: Option<usize>,
}

fn ids_after(prompt: &str, tag: char) -> Vec<u32> {
    prompt
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix(tag)?;
            let d: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let after = rest[d.len()..].chars().next()?;
            (after == ':' || after == ' ').then(|| d.parse().ok()).flatten()
        })
        .collect()
}

fn verifier(s: VerifierScript) -> Arc<Scripted> {
    Scripted::new(move |p| {
        if s.garbage {
            return "I read them all and they look fine to me.".into();
        }
        if let Some(n) = s.empty_over {
            let sentences = p
                .lines()
                .filter(|l| l.strip_prefix('S').and_then(|r| r.split_once(": ")).is_some_and(|(k, _)| k.parse::<u32>().is_ok()))
                .count();
            if ids_after(p, 'G').len() + ids_after(p, 'C').len() + sentences > n {
                return String::new();
            }
        }
        if p.contains("Try to break each claim group") {
            let ids = ids_after(p, 'G');
            let first = ids.len() > 1;
            ids.iter()
                .filter(|g| !(first && s.omit_first == Some(**g)))
                .map(|g| match s.weakened.iter().find(|(w, _)| w == g) {
                    Some((_, why)) => format!("G{g}: weakened: {why}"),
                    None => format!("G{g}: upheld"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else if p.contains("For each claim below") {
            ids_after(p, 'C')
                .iter()
                .map(|c| {
                    if s.unsupported.contains(c) {
                        format!("C{c}: unsupported: the passage says otherwise")
                    } else {
                        format!("C{c}: supported")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else if p.contains("Each numbered sentence below") {
            p.lines()
                .filter_map(|l| {
                    let rest = l.strip_prefix('S')?;
                    let (k, sentence) = rest.split_once(": ")?;
                    let k: u32 = k.parse().ok()?;
                    Some(if s.audit_unsupported.iter().any(|x| sentence.contains(x)) {
                        format!("S{k}: unsupported: the passages do not say this")
                    } else if s.audit_partial.iter().any(|x| sentence.contains(x)) {
                        format!("S{k}: partial: the survey says 8 or 16, not mostly")
                    } else {
                        format!("S{k}: supported")
                    })
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            "no idea".into()
        }
    })
}

// ---------------------------------------------------------------- the LoRA scenario

const A_URL: &str = "https://arxiv.org/abs/2106.09685";
const B_URL: &str = "https://arxiv.org/abs/2303.10512";
const C_URL: &str = "https://blog.example/lora-rank";
const D_URL: &str = "https://notes.example/7b-survey";
const E_URL: &str = "https://bench.example/lora-glue-rerun";
const Q_URL: &str = "https://arxiv.org/abs/2305.14314";
const Q1: &str = "Does LoRA rank above 8 improve GLUE accuracy?";
const Q2: &str = "What rank do practitioners use for 7B models?";
const GAP: &str = "Which methods use ranks far above 8?";
const GOAL: &str = "What LoRA rank should we use?";
const MEMORY: &str = "Matthew used rank 16 for the 7B Mistral run and saw no gain over rank 8.";

const PLAN: &str = r#"Here is the plan:
```json
{"sub_questions": [
  {"question": "Does LoRA rank above 8 improve GLUE accuracy?", "channels": ["arxiv", "web"], "rationale": "papers first"},
  {"question": "What rank do practitioners use for 7B models?", "channels": ["web", "memory"]}],
 "breadth": 4, "depth": 1, "source_budget": 8, "notes": "GLUE and 7B decoder models only"}
```"#;

const SEC1: &str = "LoRA at rank 8 comes within about 0.4 points of full fine-tuning on GLUE; two independent \
                    sources establish it [C2, C6]. Whether ranks above 8 help is contested: the LoRA paper saw no \
                    gain [C3], while AdaLoRA reports 0.9 points on DeBERTaV3-base [C4]. QLoRA trains 65B models \
                    with rank 16 adapters [C8]. Rank 4 beats rank 8 everywhere [C2].";
const SEC2: &str = "Practitioners mostly use rank 8 or 16 for 7B models, suggested by one survey [C7], and the \
                    user's own 7B run saw no gain from rank 16 over rank 8 [C1].";
const ANSWER: &str = "Rank 8 is enough for GLUE-style fine-tuning: it is within about 0.4 points of full \
                      fine-tuning [C2, C6], and gains from higher ranks are contested [C3, C4].";

fn lora_world() -> World {
    let mut w = World::default();
    w.results.insert(
        ChannelId::Arxiv,
        vec![
            arxiv("2106.09685", "LoRA: Low-Rank Adaptation of Large Language Models", "Edward Hu", "2021"),
            arxiv("2303.10512", "AdaLoRA: Adaptive Budget Allocation", "Qingru Zhang", "2023"),
        ],
    );
    w.results.insert(
        ChannelId::Web,
        vec![
            web(C_URL, "LoRA rank explained"),
            web(D_URL, "A survey of 7B fine-tunes"),
            web(E_URL, "Rerunning LoRA on GLUE"),
            web(A_URL, "LoRA paper"),
        ],
    );
    let qlora = Link { url: Q_URL.into(), text: "QLoRA: Efficient Finetuning of Quantized LLMs".into() };
    for p in [
        page(
            A_URL,
            "LoRA: Low-Rank Adaptation of Large Language Models",
            "We find that rank r = 8 matches full fine-tuning on GLUE within 0.3 points. Increasing the rank \
             beyond 8 did not improve GLUE accuracy in our experiments.",
            vec![qlora],
        ),
        page(
            B_URL,
            "AdaLoRA",
            "On DeBERTaV3-base, a rank budget above 8 improves GLUE accuracy by 0.9 points.",
            vec![],
        ),
        page(
            C_URL,
            "LoRA rank explained",
            "According to Hu et al. (arXiv:2106.09685), rank 8 matches full fine-tuning on GLUE.",
            vec![],
        ),
        page(D_URL, "A survey of 7B fine-tunes", "Most 7B fine-tunes we surveyed use rank 8 or 16.", vec![]),
        page(
            E_URL,
            "Rerunning LoRA on GLUE",
            "Our independent rerun finds rank 8 within 0.4 points of full fine-tuning on GLUE.",
            vec![],
        ),
        page(
            Q_URL,
            "QLoRA",
            "QLoRA finetunes 65B models on a single 48GB GPU using rank 64 adapters on all layers.",
            vec![],
        ),
    ] {
        w.pages.insert(p.url.clone(), p);
    }
    w
}

fn claim(c: &str, q: &str, cites: &str) -> String {
    serde_json::json!({"claim": c, "quote": q, "cites": cites}).to_string()
}

fn lora_crew(v: VerifierScript) -> Crew {
    let researcher = Scripted::new(|p| {
        if p.contains("Draft the research plan") {
            PLAN.into()
        } else if p.contains("section for sub-question 1") {
            SEC1.into()
        } else if p.contains("section for sub-question 2") {
            SEC2.into()
        } else if p.contains("Write the answer to the goal") {
            format!("## Answer\n\n{ANSWER}")
        } else {
            "unexpected".into()
        }
    });
    let scout = Scripted::new(|p| {
        if p.contains(&format!("Your question: {Q1}")) {
            format!("PICK: 2, 4, 3, 6\nGAP: {GAP}")
        } else if p.contains(&format!("Your question: {Q2}")) {
            "PICK: 1, 3".into()
        } else if p.contains(&format!("Your question: {GAP}")) {
            "PICK: 7".into()
        } else {
            "PICK: 1".into()
        }
    });
    let compressor = Scripted::new(|p| {
        if p.contains("Group the claims") {
            // One call over the whole run: the claims read for both sub-questions are in it.
            assert!(p.contains("C1 (q2") && p.contains("C2 (q1"), "{p}");
            return r#"[{"statement": "LoRA at rank 8 is within about 0.4 points of full fine-tuning on GLUE", "supports": [2, 5, 6]},
                {"statement": "Ranks above 8 do not improve GLUE accuracy", "supports": [3], "contradicts": [4]},
                {"statement": "Practitioners use rank 8 to 16 for 7B models", "supports": ["C1", "C7"]}]"#
                .into();
        }
        let items: Vec<String> = if p.contains(&format!("— {A_URL}")) {
            vec![
                claim(
                    "LoRA at rank 8 matches full fine-tuning on GLUE within 0.3 points",
                    "rank r = 8 matches full fine-tuning on GLUE within 0.3 points",
                    "",
                ),
                claim(
                    "Ranks above 8 did not improve GLUE accuracy",
                    "Increasing the rank beyond 8 did not improve GLUE accuracy in our experiments",
                    "",
                ),
            ]
        } else if p.contains(&format!("— {B_URL}")) {
            vec![
                claim(
                    "A rank budget above 8 improves GLUE accuracy by 0.9 points on DeBERTaV3-base",
                    "a rank budget above 8 improves GLUE accuracy by 0.9 points",
                    "",
                ),
                claim("AdaLoRA is always better", "AdaLoRA beats every baseline on every task", ""),
            ]
        } else if p.contains(&format!("— {C_URL}")) {
            vec![claim(
                "Rank 8 matches full fine-tuning on GLUE",
                "rank 8 matches full fine-tuning on GLUE",
                "arXiv:2106.09685",
            )]
        } else if p.contains(&format!("— {D_URL}")) {
            vec![claim(
                "Most surveyed 7B fine-tunes use rank 8 or 16",
                "Most 7B fine-tunes we surveyed use rank 8 or 16",
                "",
            )]
        } else if p.contains(&format!("— {E_URL}")) {
            vec![claim(
                "An independent rerun finds rank 8 within 0.4 points of full fine-tuning on GLUE",
                "Our independent rerun finds rank 8 within 0.4 points of full fine-tuning on GLUE",
                "",
            )]
        } else if p.contains(&format!("— {Q_URL}")) {
            vec![claim("QLoRA uses rank 16 adapters", "using rank 64 adapters on all layers", "")]
        } else if p.contains("— memory:m-1") {
            vec![claim(
                "Rank 16 gave no gain over rank 8 on the user's 7B Mistral run",
                "used rank 16 for the 7B Mistral run and saw no gain over rank 8",
                "",
            )]
        } else {
            vec![]
        };
        format!("[{}]", items.join(", "))
    });
    Crew { researcher, scout, compressor, verifier: verifier(v) }
}

fn lora_verifier() -> VerifierScript {
    VerifierScript {
        omit_first: Some(3),
        unsupported: vec![8],
        audit_unsupported: vec!["Rank 4 beats"],
        audit_partial: vec!["Practitioners mostly"],
        weakened: vec![(2, "the gain is on DeBERTaV3-base only")],
        garbage: false,
        empty_over: None,
    }
}

fn lora_host(world: Arc<World>) -> Host {
    let mut host = Host::new(world);
    host.recalled = vec![Recalled { id: "m-1".into(), text: MEMORY.into(), provenance: "you, session 3f2a".into() }];
    host.profile = Some("Matthew fine-tunes 7B decoder models on one RTX 4080.".into());
    host
}

fn record(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(RECORD_FILE)).unwrap()).unwrap()
}

#[test]
fn the_full_run_plans_explores_by_selection_verifies_and_writes_a_checked_report() {
    let dir = temp_dir("full");
    let world = Arc::new(lora_world());
    let host = lora_host(world.clone());
    let crew = lora_crew(lora_verifier());
    let hooks = Arc::new(Checkpoints::default());
    let r = runner(&crew, &dir, hooks.clone());
    let cfg = ResearchV2Config::default();

    let d = deep_research_v2(&r, &host, GOAL, &cfg).unwrap();

    // PLAN: a clean run (no tools) that saw the profile and the recalled memory, approved by the user.
    assert_eq!(crew.researcher.tools.lock().unwrap()[0], 0, "the plan step has no tools");
    let plan_prompt = &crew.researcher.prompts()[0];
    assert!(plan_prompt.contains(MEMORY) && plan_prompt.contains("RTX 4080"), "{plan_prompt}");
    assert!(plan_prompt.contains("- arxiv:") && plan_prompt.contains("- memory:"));
    let proposals = host.proposals.lock().unwrap().clone();
    assert_eq!(proposals.len(), 1, "auto-approval is off by default");
    let (plan, quote) = &proposals[0];
    assert_eq!((plan.breadth, plan.depth, plan.source_budget), (4, 1, 8));
    assert_eq!(plan.sub_questions[0].channels, [ChannelId::Arxiv, ChannelId::Web, ChannelId::Memory]);
    assert_eq!(plan.recalled, ["m-1"]);
    assert!(quote.model_tokens > 0 && quote.fetches > 0);

    // EXPLORE: channel queries are search text, memory is recalled (never searched), every
    // selected page is fetched once at the configured concurrency, the duplicate web route to the
    // LoRA paper is not fetched again, and the follow-up read a page it selected from a link table.
    let searches = world.searches.lock().unwrap().clone();
    assert!(
        searches.contains(&(ChannelId::Arxiv, "LoRA rank above 8 improve GLUE accuracy".to_string())),
        "{searches:?}"
    );
    assert!(searches.iter().all(|(c, _)| *c != ChannelId::Memory));
    let fetches = world.fetches.lock().unwrap().clone();
    assert_eq!(fetches[0].0, [A_URL, B_URL, C_URL, E_URL, D_URL]);
    assert_eq!(fetches[1].0, [Q_URL], "the follow-up picked the QLoRA link off the LoRA page");
    assert!(fetches.iter().all(|(_, c)| *c == cfg.fetch_concurrency));
    assert!(!world.results.values().flatten().any(|r| r.url == Q_URL), "the QLoRA URL came from no search result");
    let gap_prompt = crew.scout.prompts().into_iter().find(|p| p.contains(GAP)).unwrap();
    assert!(gap_prompt.contains("[7] arxiv.org — QLoRA") && gap_prompt.contains("(already read)"), "{gap_prompt}");
    assert!(crew.scout.tools.lock().unwrap().iter().all(|n| *n == 0), "scouts select, they do not browse");
    // A result returned again (for another question, or another wave) is one route, not many.
    assert!(!crew.scout.prompts().iter().any(|p| p.contains("arxiv, arxiv") || p.contains("also on web, web")));
    let waves: Vec<ExploreState> = (0..2)
        .filter_map(|d| host.load(&checkpoint_key(ResearchStage::Explore, Some(&format!("wave{d}")))))
        .map(|s| serde_json::from_str(&s).unwrap())
        .collect();
    let routes: Vec<(ChannelId, String)> =
        waves.last().unwrap().routes.iter().map(|r| (r.channel, r.url.clone())).collect();
    let mut unique = routes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(routes.len(), unique.len(), "{routes:?}");
    assert!(host.tainted.load(Ordering::SeqCst));

    // The eval bundle: every numbered reference with the passages the report cited from it and
    // the stored text, each cited passage verbatim in that text.
    let bundle: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(super::BUNDLE_FILE)).unwrap()).unwrap();
    assert_eq!(bundle["schema"], super::BUNDLE_SCHEMA);
    let refs = bundle["references"].as_array().unwrap();
    assert!(!refs.is_empty());
    for r in refs {
        let doc = r["document_text"].as_str().unwrap_or_default();
        let passages = r["passages"].as_array().unwrap();
        assert!(!passages.is_empty(), "{r}");
        assert!(passages.iter().all(|p| doc.contains(p["text"].as_str().unwrap())), "{r}");
    }

    // COMPRESS: a paraphrased quote is dropped; the quotes kept are verbatim in the stored pages.
    let rec = record(&dir);
    assert_eq!(world.fetched_urls().len() as u32, 6);
    let counts = host.last_counts();
    assert_eq!(counts.sources_fetched, 7, "six pages and one memory");
    assert_eq!(counts.claims, 8);

    // CORROBORATE: three sources back group 1 but the blog only repeats the paper: two roots.
    let cor = rec["corroboration"].as_array().unwrap();
    let g1 = cor.iter().find(|c| c["group"] == 1).unwrap();
    assert_eq!((g1["supporting_roots"].as_u64(), g1["standing"].as_str()), (Some(2), Some("established")));
    let g2 = cor.iter().find(|c| c["group"] == 2).unwrap();
    assert_eq!(g2["standing"], "contested");
    assert!(cor.iter().all(|c| c["group"] != 4), "the group whose only claim failed its passage is gone");
    let g3 = cor.iter().find(|c| c["group"] == 3).unwrap();
    assert_eq!((g3["supporting_roots"].as_u64(), g3["memory_support"].as_u64()), (Some(1), Some(1)));
    assert_eq!(counts.contradictions, 1);

    // VERIFY: the group left out of the first review was asked for again, alone.
    let reviews: Vec<String> =
        crew.verifier.prompts().into_iter().filter(|p| p.contains("Try to break each claim group")).collect();
    assert_eq!(reviews.len(), 2);
    assert_eq!(ids_after(&reviews[1], 'G'), [3]);
    let reviews = rec["verification"]["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 4);

    // SYNTHESISE: answer first, claim ids turned into numbered references with identities, the
    // unsupported sentences removed and listed, the partial one marked.
    let report = std::fs::read_to_string(dir.join(REPORT_FILE)).unwrap();
    let answer_at = report.find("## Answer").unwrap();
    assert!(answer_at < report.find("## Evidence").unwrap());
    assert!(report.contains("full fine-tuning [1, 2], and gains from higher ranks are contested [1, 3]."), "{report}");
    assert!(!report.contains("[C"), "no claim id survives into the report");
    assert!(report.contains("[1] LoRA: Low-Rank Adaptation of Large Language Models. Edward Hu (2021). <https://arxiv.org/abs/2106.09685> · arXiv:2106.09685"), "{report}");
    assert!(report.contains("[5] Your memory"), "{report}");
    let (body, checked) = report.split_once("## How this was checked").unwrap();
    assert!(!body.contains("Rank 4 beats") && !body.contains("QLoRA trains"), "unsupported sentences leave the report");
    assert!(checked.contains("\"Rank 4 beats rank 8 everywhere.\""), "and are listed: {checked}");
    assert!(report.contains("QLoRA trains 65B models") && report.contains("cites no claim from the verified evidence"));
    assert!(body.contains("over rank 8 (partly supported) [5]."), "{body}");
    assert!(report.contains("## Contested") && report.contains("the gain is on DeBERTaV3-base only"));
    assert!(report.contains("1 quotes were not in their source"), "{report}");
    assert!(report.contains("2 unsupported (removed); citation accuracy 0.58"), "{report}");
    assert!(report.trim_end().ends_with("Confidence: low"));
    assert_eq!(d.confidence, Confidence::Low, "accuracy 0.58 caps it");
    assert!(d.summary.starts_with("Rank 8 is enough for GLUE-style fine-tuning"));
    assert!(d.citations.iter().any(|c| c.url_or_path == A_URL));
    assert_eq!(d.artifacts.len(), 2);
    assert_eq!(rec["effective_citations"], 6);

    // REMEMBER: the answer and the established finding, at untrusted trust (the web was read).
    let mem = host.remembered.lock().unwrap().clone();
    assert_eq!(mem.len(), 2);
    assert!(mem.iter().all(|(_, tainted)| *tainted));
    assert!(mem[0].0.text.contains("Rank 8 is enough") && mem[0].0.text.contains("Sources: LoRA"));
    assert!(mem[1].0.text.contains("established by 2 independent sources"));

    // Every stage reported, in order, each ending done.
    let stages = host.stages();
    for s in ResearchStage::ALL {
        assert!(stages.contains(&(s, StageState::Done)), "{s:?} done");
    }
    assert!(stages.contains(&(ResearchStage::Plan, StageState::NeedsYou)));
    let firsts: Vec<ResearchStage> = stages.iter().map(|(s, _)| *s).fold(Vec::new(), |mut v, s| {
        if v.last() != Some(&s) && !v.contains(&s) {
            v.push(s);
        }
        v
    });
    assert_eq!(firsts, ResearchStage::ALL);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resumed_run_repeats_no_model_call_search_fetch_or_memory() {
    let dir = temp_dir("resume");
    let world = Arc::new(lora_world());
    let host = lora_host(world.clone());
    let hooks = Arc::new(Checkpoints::default());
    let first = lora_crew(lora_verifier());
    let d1 = deep_research_v2(&runner(&first, &dir, hooks.clone()), &host, GOAL, &ResearchV2Config::default()).unwrap();

    let world2 = Arc::new(lora_world());
    let mut again = lora_host(world2.clone());
    again.store = host.store.clone();
    let second = lora_crew(lora_verifier());
    let d2 = deep_research_v2(&runner(&second, &dir, hooks), &again, GOAL, &ResearchV2Config::default()).unwrap();
    assert_eq!(second.total_calls(), 0, "every agent step came from its checkpoint");
    assert!(world2.searches.lock().unwrap().is_empty() && world2.fetches.lock().unwrap().is_empty());
    assert!(again.proposals.lock().unwrap().is_empty(), "an approved plan is not proposed again");
    assert!(again.remembered.lock().unwrap().is_empty(), "findings are stored once");
    assert_eq!(d1.summary, d2.summary);
    assert_eq!(d1.confidence, d2.confidence);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_verification_means_no_report_and_a_refusal_naming_what_is_missing() {
    let dir = temp_dir("refuse");
    let world = Arc::new(lora_world());
    let host = lora_host(world);
    let crew = lora_crew(VerifierScript { garbage: true, ..Default::default() });
    let err = deep_research_v2(
        &runner(&crew, &dir, Arc::new(Checkpoints::default())),
        &host,
        GOAL,
        &ResearchV2Config::default(),
    )
    .unwrap_err();
    let CrewError::Step { step, message } = err else { panic!("a refusal, not {err:?}") };
    assert_eq!(step, "verify");
    assert!(message.contains("4 of 4 claim groups were not reviewed"), "{message}");
    assert!(message.contains("no citation was checked against its stored passage"), "{message}");
    assert_eq!(crew.researcher.calls(), 1, "only the plan: nothing was synthesised");
    assert!(!dir.join(REPORT_FILE).exists());
    let refusal = std::fs::read_to_string(dir.join("refusal.md")).unwrap();
    assert!(refusal.contains("claim groups were not reviewed") && refusal.contains(A_URL));
    assert!(host.stages().contains(&(ResearchStage::Verify, StageState::Failed)));
    assert!(host.remembered.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_verification_call_is_split_down_to_single_items_and_named_when_it_still_fails() {
    // Every call with more than one item fails (the model gives no answer): the retries split
    // the batches down to one item per call and the run still reports.
    let dir = temp_dir("split");
    let host = lora_host(Arc::new(lora_world()));
    let crew = lora_crew(VerifierScript { empty_over: Some(1), ..lora_verifier() });
    deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, GOAL, &ResearchV2Config::default())
        .unwrap();
    let singles = crew.verifier.prompts().iter().filter(|p| p.contains("Try to break each claim group")).count();
    assert!(singles > 4, "one batched call, then one per group: {singles}");
    assert!(dir.join(REPORT_FILE).exists());

    // When every call fails, the refusal says why.
    let dir2 = temp_dir("split-fail");
    let host = lora_host(Arc::new(lora_world()));
    let crew = lora_crew(VerifierScript { empty_over: Some(0), ..lora_verifier() });
    let err = deep_research_v2(
        &runner(&crew, &dir2, Arc::new(Checkpoints::default())),
        &host,
        GOAL,
        &ResearchV2Config::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("the agent gave no answer"), "{err}");
    let refusal = std::fs::read_to_string(dir2.join("refusal.md")).unwrap();
    assert!(refusal.contains("verification step"), "{refusal}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

// ---------------------------------------------------------------- contradiction, approval, budget

const M_URL: &str = "https://models.example/m";
const R_URL: &str = "https://replication.example/m";
const QM: &str = "What top-1 accuracy does model M reach on ImageNet?";

fn imagenet_world() -> World {
    let mut w = World::default();
    w.results.insert(ChannelId::Web, vec![web(M_URL, "Model M"), web(R_URL, "Replicating model M")]);
    for p in [
        page(M_URL, "Model M", "Model M reaches 91.2% top-1 accuracy on ImageNet.", vec![]),
        page(
            R_URL,
            "Replicating model M",
            "Our replication finds that model M reaches only 85.0% top-1 on ImageNet.",
            vec![],
        ),
    ] {
        w.pages.insert(p.url.clone(), p);
    }
    w
}

fn imagenet_crew(groups: &'static str, v: VerifierScript) -> Crew {
    let plan = format!(
        r#"{{"sub_questions": [{{"question": "{QM}", "channels": ["web"]}}], "breadth": 2, "depth": 0, "source_budget": 4}}"#
    );
    let researcher = Scripted::new(move |p| {
        if p.contains("Draft the research plan") {
            plan.clone()
        } else if p.contains("section for sub-question 1") {
            "Model M's ImageNet accuracy rests on its paper [C1] and on a replication [C2].".into()
        } else {
            "Model M's top-1 accuracy is reported at 91.2% [C1].".into()
        }
    });
    let compressor = Scripted::new(move |p| {
        if p.contains("Group the claims") {
            groups.into()
        } else if p.contains(&format!("— {M_URL}")) {
            format!(
                "[{}]",
                claim(
                    "Model M reaches 91.2% top-1 on ImageNet",
                    "Model M reaches 91.2% top-1 accuracy on ImageNet",
                    ""
                )
            )
        } else {
            format!(
                "[{}]",
                claim(
                    "A replication finds model M at 85.0% top-1 on ImageNet",
                    "model M reaches only 85.0% top-1 on ImageNet",
                    ""
                )
            )
        }
    });
    Crew { researcher, scout: Scripted::new(|_| "PICK: 1, 2".into()), compressor, verifier: verifier(v) }
}

#[test]
fn contradicting_sources_are_flagged_contested_with_the_reason_and_cap_confidence() {
    let dir = temp_dir("contra");
    let world = Arc::new(imagenet_world());
    let host = Host::new(world.clone());
    let crew = imagenet_crew(
        r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1], "contradicts": [2]}]"#,
        VerifierScript { weakened: vec![(1, "an independent replication finds 85.0%")], ..Default::default() },
    );
    let cfg = ResearchV2Config { auto_approve_under_tokens: Some(10_000_000), ..Default::default() };
    let d = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, QM, &cfg).unwrap();

    assert!(host.proposals.lock().unwrap().is_empty(), "auto-approved under the token limit");
    assert_eq!(crew.scout.calls(), 0, "two candidates, breadth two, no follow-ups: nothing to choose");
    let rec = record(&dir);
    let g = &rec["corroboration"][0];
    assert_eq!(
        (g["standing"].as_str(), g["supporting_roots"].as_u64(), g["contradicting_roots"].as_u64()),
        (Some("contested"), Some(1), Some(1))
    );
    let report = std::fs::read_to_string(dir.join(REPORT_FILE)).unwrap();
    let contested = report.split("## Contested").nth(1).expect("a contested section");
    assert!(
        contested.contains("Model M reaches 91.2% top-1 on ImageNet — an independent replication finds 85.0%"),
        "{report}"
    );
    assert_eq!(host.last_counts().contradictions, 1);
    assert_eq!(d.confidence, Confidence::Low, "nothing is established");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_step_that_gives_no_answer_runs_once_more() {
    let dir = temp_dir("again");
    let host = Host::new(Arc::new(imagenet_world()));
    let mut crew = imagenet_crew(
        r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1], "contradicts": [2]}]"#,
        VerifierScript::default(),
    );
    // The grouping call gives no answer the first time (a local model that only thought).
    let inner = crew.compressor.clone();
    let tries = Arc::new(AtomicUsize::new(0));
    let t = tries.clone();
    crew.compressor = Scripted::new(move |p| {
        if p.contains("Group the claims") && t.fetch_add(1, Ordering::SeqCst) == 0 {
            return String::new();
        }
        (inner.respond)(p)
    });
    let cfg = ResearchV2Config { auto_approve_under_tokens: Some(10_000_000), ..Default::default() };
    deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, QM, &cfg).unwrap();
    assert_eq!(tries.load(Ordering::SeqCst), 2, "asked once more");
    let asked: Vec<(String, Effort)> = crew
        .compressor
        .prompts()
        .into_iter()
        .zip(crew.compressor.efforts.lock().unwrap().iter().copied())
        .filter(|(p, _)| p.contains("Group the claims"))
        .collect();
    assert_eq!(asked.len(), 2);
    assert_ne!(asked[0].1, Effort::Off, "the first ask uses the card's effort");
    assert_eq!(asked[1].1, Effort::Off, "the second answers directly");
    assert_eq!(record(&dir)["corroboration"][0]["standing"], "contested", "the retried grouping was used");
    let report = std::fs::read_to_string(dir.join(REPORT_FILE)).unwrap();
    assert!(!report.contains("model steps failed"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agreeing_independent_sources_and_clean_citations_give_high_confidence() {
    let dir = temp_dir("high");
    let mut w = World::default();
    let (x, y) = ("https://one.example/m", "https://two.example/m");
    w.results.insert(ChannelId::Web, vec![web(x, "Benchmark one"), web(y, "Benchmark two")]);
    w.pages.insert(x.into(), page(x, "Benchmark one", "Model M reaches 91.2% top-1 accuracy on ImageNet.", vec![]));
    w.pages.insert(
        y.into(),
        page(y, "Benchmark two", "We measure model M at 91.2% top-1 accuracy on ImageNet as well.", vec![]),
    );
    let world = Arc::new(w);
    let host = Host::new(world);
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
    let cfg = ResearchV2Config { auto_approve_under_tokens: Some(10_000_000), ..Default::default() };
    let d = deep_research_v2(&runner(&crew, &dir, Arc::new(Checkpoints::default())), &host, QM, &cfg).unwrap();
    assert_eq!(d.confidence, Confidence::High, "{}", d.summary);
    assert!(d.summary.contains("citation accuracy 1.00"), "{}", d.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

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

// ---------------------------------------------------------------- pure parts

#[test]
fn a_plan_reply_parses_leniently_and_is_clamped() {
    let available = [ChannelId::Arxiv, ChannelId::Web, ChannelId::Wikipedia, ChannelId::Memory];
    let p = parse_plan("goal", "1. How does attention scale?\n2. Who invented the transformer?", &[], &available);
    assert_eq!(p.sub_questions.len(), 2);
    assert!(p.sub_questions.iter().all(|q| !q.channels.is_empty() && q.channels.iter().all(|c| available.contains(c))));
    assert_eq!((p.breadth, p.depth), (4, 1));
    p.validate().unwrap();
    let p = parse_plan(
        "goal",
        r#"{"sub_questions": [{"question": "q", "channels": ["s2", "nonsense"]}], "breadth": 99, "depth": 9, "source_budget": 0}"#,
        &[],
        &available,
    );
    assert_eq!((p.breadth, p.depth, p.source_budget), (8, 4, 1));
    assert!(!p.sub_questions[0].channels.is_empty(), "an unavailable channel falls back to routing");
    p.validate().unwrap();
    let p = parse_plan("Is the sky blue?", "I would rather not.", &[], &available);
    assert_eq!(p.sub_questions[0].question, "Is the sky blue?");
}

#[test]
fn attributions_chunks_and_queries() {
    let r = attribution_route("Hu et al. 2021 (arXiv:2106.09685v2)").unwrap();
    assert_eq!(r.identity.unwrap().arxiv_id.as_deref(), Some("2106.09685"));
    assert!(r.url.is_empty(), "an identifier in content never merges by URL");
    let r = attribution_route("doi:10.1000/XYZ.1.").unwrap();
    assert_eq!(r.identity.unwrap().doi.as_deref(), Some("10.1000/xyz.1"));
    assert_eq!(attribution_route("https://x.example/p").unwrap().url, "https://x.example/p");
    assert!(attribution_route("Smith 2020").is_none());

    let text = format!("{}\n\n{}", "a".repeat(60), "b".repeat(60));
    let c = chunks(&text, 100, 4);
    assert_eq!(c.len(), 2);
    assert!(c[0].ends_with("\n\n") && c[1] == "b".repeat(60));
    assert_eq!(chunks(&"x".repeat(500), 100, 3).len(), 3, "at most the limit");
    assert_eq!(search_text("What is the best LoRA rank for a 7B model?"), "best LoRA rank 7B model");

    use super::synth::{drop_open_markers, mark_partial, strip_heading};
    assert_eq!(strip_heading("## Answer\n\nRank 8 [C1].\n\n**Confidence:** medium"), "Rank 8 [C1].");
    assert_eq!(strip_heading("Section 1/5: The run used rank 16 [C1]."), "The run used rank 16 [C1].");
    assert_eq!(strip_heading("Confidence in rank 8 is high [C2]."), "Confidence in rank 8 is high [C2].");
    assert_eq!(drop_open_markers("Rank 8 is common [C3, C6, C7,"), "Rank 8 is common");
    assert_eq!(drop_open_markers("Kept [C1].\nCut [C2, C"), "Kept [C1].\nCut");
    assert_eq!(drop_open_markers("A list [C4] and [Ccc] stay."), "A list [C4] and [Ccc] stay.");
    assert_eq!(mark_partial("Rank 8 is enough [C1]."), "Rank 8 is enough (partly supported) [C1].");
    assert_eq!(mark_partial("Rank 8 is enough. [C1][C2]"), "Rank 8 is enough (partly supported). [C1][C2]");
    assert_eq!(mark_partial("No stop [C4]"), "No stop (partly supported) [C4]");

    let cited = cited_sentences("First claim [C1]. Uncited sentence. Second [C2, C3].");
    assert_eq!(cited.len(), 2);
    assert_eq!(cited[1].claims.iter().map(|c| c.0).collect::<Vec<_>>(), [2, 3]);
}
