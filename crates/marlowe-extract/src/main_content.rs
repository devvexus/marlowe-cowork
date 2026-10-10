//! Main-content selection over several extractors.
//!
//! Each [`MainContent`] candidate reads one decoded HTML page and returns its main content with
//! whatever it knows about it (confidence, page type). The html.rs [`Baseline`] always runs and is
//! the never-empty fallback; the others (`rs-trafilatura`, `rust-trafilatura`, each behind a cargo
//! feature of the same name) are tried in policy order and the first acceptable reading wins. The
//! chosen text is then mapped back onto html.rs blocks ([`align`]) so the markdown keeps links,
//! headings, tables and code for citation anchors ([`to_markdown`]).

mod candidates;
mod markdown;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{Document, ExtractError};

pub use candidates::{lighten, url_page_type, RsTrafilatura, RustTrafilatura};
pub use markdown::{align, coverage, to_markdown};

/// Per-page latency budget at p95, CPU only.
pub const P95_BUDGET: Duration = Duration::from_millis(50);

/// Below this many characters a candidate's text is thin and loses to a longer one.
pub const THIN_CHARS: usize = 250;

/// The default cap on the first candidate per page (see [`SelectionPolicy::budget`]). Quality
/// first: it only stops pathological pages from holding a corpus worker. The slowest WCXB page
/// takes ~0.5 s and a 1 MB Wikipedia article 160-190 ms (release), so ordinary pages are never
/// cut short and the reading does not depend on CPU load.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(1);

/// When, under the default cap, the second candidate starts racing the first. On large pages
/// rust-trafilatura is no faster (255 ms on that Wikipedia article), so it starts late.
pub const DEFAULT_RACE_START: Duration = Duration::from_millis(400);

/// [`SelectionPolicy::latency`]: the cap that holds the shipped path's p95 under [`P95_BUDGET`]
/// over 2,189 WCXB + ScrapingHub pages (49.3 ms, run `final`) at a cost of 0.002-0.006 WCXB test F1
/// that varies with load.
pub const LATENCY_BUDGET: Duration = Duration::from_millis(40);

/// When, under [`LATENCY_BUDGET`], the second candidate starts racing the first.
pub const RACE_START: Duration = Duration::from_millis(10);

/// Below this share of the chosen text mapped onto blocks, the markdown is the chosen text itself.
pub const MIN_COVERAGE: f32 = 0.6;

/// An extractor that can be selected per page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Candidate {
    /// The streaming html.rs extractor in this crate.
    Baseline,
    /// `rs-trafilatura` (MIT/Apache-2.0): page-type classification, best WCXB F1.
    RsTrafilatura,
    /// `rust-trafilatura` (Apache-2.0): the fast port of go-trafilatura.
    RustTrafilatura,
}

impl Candidate {
    pub const ALL: [Candidate; 3] = [Candidate::Baseline, Candidate::RsTrafilatura, Candidate::RustTrafilatura];

    pub fn as_str(self) -> &'static str {
        match self {
            Candidate::Baseline => "baseline",
            Candidate::RsTrafilatura => "rs-trafilatura",
            Candidate::RustTrafilatura => "rust-trafilatura",
        }
    }

    /// Whether this build compiled the candidate in.
    pub fn compiled(self) -> bool {
        match self {
            Candidate::Baseline => true,
            Candidate::RsTrafilatura => cfg!(feature = "rs-trafilatura"),
            Candidate::RustTrafilatura => cfg!(feature = "rust-trafilatura"),
        }
    }
}

/// WCXB's page types, plus `Unknown` when a candidate does not classify.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PageType {
    Article,
    Forum,
    Product,
    Collection,
    Listing,
    Documentation,
    Service,
    #[default]
    Unknown,
}

impl PageType {
    pub fn as_str(self) -> &'static str {
        match self {
            PageType::Article => "article",
            PageType::Forum => "forum",
            PageType::Product => "product",
            PageType::Collection => "collection",
            PageType::Listing => "listing",
            PageType::Documentation => "documentation",
            PageType::Service => "service",
            PageType::Unknown => "unknown",
        }
    }

    /// Parses a classifier label (`category` is WCXB's older name for `collection`).
    pub fn parse(s: &str) -> PageType {
        match s.trim().to_ascii_lowercase().as_str() {
            "article" => PageType::Article,
            "forum" => PageType::Forum,
            "product" => PageType::Product,
            "collection" | "category" => PageType::Collection,
            "listing" => PageType::Listing,
            "documentation" | "docs" => PageType::Documentation,
            "service" => PageType::Service,
            _ => PageType::Unknown,
        }
    }
}

/// One page handed to a candidate: already decoded (charset handled once, by html.rs).
#[derive(Debug, Clone, Copy)]
pub struct PageInput<'a> {
    pub html: &'a str,
    pub url: Option<&'a str>,
}

/// One candidate's reading of a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub candidate: Candidate,
    /// Main content as plain text, blocks separated by newlines.
    pub text: String,
    pub title: Option<String>,
    pub page_type: PageType,
    /// The candidate's own confidence in 0..=1, when it reports one.
    pub confidence: Option<f32>,
    pub elapsed: Duration,
}

impl Extracted {
    /// Text length in characters, ignoring surrounding whitespace.
    pub fn chars(&self) -> usize {
        self.text.trim().chars().count()
    }

    pub fn is_thin(&self) -> bool {
        self.chars() < THIN_CHARS
    }
}

/// Why a candidate produced nothing.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MainContentError {
    #[error("{} is not compiled into this build", .0.as_str())]
    Unavailable(Candidate),
    #[error("{}: {detail}", candidate.as_str())]
    Failed { candidate: Candidate, detail: String },
    #[error("{} panicked", .0.as_str())]
    Panicked(Candidate),
    /// Not finished within the policy's budget; its reading (if any) arrives too late to use.
    #[error("{} ran past the time budget", .0.as_str())]
    OverBudget(Candidate),
}

/// A main-content extractor.
pub trait MainContent: Send + Sync {
    fn candidate(&self) -> Candidate;
    fn extract(&self, page: &PageInput<'_>) -> Result<Extracted, MainContentError>;
}

/// The html.rs extractor as a candidate. Never fails on decoded input.
#[derive(Debug, Clone, Copy, Default)]
pub struct Baseline;

impl MainContent for Baseline {
    fn candidate(&self) -> Candidate {
        Candidate::Baseline
    }

    fn extract(&self, page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        let t = Instant::now();
        let doc = crate::html::extract(page.html.as_bytes(), Some("text/html; charset=utf-8"), page.url)
            .map_err(|e| MainContentError::Failed { candidate: Candidate::Baseline, detail: e.to_string() })?;
        Ok(baseline_reading(&doc, t.elapsed()))
    }
}

fn baseline_reading(doc: &Document, elapsed: Duration) -> Extracted {
    Extracted {
        candidate: Candidate::Baseline,
        text: doc.text.clone(),
        title: doc.title.clone(),
        page_type: PageType::Unknown,
        confidence: None,
        elapsed,
    }
}

/// A candidate that is declared but not compiled in.
#[derive(Debug, Clone, Copy)]
pub struct NotCompiled(pub Candidate);

impl MainContent for NotCompiled {
    fn candidate(&self) -> Candidate {
        self.0
    }

    fn extract(&self, _page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
        Err(MainContentError::Unavailable(self.0))
    }
}

/// One instance of `c`: the real extractor when compiled in, else [`NotCompiled`].
pub fn candidate(c: Candidate) -> Box<dyn MainContent> {
    match c {
        Candidate::Baseline => Box::new(Baseline),
        #[cfg(feature = "rs-trafilatura")]
        Candidate::RsTrafilatura => Box::new(RsTrafilatura::default()),
        #[cfg(feature = "rust-trafilatura")]
        Candidate::RustTrafilatura => Box::new(RustTrafilatura::default()),
        #[allow(unreachable_patterns)]
        other => Box::new(NotCompiled(other)),
    }
}

/// Every candidate this build knows, baseline first.
pub fn all_candidates() -> Vec<Box<dyn MainContent>> {
    Candidate::ALL.iter().map(|c| candidate(*c)).collect()
}

/// How [`choose`] weighs candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionPolicy {
    /// A candidate below this confidence is not preferred over the baseline.
    pub min_confidence: f32,
    /// Text shorter than this is thin.
    pub thin_chars: usize,
    /// A candidate keeping less than this share of a long baseline (`>= 2,000` chars) is treated
    /// as truncated. Research prefers some chrome over a missing half.
    pub min_share_of_baseline: f32,
    /// Preference order when several candidates are acceptable; earlier wins.
    pub order: Vec<Candidate>,
    /// Per page type overrides of `order`, applied once the first candidate has classified the page.
    pub by_type: Vec<(PageType, Vec<Candidate>)>,
    /// Hard limit on the first candidate; from `race_start` the next one races it on its own
    /// worker, and at the deadline whatever has answered is used (else the baseline). `None`: no
    /// limit.
    pub budget: Option<Duration>,
    /// When the second candidate starts racing the first (within `budget`).
    pub race_start: Duration,
    /// Page types (from the URL alone) that get no budget: forums are where the slow candidate
    /// earns most of its gain (WCXB forum F1 0.836 vs 0.583 for the fast one).
    pub budget_exempt: Vec<PageType>,
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self {
            min_confidence: 0.5,
            thin_chars: THIN_CHARS,
            min_share_of_baseline: 0.05,
            order: vec![Candidate::RsTrafilatura, Candidate::RustTrafilatura, Candidate::Baseline],
            by_type: Vec::new(),
            budget: Some(DEFAULT_BUDGET),
            race_start: DEFAULT_RACE_START,
            budget_exempt: vec![PageType::Forum],
        }
    }
}

impl SelectionPolicy {
    /// The default order with the latency cap: p95 within [`P95_BUDGET`], a little F1 traded.
    pub fn latency() -> Self {
        Self { budget: Some(LATENCY_BUDGET), race_start: RACE_START, ..Self::default() }
    }

    fn order_for(&self, page_type: PageType) -> &[Candidate] {
        self.by_type.iter().find(|(t, _)| *t == page_type).map_or(&self.order, |(_, o)| o)
    }

    fn rank(&self, page_type: PageType, c: Candidate) -> usize {
        self.order_for(page_type).iter().position(|o| *o == c).unwrap_or(usize::MAX)
    }

    /// Whether `e` may be chosen on this page.
    pub fn acceptable(&self, e: &Extracted, baseline: &Extracted) -> bool {
        let chars = e.chars();
        if chars < self.thin_chars {
            return false;
        }
        if e.candidate == Candidate::Baseline {
            return true;
        }
        let confident = e.confidence.is_none_or(|c| c >= self.min_confidence);
        let base = baseline.chars();
        let truncated = base >= 2_000 && (chars as f32) < base as f32 * self.min_share_of_baseline;
        confident && !truncated
    }
}

/// Why a candidate was chosen.
#[derive(Debug, Clone, PartialEq)]
pub enum ChoiceReason {
    /// The first acceptable candidate in policy order.
    Preferred,
    /// Every other candidate failed, was thin, or was unsure.
    Fallback,
    /// All candidates were thin; the longest text won.
    LongestOfThin,
}

/// The chosen reading of a page and how it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub chosen: Extracted,
    pub reason: ChoiceReason,
    /// Candidates that produced nothing, with why.
    pub failures: Vec<MainContentError>,
    /// The page type from the first candidate that classified the page.
    pub page_type: PageType,
    /// Wall-clock for every candidate run on this page.
    pub elapsed: Duration,
}

/// Picks one reading from precomputed results. `baseline` always exists, so a choice always exists.
pub fn choose(
    baseline: Extracted,
    others: Vec<Result<Extracted, MainContentError>>,
    policy: &SelectionPolicy,
) -> Choice {
    let mut failures = Vec::new();
    let mut ok = vec![baseline];
    for r in others {
        match r {
            Ok(e) => ok.push(e),
            Err(e) => failures.push(e),
        }
    }
    let page_type = ok.iter().map(|e| e.page_type).find(|t| *t != PageType::Unknown).unwrap_or_default();
    let elapsed = ok.iter().map(|e| e.elapsed).sum();
    let base = ok[0].clone();
    if let Some(best) =
        ok.iter().filter(|e| policy.acceptable(e, &base)).min_by_key(|e| policy.rank(page_type, e.candidate))
    {
        let reason = if best.candidate == Candidate::Baseline && ok.len() + failures.len() > 1 {
            ChoiceReason::Fallback
        } else {
            ChoiceReason::Preferred
        };
        return Choice { chosen: best.clone(), reason, failures, page_type, elapsed };
    }
    let longest = ok.into_iter().max_by_key(Extracted::chars).expect("the baseline is always present");
    Choice { chosen: longest, reason: ChoiceReason::LongestOfThin, failures, page_type, elapsed }
}

/// What kind of html.rs block a span is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    ListItem,
    /// One table cell; cells sharing `row` render as one markdown table row.
    TableCell {
        row: u32,
        header: bool,
    },
    Code,
    Quote,
}

/// One html.rs block with its structure, for markdown rendering after selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpan {
    pub kind: BlockKind,
    /// Collapsed text; code keeps its line breaks.
    pub text: String,
    /// Indices into the document's `links` closed inside this block.
    pub links: Vec<usize>,
    /// Inside nav/header/footer/aside/figcaption, or a link farm.
    pub boilerplate: bool,
    /// Inside `<article>` or `<main>`.
    pub content: bool,
}

/// A page read for research: the document (its `text` is the chosen main content), the chosen
/// content as markdown with links, headings, tables and code, and how it was chosen.
#[derive(Debug, Clone)]
pub struct MainText {
    pub document: Document,
    pub markdown: String,
    pub choice: Choice,
    /// Share of the chosen text that mapped onto html.rs blocks (1.0 when the baseline won).
    pub coverage: f32,
    /// Whether `markdown` was rendered from blocks (false: the chosen plain text was used).
    pub structured: bool,
    /// Thin text on a page that is mostly script or an empty app mount point: a browser would see
    /// something else. The caller may render it (see marlowe-exec's render path).
    pub client_rendered: bool,
}

/// Mount points of client-rendered apps, matched on the raw source.
const SPA_MARKERS: [&str; 7] =
    ["id=\"root\"", "id=\"app\"", "id=\"__next\"", "id=\"__nuxt\"", "<app-root", "data-reactroot", "ng-version="];

/// Whether a page whose readable text is `chars` long looks like an unrendered app shell.
pub fn looks_client_rendered(doc: &Document, html: &str, chars: usize) -> bool {
    if chars >= THIN_CHARS {
        return false;
    }
    let warned = doc.warnings.iter().any(|w| matches!(w, crate::Warning::LikelyClientRendered { .. }));
    let head = &html[..crate::floor_char_boundary(html, 256 * 1024)];
    warned || SPA_MARKERS.iter().any(|m| head.contains(m)) || head.contains("<noscript>You need to enable JavaScript")
}

/// Runs candidates on one page and chooses.
pub struct Selector {
    candidates: Vec<Arc<dyn MainContent>>,
    policy: SelectionPolicy,
}

impl Selector {
    /// The baseline plus the given candidates.
    pub fn new(candidates: Vec<Box<dyn MainContent>>, policy: SelectionPolicy) -> Self {
        Self { candidates: candidates.into_iter().map(Arc::from).collect(), policy }
    }

    /// Every candidate this build knows (uncompiled ones report `Unavailable`), default policy.
    pub fn with_defaults() -> Self {
        Self::with_policy(SelectionPolicy::default())
    }

    /// Every candidate this build knows, under `policy`.
    pub fn with_policy(policy: SelectionPolicy) -> Self {
        let others = Candidate::ALL.iter().filter(|c| **c != Candidate::Baseline).map(|c| candidate(*c)).collect();
        Self::new(others, policy)
    }

    /// Only the baseline: today's html.rs behaviour, through the same interface.
    pub fn baseline_only() -> Self {
        Self::new(Vec::new(), SelectionPolicy { order: vec![Candidate::Baseline], ..SelectionPolicy::default() })
    }

    pub fn policy(&self) -> &SelectionPolicy {
        &self.policy
    }

    /// Chooses among the candidates for `page`, parsing it with html.rs for the baseline.
    pub fn choose(&self, page: &PageInput<'_>) -> Result<Choice, ExtractError> {
        let baseline =
            Baseline.extract(page).map_err(|e| ExtractError::Backend { format: "html", detail: e.to_string() })?;
        Ok(self.choose_with(baseline, page))
    }

    /// Runs candidates lazily in policy order and stops once the best-ranked acceptable reading is
    /// known. The first candidate that classifies the page selects the `by_type` order for the
    /// rest. A candidate that panics is recorded as a failure.
    fn choose_with(&self, baseline: Extracted, page: &PageInput<'_>) -> Choice {
        let mut tried: Vec<Result<Extracted, MainContentError>> = Vec::new();
        let mut page_type = PageType::Unknown;
        loop {
            let mut order: Vec<Candidate> = self.policy.order_for(page_type).to_vec();
            let extra: Vec<Candidate> =
                self.candidates.iter().map(|c| c.candidate()).filter(|c| !order.contains(c)).collect();
            order.extend(extra);
            let mut next = None;
            for c in order {
                if c == Candidate::Baseline {
                    if self.policy.acceptable(&baseline, &baseline) {
                        break;
                    }
                    continue;
                }
                match tried.iter().find(|r| result_candidate(r) == c) {
                    Some(Ok(e)) if self.policy.acceptable(e, &baseline) => break,
                    Some(_) => continue,
                    None => {
                        next = Some(c);
                        break;
                    }
                }
            }
            let Some(next) = next else { break };
            let runner = self.runner(next);
            let raced = match (tried.is_empty(), self.policy.budget, &runner) {
                (true, Some(budget), Some(first)) if !self.exempt(page) => {
                    let ok = |e: &Extracted| self.policy.acceptable(e, &baseline);
                    let start = self.policy.race_start;
                    self.second_after(next, page_type).map(|second| race(first, &second, page, budget, start, &ok))
                }
                _ => None,
            };
            let (r, also) = match (raced, runner) {
                (Some((r, also)), _) => (r, also),
                (None, Some(runner)) => (run(runner.as_ref(), page), None),
                (None, None) => (Err(MainContentError::Unavailable(next)), None),
            };
            if let Ok(e) = &r {
                if page_type == PageType::Unknown {
                    page_type = e.page_type;
                }
            }
            tried.push(r);
            tried.extend(also);
        }
        choose(baseline, tried, &self.policy)
    }

    fn runner(&self, c: Candidate) -> Option<Arc<dyn MainContent>> {
        self.candidates.iter().find(|r| r.candidate() == c).cloned()
    }

    /// Whether the URL alone puts the page in a budget-exempt type.
    fn exempt(&self, page: &PageInput<'_>) -> bool {
        let t = page.url.map_or(PageType::Unknown, url_page_type);
        self.policy.budget_exempt.contains(&t)
    }

    /// The compiled candidate after `first` in the order, if it is ranked above the baseline.
    fn second_after(&self, first: Candidate, page_type: PageType) -> Option<Arc<dyn MainContent>> {
        let order = self.policy.order_for(page_type);
        let at = order.iter().position(|c| *c == first)?;
        let next = *order.get(at + 1)?;
        if next == Candidate::Baseline {
            return None;
        }
        self.runner(next)
    }

    /// Extracts `page` as a [`Document`] whose text is the chosen main content (plain text).
    pub fn document(
        &self,
        bytes: &[u8],
        content_type: Option<&str>,
        url: Option<&str>,
    ) -> Result<(Document, Choice), ExtractError> {
        let m = self.read(bytes, content_type, url)?;
        Ok((m.document, m.choice))
    }

    /// Reads one HTML page: chosen main content as plain text in `document.text`, plus markdown
    /// rendered from the html.rs blocks the chosen text maps onto.
    pub fn read(&self, bytes: &[u8], content_type: Option<&str>, url: Option<&str>) -> Result<MainText, ExtractError> {
        if bytes.len() > crate::MAX_INPUT_BYTES {
            return Err(ExtractError::TooLarge { bytes: bytes.len() });
        }
        let t = Instant::now();
        let parsed = std::panic::catch_unwind(|| crate::html::extract_structured(bytes, content_type, url))
            .map_err(|_| ExtractError::Backend { format: "html", detail: "the html extractor panicked".into() })?;
        let (mut doc, blocks, html) = parsed;
        let baseline = baseline_reading(&doc, t.elapsed());
        let choice = self.choose_with(baseline, &PageInput { html: &html, url });
        let (keep, coverage) = if choice.chosen.candidate == Candidate::Baseline {
            let keep = align(&doc.text, &blocks);
            (keep, 1.0)
        } else {
            let keep = align(&choice.chosen.text, &blocks);
            let cov = coverage(&choice.chosen.text, &blocks, &keep);
            (keep, cov)
        };
        let structured = coverage >= MIN_COVERAGE && !keep.is_empty();
        let markdown =
            if structured { to_markdown(&blocks, &keep, &doc.links) } else { choice.chosen.text.trim().to_string() };
        let client_rendered = looks_client_rendered(&doc, &html, choice.chosen.chars());
        if choice.chosen.candidate != Candidate::Baseline {
            doc.text = choice.chosen.text.trim().to_string();
            if doc.title.is_none() {
                doc.title = choice.chosen.title.clone();
            }
        }
        Ok(MainText { document: doc, markdown, choice, coverage, structured, client_rendered })
    }
}

fn run(c: &dyn MainContent, page: &PageInput<'_>) -> Result<Extracted, MainContentError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.extract(page)))
        .unwrap_or(Err(MainContentError::Panicked(c.candidate())))
}

/// Runs `first` on a worker under a hard `budget`. If it has not answered by `race_start`,
/// `second` starts on another worker. Returns as soon as `first` answers acceptably, otherwise at
/// the deadline with whatever arrived; a candidate still running is `OverBudget` and finishes
/// detached. `second` is `None` when it never started.
fn race(
    first: &Arc<dyn MainContent>,
    second: &Arc<dyn MainContent>,
    page: &PageInput<'_>,
    budget: Duration,
    race_start: Duration,
    acceptable: &dyn Fn(&Extracted) -> bool,
) -> (Result<Extracted, MainContentError>, Option<Result<Extracted, MainContentError>>) {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = Instant::now() + budget;
    let (a, b) = (first.candidate(), second.candidate());
    let (tx, rx) = std::sync::mpsc::channel::<Result<Extracted, MainContentError>>();
    let html: Arc<str> = Arc::from(page.html);
    let url: Option<Arc<str>> = page.url.map(Arc::from);
    let spawn = |c: &Arc<dyn MainContent>| {
        let (runner, tx, html, url) = (c.clone(), tx.clone(), html.clone(), url.clone());
        std::thread::Builder::new()
            .name("marlowe-main-content".into())
            .spawn(move || {
                let _ = tx.send(run(runner.as_ref(), &PageInput { html: &html, url: url.as_deref() }));
            })
            .is_ok()
    };
    if !spawn(first) {
        return (run(first.as_ref(), page), None);
    }
    let (mut ra, mut rb) = (None, None);
    let mut started_b = false;
    loop {
        let now = Instant::now();
        let wake = if started_b { deadline } else { (deadline - budget + race_start).min(deadline) };
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(r) => {
                let from_a = result_candidate(&r) == a;
                if from_a {
                    let good = matches!(&r, Ok(e) if acceptable(e));
                    ra = Some(r);
                    if good || !started_b || rb.is_some() {
                        break;
                    }
                } else {
                    rb = Some(r);
                    if ra.is_some() {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) if !started_b && Instant::now() < deadline => {
                started_b = spawn(second);
                if !started_b {
                    rb = Some(run(second.as_ref(), page));
                }
            }
            Err(_) => break,
        }
    }
    let first_won = matches!(&ra, Some(Ok(e)) if acceptable(e));
    let ra = ra.unwrap_or(Err(MainContentError::OverBudget(a)));
    // A second that was not waited for because the first won is simply not considered.
    let rb = if first_won || !started_b { rb } else { Some(rb.unwrap_or(Err(MainContentError::OverBudget(b)))) };
    (ra, rb)
}

fn result_candidate(r: &Result<Extracted, MainContentError>) -> Candidate {
    match r {
        Ok(e) => e.candidate,
        Err(MainContentError::Unavailable(c) | MainContentError::Panicked(c) | MainContentError::OverBudget(c)) => *c,
        Err(MainContentError::Failed { candidate, .. }) => *candidate,
    }
}

#[cfg(test)]
mod tests;
