//! The `recall` tool — §5.5's explicit memory search. M2 Session D.
//!
//! # Why this is a wrapper rather than an arm in `marlowe-exec`
//!
//! `marlowe-exec` executes tools against the filesystem and the network. Teaching it about beliefs
//! would point the tool-executor crate at the memory crate for one arm, and every future tool
//! touching a subsystem would do the same until `marlowe-exec` depended on everything. So the
//! daemon — the composition root, which already owns both — wraps the filesystem host and answers
//! `recall` itself, delegating everything else unchanged.
//!
//! # Why `recall` is not gated by the declared operating point
//!
//! Auto-injection is content the model did **not** ask for, arriving in its context where it reads
//! as its own knowledge. K1 condition 3 governs that, at 10% coverage, because the cost of being
//! wrong is a confident false statement the user cannot trace.
//!
//! `recall` is the model deliberately searching, and evaluating what comes back. CONTRACTS §3.6
//! says it sees **tombstones and unmatured entries** — precisely the things auto-injection must
//! not. Applying the injection cut point here would make explicit search strictly worse than the
//! thing it exists to be better than, and brief §5.5 asks for the opposite: *"recall recovered by
//! making the agent's explicit memory search tool excellent."*
//!
//! **Security is unchanged by that choice.** Recalled text lands in the context view carrying its
//! own trust class, exactly as an injected memory does, and `ContextView::trust_floor` is `min`
//! over every block. Nothing is smuggled past a guard; a different question is being answered.

use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_loop::driver::{ToolBody, ToolHost, ToolOutcome};
use marlowe_tools::{Metric, ResultSummary};
use marlowe_memory::cue::lexical;
use marlowe_memory::BeliefStore;
use marlowe_permission::{Adjudication, Args};
use marlowe_tools::ToolId;

/// How many memories one `recall` returns.
///
/// Small on purpose. `recall` competes for the same context the conversation needs, and a search
/// that returns forty results has moved the retrieval problem into the model's attention rather
/// than solving it.
const RECALL_LIMIT: usize = 5;

/// The daemon's tool host: the filesystem tools, plus `recall` over the belief store.
pub struct RecallTools<H: ToolHost> {
    inner: H,
    beliefs: Arc<Mutex<BeliefStore>>,
}

impl<H: ToolHost> RecallTools<H> {
    pub fn new(inner: H, beliefs: Arc<Mutex<BeliefStore>>) -> Self {
        Self { inner, beliefs }
    }

    fn recall(&self, args: &Args) -> ToolOutcome {
        let Some(query) = args.get("query").and_then(|v| v.as_text()) else {
            return failed("`query` is required");
        };

        let beliefs = self.beliefs.lock().expect("the belief store lock was poisoned");
        // §4.3: hot ∪ cold. **Tombstones and unmatured entries included** — this is the "I used to
        // know" path, and excluding them would make explicit recall a slower copy of injection.
        let candidates = beliefs.recall_candidates();
        if candidates.is_empty() {
            return ToolOutcome {
                summary: ResultSummary::new(vec![Metric::Count { n: 0, unit: "memories" }]),
                body: ToolBody::Inline(
                    "no memories are stored yet. Nothing has been remembered in this profile."
                        .to_string(),
                ),
                // The HARNESS computed this: it enumerated the store and counted zero. No belief's
                // content is in the answer, so no belief's class is inherited.
                trust: TrustClass::AgentObserved,
                failed: false,
                wall_ms: 0,
                preview: None,
            };
        }

        let scores = lexical::score_all(&candidates, query);
        let mut ranked: Vec<usize> = (0..candidates.len()).collect();
        // Score descending, then id ascending. The id tiebreak is not decoration: two equal scores
        // ordered by whatever the collection did would make one run's recall differ from the next.
        ranked.sort_by(|a, b| {
            scores[*b]
                .total_cmp(&scores[*a])
                .then_with(|| candidates[*a].id.cmp(&candidates[*b].id))
        });
        // A zero lexical score means the query shares nothing with the memory. Returning those
        // would pad the answer with text the model then has to argue with.
        ranked.retain(|i| scores[*i] > 0.0);
        ranked.truncate(RECALL_LIMIT);

        if ranked.is_empty() {
            return ToolOutcome {
                summary: ResultSummary::new(vec![
                    Metric::Count { n: 0, unit: "memories" },
                    Metric::Count { n: candidates.len() as u64, unit: "searched" },
                ]),
                body: ToolBody::Inline(format!(
                    "nothing stored matches {query:?}. {} memories were searched.",
                    candidates.len()
                )),
                trust: TrustClass::AgentObserved,
                failed: false,
                wall_ms: 0,
                preview: None,
            };
        }

        // **`min` over what is actually returned — §3.3's worst case.** A `ToolOutcome` carries one
        // trust class, so a result mixing a user-asserted memory with a web-derived one must take
        // the lower. Taking the higher, or the first, would let one untrusted belief ride into the
        // context at another's authority: laundering through the search tool.
        let floor = ranked
            .iter()
            .map(|i| candidates[*i].effective_trust)
            .min()
            .unwrap_or(TrustClass::AgentObserved);

        let mut out = String::new();
        for i in &ranked {
            let e = candidates[*i];
            // The fidelity and the maturation state are stated, because a tombstone with empty text
            // is otherwise indistinguishable from a memory that says nothing, and an unmatured
            // belief is one the model should weigh differently from a settled one.
            out.push_str(&format!(
                "- [{}] {} ({:?}, {})\n",
                e.id,
                if e.text.is_empty() { "(forgotten — the record was tombstoned)" } else { &e.text },
                e.effective_trust,
                match e.silent_until {
                    Some(_) => "not yet matured",
                    None => "matured",
                }
            ));
        }

        ToolOutcome {
            summary: ResultSummary::new(vec![
                Metric::Count { n: ranked.len() as u64, unit: "memories" },
                Metric::Count { n: candidates.len() as u64, unit: "searched" },
            ]),
            body: ToolBody::Inline(out),
            trust: floor,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }
}

fn failed(detail: &str) -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::Count { n: 0, unit: "memories" }]),
        body: ToolBody::Inline(detail.to_string()),
        trust: TrustClass::AgentObserved,
        failed: true,
        wall_ms: 0,
        preview: None,
    }
}

impl<H: ToolHost> ToolHost for RecallTools<H> {
    fn executes(&self) -> Vec<ToolId> {
        // **The inner host's set plus `recall`, never a hardcoded list.** A hardcoded one would
        // drift the moment `marlowe-exec` gained or lost an executor, and
        // `verify_every_exposed_tool_is_runnable` would then refuse at load with a name that had
        // nothing to do with the change.
        let mut tools = self.inner.executes();
        tools.push(ToolId::new("recall"));
        tools
    }

    fn execute(
        &mut self,
        tool: &ToolId,
        args: &Args,
        adjudication: &Adjudication,
    ) -> ToolOutcome {
        if tool.as_str() == "recall" {
            return self.recall(args);
        }
        self.inner.execute(tool, args, adjudication)
    }

    /// **Forward the batch, or this wrapper silently un-parallelises the product.**
    ///
    /// `ToolHost::execute_batch` has a default implementation that loops serially, which is what
    /// makes the trait extension safe for every other implementor. It is also exactly what makes
    /// *this* type dangerous: `RecallTools` wraps `FileSystemTools` in the **only** tool host the
    /// daemon ever builds, so without this override the concurrent fetch path would exist, be
    /// tested, be green, and never once run in the shipped product — the inner host's override
    /// would simply never be reached.
    ///
    /// That is the same shape as a persona that loads but never reaches the request body: a
    /// capability that is present at one layer and dropped by the layer above it, with nothing
    /// observing the difference. `a_batch_of_web_calls_reaches_the_inner_host_as_a_batch` asserts
    /// the forwarding rather than trusting this comment.
    ///
    /// `recall` is served here and everything else is delegated **as one batch**, then results are
    /// reassembled into input order.
    fn execute_batch(
        &mut self,
        items: &[marlowe_loop::BatchItem<'_>],
    ) -> Vec<ToolOutcome> {
        let mut delegated_slots: Vec<usize> = Vec::new();
        let mut delegated: Vec<marlowe_loop::BatchItem<'_>> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if it.tool.as_str() != "recall" {
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
            // **Positional, and short-counting is a failure rather than a shift.** If the inner
            // host returned too few, the remaining delegated slots stay `None` and become an
            // explicit error below — they must never slide onto a later call's position.
            match got.next() {
                Some(o) => out[*slot] = Some(o),
                None => break,
            }
        }

        out.into_iter()
            .enumerate()
            .map(|(i, o)| match o {
                Some(o) => o,
                None if items[i].tool.as_str() == "recall" => self.recall(items[i].args),
                None => ToolOutcome {
                    summary: ResultSummary::new(vec![Metric::State("host-error")]),
                    body: ToolBody::Inline(
                        "the inner tool host returned no result for this call".into(),
                    ),
                    trust: TrustClass::AgentObserved,
                    failed: true,
                    wall_ms: 0,
                    preview: None,
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod batch_forwarding_tests {
    use super::*;

    /// A host that records whether it was handed a batch or a sequence of single calls.
    #[derive(Default)]
    struct Spy {
        /// The size of each `execute_batch` call it received.
        batches: Arc<Mutex<Vec<usize>>>,
        /// How many times the single-call path was used instead.
        singles: Arc<Mutex<usize>>,
    }

    impl ToolHost for Spy {
        fn executes(&self) -> Vec<ToolId> {
            vec![ToolId::new("web")]
        }

        fn execute(&mut self, _t: &ToolId, _a: &Args, _adj: &Adjudication) -> ToolOutcome {
            *self.singles.lock().unwrap() += 1;
            ok("single")
        }

        fn execute_batch(&mut self, items: &[marlowe_loop::BatchItem<'_>]) -> Vec<ToolOutcome> {
            self.batches.lock().unwrap().push(items.len());
            items.iter().map(|i| ok(i.args.get("url").and_then(|v| v.as_text()).unwrap_or("?"))).collect()
        }
    }

    fn ok(body: &str) -> ToolOutcome {
        ToolOutcome {
            summary: ResultSummary::new(vec![Metric::State("ok")]),
            body: ToolBody::Inline(body.to_string()),
            trust: TrustClass::UntrustedContent,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }

    fn args(url: &str) -> Args {
        Args::new().with("url", marlowe_permission::ArgValue::Text(url.to_string()))
    }

    /// An `Allowed` decision. The spy never inspects it — what is under test is the WIRING, not
    /// the permission layer, which has its own suite.
    fn allowed(tool: &ToolId) -> Adjudication {
        Adjudication {
            decision: marlowe_permission::PermissionDecision {
                id: marlowe_permission::DecisionId(1),
                tool: tool.clone(),
                action_class: marlowe_permission::ActionClass {
                    tool: tool.clone(),
                    shape: 0,
                    label: "test".into(),
                },
                outcome: marlowe_permission::Outcome::Allowed,
                blast_radius: marlowe_permission::BlastRadius {
                    verb: "web".into(),
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

    /// **The wrapper must not swallow the batch.**
    ///
    /// `execute_batch` has a serial default, so a `RecallTools` that failed to override it would
    /// compile, pass every other test, and quietly turn every concurrent fetch back into a
    /// sequence — in the ONLY tool host the daemon builds. The assertion is on what the inner
    /// host actually received, not on what this type declares.
    #[test]
    fn a_batch_of_web_calls_reaches_the_inner_host_as_a_batch() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let singles = Arc::new(Mutex::new(0));
        let spy = Spy { batches: Arc::clone(&batches), singles: Arc::clone(&singles) };
        let beliefs = Arc::new(Mutex::new(BeliefStore::default()));
        let mut host = RecallTools::new(spy, beliefs);

        let a = [args("https://a.example/"), args("https://b.example/"), args("https://c.example/")];
        let adj = allowed(&ToolId::new("web"));
        let web = ToolId::new("web");
        let items: Vec<marlowe_loop::BatchItem<'_>> = a
            .iter()
            .map(|args| marlowe_loop::BatchItem { tool: &web, args, adjudication: &adj })
            .collect();

        let out = host.execute_batch(&items);

        assert_eq!(*batches.lock().unwrap(), vec![3], "the inner host must see ONE batch of 3");
        assert_eq!(*singles.lock().unwrap(), 0, "no call may fall back to the single-call path");
        assert_eq!(out.len(), 3);
        // Positional attribution: result i belongs to call i.
        for (i, expect) in ["https://a.example/", "https://b.example/", "https://c.example/"]
            .iter()
            .enumerate()
        {
            assert_eq!(out[i].body, ToolBody::Inline((*expect).to_string()), "slot {i}");
        }
    }

    /// `recall` is served by the wrapper, everything else is delegated — and the results still
    /// land in input order.
    #[test]
    fn recall_is_served_locally_without_breaking_the_order_of_the_rest() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let spy = Spy { batches: Arc::clone(&batches), singles: Arc::new(Mutex::new(0)) };
        let beliefs = Arc::new(Mutex::new(BeliefStore::default()));
        let mut host = RecallTools::new(spy, beliefs);

        let a = [args("https://a.example/"), args("anything"), args("https://c.example/")];
        let adj = allowed(&ToolId::new("web"));
        let (web, rec) = (ToolId::new("web"), ToolId::new("recall"));
        let items = vec![
            marlowe_loop::BatchItem { tool: &web, args: &a[0], adjudication: &adj },
            marlowe_loop::BatchItem { tool: &rec, args: &a[1], adjudication: &adj },
            marlowe_loop::BatchItem { tool: &web, args: &a[2], adjudication: &adj },
        ];

        let out = host.execute_batch(&items);
        assert_eq!(*batches.lock().unwrap(), vec![2], "only the two web calls are delegated");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].body, ToolBody::Inline("https://a.example/".into()));
        assert_eq!(out[2].body, ToolBody::Inline("https://c.example/".into()));
    }
}
