//! Deep research v2 values on the wire: the plan a run proposes, its cost quote, the user's
//! decision on it, and per-stage progress for the Run view. Shared by the daemon,
//! `marlowe-crew`, `marlowe-research` and the clients.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A research source channel. Keyless by construction: no channel carries a credential
/// (ADR-036 §2). `Memory` is the user's own store, recalled through the Mind and kept in its own
/// trust bucket; every other channel returns untrusted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelId {
    Arxiv,
    OpenAlex,
    Crossref,
    SemanticScholar,
    Wikipedia,
    Wikidata,
    Github,
    HackerNews,
    StackExchange,
    Pubmed,
    Rss,
    /// General web search: SearXNG when configured, else DuckDuckGo HTML lite, else a hosted
    /// search the daemon supplies.
    Web,
    Memory,
}

impl ChannelId {
    /// Every channel, in display order.
    pub const ALL: [ChannelId; 13] = [
        ChannelId::Arxiv,
        ChannelId::OpenAlex,
        ChannelId::Crossref,
        ChannelId::SemanticScholar,
        ChannelId::Wikipedia,
        ChannelId::Wikidata,
        ChannelId::Github,
        ChannelId::HackerNews,
        ChannelId::StackExchange,
        ChannelId::Pubmed,
        ChannelId::Rss,
        ChannelId::Web,
        ChannelId::Memory,
    ];

    /// Stable, model-visible name.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelId::Arxiv => "arxiv",
            ChannelId::OpenAlex => "open_alex",
            ChannelId::Crossref => "crossref",
            ChannelId::SemanticScholar => "semantic_scholar",
            ChannelId::Wikipedia => "wikipedia",
            ChannelId::Wikidata => "wikidata",
            ChannelId::Github => "github",
            ChannelId::HackerNews => "hacker_news",
            ChannelId::StackExchange => "stack_exchange",
            ChannelId::Pubmed => "pubmed",
            ChannelId::Rss => "rss",
            ChannelId::Web => "web",
            ChannelId::Memory => "memory",
        }
    }

    /// The name a person reads: `Semantic Scholar`, `arXiv`.
    pub fn label(self) -> &'static str {
        match self {
            ChannelId::Arxiv => "arXiv",
            ChannelId::OpenAlex => "OpenAlex",
            ChannelId::Crossref => "Crossref",
            ChannelId::SemanticScholar => "Semantic Scholar",
            ChannelId::Wikipedia => "Wikipedia",
            ChannelId::Wikidata => "Wikidata",
            ChannelId::Github => "GitHub",
            ChannelId::HackerNews => "Hacker News",
            ChannelId::StackExchange => "Stack Exchange",
            ChannelId::Pubmed => "PubMed",
            ChannelId::Rss => "RSS",
            ChannelId::Web => "Web",
            ChannelId::Memory => "Memory",
        }
    }

    /// False only for `Memory`, which is read through the Mind, never the network.
    pub fn is_network(self) -> bool {
        self != ChannelId::Memory
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not a channel.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown research channel '{0}'")]
pub struct ChannelParseError(pub String);

impl FromStr for ChannelId {
    type Err = ChannelParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let k = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let alias = match k.as_str() {
            "openalex" => "open_alex",
            "s2" | "semanticscholar" => "semantic_scholar",
            "hn" | "hackernews" => "hacker_news",
            "stackexchange" | "stackoverflow" | "stack_overflow" => "stack_exchange",
            "atom" | "feed" => "rss",
            "searxng" | "search" | "web_search" => "web",
            other => other,
        };
        ChannelId::ALL.into_iter().find(|c| c.as_str() == alias).ok_or_else(|| ChannelParseError(s.to_string()))
    }
}

/// One sub-question of a plan, with the channels routed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubQuestion {
    /// 1-based, stable across edits; checkpoint keys use it.
    pub id: u32,
    pub question: String,
    pub channels: Vec<ChannelId>,
    /// Why these channels, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// Short keyword queries for the channels (named methods, model families, benchmarks), as
    /// a search engine would match them. Empty: the question itself is searched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<String>,
}

/// Bounds a plan must respect. An edited plan outside them is refused, not clamped.
pub const PLAN_MAX_SUBQUESTIONS: usize = 12;
pub const PLAN_MAX_BREADTH: u32 = 8;
pub const PLAN_MAX_DEPTH: u32 = 4;
pub const PLAN_MAX_SOURCES: u32 = 500;
pub const PLAN_MAX_QUERIES: usize = 6;

/// What a deep research run intends to do, composed in a clean run before any channel is read
/// (ADR-043 §5) and approved by the user before it spends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchPlan {
    pub goal: String,
    pub sub_questions: Vec<SubQuestion>,
    /// Search results selected per (sub-)question.
    pub breadth: u32,
    /// Follow-up levels below each sub-question (0: no follow-ups).
    pub depth: u32,
    /// Most sources fetched over the whole run.
    pub source_budget: u32,
    /// Memory ids recalled from the Mind that informed the plan (their own trust bucket).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recalled: Vec<String>,
    /// The researcher's notes for the user: scope, assumptions, what is out of scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Why a plan is not runnable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("the plan has no goal")]
    NoGoal,
    #[error("the plan has no sub-questions")]
    NoSubQuestions,
    #[error("the plan has {0} sub-questions (at most {PLAN_MAX_SUBQUESTIONS})")]
    TooManySubQuestions(usize),
    #[error("sub-question {0} is empty")]
    EmptySubQuestion(u32),
    #[error("sub-question {0} names no channel")]
    NoChannels(u32),
    #[error("sub-question id {0} is used twice")]
    DuplicateId(u32),
    #[error("breadth {0} is outside 1..={PLAN_MAX_BREADTH}")]
    Breadth(u32),
    #[error("depth {0} is above {PLAN_MAX_DEPTH}")]
    Depth(u32),
    #[error("source budget {0} is outside 1..={PLAN_MAX_SOURCES}")]
    SourceBudget(u32),
    #[error("sub-question {0} has more than {PLAN_MAX_QUERIES} queries or one over 200 characters")]
    Queries(u32),
}

impl ResearchPlan {
    /// Checks the plan is runnable as written.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.goal.trim().is_empty() {
            return Err(PlanError::NoGoal);
        }
        if self.sub_questions.is_empty() {
            return Err(PlanError::NoSubQuestions);
        }
        if self.sub_questions.len() > PLAN_MAX_SUBQUESTIONS {
            return Err(PlanError::TooManySubQuestions(self.sub_questions.len()));
        }
        let mut seen = Vec::with_capacity(self.sub_questions.len());
        for q in &self.sub_questions {
            if seen.contains(&q.id) {
                return Err(PlanError::DuplicateId(q.id));
            }
            seen.push(q.id);
            if q.question.trim().is_empty() {
                return Err(PlanError::EmptySubQuestion(q.id));
            }
            if q.channels.is_empty() {
                return Err(PlanError::NoChannels(q.id));
            }
            if q.queries.len() > PLAN_MAX_QUERIES || q.queries.iter().any(|s| s.chars().count() > 200) {
                return Err(PlanError::Queries(q.id));
            }
        }
        if !(1..=PLAN_MAX_BREADTH).contains(&self.breadth) {
            return Err(PlanError::Breadth(self.breadth));
        }
        if self.depth > PLAN_MAX_DEPTH {
            return Err(PlanError::Depth(self.depth));
        }
        if !(1..=PLAN_MAX_SOURCES).contains(&self.source_budget) {
            return Err(PlanError::SourceBudget(self.source_budget));
        }
        Ok(())
    }

    /// Distinct channels the plan uses, in `ChannelId` order.
    pub fn channels(&self) -> Vec<ChannelId> {
        let mut out: Vec<ChannelId> = self.sub_questions.iter().flat_map(|q| q.channels.iter().copied()).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// The estimated cost of running a plan, shown with it for approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostQuote {
    /// Model tokens, input plus output, over every stage.
    pub model_tokens: u64,
    /// Channel queries plus page fetches.
    pub fetches: u32,
    /// Wall-clock estimate.
    pub minutes: u32,
    /// Hosted-model spend, when a hosted model is in a slot the run uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    /// One line: how the numbers were derived.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub basis: String,
}

/// The user's answer to a proposed plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanDecision {
    Approve,
    /// Run this plan instead (validated before it is accepted).
    Edit {
        plan: ResearchPlan,
    },
    Reject {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// A deep research v2 stage, in run order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchStage {
    Plan,
    Explore,
    Compress,
    Corroborate,
    Verify,
    Synthesise,
    Remember,
}

impl ResearchStage {
    pub const ALL: [ResearchStage; 7] = [
        ResearchStage::Plan,
        ResearchStage::Explore,
        ResearchStage::Compress,
        ResearchStage::Corroborate,
        ResearchStage::Verify,
        ResearchStage::Synthesise,
        ResearchStage::Remember,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ResearchStage::Plan => "plan",
            ResearchStage::Explore => "explore",
            ResearchStage::Compress => "compress",
            ResearchStage::Corroborate => "corroborate",
            ResearchStage::Verify => "verify",
            ResearchStage::Synthesise => "synthesise",
            ResearchStage::Remember => "remember",
        }
    }

    /// 1-based position, for `stage 3/7`.
    pub fn ordinal(self) -> u32 {
        ResearchStage::ALL.iter().position(|s| *s == self).map_or(0, |i| i as u32 + 1)
    }
}

/// Where a stage is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    #[default]
    Started,
    Running,
    Done,
    /// Waiting on the user (the plan stage after `PlanProposed`).
    NeedsYou,
    Failed,
    Skipped,
}

/// Running totals for a research run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchCounts {
    #[serde(default)]
    pub queries: u32,
    /// Distinct sources after identity dedup.
    #[serde(default)]
    pub sources_found: u32,
    #[serde(default)]
    pub sources_fetched: u32,
    /// Fetched but not readable (robots, render failure, no text).
    #[serde(default)]
    pub sources_unreadable: u32,
    #[serde(default)]
    pub claims: u32,
    #[serde(default)]
    pub independent_roots: u32,
    #[serde(default)]
    pub contradictions: u32,
    #[serde(default)]
    pub citations_supported: u32,
    #[serde(default)]
    pub citations_partial: u32,
    #[serde(default)]
    pub citations_unsupported: u32,
}

/// One progress report from a research run's stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchProgress {
    pub stage: ResearchStage,
    pub state: StageState,
    /// Units done and expected in this stage (sub-questions, sources, claims); 0/0 if unknown.
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub total: u32,
    /// One line for the Run view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub counts: ResearchCounts,
}
