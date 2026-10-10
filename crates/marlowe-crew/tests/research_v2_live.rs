//! Deep research v2 against a local model: the real prompts, a real Ollama model, and a canned
//! in-memory web, so what is measured is whether the model keeps to the reply formats (plan JSON,
//! PICK lines, claims with verbatim quotes, verdict lines) and whether the run reaches a checked
//! report. Opt-in, needs Ollama on loopback and the GPU:
//!
//! ```text
//! MARLOWE_LIVE_MODEL=marlowe-dusk:27b-super cargo test -p marlowe-crew --test research_v2_live -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_crew::playbook::research_v2::{
    deep_research_v2, FetchedPage, Finding, Recalled, ResearchHost, ResearchIo, ResearchV2Config,
};
use marlowe_crew::playbook::{AgentRunner, StepHooks, StepModel, StepResult, StepSpec};
use marlowe_crew::{RoleCard, Roles, RunId, Sandbox, SandboxKind};
use marlowe_extract::Link;
use marlowe_model::{CancelToken, Message, ModelInfo, Provider, ProviderKind, ToolSpec};
use marlowe_proto::{ChannelId, CostQuote, PlanDecision, ResearchPlan, ResearchProgress};
use marlowe_provider::OllamaProvider;
use marlowe_research::{ChannelError, ChannelQuery, Identity, SourceRef};

/// Steps, tokens in and tokens out, by role.
type Totals = Arc<Mutex<HashMap<String, (u32, u64, u64)>>>;

/// Appends every step's prompt and reply to `<sandbox>\steps.log` and totals tokens per role,
/// for calibrating the cost quote.
struct StepLog(PathBuf, Totals);

impl StepHooks for StepLog {
    fn finished(&self, spec: &StepSpec, _messages: &[Message], result: &StepResult) {
        use std::io::Write;
        {
            let mut t = self.1.lock().unwrap();
            let e = t.entry(spec.role.clone()).or_default();
            e.0 += 1;
            e.1 += result.usage.input;
            e.2 += result.usage.output;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.0) {
            let _ = writeln!(
                f,
                "===== {} ({})\n--- prompt\n{}\n--- reply\n{}\n",
                spec.key, spec.role, spec.prompt, result.text
            );
        }
    }
}

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

const FILLER: &str = "The remainder of the page covers the experimental setup, datasets, optimiser settings and \
                      hardware; it does not bear on the question.";

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

/// A small fixed web about LoRA rank: two papers that disagree, a blog repeating one of them, an
/// independent rerun, a survey, and a linked follow-up paper.
struct Web {
    results: HashMap<ChannelId, Vec<SourceRef>>,
    pages: HashMap<String, FetchedPage>,
}

fn web() -> Web {
    let arxiv = |id: &str, title: &str, author: &str, year: &str| {
        let mut r = SourceRef::new(ChannelId::Arxiv, &format!("https://arxiv.org/abs/{id}"), title);
        r.identity = Some(Identity::default().with_arxiv(id));
        r.authors = vec![author.into()];
        r.date = Some(year.into());
        r.snippet = format!("{title}.");
        r
    };
    let site = |url: &str, title: &str, snippet: &str| {
        let mut r = SourceRef::new(ChannelId::Web, url, title);
        r.snippet = snippet.into();
        r
    };
    let mut results = HashMap::new();
    results.insert(
        ChannelId::Arxiv,
        vec![
            arxiv("2106.09685", "LoRA: Low-Rank Adaptation of Large Language Models", "Edward J. Hu", "2021"),
            arxiv(
                "2303.10512",
                "AdaLoRA: Adaptive Budget Allocation for Parameter-Efficient Fine-Tuning",
                "Qingru Zhang",
                "2023",
            ),
        ],
    );
    results.insert(
        ChannelId::Web,
        vec![
            site(
                "https://blog.example/lora-rank",
                "What LoRA rank should you use?",
                "A practitioner's summary of the LoRA paper.",
            ),
            site(
                "https://bench.example/lora-glue-rerun",
                "Rerunning LoRA on GLUE",
                "An independent reproduction of LoRA on GLUE.",
            ),
            site("https://notes.example/7b-survey", "A survey of 7B fine-tunes", "Ranks used in public 7B fine-tunes."),
        ],
    );
    let mut pages = HashMap::new();
    for p in [
        page(
            "https://arxiv.org/abs/2106.09685",
            "LoRA: Low-Rank Adaptation of Large Language Models",
            "On GLUE with RoBERTa-base, LoRA with rank r = 8 matches full fine-tuning within 0.3 points on \
             average. Increasing the rank beyond 8 did not improve GLUE accuracy in our experiments, which \
             suggests the update matrices have a very low intrinsic rank.",
            vec![Link {
                url: "https://arxiv.org/abs/2305.14314".into(),
                text: "QLoRA: Efficient Finetuning of Quantized LLMs".into(),
            }],
        ),
        page(
            "https://arxiv.org/abs/2303.10512",
            "AdaLoRA",
            "On DeBERTaV3-base, allocating a rank budget above 8 to the most important weight matrices improves \
             GLUE accuracy by 0.9 points over LoRA at rank 8.",
            vec![],
        ),
        page(
            "https://blog.example/lora-rank",
            "What LoRA rank should you use?",
            "According to Hu et al. (arXiv:2106.09685), rank 8 matches full fine-tuning on GLUE, so we default to 8.",
            vec![],
        ),
        page(
            "https://bench.example/lora-glue-rerun",
            "Rerunning LoRA on GLUE",
            "Our independent rerun on four seeds finds LoRA at rank 8 within 0.4 points of full fine-tuning on GLUE.",
            vec![],
        ),
        page(
            "https://notes.example/7b-survey",
            "A survey of 7B fine-tunes",
            "Of the 40 public 7B fine-tunes we surveyed, 31 use rank 8 or 16 and 6 use rank 64.",
            vec![],
        ),
        page(
            "https://arxiv.org/abs/2305.14314",
            "QLoRA",
            "QLoRA finetunes 65B models on a single 48GB GPU using rank 64 adapters on all linear layers.",
            vec![],
        ),
    ] {
        pages.insert(p.url.clone(), p);
    }
    Web { results, pages }
}

impl ResearchIo for Web {
    fn channels(&self) -> Vec<ChannelId> {
        vec![ChannelId::Arxiv, ChannelId::Web]
    }

    fn search(&self, channel: ChannelId, _q: &ChannelQuery, _c: &CancelToken) -> Result<Vec<SourceRef>, ChannelError> {
        Ok(self.results.get(&channel).cloned().unwrap_or_default())
    }

    fn fetch(&self, urls: &[String], _n: usize, _c: &CancelToken) -> Vec<FetchedPage> {
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

struct Host {
    web: Web,
    cancel: CancelToken,
    store: Mutex<HashMap<String, String>>,
    started: Instant,
}

impl ResearchHost for Host {
    fn run_id(&self) -> RunId {
        RunId("live".into())
    }

    fn io(&self) -> &dyn ResearchIo {
        &self.web
    }

    fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    fn progress(&self, p: &ResearchProgress) {
        eprintln!(
            "[{:>5.0}s] {:<11} {:?} {}/{} {}",
            self.started.elapsed().as_secs_f64(),
            p.stage.as_str(),
            p.state,
            p.done,
            p.total,
            p.detail.as_deref().unwrap_or("")
        );
    }

    fn propose_plan(&self, plan: &ResearchPlan, quote: &CostQuote) -> Result<PlanDecision, marlowe_crew::CrewError> {
        eprintln!("plan: {}\nquote: {:?}", serde_json::to_string_pretty(plan).unwrap_or_default(), quote);
        Ok(PlanDecision::Approve)
    }

    fn load(&self, key: &str) -> Option<String> {
        self.store.lock().unwrap().get(key).cloned()
    }

    fn save(&self, key: &str, _label: &str, data: &str) {
        self.store.lock().unwrap().insert(key.into(), data.into());
    }

    fn recall(&self, _q: &str, _limit: usize) -> Vec<Recalled> {
        vec![Recalled {
            id: "m-1".into(),
            text: "Matthew used rank 16 for the 7B Mistral run and saw no gain over rank 8.".into(),
            provenance: "you".into(),
        }]
    }

    fn taint(&self) {}

    fn remember(&self, finding: &Finding, _tainted: bool) -> Result<String, String> {
        eprintln!("remember: {}", finding.text);
        Ok("m".into())
    }
}

#[test]
#[ignore = "needs a local Ollama model and the GPU"]
fn deep_research_v2_runs_on_the_local_model() {
    let name = std::env::var("MARLOWE_LIVE_MODEL").unwrap_or_else(|_| "marlowe-dusk:27b-super".into());
    assert!(!name.contains("valiance"), "marlowe-valiance is still in testing");
    let provider: Arc<dyn Provider> =
        Arc::new(OllamaProvider::from_base_url(None).expect("ollama on loopback").with_context_tokens(32_768));
    let mut info = ModelInfo::bare(ProviderKind::Ollama, name.as_str());
    info.local = true;
    info.context_window = Some(32_768);
    let resolver = Arc::new(move |_card: &RoleCard| Ok(StepModel { provider: provider.clone(), model: info.clone() }));
    let dir: PathBuf = std::env::temp_dir().join(format!("marlowe-v2-live-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    let sandbox = Sandbox {
        kind: SandboxKind::Scratch,
        root: dir.clone(),
        origin: None,
        branch: None,
        base: None,
        repo_digest: None,
    };
    let mut runner =
        AgentRunner::new(RunId::new_random(), Roles::builtin(), resolver, Arc::new(NoTools), sandbox, None);
    let totals = Arc::new(Mutex::new(HashMap::new()));
    runner.hooks = Arc::new(StepLog(dir.join("steps.log"), totals.clone()));
    let host =
        Host { web: web(), cancel: CancelToken::new(), store: Mutex::new(HashMap::new()), started: Instant::now() };
    let cfg = ResearchV2Config::default();

    let out = deep_research_v2(&runner, &host, "What LoRA rank should we use for fine-tuning 7B models?", &cfg);
    eprintln!("sandbox: {}", dir.display());
    match &out {
        Ok(d) => {
            eprintln!("confidence: {:?}\nsummary: {}", d.confidence, d.summary);
            eprintln!("{}", std::fs::read_to_string(dir.join("report.md")).unwrap_or_default());
        }
        Err(e) => {
            eprintln!("no report: {e}");
            eprintln!("{}", std::fs::read_to_string(dir.join("refusal.md")).unwrap_or_default());
        }
    }
    let totals = totals.lock().unwrap();
    let (mut steps, mut input, mut output) = (0, 0, 0);
    for (role, (n, i, o)) in totals.iter() {
        eprintln!("tokens {role}: {n} steps, {i} in, {o} out");
        steps += n;
        input += i;
        output += o;
    }
    eprintln!(
        "tokens total: {steps} steps, {input} in, {output} out, {:.0} s",
        host.started.elapsed().as_secs_f64()
    );
    assert!(out.is_ok() || matches!(out, Err(marlowe_crew::CrewError::Step { ref step, .. }) if step == "verify"));
}
