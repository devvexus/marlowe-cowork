//! The `use` tool — skill discovery and progressive disclosure. ADR-051.
//!
//! # This is `find_skill`, and it is not a new tool
//!
//! ROADMAP M2 C3 names *"`find_skill` semantic discovery"*. There is no `find_skill` tool and
//! there must not be: ADR-006 registered `use` — *"Find and load a skill or tool"* — in M2 Session
//! A with exactly the two parameters the job needs, and it has been **registered and unrunnable
//! ever since**. Adding a twelfth tool would spend ARCHITECTURE §5's one spare slot on something
//! that already had a slot.
//!
//! | Call | What it does |
//! |---|---|
//! | `use(query = "...")` | discovery — rank installed skills, return names and descriptions |
//! | `use(name = "...")` | disclosure — load that skill's instructions |
//!
//! # THE RANKING IS LEXICAL, AND THAT IS A DECISION — ADR-051 §5
//!
//! §7.1 says trigger phrases and descriptions are *"embedded for semantic discovery"* and the
//! ROADMAP row says "semantic". **This is BM25, and it stays.** Decided by the human, 2026-08-24.
//!
//! **The reason is a domain argument, not a capability one.** The memory system's rankers were
//! tuned on *conversations* — `ms-marco-MiniLM-L-6-v2-ft-session-j` was fine-tuned against
//! LongMemEval, whose documents are conversational turns. A `SKILL.md` description is a one-line
//! imperative label for a procedure: a different distribution. Pointing a ranker tuned on one at
//! the other and expecting its measured quality to carry is the *"a measurement is scoped to the
//! system it was taken on"* family, and it would arrive wearing the cascade's held-out numbers,
//! which say nothing about this corpus.
//!
//! **The cost is real and is stated rather than hidden:** a skill whose description uses different
//! words than the user does is not returned. `Metric::State` reads `lexical` on every result and
//! the no-match message says so in words, because **claiming "semantic discovery ships" over a
//! BM25 would be this repository's most-repeated defect** — a property asserted where it is
//! declared rather than where it is enforced.
//!
//! **A deferred experiment lives in ADR-051 §5**, gated on a precondition that is not met: a real
//! skills library. At four installed skills every ranker looks the same and the measurement is
//! noise. `rank` is deliberately the single function that decides, so running it later is one edit
//! rather than a search.
//!
//! # There is no second ranker
//!
//! `cue::lexical::score_texts` is `score_all`'s own arithmetic with the belief projection lifted
//! out (ADR-051), pinned by `the_belief_path_and_the_text_path_are_the_same_arithmetic`. A second
//! BM25 beside the retrieval one would put the tokenizer — the part most likely to be wrong — in
//! two places that could disagree silently.
//!
//! # Why a wrapper, and why the trust class is what it is
//!
//! A wrapper for `recall`'s reason: `marlowe-exec` executes against the filesystem and the
//! network, and teaching it about skills would point the executor crate at another subsystem. The
//! daemon is the composition root and already owns both.
//!
//! **A loaded skill's instructions enter at `UserAsserted`.** That is a claim, so here is the
//! argument. Installing a skill into the profile is the user directing Marlowe to follow those
//! instructions — the same reasoning ADR-052 gives for an installed MCP server being trusted, and
//! the reason `skill::load_skill` records `ManifestProvenance::UserReviewed`. The agent cannot
//! install one on its own initiative.
//!
//! What keeps that safe is **not** the class: it is that `use` is declared `Reversible` rather
//! than `Inert` specifically so §9's target check fires on `name`. Untrusted content therefore
//! cannot *choose* which skill loads, which is the supply-chain-steering attack. `builtin.rs` has
//! carried that comment since Session A; this is the first code to depend on it.
//!
//! Note also what the class does **not** buy: `ContextView::trust_floor` is `min`, so a
//! `UserAsserted` block cannot raise a floor that untrusted content has already lowered. The
//! choice between `UserAsserted` and `AgentObserved` here is about labelling the origin honestly,
//! not about a privilege — both sit above `blocks_composed_targets`'s threshold.

use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_loop::driver::{ToolBody, ToolHost, ToolOutcome};
use marlowe_memory::cue::lexical;
use marlowe_permission::{Adjudication, Args};
use marlowe_tools::skill::{Skill, SkillRegistry};
use marlowe_tools::{Metric, ResultSummary, ToolId};

/// How many skills one `use(query = ...)` returns.
///
/// `recall`'s reasoning, unchanged: this competes for the same window the conversation needs, and
/// a discovery result listing forty skills has moved the choice into the model's attention rather
/// than making it.
const DISCOVERY_LIMIT: usize = 5;

/// The daemon's tool host: whatever it wraps, plus `use` over the installed skills.
pub struct SkillTools<H: ToolHost> {
    inner: H,
    skills: Arc<Mutex<SkillRegistry>>,
}

impl<H: ToolHost> SkillTools<H> {
    pub fn new(inner: H, skills: Arc<Mutex<SkillRegistry>>) -> Self {
        Self { inner, skills }
    }

    fn discover(&self, query: &str) -> ToolOutcome {
        let skills = self.skills.lock().expect("the skill registry lock was poisoned");
        let all: Vec<&Skill> = skills.iter().collect();

        if all.is_empty() {
            return harness_says(
                "no skills are installed in this profile. A skill is a `SKILL.md` in its own \
                 directory under the profile's `skills/` folder.",
                vec![Metric::Count { n: 0, unit: "skills" }],
            );
        }

        let hits = rank(&all, query);
        if hits.is_empty() {
            return harness_says(
                &format!(
                    "no installed skill matches {query:?}. {} were searched. Note that this \
                     search is lexical, so a skill whose description uses different words will \
                     not be found by meaning alone.",
                    all.len()
                ),
                vec![
                    Metric::Count { n: 0, unit: "skills" },
                    Metric::Count { n: all.len() as u64, unit: "searched" },
                ],
            );
        }

        let mut out = String::new();
        for (skill, _) in &hits {
            // The score is deliberately NOT printed. It is a BM25 magnitude with no calibrated
            // meaning, and a number beside a name invites the model to reason about the gap
            // between two of them as though it were a probability.
            // **State, not instruction.** The byte count is here so that "there is more, and you
            // have not read it" is a FACT ABOUT THE DATA the model can reason over, rather than an
            // imperative in a tool result -- which the persona tells it to treat as data and never
            // as instruction. The size is the body's, and the body is what a load returns.
            out.push_str(&format!(
                "- {} [body {} B, not loaded]: {}\n",
                skill.id(),
                skill.body().bytes(),
                skill.description().text()
            ));
        }
        out.push_str("\nDescriptions only. No skill body above has been read.");

        ToolOutcome {
            summary: ResultSummary::new(vec![
                Metric::Count { n: hits.len() as u64, unit: "skills" },
                Metric::Count { n: all.len() as u64, unit: "searched" },
                Metric::State("lexical"),
            ]),
            body: ToolBody::Inline(out),
            // Installed-skill prose. See the module header on why this class and what it does
            // not buy.
            trust: TrustClass::UserAsserted,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }

    fn disclose(&self, name: &str) -> ToolOutcome {
        let skills = self.skills.lock().expect("the skill registry lock was poisoned");
        let Some(skill) = skills.get(name) else {
            // **Names what IS installed.** A bare "not found" invites the model to guess again,
            // and a second wrong guess costs another turn.
            let installed: Vec<&str> = skills.iter().map(|s| s.id().as_str()).collect();
            return failed(&if installed.is_empty() {
                format!("no skill named {name:?} is installed, and neither is any other.")
            } else {
                format!(
                    "no skill named {name:?} is installed. These are: {}.",
                    installed.join(", ")
                )
            });
        };

        match skill.body().read() {
            Err(e) => failed(&format!(
                "the skill `{}` is installed but its instructions could not be read: {e}",
                skill.id()
            )),
            Ok(body) => ToolOutcome {
                summary: ResultSummary::new(vec![
                    Metric::Count { n: 1, unit: "skills" },
                    Metric::Bytes { n: skill.body().bytes() },
                ]),
                body: ToolBody::Inline(body),
                trust: TrustClass::UserAsserted,
                failed: false,
                wall_ms: 0,
                preview: None,
            },
        }
    }

    fn use_tool(&self, args: &Args) -> ToolOutcome {
        let name = args.get("name").and_then(|v| v.as_text()).filter(|s| !s.trim().is_empty());
        let query = args.get("query").and_then(|v| v.as_text()).filter(|s| !s.trim().is_empty());

        match (name, query) {
            // **`name` wins when both are given.** Not arbitrary: `name` is the `Target` and
            // `query` is the `Payload` (`builtin.rs`), so resolving toward `name` means the
            // decision is made by the argument §9 actually checks the provenance of. Preferring
            // the payload would let the checked argument be present and ignored.
            (Some(name), _) => self.disclose(name),
            (None, Some(query)) => self.discover(query),
            (None, None) => failed(
                "`use` needs either `name` (load that skill) or `query` (search for one).",
            ),
        }
    }
}

/// Rank installed skills against a query. **One function**, so the `use` discovery path, the
/// per-turn surfacing in [`surface`] and any test of either cannot disagree about what is scored —
/// and so replacing BM25 with an embedder later is one edit rather than a search.
///
/// Scores exactly [`Skill::discovery_text`]: the description and the trigger phrases, never the
/// body. §7.1.
///
/// **Lifted out of `impl SkillTools` when surfacing landed.** A second ranker beside this one would
/// be two answers to *"which skill is relevant"*, which is the two-sides-silently-disagree shape.
fn rank<'a>(skills: &'a [&'a Skill], query: &str) -> Vec<(&'a Skill, f32)> {
    let texts: Vec<String> = skills.iter().map(|s| s.discovery_text()).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let scores = lexical::score_texts(&refs, query);

    let mut order: Vec<usize> = (0..skills.len()).collect();
    // Score descending, then id ascending. The tiebreak is not decoration: two equal scores
    // ordered by whatever the collection did would make one run's discovery differ from the
    // next, and `repro` compares runs byte for byte.
    order.sort_by(|a, b| {
        scores[*b].total_cmp(&scores[*a]).then_with(|| skills[*a].id().cmp(skills[*b].id()))
    });
    // A zero score means the query shares no vocabulary with the skill. Returning those would
    // pad the answer with skills the model then has to argue its way out of.
    order.retain(|i| scores[*i] > 0.0);
    order.truncate(DISCOVERY_LIMIT);
    order.into_iter().map(|i| (skills[i], scores[i])).collect()
}

/// **THE DISCOVERY BOOTSTRAP. ADR-051 amendment, 2026-08-24.**
///
/// Progressive disclosure shipped with a hole at the front of it: the body-loads-on-`use` half
/// worked, and the *discovery* half assumed something had told the model a library existed. Nothing
/// did. `SourceKind::Skills` had **zero producers** — a declared block kind nobody constructed — so
/// the only route to a skill was the model spontaneously deciding to call `use(query = ...)` with no
/// reason to suspect there was anything to find. Asked *"do you have anything that helps?"* it
/// searched the filesystem and reached for `bash`, which is the correct move on the information it
/// had.
///
/// Two lines, and they answer two different questions:
///
/// * **the count** — *"a skills library exists"*. Without it the category is invisible and the
///   model cannot ask about something it does not know is there. Costs ~12 tokens a turn.
/// * **the hits** — *"and this one matches what you were just asked"*. Ranked by [`rank`] against
///   the user's own message, so the cost is zero when nothing matches.
///
/// **This is a context block, not a tool result, and that is what makes the closing instruction
/// legitimate.** The same sentence inside a `use` result is an imperative in data the persona tells
/// the model to distrust — which is exactly why the old *"Load one with `use` and its name"* was
/// ignored. Here it is harness-authored context, the channel an operating instruction belongs in.
///
/// Returns `None` when no skills are installed, so a profile without a `skills/` directory pays
/// nothing and the block never appears.
pub(crate) fn surface(skills: &SkillRegistry, message: &str) -> Option<String> {
    let all: Vec<&Skill> = skills.iter().collect();
    if all.is_empty() {
        return None;
    }

    // **The constant half only.** See [`hits_for`] for the per-query half and why they are
    // separate: this string depends solely on how many skills are installed, so it is identical on
    // every turn of a session and can live in the cached prefix.
    let _ = message;
    Some(format!("{} skill(s) installed in this profile. Search them with `use`.\n", all.len()))
}

/// The skills that match THIS turn's message, or `None` when none do.
///
/// # Why this is not part of [`surface`] any more
///
/// It used to be appended to the same string, which put a **query-dependent** block inside the
/// leading system message. A server reuses its prompt cache only for a byte-identical prefix, so
/// the block appearing on one turn and vanishing on the next invalidated the whole prompt both
/// times — **measured at ±164 chars between consecutive turns**, and every appearance or
/// disappearance cost a full re-evaluation of ~9,900 tokens, about 140 ms.
///
/// The constant line stays in the prefix where it is free; this rides the tail with injected
/// memory, where a change costs only the tokens it actually adds. Same content, same behaviour for
/// the model, no cache invalidation.
pub(crate) fn hits_for(skills: &SkillRegistry, message: &str) -> Option<String> {
    let all: Vec<&Skill> = skills.iter().collect();
    let hits = rank(&all, message);
    if hits.is_empty() {
        return None;
    }
    let mut out = String::from("Possibly relevant here:\n");
    for (skill, _) in &hits {
        // Description only. The body is what `use(name = ...)` returns, and putting it here would
        // make every turn pay for instructions the model may never need -- which is the whole
        // point of progressive disclosure.
        out.push_str(&format!("- {}: {}\n", skill.id(), skill.description().text()));
    }
    out.push_str("Load one with `use` and its name to read its instructions.\n");
    Some(out)
}

fn harness_says(detail: &str, metrics: Vec<Metric>) -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(metrics),
        body: ToolBody::Inline(detail.to_string()),
        // The HARNESS computed this: it enumerated the registry and counted. No skill's prose is
        // in the answer, so no skill's class is inherited. `recall`'s precedent, exactly.
        trust: TrustClass::AgentObserved,
        failed: false,
        wall_ms: 0,
        preview: None,
    }
}

fn failed(detail: &str) -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::Count { n: 0, unit: "skills" }]),
        body: ToolBody::Inline(detail.to_string()),
        trust: TrustClass::AgentObserved,
        failed: true,
        wall_ms: 0,
        preview: None,
    }
}

impl<H: ToolHost> ToolHost for SkillTools<H> {
    fn executes(&self) -> Vec<ToolId> {
        // The inner host's set plus `use`, never a hardcoded list — `RecallTools`' reasoning, and
        // `verify_every_exposed_tool_is_runnable` reads this.
        let mut tools = self.inner.executes();
        tools.push(ToolId::new("use"));
        tools
    }

    fn execute(&mut self, tool: &ToolId, args: &Args, adjudication: &Adjudication) -> ToolOutcome {
        if tool.as_str() == "use" {
            return self.use_tool(args);
        }
        self.inner.execute(tool, args, adjudication)
    }

    /// **Forward the batch, or this wrapper silently un-parallelises the product.**
    ///
    /// The default `execute_batch` loops serially. `SkillTools` wraps `RecallTools` which wraps
    /// `FileSystemTools`, and the innermost one is where the concurrent fetch lives — so without
    /// this override the parallel path would exist, be tested, be green, and never run. That is
    /// exactly the trap `RecallTools` documents at length, one layer further out.
    ///
    /// `use` is served here; everything else is delegated **as one batch** and reassembled into
    /// input order.
    fn execute_batch(&mut self, items: &[marlowe_loop::BatchItem<'_>]) -> Vec<ToolOutcome> {
        let mut delegated_slots: Vec<usize> = Vec::new();
        let mut delegated: Vec<marlowe_loop::BatchItem<'_>> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if it.tool.as_str() != "use" {
                delegated_slots.push(i);
                delegated.push(marlowe_loop::BatchItem {
                    tool: it.tool,
                    args: it.args,
                    adjudication: it.adjudication,
                });
            }
        }

        let inner_out = if delegated.is_empty() {
            Vec::new()
        } else {
            self.inner.execute_batch(&delegated)
        };

        let mut out: Vec<Option<ToolOutcome>> = (0..items.len()).map(|_| None).collect();
        let mut got = inner_out.into_iter();
        for slot in &delegated_slots {
            // Positional, and short-counting is a failure rather than a shift: a missing inner
            // result must never slide onto a later call's position.
            match got.next() {
                Some(o) => out[*slot] = Some(o),
                None => break,
            }
        }
        for (i, it) in items.iter().enumerate() {
            if it.tool.as_str() == "use" {
                out[i] = Some(self.use_tool(it.args));
            }
        }

        out.into_iter()
            .enumerate()
            .map(|(i, o)| {
                o.unwrap_or_else(|| {
                    failed(&format!(
                        "the inner tool host returned no outcome for call {i} (`{}`)",
                        items[i].tool.as_str()
                    ))
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_tools::skill::load_skill;
    use std::path::PathBuf;

    /// A host that answers nothing, so every assertion below is about `SkillTools` itself.
    struct Nothing;

    impl ToolHost for Nothing {
        fn executes(&self) -> Vec<ToolId> {
            vec![ToolId::new("read")]
        }
        fn execute(&mut self, _: &ToolId, _: &Args, _: &Adjudication) -> ToolOutcome {
            failed("the inner host was reached")
        }
    }

    fn skill_file(dir: &std::path::Path, name: &str, description: &str, body: &str) -> PathBuf {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("SKILL.md");
        std::fs::write(
            &f,
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
        f
    }

    /// An `Allowed` decision. `SkillTools` never inspects it — what is under test here is the
    /// wiring, and the permission layer has its own suite.
    fn allowed() -> Adjudication {
        let tool = ToolId::new("use");
        Adjudication {
            decision: marlowe_permission::PermissionDecision {
                id: marlowe_permission::DecisionId(1),
                tool: tool.clone(),
                action_class: marlowe_permission::ActionClass {
                    tool,
                    shape: 0,
                    label: "test".into(),
                },
                outcome: marlowe_permission::Outcome::Allowed,
                blast_radius: marlowe_permission::BlastRadius {
                    verb: "use".into(),
                    scope: "test".into(),
                    reversible: true,
                    novelty: None,
                },
                taint: marlowe_permission::TaintSet::new(),
                reasons: Vec::new(),
            },
            handles: Default::default(),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-usetool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn host(root: &std::path::Path) -> SkillTools<Nothing> {
        let (registry, errors) = marlowe_tools::skill::scan(root, 0);
        assert!(errors.is_empty(), "the fixture skills must all load: {errors:?}");
        SkillTools::new(Nothing, Arc::new(Mutex::new(registry)))
    }

    fn registry(root: &std::path::Path) -> SkillRegistry {
        let (registry, errors) = marlowe_tools::skill::scan(root, 0);
        assert!(errors.is_empty(), "the fixture skills must all load: {errors:?}");
        registry
    }

    /// **The bootstrap: the model is told a library exists even when nothing matches.**
    ///
    /// This is the half that fixes *"do you have anything that helps?"* → filesystem search. The
    /// model cannot ask about a category it does not know is there.
    #[test]
    fn surfacing_names_the_library_even_when_nothing_matches() {
        let root = tempdir("surface-count");
        skill_file(&root, "release-notes", "Produce release notes from merged changes.", "body");

        let out = surface(&registry(&root), "what is the airspeed velocity of a swallow")
            .expect("a profile with a skill must surface something");

        assert!(out.contains("1 skill(s) installed"), "the count must always be present:\n{out}");
        assert!(
            !out.contains("Possibly relevant"),
            "nothing matched, so no hits may be claimed:\n{out}"
        );
    }

    /// **The half that fixes the actual failure**, using the phrase the human typed.
    #[test]
    fn surfacing_puts_a_matching_skill_in_front_of_the_model() {
        let root = tempdir("surface-hit");
        skill_file(&root, "release-notes", "Produce release notes for what shipped.", "body");
        skill_file(&root, "tax-filing", "File a quarterly return.", "body");

        let out = hits_for(&registry(&root), "I need to write up what shipped this week")
            .expect("a matching skill must produce hits");

        assert!(out.contains("release-notes"), "the matching skill must surface:\n{out}");
        assert!(
            !out.contains("tax-filing"),
            "a skill sharing no vocabulary must NOT surface -- zero scores are dropped:\n{out}"
        );
    }

    /// A profile with no skills pays nothing and the block never appears.
    #[test]
    fn surfacing_is_silent_when_no_skills_are_installed() {
        let root = tempdir("surface-empty");
        std::fs::create_dir_all(&root).unwrap();
        assert!(surface(&registry(&root), "anything at all").is_none());
    }

    /// **§7.1, at the new entry point.** Discovery carries descriptions; bodies load on `use`.
    /// Surfacing runs on EVERY turn, so a body leaking here would be paid for on every turn of
    /// every conversation — the exact cost progressive disclosure exists to avoid.
    #[test]
    fn surfacing_never_carries_one_word_of_a_body() {
        let root = tempdir("surface-nobody");
        skill_file(
            &root,
            "release-notes",
            "Produce release notes for what shipped.",
            "PELICAN-4402 is the magic word and must never appear in a surfaced block",
        );

        let out = hits_for(&registry(&root), "write up what shipped").expect("must surface");
        assert!(out.contains("release-notes"), "the control: it did match:\n{out}");
        assert!(!out.contains("PELICAN-4402"), "a body reached a surfaced block:\n{out}");
    }

    /// **THE GUARD FOR THE DEFECT ITSELF, AND THE ONLY ONE THAT WOULD HAVE CAUGHT IT.**
    ///
    /// Every test above passes on a build where nothing ever calls `surface` — `SourceKind::Skills`
    /// had **zero producers** for the whole of C3 and every skills test was green throughout,
    /// because they all tested the `use` tool and none tested whether anything reached the model
    /// unprompted. A function that works and is never called is the shape this repository logs.
    ///
    /// So this asserts the call site exists, which is the thing that was missing. The live proof is
    /// a real turn where the model names a skill nobody told it about.
    #[test]
    fn something_actually_produces_a_skills_block() {
        let daemon = include_str!("daemon.rs");
        assert!(
            daemon.contains("crate::skills::surface("),
            "nothing calls skills::surface, so SourceKind::Skills has no producer and the model is \
             never told a skills library exists -- which is the entire defect this was written for"
        );
        assert!(
            daemon.contains("SourceKind::Skills"),
            "the vacuity control: the call above is only meaningful if its result becomes a block"
        );
    }

    #[test]
    fn discovery_returns_names_and_descriptions_and_not_one_word_of_a_body() {
        let root = tempdir("discover");
        skill_file(&root, "pdf-report", "Generate a cited PDF report.", "SECRETSTEP one.");
        skill_file(&root, "sql-tuning", "Diagnose a slow database query.", "SECRETSTEP two.");
        let mut h = host(&root);

        let out = h.execute(
            &ToolId::new("use"),
            &Args::new().text("query", "make me a pdf report"),
            &allowed(),
        );
        let ToolBody::Inline(text) = &out.body else { panic!("expected an inline body") };

        assert!(text.contains("pdf-report"), "the matching skill is missing: {text}");
        assert!(
            !text.contains("SECRETSTEP"),
            "a body reached the discovery result. §7.1: only description + trigger_phrases"
        );
        assert!(!out.failed);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disclosure_returns_the_body_and_only_when_asked_by_name() {
        let root = tempdir("disclose");
        skill_file(&root, "pdf-report", "Generate a cited PDF report.", "SECRETSTEP one.");
        let mut h = host(&root);

        let out = h.execute(
            &ToolId::new("use"),
            &Args::new().text("name", "pdf-report"),
            &allowed(),
        );
        let ToolBody::Inline(text) = &out.body else { panic!("expected an inline body") };
        assert!(text.contains("SECRETSTEP one."), "the body did not load: {text}");
        assert_eq!(out.trust, TrustClass::UserAsserted);
        assert!(!out.failed);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The two halves of progressive disclosure, in one assertion: the same registry, the same
    /// tool, and the body crosses on exactly one of the two calls.
    ///
    /// **This is the control for the test above.** Without it, a `discover` that returned nothing
    /// at all would satisfy "no body in the discovery result".
    #[test]
    fn the_body_crosses_on_disclosure_and_not_on_discovery() {
        let root = tempdir("both");
        skill_file(&root, "pdf-report", "Generate a cited PDF report.", "SECRETSTEP one.");
        let mut h = host(&root);
        let adj = allowed();

        let discovered = h.execute(
            &ToolId::new("use"),
            &Args::new().text("query", "pdf report"),
            &adj,
        );
        let disclosed =
            h.execute(&ToolId::new("use"), &Args::new().text("name", "pdf-report"), &adj);

        let body_of = |o: &ToolOutcome| match &o.body {
            ToolBody::Inline(s) => s.clone(),
            _ => panic!("expected inline"),
        };
        let d = body_of(&discovered);
        let l = body_of(&disclosed);

        assert!(d.contains("pdf-report"), "discovery found nothing, so it proves nothing");
        assert!(!d.contains("SECRETSTEP"));
        assert!(l.contains("SECRETSTEP"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_that_is_not_installed_says_what_is() {
        let root = tempdir("missing");
        skill_file(&root, "pdf-report", "Generate a cited PDF report.", "body");
        let mut h = host(&root);
        let out = h.execute(
            &ToolId::new("use"),
            &Args::new().text("name", "no-such-skill"),
            &allowed(),
        );
        assert!(out.failed);
        let ToolBody::Inline(text) = &out.body else { panic!() };
        assert!(text.contains("pdf-report"), "the refusal must name what IS installed: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `name` is the `Target` and `query` is the `Payload`. Resolving toward `name` means the
    /// decision is made by the argument the permission layer actually provenance-checks.
    #[test]
    fn name_wins_over_query_when_both_are_supplied() {
        let root = tempdir("both-args");
        skill_file(&root, "alpha", "The first skill.", "ALPHABODY");
        skill_file(&root, "beta", "The second skill.", "BETABODY");
        let mut h = host(&root);

        let out = h.execute(
            &ToolId::new("use"),
            &Args::new().text("name", "alpha").text("query", "second skill"),
            &allowed(),
        );
        let ToolBody::Inline(text) = &out.body else { panic!() };
        assert!(text.contains("ALPHABODY"));
        assert!(!text.contains("BETABODY"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn use_is_in_the_executed_set_alongside_whatever_is_wrapped() {
        let root = tempdir("executes");
        let h = host(&root);
        let ids: Vec<String> = h.executes().iter().map(|t| t.as_str().to_string()).collect();
        assert!(ids.contains(&"use".to_string()));
        assert!(ids.contains(&"read".to_string()), "the inner host's set must survive");
    }

    #[test]
    fn a_profile_with_no_skills_says_so_rather_than_failing() {
        let root = tempdir("empty");
        let mut h = host(&root);
        let out = h.execute(
            &ToolId::new("use"),
            &Args::new().text("query", "anything"),
            &allowed(),
        );
        assert!(!out.failed, "an empty skills directory is not an error");
        let ToolBody::Inline(text) = &out.body else { panic!() };
        assert!(text.contains("no skills are installed"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
