//! Deep research v2 stage types: the config, the cost quote, the plan gate, the explore tree and
//! source budget, and what the compress, verify and synthesise stages hand on.

use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use marlowe_proto::{ChannelId, Confidence, CostQuote, PlanDecision, PlanError, ResearchPlan, ResearchStage, RunId};
use marlowe_research::{Claim, ClaimId, Corroboration, DocRef, Identity, SourceId, SourceRef, Standing};

/// Deep research v2 settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchV2Config {
    /// Plans quoted under this many model tokens start without asking. Off by default.
    pub auto_approve_under_tokens: Option<u64>,
    /// Concurrent page fetches across all scouts.
    pub fetch_concurrency: usize,
    /// Follow-up sub-questions one explore node may spawn.
    pub max_followups_per_node: u32,
    /// Claims the compression role may return per source.
    pub max_claims_per_source: u32,
}

impl Default for ResearchV2Config {
    fn default() -> Self {
        Self {
            auto_approve_under_tokens: None,
            fetch_concurrency: 16,
            max_followups_per_node: 2,
            max_claims_per_source: 12,
        }
    }
}

/// Per-unit estimates behind a [`CostQuote`]. Calibrated from measured runs, not asserted.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteModel {
    pub plan_tokens: u64,
    /// Scout tokens per query (selection over results).
    pub tokens_per_query: u64,
    /// Compression tokens per fetched source.
    pub tokens_per_source: u64,
    /// Verification tokens per fetched source (claims and citations scale with sources).
    pub verify_tokens_per_source: u64,
    pub synthesis_tokens: u64,
    pub seconds_per_fetch: f64,
    pub seconds_per_kilotoken: f64,
}

/// Measured on marlowe-dusk:27b-super (RTX 4080 SUPER, 2026-10-09, `tests/research_v2_live.rs`):
/// 29 steps, 37.7k tokens in and 92.5k out (mostly thinking) in 1,523 s, about 11.7 s per
/// kilotoken; compression about 3.5k and verification about 6k tokens per source on short pages.
/// Per-source figures are raised for real pages (a 14k-character chunk is about 3.5k tokens in).
/// A cloud model is several times faster per token, so for one the minutes read high.
impl Default for QuoteModel {
    fn default() -> Self {
        Self {
            plan_tokens: 6_000,
            tokens_per_query: 3_000,
            tokens_per_source: 6_000,
            verify_tokens_per_source: 5_000,
            synthesis_tokens: 30_000,
            seconds_per_fetch: 1.5,
            seconds_per_kilotoken: 11.0,
        }
    }
}

/// Nodes an explore tree of breadth `b`, depth `d` and `followups` per node can reach per root.
fn nodes_per_root(depth: u32, followups: u32) -> u64 {
    (0..=depth).map(|level| (followups as u64).pow(level)).sum()
}

/// Prices a plan: every bound the plan sets is an upper bound here.
pub fn quote(plan: &ResearchPlan, cfg: &ResearchV2Config, m: &QuoteModel) -> CostQuote {
    let nodes = plan.sub_questions.len() as u64 * nodes_per_root(plan.depth, cfg.max_followups_per_node);
    let queries: u64 =
        plan.sub_questions.iter().map(|q| q.channels.iter().filter(|c| c.is_network()).count() as u64).sum::<u64>()
            * nodes_per_root(plan.depth, cfg.max_followups_per_node);
    let sources = (nodes * plan.breadth as u64).min(plan.source_budget as u64);
    let tokens = m.plan_tokens
        + queries * m.tokens_per_query
        + sources * (m.tokens_per_source + m.verify_tokens_per_source)
        + m.synthesis_tokens;
    let fetches = (queries + sources) as u32;
    let secs = fetches as f64 * m.seconds_per_fetch / cfg.fetch_concurrency.max(1) as f64
        + tokens as f64 / 1000.0 * m.seconds_per_kilotoken;
    CostQuote {
        model_tokens: tokens,
        fetches,
        minutes: (secs / 60.0).ceil() as u32,
        usd: None,
        basis: format!(
            "{} nodes x b{} (budget {}), {queries} queries, {sources} sources",
            nodes, plan.breadth, plan.source_budget
        ),
    }
}

/// Model calls a plan is expected to make (typical claim yield, retry headroom on verification).
/// Approving a plan raises the run's steps budget to this, as approving its quote does tokens.
pub fn step_estimate(plan: &ResearchPlan, cfg: &ResearchV2Config) -> u32 {
    let subs = plan.sub_questions.len() as u64;
    let nodes = subs * nodes_per_root(plan.depth, cfg.max_followups_per_node);
    let sources = (nodes * plan.breadth as u64).min(plan.source_budget as u64);
    let claims = sources * (cfg.max_claims_per_source as u64).min(6);
    let verify = (claims * 7 / 10).div_ceil(8) + claims.div_ceil(12);
    let audit = (subs * 8).div_ceil(6);
    let steps = 1 + nodes + sources * 2 + claims.div_ceil(100) + verify * 3 / 2 + subs + 1 + audit * 3 / 2;
    steps.min(u32::MAX as u64) as u32
}

/// Whether a proposed plan waits for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanGate {
    AutoApproved,
    AwaitUser,
}

pub fn gate(quote: &CostQuote, cfg: &ResearchV2Config) -> PlanGate {
    match cfg.auto_approve_under_tokens {
        Some(n) if quote.model_tokens < n => PlanGate::AutoApproved,
        _ => PlanGate::AwaitUser,
    }
}

/// The plan to run after the user's decision: `Ok(None)` when rejected. An edit must validate
/// and keep the goal.
pub fn apply_decision(proposed: ResearchPlan, decision: PlanDecision) -> Result<Option<ResearchPlan>, PlanError> {
    match decision {
        PlanDecision::Approve => {
            proposed.validate()?;
            Ok(Some(proposed))
        }
        PlanDecision::Edit { mut plan } => {
            if plan.goal.trim().is_empty() {
                plan.goal = proposed.goal;
            }
            plan.validate()?;
            Ok(Some(plan))
        }
        PlanDecision::Reject { .. } => Ok(None),
    }
}

/// Sources the whole run may fetch, shared by every parallel scout.
#[derive(Debug)]
pub struct SourceBudget {
    limit: u32,
    used: AtomicU32,
}

impl SourceBudget {
    pub fn new(limit: u32) -> Self {
        Self { limit, used: AtomicU32::new(0) }
    }

    /// Takes up to `want` slots; returns how many were granted.
    pub fn take(&self, want: u32) -> u32 {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            let grant = want.min(self.limit.saturating_sub(cur));
            if grant == 0 {
                return 0;
            }
            match self.used.compare_exchange(cur, cur + grant, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return grant,
                Err(now) => cur = now,
            }
        }
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used())
    }
}

/// An explore-tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

/// One (sub-)question being explored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExploreNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    /// The plan sub-question this descends from.
    pub sub_question: u32,
    pub question: String,
    pub depth: u32,
    pub channels: Vec<ChannelId>,
    /// Sources selected for this node, after dedup.
    #[serde(default)]
    pub sources: Vec<SourceId>,
    /// A citation-chasing node: it searches the citation graph around its parent's papers
    /// instead of the question text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chase: bool,
}

/// Why a follow-up was not added.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExploreError {
    #[error("no node {0:?}")]
    UnknownNode(NodeId),
    #[error("depth {0} is the plan's limit")]
    TooDeep(u32),
    #[error("node {0:?} already has its follow-ups")]
    FollowupsSpent(NodeId),
    #[error("the follow-up repeats an explored question")]
    Duplicate,
}

/// The breadth-b, depth-d exploration of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExploreTree {
    pub breadth: u32,
    pub depth: u32,
    pub max_followups: u32,
    nodes: Vec<ExploreNode>,
}

impl ExploreTree {
    /// One root per plan sub-question.
    pub fn from_plan(plan: &ResearchPlan, cfg: &ResearchV2Config) -> Self {
        let nodes = plan
            .sub_questions
            .iter()
            .enumerate()
            .map(|(i, q)| ExploreNode {
                id: NodeId(i as u32),
                parent: None,
                sub_question: q.id,
                question: q.question.clone(),
                depth: 0,
                channels: q.channels.clone(),
                sources: Vec::new(),
                chase: false,
            })
            .collect();
        Self { breadth: plan.breadth, depth: plan.depth, max_followups: cfg.max_followups_per_node, nodes }
    }

    pub fn nodes(&self) -> &[ExploreNode] {
        &self.nodes
    }

    pub fn get(&self, id: NodeId) -> Option<&ExploreNode> {
        self.nodes.get(id.0 as usize)
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut ExploreNode> {
        self.nodes.get_mut(id.0 as usize)
    }

    /// Adds a follow-up from a gap found at `parent`, inheriting its channels.
    pub fn add_followup(&mut self, parent: NodeId, question: &str) -> Result<NodeId, ExploreError> {
        let p = self.get(parent).ok_or(ExploreError::UnknownNode(parent))?.clone();
        if p.depth >= self.depth {
            return Err(ExploreError::TooDeep(self.depth));
        }
        if self.nodes.iter().filter(|n| n.parent == Some(parent)).count() as u32 >= self.max_followups {
            return Err(ExploreError::FollowupsSpent(parent));
        }
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if self.nodes.iter().any(|n| norm(&n.question) == norm(question)) {
            return Err(ExploreError::Duplicate);
        }
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(ExploreNode {
            id,
            parent: Some(parent),
            sub_question: p.sub_question,
            question: question.trim().to_string(),
            depth: p.depth + 1,
            channels: p.channels,
            sources: Vec::new(),
            chase: false,
        });
        Ok(id)
    }

    /// Nodes at `depth`, in id order: one explore wave.
    pub fn wave(&self, depth: u32) -> Vec<NodeId> {
        self.nodes.iter().filter(|n| n.depth == depth).map(|n| n.id).collect()
    }
}

/// A source the explore stage fetched (or failed to read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchedSource {
    pub source: SourceId,
    pub node: NodeId,
    pub url: String,
    pub title: String,
    /// Content address of the stored document; `None` when unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<DocRef>,
    #[serde(default)]
    pub chars: u32,
    /// Why it could not be read (robots, render failure, no text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,
    /// What the page says it is (its `citation_*` tags, arXiv's stamp): a claim, paired with the
    /// work it names before corroboration so two copies of one paper are one root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<SourceRef>,
}

/// What the compression role returned for one source. Claims whose quote is not verbatim in
/// the stored document are dropped and counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressedSource {
    pub source: SourceId,
    pub doc: DocRef,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub rejected_quotes: u32,
}

/// Does the cited passage support the sentence?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationSupport {
    Supported,
    Partial,
    Unsupported,
}

/// One cited sentence checked against its re-read stored passage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationCheck {
    pub sentence: String,
    pub claim: ClaimId,
    pub support: CitationSupport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The adversarial reviewer's verdict on a claim group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum ClaimVerdict {
    Upheld,
    Weakened(String),
    Refuted(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimReview {
    pub group: u32,
    pub verdict: ClaimVerdict,
}

/// The VERIFY stage's output: both halves are required.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub reviews: Vec<ClaimReview>,
    pub citations: Vec<CitationCheck>,
}

impl Verification {
    /// Supported citations over all checked citations (partial counts half). `None` if none.
    pub fn citation_accuracy(&self) -> Option<f64> {
        if self.citations.is_empty() {
            return None;
        }
        let score: f64 = self
            .citations
            .iter()
            .map(|c| match c.support {
                CitationSupport::Supported => 1.0,
                CitationSupport::Partial => 0.5,
                CitationSupport::Unsupported => 0.0,
            })
            .sum();
        Some(score / self.citations.len() as f64)
    }

    /// Supported citations over all checked, partial counting as not supported (the research
    /// eval's rule). `None` if none.
    pub fn strict_accuracy(&self) -> Option<f64> {
        let n = self.citations.len();
        (n > 0).then(|| {
            self.citations.iter().filter(|c| c.support == CitationSupport::Supported).count() as f64 / n as f64
        })
    }

    /// Both readings, to three decimals: `0.897 strict, 0.949 with partial as half`.
    pub fn accuracy_line(&self) -> String {
        match (self.strict_accuracy(), self.citation_accuracy()) {
            (Some(s), Some(h)) => format!("{s:.3} strict, {h:.3} with partial as half"),
            _ => "n/a".into(),
        }
    }
}

/// A run that cannot produce a report, naming what is missing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no report: verification is incomplete ({})", missing.join("; "))]
pub struct VerificationRefusal {
    pub missing: Vec<String>,
}

/// No verification, no report (ADR-037 §5).
pub fn require_verification(v: Option<&Verification>, groups: usize) -> Result<&Verification, VerificationRefusal> {
    let Some(v) = v else {
        return Err(VerificationRefusal { missing: vec!["the verify stage did not run".into()] });
    };
    let mut missing = Vec::new();
    if v.reviews.len() < groups {
        missing.push(format!("{} of {groups} claim groups were not reviewed", groups - v.reviews.len()));
    }
    if v.citations.is_empty() {
        missing.push("no citation was checked against its stored passage".into());
    }
    if missing.is_empty() {
        Ok(v)
    } else {
        Err(VerificationRefusal { missing })
    }
}

/// A numbered reference with its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub n: u32,
    pub source: SourceId,
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
}

/// Evidence for one plan sub-question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSection {
    pub sub_question: u32,
    pub heading: String,
    /// Markdown with `[n]` citations, claims marked established / suggested / inferred.
    pub body: String,
}

/// The SYNTHESISE stage's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The answer, first.
    pub answer: String,
    pub sections: Vec<ReportSection>,
    /// Contested claim groups with the reason.
    #[serde(default)]
    pub contested: Vec<(u32, String)>,
    pub references: Vec<Reference>,
    pub confidence: Confidence,
}

/// How much of the planned ground a run covered. Thin coverage caps confidence at Low and is
/// said at the top of the report and in the dispatch summary: a run never reads as settled when
/// it went in half-blind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub sources_read: u32,
    /// What the run could have read: the source budget, or explored nodes x breadth if fewer.
    pub sources_expected: u32,
    pub queries: u32,
    pub queries_failed: u32,
    /// Sub-questions for which every channel query failed.
    pub blind_sub_questions: Vec<u32>,
    /// Sections with no evidence of their own.
    pub empty_sections: Vec<u32>,
    /// Sections (0 for the answer) still cut off at the output limit after the retry.
    pub truncated_sections: Vec<u32>,
    /// Channels left out before planning, with why.
    pub unavailable: Vec<String>,
}

impl Coverage {
    /// Read under this share of what the run could read is thin.
    pub const MIN_READ_SHARE: f64 = 0.6;
    /// More than this share of channel queries failing is thin.
    pub const MAX_FAILED_SHARE: f64 = 0.25;

    /// Why the run's coverage caps its confidence, one line each; empty when it does not.
    pub fn caps(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.sources_expected > 0 && (self.sources_read as f64) < Self::MIN_READ_SHARE * self.sources_expected as f64 {
            out.push(format!("{} of {} planned sources were read", self.sources_read, self.sources_expected));
        }
        if self.queries > 0 && self.queries_failed as f64 > Self::MAX_FAILED_SHARE * self.queries as f64 {
            out.push(format!("{} of {} channel queries failed", self.queries_failed, self.queries));
        }
        let list = |v: &[u32]| v.iter().map(|q| q.to_string()).collect::<Vec<_>>().join(", ");
        if !self.blind_sub_questions.is_empty() {
            out.push(format!("every query failed for sub-question {}", list(&self.blind_sub_questions)));
        }
        if !self.empty_sections.is_empty() {
            out.push(format!("no evidence of its own for section {}", list(&self.empty_sections)));
        }
        if !self.truncated_sections.is_empty() {
            let named: Vec<String> = self
                .truncated_sections
                .iter()
                .map(|s| if *s == 0 { "the answer".to_string() } else { format!("section {s}") })
                .collect();
            out.push(format!("{} cut off at the model's output limit", named.join(", ")));
        }
        out
    }

    /// Everything the reader should know first: the caps, then channels that were not searched.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = self.caps();
        out.extend(self.unavailable.iter().map(|u| format!("not searched: {u}")));
        out
    }
}

/// Confidence from verification, corroboration and coverage: High needs citation accuracy >=
/// 0.95 and most groups established; Low when accuracy is under 0.8, nothing is established,
/// or coverage is thin ([`Coverage::caps`]).
pub fn derive_confidence(corroboration: &[Corroboration], v: &Verification, coverage: &Coverage) -> Confidence {
    let acc = v.citation_accuracy().unwrap_or(0.0);
    let refuted = v.reviews.iter().filter(|r| matches!(r.verdict, ClaimVerdict::Refuted(_))).count();
    let total = corroboration.len().max(1);
    let established = corroboration.iter().filter(|c| c.standing == Standing::Established).count();
    if acc < 0.8 || established == 0 || refuted * 4 > total || !coverage.caps().is_empty() {
        Confidence::Low
    } else if acc >= 0.95 && established * 2 > total && refuted == 0 {
        Confidence::High
    } else {
        Confidence::Medium
    }
}

/// One finding to store as a Result memory, with run provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub run: RunId,
    pub text: String,
    pub standing: Standing,
    pub references: Vec<u32>,
    /// Ids of recalled memories it rests on: its lineage in the store.
    #[serde(default)]
    pub derived_from: Vec<String>,
    /// URLs (or `memory:<id>`) of the sources it rests on.
    #[serde(default)]
    pub sources: Vec<String>,
}

/// The checkpoint key for a stage (and an optional part, e.g. a node id).
pub fn checkpoint_key(stage: ResearchStage, part: Option<&str>) -> String {
    match part {
        Some(p) => format!("v2:{}:{p}", stage.as_str()),
        None => format!("v2:{}", stage.as_str()),
    }
}
