//! Deep research v2. Seven stages, each checkpointed and reported as `ResearchProgress`:
//!
//! 1. PLAN (clean run: no tools, nothing fetched): the Researcher drafts a [`ResearchPlan`] from
//!    the goal, the user's profile and Mind recall; [`quote`] prices it; the run waits on the
//!    user (`PlanProposed`) unless [`gate`] auto-approves (off by default).
//! 2. EXPLORE: a breadth-b, depth-d [`ExploreTree`]. Channels are queried concurrently, results
//!    deduplicated by identity, scouts SELECT by number (from results and from link tables of
//!    pages already read), the corpus fetcher reads selections at `fetch_concurrency` against
//!    one [`SourceBudget`] shared by every scout, and gaps become bounded follow-ups.
//! 3. COMPRESS (Quick slot): every stored source, in chunks, becomes claims with verbatim quotes
//!    and locators. Nothing else about a source reaches the synthesiser.
//! 4. CORROBORATE: claims grouped with stance, counted over independent roots of the derivation
//!    graph (three posts quoting one paper are one root).
//! 5. VERIFY (required): adversarial claim review plus a support check of every claim against
//!    its re-read stored passage. Incomplete verification is a refusal naming what is missing,
//!    never a report.
//! 6. SYNTHESISE: sections per sub-question and the answer, cited by claim id; every cited
//!    sentence is checked again against its passages, unsupported ones removed; numbered
//!    references with identities; confidence from verification and corroboration.
//! 7. REMEMBER: the answer and the strongest established findings as Result memories with the
//!    run's provenance.
//!
//! Everything a channel or page returns is untrusted: the run is tainted from the first search,
//! fetched text only reaches prompts fenced as data, and the model never writes a URL.
//!
//! Output budget: every step is sized ([`FIRST_OUTPUT`], lowered by the runner to the room its
//! prompt leaves in the window). A local thinking model spends that budget on reasoning first,
//! so a step that comes back empty or cut off is asked again with twice the budget and the same
//! effort; nothing is ever re-asked with reasoning off (a verifier that does not think
//! rubber-stamps). Coverage ([`Coverage`]) is measured as the run goes and caps confidence.

mod bundle;
mod compress;
mod explore;
mod host;
mod parse;
mod plan;
mod queries;
mod remember;
mod standing;
mod synth;
mod types;
mod verify;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_types;

use std::path::PathBuf;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::Serialize;

use marlowe_proto::{Dispatch, ResearchCounts, ResearchProgress, ResearchStage, StageState};
use marlowe_research::DocRef;

pub use bundle::{BUNDLE_FILE, BUNDLE_SCHEMA};
pub use compress::{attribution_route, chunks, Compressed, Corroborated, CHUNK_CHARS, MAX_CHUNKS};
pub use explore::{search_text, ExploreState, MIN_READABLE_CHARS};
pub use queries::{looks_like_url, query_text, since_year, QUERIES_PER_CHANNEL};
pub use host::{ChannelIo, FetchedPage, PageFetcher, Recalled, ResearchHost, ResearchIo};
pub use plan::{not_searched, parse_plan, Proposal, NOT_SEARCHED};
pub use synth::{cited_sentences, CUT_NOTE, RECORD_FILE};
pub use verify::VERIFY_OUTPUT;
pub use types::*;

use super::{StepResult, StepRunner, StepSpec};
use crate::CrewError;

/// Where fetched documents are stored, under the run's sandbox.
pub const SOURCES_DIR: &str = "sources";
/// Reply budget of a research step's first attempt, thinking included (Ollama's default for a
/// 32k window is 8,192, which dusk spent on reasoning alone in the live run).
pub const FIRST_OUTPUT: u32 = 12_288;

/// A no-tool research step: the card's effort and a [`FIRST_OUTPUT`] reply budget.
pub(crate) fn step_spec(key: String, role: &str, label: String, prompt: String) -> StepSpec {
    StepSpec {
        key,
        role: role.to_string(),
        label,
        prompt,
        tools: Some(Vec::new()),
        effort: None,
        max_output: Some(FIRST_OUTPUT),
    }
}

/// The budget of a retry: twice the last one.
pub(crate) fn retry_output(last: Option<u32>) -> u32 {
    last.unwrap_or(FIRST_OUTPUT).saturating_mul(2)
}

/// What every stage shares.
pub(crate) struct Ctx<'a> {
    pub runner: &'a dyn StepRunner,
    pub host: &'a dyn ResearchHost,
    pub goal: &'a str,
    pub cfg: &'a ResearchV2Config,
    counts: Mutex<ResearchCounts>,
    /// Model steps that failed (not cancelled), one line each, for the report.
    failures: Mutex<Vec<String>>,
}

impl<'a> Ctx<'a> {
    fn new(runner: &'a dyn StepRunner, host: &'a dyn ResearchHost, goal: &'a str, cfg: &'a ResearchV2Config) -> Self {
        Self {
            runner,
            host,
            goal,
            cfg,
            counts: Mutex::new(ResearchCounts::default()),
            failures: Mutex::new(Vec::new()),
        }
    }

    /// A step's text; a failed step is recorded and reads as empty, cancel and stop propagate.
    pub fn text_of(&self, label: &str, r: Result<StepResult, CrewError>) -> Result<String, CrewError> {
        match r {
            Ok(r) => Ok(r.text),
            Err(e @ (CrewError::Cancelled | CrewError::Stopped)) => Err(e),
            Err(e) => {
                self.failures.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{label}: {e}"));
                Ok(String::new())
            }
        }
    }

    /// Runs `specs` in parallel. A step that fails, or whose answer was cut off at the output
    /// limit, runs once more under `<key>:again` with twice the reply budget and the same effort
    /// (a local model can spend its whole budget thinking and give no answer, or half of one).
    /// A complete retry wins; a failed one keeps the cut-off first answer; when both are cut off
    /// the longer stays, still marked `truncated` for the caller to handle. Cancel and stop are
    /// not retried.
    pub fn parallel(&self, specs: Vec<StepSpec>) -> Vec<Result<StepResult, CrewError>> {
        let mut out = self.runner.parallel(specs.clone());
        let again: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, r)| match r {
                Ok(r) => r.truncated,
                Err(e) => !matches!(e, CrewError::Cancelled | CrewError::Stopped),
            })
            .map(|(i, _)| i)
            .collect();
        if again.is_empty() || self.host.cancel().is_cancelled() {
            return out;
        }
        let retry: Vec<StepSpec> = again
            .iter()
            .map(|&i| StepSpec {
                key: format!("{}:again", specs[i].key),
                max_output: Some(retry_output(specs[i].max_output)),
                ..specs[i].clone()
            })
            .collect();
        for (i, r) in again.into_iter().zip(self.runner.parallel(retry)) {
            let better = match (&out[i], &r) {
                (_, Ok(b)) if !b.truncated => true,
                (Err(_), Ok(_)) => true,
                (Ok(a), Ok(b)) => b.text.trim().len() > a.text.trim().len(),
                _ => false,
            };
            if better {
                out[i] = r;
            }
        }
        out
    }

    /// One step, retried once like [`Ctx::parallel`].
    pub fn step(&self, spec: StepSpec) -> Result<StepResult, CrewError> {
        let key = spec.key.clone();
        self.parallel(vec![spec])
            .pop()
            .unwrap_or_else(|| Err(CrewError::Step { step: key, message: "the step never ran".into() }))
    }

    pub fn failures(&self) -> Vec<String> {
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn counts(&self) -> ResearchCounts {
        *self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn count(&self, f: impl FnOnce(&mut ResearchCounts)) {
        f(&mut self.counts.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Reports a stage's state with the run's running totals.
    pub fn stage(&self, stage: ResearchStage, state: StageState, done: u32, total: u32, detail: Option<String>) {
        let p = ResearchProgress { stage, state, done, total, detail, counts: self.counts() };
        self.host.progress(&p);
    }

    pub fn set_explore_counts(&self, ex: &explore::Explored) {
        let fetched = ex.readable().count() as u32;
        let unreadable = ex.state.fetched.len() as u32 - fetched;
        self.count(|c| {
            c.queries = ex.state.queries;
            c.sources_found = ex.dedup.len() as u32;
            c.sources_fetched = fetched;
            c.sources_unreadable = unreadable;
        });
    }

    pub fn root(&self) -> PathBuf {
        self.runner.sandbox().root.clone()
    }

    fn doc_path(&self, doc: &DocRef) -> PathBuf {
        self.root().join(SOURCES_DIR).join(format!("{}.md", doc.as_str()))
    }

    /// Stores a document under its content address.
    pub fn store_doc(&self, text: &str) -> Result<DocRef, CrewError> {
        let doc = DocRef::of(text.as_bytes());
        let path = self.doc_path(&doc);
        if !path.is_file() {
            std::fs::create_dir_all(self.root().join(SOURCES_DIR))?;
            std::fs::write(&path, text)?;
        }
        Ok(doc)
    }

    /// A stored document, re-read from disk.
    pub fn read_doc(&self, doc: &DocRef) -> Option<String> {
        std::fs::read_to_string(self.doc_path(doc)).ok()
    }

    pub fn load<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.host.load(key).and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn save<T: Serialize>(&self, key: &str, label: &str, value: &T) {
        if let Ok(s) = serde_json::to_string(value) {
            self.host.save(key, label, &s);
        }
    }
}

/// Runs deep research v2 for `goal`. A rejected plan ends the run cancelled; incomplete
/// verification ends it with a refusal (`CrewError::Step` on `verify`) and writes
/// `refusal.md` with what was gathered.
pub fn deep_research_v2(
    runner: &dyn StepRunner,
    host: &dyn ResearchHost,
    goal: &str,
    cfg: &ResearchV2Config,
) -> Result<Dispatch, CrewError> {
    let ctx = Ctx::new(runner, host, goal, cfg);

    let Some(plan) = plan::plan_stage(&ctx)? else {
        return Err(CrewError::Cancelled);
    };
    host.check()?;
    let mut ex = explore::explore(&ctx, &plan)?;
    host.check()?;
    let comp = compress::compress(&ctx, &plan, &ex)?;
    host.check()?;
    let cor = compress::corroborate_stage(&ctx, &plan, &mut ex, &comp)?;
    host.check()?;
    let passages = verify::Passages::new(&ctx);
    let ver = verify::verify_stage(&ctx, &ex, &comp, &cor, &passages)?;
    if let Err(mut refusal) = require_verification(Some(&ver.verification), cor.groups.len()) {
        if comp.claims.is_empty() {
            refusal
                .missing
                .push(format!("no claim with a verbatim quote came out of the {} sources read", ex.readable().count()));
        }
        // Why verdicts are missing, when a call failed rather than leaving items out.
        let mut failed: Vec<String> = Vec::new();
        for f in ctx.failures().into_iter().filter(|f| f.starts_with("verification step")) {
            if !failed.contains(&f) {
                failed.push(f);
            }
        }
        refusal.missing.extend(failed.into_iter().take(3));
        let _ = write_refusal(&ctx, &refusal, &ex);
        ctx.stage(ResearchStage::Verify, StageState::Failed, 0, 0, Some(refusal.to_string()));
        return Err(CrewError::Step { step: "verify".into(), message: refusal.to_string() });
    }
    host.check()?;
    let ev = synth::evidence(&ex, &comp, &cor, &ver);
    let coverage = explore::coverage(&plan, &ex);
    let syn = synth::synthesise(&ctx, &plan, &ex, &comp, &ev, &ver, &passages, coverage)?;
    let (s, p, u) = verify::tally(&syn.report_checks.citations);
    ctx.count(|c| {
        c.citations_supported = s;
        c.citations_partial = p;
        c.citations_unsupported = u;
    });
    let dispatch = synth::dispatch(&ctx, &syn, &ex, &ev)?;
    bundle::write_bundle(&ctx, &plan, &syn, &ex, &comp)?;
    remember::remember(&ctx, &syn, &ex, &comp, &ev)?;
    Ok(dispatch)
}

/// `refusal.md`: why there is no report, and what the run did gather.
fn write_refusal(ctx: &Ctx<'_>, refusal: &VerificationRefusal, ex: &explore::Explored) -> Result<(), CrewError> {
    let mut md = format!("# No report: {}\n\n{}\n\nMissing:\n", crate::run::title_from_goal(ctx.goal), refusal);
    for m in &refusal.missing {
        md.push_str(&format!("- {m}\n"));
    }
    md.push_str("\nSources read:\n");
    for f in ex.readable() {
        md.push_str(&format!("- {} <{}>\n", f.title, f.url));
    }
    std::fs::create_dir_all(ctx.root())?;
    std::fs::write(ctx.root().join("refusal.md"), md)?;
    Ok(())
}
