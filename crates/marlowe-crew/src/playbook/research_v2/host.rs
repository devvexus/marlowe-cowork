//! What deep research v2 needs beyond agent steps: channel search and page fetching
//! ([`ResearchIo`]), and the run around it ([`ResearchHost`]): plan approval, stage progress,
//! data checkpoints, the user's profile and memory, and Result memories.

use std::sync::Arc;

use marlowe_extract::Link;
use marlowe_model::CancelToken;
use marlowe_proto::{ChannelId, CostQuote, MemoryTrust, PlanDecision, ResearchPlan, ResearchProgress, RunId};
use marlowe_research::{ChannelCtx, ChannelError, ChannelQuery, ChannelRegistry, HttpGet, ResearchCache, SourceRef};

use super::types::Finding;
use crate::CrewError;

/// One page the corpus fetcher read (or could not).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedPage {
    /// As requested.
    pub url: String,
    /// After redirects.
    pub final_url: String,
    pub title: Option<String>,
    /// Main content as markdown (headings, links, tables, code kept); empty when unreadable.
    pub text: String,
    /// Links found on the page, resolved against it. They become the page's link table.
    pub links: Vec<Link>,
    /// The page's scholarly identity `<meta>` tags (`citation_doi`, `citation_title`, ...).
    pub meta: Vec<(String, String)>,
    /// Why it could not be read: robots.txt, HTTP status, render failure, no text.
    pub error: Option<String>,
}

/// The corpus fetcher: politeness (robots.txt, per-host spacing), extraction and the JS-render
/// escalation live behind it.
pub trait PageFetcher: Send + Sync {
    /// Fetches `urls` with at most `concurrency` in flight. One result per url, in order.
    fn fetch(&self, urls: &[String], concurrency: usize, cancel: &CancelToken) -> Vec<FetchedPage>;
}

/// Channel search and page fetching for a research run.
pub trait ResearchIo: Send + Sync {
    /// Channels that can be searched (`Memory` is the host's, never listed here).
    fn channels(&self) -> Vec<ChannelId> {
        ChannelId::ALL.into_iter().filter(|c| c.is_network()).collect()
    }

    fn search(
        &self,
        channel: ChannelId,
        query: &ChannelQuery,
        cancel: &CancelToken,
    ) -> Result<Vec<SourceRef>, ChannelError>;

    /// One result per url, in order (see [`PageFetcher::fetch`]).
    fn fetch(&self, urls: &[String], concurrency: usize, cancel: &CancelToken) -> Vec<FetchedPage>;

    /// Channels that cannot answer now, with why (a web channel with no live backend). Checked
    /// before planning, cheaply; a plan never routes to them.
    fn unavailable(&self, _cancel: &CancelToken) -> Vec<(ChannelId, String)> {
        Vec::new()
    }
}

/// The product [`ResearchIo`]: the keyless channel registry (rate limits and caches inside it)
/// and the corpus fetcher.
pub struct ChannelIo {
    pub registry: Arc<ChannelRegistry>,
    pub http: Arc<dyn HttpGet>,
    pub cache: Option<Arc<ResearchCache>>,
    pub pages: Arc<dyn PageFetcher>,
}

impl ResearchIo for ChannelIo {
    fn channels(&self) -> Vec<ChannelId> {
        self.registry.ids().into_iter().filter(|c| c.is_network()).collect()
    }

    fn search(
        &self,
        channel: ChannelId,
        query: &ChannelQuery,
        cancel: &CancelToken,
    ) -> Result<Vec<SourceRef>, ChannelError> {
        let ctx = ChannelCtx { http: self.http.as_ref(), cache: self.cache.as_deref(), cancel };
        self.registry.search(channel, query, &ctx)
    }

    fn fetch(&self, urls: &[String], concurrency: usize, cancel: &CancelToken) -> Vec<FetchedPage> {
        self.pages.fetch(urls, concurrency, cancel)
    }

    /// Channels the settings left out, and web search when no backend answers its probe.
    fn unavailable(&self, cancel: &CancelToken) -> Vec<(ChannelId, String)> {
        let mut out: Vec<(ChannelId, String)> = self.registry.skipped().to_vec();
        if self.registry.ids().contains(&ChannelId::Web) {
            let ctx = ChannelCtx { http: self.http.as_ref(), cache: self.cache.as_deref(), cancel };
            if let Err(why) = self.registry.ready(ChannelId::Web, &ctx) {
                out.push((ChannelId::Web, why));
            }
        }
        out
    }
}

/// A memory recalled for research. It is a source in its own trust bucket, never instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recalled {
    pub id: String,
    pub text: String,
    /// Human-readable origin (`you, session 3f2a…`).
    pub provenance: String,
    /// Its trust in the store. A finding resting on an untrusted memory is untrusted like a web
    /// source; an inferred one is capped by lineage (the memory id is the finding's parent).
    pub trust: MemoryTrust,
}

impl Recalled {
    /// Not untrusted content.
    pub fn trusted(&self) -> bool {
        self.trust != MemoryTrust::Untrusted
    }
}

/// The run a research playbook runs in. The supervisor implements it; tests script it.
pub trait ResearchHost: Send + Sync {
    fn run_id(&self) -> RunId;

    fn io(&self) -> &dyn ResearchIo;

    /// The run's cancel token: searches and fetches stop on it.
    fn cancel(&self) -> &CancelToken;

    /// `Err` when the run was cancelled, paused or is shutting down.
    fn check(&self) -> Result<(), CrewError> {
        if self.cancel().is_cancelled() {
            Err(CrewError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// A stage moved.
    fn progress(&self, progress: &ResearchProgress);

    /// Shows the plan and its quote and returns the user's decision. The supervisor's host does
    /// not wait for it: it parks the run (`Err(Stopped)`) and the answer, given later, is returned
    /// here when the run starts again. `Err` on cancel, pause or shutdown.
    fn propose_plan(&self, plan: &ResearchPlan, quote: &CostQuote) -> Result<PlanDecision, CrewError>;

    /// The plan was approved, by the user or under their auto-approve threshold: the run's
    /// budget is raised to cover `quote` and `steps` model calls (never lowered).
    fn plan_approved(&self, _plan: &ResearchPlan, _quote: &CostQuote, _steps: u32) {}

    /// A data checkpoint stored under `key` by an earlier attempt of this run.
    fn load(&self, key: &str) -> Option<String>;

    /// Stores a data checkpoint; `label` is the transcript line.
    fn save(&self, key: &str, label: &str, data: &str);

    /// The user's profile block, when there is one.
    fn profile(&self) -> Option<String> {
        None
    }

    /// Memories recalled for `query`.
    fn recall(&self, _query: &str, _limit: usize) -> Vec<Recalled> {
        Vec::new()
    }

    /// Untrusted content (channel results, pages) has entered the run.
    fn taint(&self);

    /// The run's taint latch: untrusted content entered it at some point (this run's searches,
    /// or before it was dispatched). Everything the run remembers is then untrusted.
    fn tainted(&self) -> bool;

    /// Stores a finding as a Result memory with the run's provenance; returns its id.
    fn remember(&self, finding: &Finding, tainted: bool) -> Result<String, String>;
}
