# Deep research v2: evaluation

Code: `tools/extraction_eval.py`, `crates/marlowe-extract/examples/extract_eval.rs`,
`tools/research_eval.py`, `tools/research_questions.json`, `tools/research_eval_fixtures/`.
Raw results: `data/runs/extraction-eval/<label>/` and `data/runs/research-eval/<label>/` (git-ignored).
Commands: `tools/README.md`.

## 1. Extraction (E1)

**Data.** Two benchmarks are fetched into `data/extraction/` at pinned commits:
- WCXB (CC-BY-4.0, `c039d5e`): 1,497 dev and 511 test pages across 7 page types.
- ScrapingHub article-extraction-benchmark (`4a3bc97`): 181 article pages.

**Method.** The Rust harness runs every `MainContent` candidate on each page, on one thread,
timing each call. It also runs `Selector::with_defaults().document`, the shipped path, labelled
`selected`. That path does the html.rs parse, then runs every candidate, then makes the choice.

The scorers reproduce the published references:
- The token metric is WCXB's `word_f1`, with the same semantics.
- The shingle metric reproduces ScrapingHub's published F1 for trafilatura, readability and
  go-trafilatura to three decimals (0.958 / 0.922 / 0.960).

Link recall: the gold set is the links on html.rs's link list whose anchor text appears in the
ground truth. Anchor-text recall counts a link as kept when its anchor text survives in the plain
output. Markdown URL recall (shipped path only) counts it as kept only when `[..](url)` with that
exact URL is in the returned markdown, which is what a citation anchor needs.

**Results** (run `final`, 2026-10-09, on Track A's selector at that time: per-type candidate order,
a 40 ms budget with a race against rust-trafilatura after 10 ms, and markdown from html.rs blocks.
That setting is now `SelectionPolicy::latency()`; the shipped default's numbers are in the
integrator re-measure below).
Candidate columns are each extractor alone; `selected` is the shipped `Selector::read` end to end.

| WCXB test (n=511) | baseline (html.rs) | rs-trafilatura 0.2.2 | rust-trafilatura 2.2.8 | selected |
|---|---:|---:|---:|---:|
| token F1 | 0.793 | 0.891 | 0.824 | **0.890** |
| precision / recall | 0.721 / 0.956 | 0.894 / 0.926 | 0.896 / 0.824 | 0.895 / 0.925 |
| article | 0.877 | 0.951 | 0.935 | 0.952 |
| forum | 0.720 | 0.836 | 0.583 | 0.836 |
| product | 0.570 | 0.777 | 0.686 | 0.742 |
| collection | 0.514 | 0.775 | 0.609 | 0.765 |
| listing | 0.663 | 0.748 | 0.677 | 0.776 |
| documentation | 0.901 | 0.973 | 0.933 | 0.973 |
| service | 0.768 | 0.832 | 0.763 | 0.823 |
| with / without snippets | 0.884 / 0.306 | 0.862 / 0.103 | 0.768 / 0.075 | 0.859 / 0.104 |
| anchor-text link recall | 0.930 | 0.931 | 0.806 | 0.928 |
| markdown URL link recall | — | — | — | 0.598 |
| thin / empty | 0.014 / 0.008 | 0.008 / 0 | 0.027 / 0.006 | 0.006 / 0 |
| p50 / p95 ms | 0.44 / 1.1 | 15.0 / 49.2 | 5.3 / 14.4 | 15.8 / 47.3 |
| WCXB dev F1 (n=1,497) | 0.758 | 0.846 | 0.785 | 0.838 |
| ScrapingHub shingle F1 | 0.818 | 0.956 | 0.960 | 0.956 |
| p95 ms, all 2,189 pages | 1.6 | 59.9 | 19.3 | **49.3** |

- `selected` chose rs-trafilatura on 2,001 of 2,189 pages, rust-trafilatura on 123 and the
  baseline on 65. Its markdown came from html.rs blocks (`structured`) on 97.7% of test pages.
- rs-trafilatura agrees with its published numbers: WCXB test F1 0.891 here against 0.893
  published; ScrapingHub shingle F1 0.956 against 0.970, which came from another revision
  (`9261e08`) through the published harness rather than our UTF-8 decode.
- Run-to-run noise: standalone rs-trafilatura p95 on test read 37.6–49.2 ms over six runs on the
  same code and pages, so latency differences under ~10 ms are not signal. F1 is deterministic.

**Pass bar** (WCXB test vs the baseline): **`selected` PASSES, including the design-A budget.**
- F1 gain: +0.097 (the bar is +0.08).
- Forum F1: 0.836 (the bar is 0.75).
- No page type regresses. The smallest gain is service, at +0.055.
- p95: 47.3 ms on WCXB test and 49.3 ms over all 2,189 pages (the bar is 50 ms). Both are within
  the noise above of the line, so the budget holds but has no margin.

rs-trafilatura alone passes on WCXB test but not on all pages (59.9 ms). rust-trafilatura alone
fails: forum F1 is 0.583, a regression of 0.137.

**How the budget was bought.** The first candidate run (`candidates-v1`: run every candidate,
then choose) read F1 0.894 and p95 64.5 ms over all pages. The budget-and-race selector costs
0.004 F1 on test and 0.010 on dev. On test the loss is on product (−0.035) and collection
(−0.036) pages; listing gains 0.010. p95 by page size for `selected`: 38.5 ms
under 200 KB (1,561 pages), 53.1 ms at 200–500 KB, 67.8 ms at 500 KB–1 MB, 56.9 ms over 1 MB;
102 of 2,189 pages exceed 50 ms, worst 140.9 ms.

**Finding for the markdown path (for Track A).** Anchor text survives (0.928), but only 0.598 of
the gold links keep their URL as `[text](url)` in the markdown the tools return. Of the 2,381
gold links missing on structured WCXB test pages, 583 sit in headings (`to_markdown` writes `Heading` blocks
without `inline_links`), 542 are in paragraphs, list items or cells whose anchor is present but
not linked, 3 are in code, and 1,253 have anchor text that is not verbatim in any markdown line (whitespace or
entity differences against `inline_links`' exact `find`). Citation anchors depend on these URLs,
so this is the next fix for the markdown path. Metric: `url_link_recall` in
`tools/extraction_eval.py`.

**Shipped path after the latency work** (Track A, 2026-10-09). Two cheap levers were tried:
`lighten` (drop `script`/`style`/`svg`/`template`/`iframe`/`noscript` and comments before the DOM
parse, keeping JSON-LD) and a time-boxed race (rs-trafilatura capped, rust-trafilatura started on
a second worker partway through, forum URLs exempt). Owner's decision: quality wins.

| `selected`, same code | F1 test | forum | F1 dev | link recall | p95 test | p95 all 2,189 | max |
|---|---:|---:|---:|---:|---:|---:|---:|
| default: 1 s cap, race from 400 ms (`tracka-quality-1s`) | **0.894** | 0.836 | **0.846** | **0.937** | 52.8 ms | 63.6 ms | 467 ms |
| 250 ms cap, race from 100 ms (`tracka-quality`) | 0.894 | 0.836 | 0.846 | 0.937 | 48.5 ms | 67.3 ms | 268 ms |
| `SelectionPolicy::latency()`: 40 ms cap, race from 10 ms (`final`) | 0.890 | 0.836 | 0.838 | 0.928 | 47.3 ms | **49.3 ms** | 141 ms |

The default ships: it keeps the full quality gain and only cuts pathological pages, so a page
reads the same whatever the CPU load. The cap is 1 s because a 1 MB Wikipedia article takes
160-190 ms in rs-trafilatura (release); under a 250 ms cap it fell back to html.rs in a live
concurrent pass. **Its p95 is over the 50 ms budget** (52.8 ms test, 63.6 ms all pages); network
fetch dominates research latency. Other tracks were compiling during both runs, so
absolute times carry about ±5 ms of noise (rs-trafilatura alone read 59.9 and 61.0 ms all-pages p95).

**Integrator re-measure, quiet machine** (run `m45-integrator`, 2026-10-09 18:30; no other build
running, only a local model generating on the GPU). Shipped default (1 s cap, race from 400 ms):

| | html.rs | rs-trafilatura | rust-trafilatura | **selected (default)** |
|---|---:|---:|---:|---:|
| WCXB test F1 (n=511) | 0.793 | 0.891 | 0.824 | **0.894** |
| forum F1 (n=51) | 0.720 | 0.836 | 0.583 | 0.836 |
| WCXB dev F1 (n=1,497) | 0.758 | 0.846 | 0.785 | 0.846 |
| ScrapingHub token F1 / shingle F1 (n=181) | 0.803 / 0.818 | 0.951 / 0.956 | 0.958 / 0.960 | 0.951 / 0.956 |
| anchor-text link recall (test) | 0.930 | 0.931 | 0.806 | 0.937 |
| thin / empty (test) | 0.014 / 0.008 | 0.008 / 0 | 0.027 / 0.006 | 0.006 / 0 |
| p50 / p95 ms, WCXB test | 0.36 / 0.92 | 12.2 / 38.3 | 4.5 / 12.0 | 13.4 / **43.1** |
| p95 ms, all 2,189 pages | 1.4 | 53.6 | 17.5 | 57.6 |

**Pass bar: `selected` PASSES on every check**: F1 +0.101 (bar +0.08), forum 0.836 (bar 0.75), no
page type regresses (smallest gain service +0.064), p95 43.1 ms on WCXB test (bar 50 ms). The
default stays; the earlier 52.8 ms reading was build noise. Over all pages (dev included, not
part of the bar) p95 is 57.6 ms: the 1 s cap only stops pathological pages. rs-trafilatura alone
also passes; rust-trafilatura alone fails on forum.

**Markdown links, fixed** (run `m45-md-links`): heading blocks now carry their links, and an
anchor matches with its whitespace collapsed. Markdown URL link recall 0.604 → **0.641** on test
(dev 0.583 → 0.614); F1 and the pass bar are unchanged (p95 test 43.5 ms). The rest of the gap is
anchors that are not verbatim in the block text (entity differences) and out-of-order matches.

**Finding for html.rs:** 46 WCXB pages come out thin under the baseline, and 30 of them are dev
forum pages. 26 of the thin pages are Discourse (users.rust-lang.org, community.openai.com,
forums.docker.com and others), and 19 carry more than 2 KB of content inside `<noscript>`.
html.rs skips `<noscript>`, so these pages read as empty. rs-trafilatura reads them, with forum
dev thin rate 0.009 against 0.265.

## 2. Research (E2)

**Set.** `tools/research_questions.json` holds 10 questions with 4–5 key points each:
- ML research (4): LoRA vs full fine-tuning, FlashAttention, MoE, Chinchilla.
- Engineering (3): SQLite WAL, RFC 9309, LLM decode latency.
- General (3): Challenger, mRNA vaccines, intermittent fasting. The fasting question is a
  contested-evidence case.

Each key point names the source a reviewer can check it against. All 19 arXiv IDs and 4 DOIs
cited there were resolved against arXiv and Crossref on 2026-10-09. **The key points are drafts
until Matthew reviews them.**

**Scorer** (FACT-style, Du et al. 2025, DeepResearch Bench):
- Every (cited sentence, reference) pair is extracted from the report and duplicates are merged.
- The judge decides supported, partial or unsupported against the reference's **stored**
  evidence: the run's quotes first, then the best-overlapping chunks of the stored document text.
  This is what VERIFY re-reads, so a score reproduces offline.
- Citation accuracy = supported / judged. Partial counts as not supported, as in FACT. The
  half-credit reading that matches `Verification::citation_accuracy` is reported alongside.
- Effective citations = distinct supported pairs per report.
- Key-point coverage = (covered + 0.5 × partial) / key points, with contradictions flagged.
- A marker that names no reference counts as unsupported.
- The target is **citation accuracy >= 0.95** (ADR-037).

**Bundle the playbook must write** (`marlowe.research-bundle/1`), one per question:
- `report_markdown`: the synthesised report.
- `references[]`: built from `Report.references`. Each entry has `n`, `title`, `url` and
  `identity`, plus `passages`, which are that source's `Claim.quote` text and locators, and
  `document_text`, the stored extracted text for its `DocRef`.
- Optional: `verification.citations` (the run's own `CitationCheck`s, so self-reported support can
  be compared with the judge), `channels_used` and `stats`.

**Validation** (judge `marlowe-dusk:27b-super`, temperature 0, GPU checked free first):

| Fixture | Citations | Supported | Unsupported | Accuracy | Planted errors caught | False flags | Key points |
|---|---:|---:|---:|---:|---:|---:|---:|
| good | 11 | 11 | 0 | 1.000 | — | 0 | 0.90 |
| bad (one invented claim: "40% faster inference") | 11 | 10 | 1 | 0.909 | 1/1 | 0 | 0.80 |
| subtle (wrong number, reversed direction, misattributed source) | 11 | 8 | 3 | 0.727 | 3/3 | 0 | 0.90 |

- **Rubric change.** The first rubric marked the wrong-number sentence *partial*. The rubric now
  says that any conflicting number, name or direction is *unsupported*. After the change all
  three planted errors read unsupported.
- **Stability.** A second pass with an empty cache gave the same verdict on 30 of 30 distinct
  prompts.
- **Cost.** A judge call took 1.8 s on average (median 1.6 s, max 8.4 s), so a 10-report run with
  about 30 citations each should take about 10 minutes of judging.

**Not yet run:** the full 10-question research eval. The integrator runs it after the v2 playbook
can write bundles:

```
python tools/research_eval.py run --bundles <dir> --label v2-first
```

**Known limit:** when the judge and the report writer are the same model (marlowe-dusk), the score
may carry self-preference bias. `--judge-model` takes another local model for a cross-check.
