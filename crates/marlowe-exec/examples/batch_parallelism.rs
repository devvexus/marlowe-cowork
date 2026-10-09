//! **Are the agent's batched tool calls actually parallel?**
//!
//! Answered by driving the REAL `Engine` with a real batch of tool calls, not by reading the
//! source. Each call sleeps a known duration and records its own start and end instant; the
//! answer is then arithmetic on the observed intervals rather than an opinion about a `for` loop.
//!
//! The standing rule applies to this probe as much as anything else: **what would this print if
//! the calls WERE parallel?** A serial run of N calls at D each takes N*D and shows max-overlap 1;
//! a parallel run takes ~D and shows max-overlap N. The two readings are not close, which is what
//! makes the measurement worth taking.
//!
//! Run: `cargo run --release -p marlowe-exec --example batch_parallelism`

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_contract::TrustClass;
use marlowe_loop::{
    ApprovalGate, Budget, CapabilityProfile, ClockSource, ContextView, Engine, ModelCall,
    ModelDriver, ModelStep, OutputContract, Ports, ProviderError, Provenance, Recorder, Run, RunId,
    SessionId, SessionState, Summarizer, ToolBody, ToolHost, ToolOutcome, TurnSink, Usage,
};
use marlowe_permission::{scope::WorkspaceScope, Adjudication, Args, Tier};
use marlowe_tools::{builtin_registry, ExposedSet, Metric, ResultSummary, ToolId};

const CALLS: usize = 8;
const SLEEP: Duration = Duration::from_millis(250);

/// Records when each execution began and ended.
#[derive(Clone, Default)]
struct Timeline(Arc<Mutex<Vec<(Instant, Instant)>>>);

#[derive(Clone, Default)]
struct BatchSizes(Arc<Mutex<Vec<usize>>>);

struct SleepyTools {
    timeline: Timeline,
    /// The size of every batch the ENGINE handed over. This is the engine's contribution,
    /// separate from whether the host chooses to parallelise it.
    sizes: BatchSizes,
}

fn work(timeline: &Timeline) -> ToolOutcome {
    let start = Instant::now();
    // Stands in for a network round trip. Sleeping is the honest model of a `web` fetch:
    // the thread is blocked and the CPU is idle, which is exactly when parallelism pays.
    std::thread::sleep(SLEEP);
    let end = Instant::now();
    timeline.0.lock().unwrap().push((start, end));
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::State("ok")]),
        body: ToolBody::Inline("ok".into()),
        trust: TrustClass::AgentObserved,
        failed: false,
        wall_ms: SLEEP.as_millis() as u64,
        preview: None,
    }
}

impl ToolHost for SleepyTools {
    fn executes(&self) -> Vec<ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()
    }

    fn execute(&mut self, _tool: &ToolId, _args: &Args, _a: &Adjudication) -> ToolOutcome {
        work(&self.timeline)
    }

    /// **Mirrors `FileSystemTools::execute_batch`.**
    ///
    /// The first version of this probe left this method un-overridden and therefore inherited the
    /// trait's serial default -- so it printed `VERDICT: SERIAL` against an engine that was
    /// already delivering the whole batch in one call. It was measuring its own mock, not the
    /// product. The default being serial is exactly what makes the trait extension safe, and
    /// exactly what makes a mock a misleading instrument.
    fn execute_batch(&mut self, items: &[marlowe_loop::BatchItem<'_>]) -> Vec<ToolOutcome> {
        self.sizes.0.lock().unwrap().push(items.len());
        if items.len() <= 1 {
            return items.iter().map(|_| work(&self.timeline)).collect();
        }
        let timeline = &self.timeline;
        let slots: Vec<Mutex<Option<ToolOutcome>>> =
            (0..items.len()).map(|_| Mutex::new(None)).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..items.len() {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= items.len() {
                        break;
                    }
                    *slots[i].lock().unwrap() = Some(work(timeline));
                });
            }
        });
        slots.into_iter().map(|s| s.into_inner().unwrap().unwrap()).collect()
    }
}

struct Script(Vec<ModelCall>);

impl ModelDriver for Script {
    fn call(
        &mut self,
        _view: &ContextView,
        _tools: &ExposedSet,
        _limits: marlowe_loop::CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        if self.0.is_empty() {
            return Err(ProviderError { detail: "script exhausted".into(), retriable: false });
        }
        Ok(self.0.remove(0))
    }
    fn failover(&mut self, _e: &ProviderError) -> bool {
        false
    }
}

struct Nothing;
impl Summarizer for Nothing {
    fn summarize(&mut self, _v: &ContextView) -> String {
        String::new()
    }
}
impl ApprovalGate for Nothing {
    fn await_approval(&mut self, _r: &marlowe_permission::BlastRadius) -> bool {
        true
    }
}
impl TurnSink for Nothing {
    fn emit(&mut self, e: marlowe_loop::TurnEvent) {
        // Blocked calls surface here. A probe that cannot see a refusal would report
        // "0 executed" as though it were a timing result.
        if let marlowe_loop::TurnEvent::Degraded { what, .. } = &e {
            eprintln!("  [degraded] {what:?}");
        }
    }
}
impl marlowe_loop::Control for Nothing {
    fn cancelled(&self, _run: marlowe_loop::RunId) -> bool {
        false
    }
}
impl ClockSource for Nothing {
    fn now_ms(&mut self) -> i64 {
        0
    }
}
impl Recorder for Nothing {
    fn append(
        &mut self,
        _clock: marlowe_contract::Clock,
        _kind: marlowe_journal::EventKind,
        _run: RunId,
        _session: SessionId,
        _payload: serde_json::Value,
    ) -> Result<u64, String> {
        Ok(0)
    }
}

fn main() {
    // A REAL workspace with a REAL file. The first version used `Unavailable` as the path scope,
    // every `read` was refused at adjudication, and the probe printed `executed 0` beside a
    // confident "SERIAL" verdict -- a vacuous reading that looked exactly like a result.
    // `executed` is printed for precisely this reason.
    let ws = std::env::temp_dir().join("marlowe-batch-probe");
    std::fs::create_dir_all(&ws).expect("workspace");
    std::fs::write(ws.join("notes.md"), "probe fixture
").expect("fixture");

    let timeline = Timeline::default();

    // ONE assistant message emitting CALLS tool calls — the shape the model actually produces
    // when asked to fetch several pages, and the shape `driver.rs` documents as provably
    // independent ("no call here can have been shaped by another call's output").
    let calls: Vec<marlowe_loop::ToolInvocation> = (0..CALLS)
        .map(|i| marlowe_loop::ToolInvocation {
            id: format!("call_{i}"),
            tool: ToolId::new("read"),
            args: Args::new().with("path", marlowe_permission::ArgValue::Text("./notes.md".into())),
        })
        .collect();

    let mut engine = Engine::new(
        builtin_registry().expect("manifests load"),
        WorkspaceScope::new().expect("verified platform"),
        100_000,
        10_000,
        ws.clone(),
        Tier::Act,
    );
    let mut driver = Script(vec![
        ModelCall {
            usage: Usage { completion_tokens: 10, ..Usage::default() },
            step: ModelStep::ToolCall { calls },
        },
        ModelCall {
            usage: Usage { completion_tokens: 5, ..Usage::default() },
            step: ModelStep::Say("done".into()),
        },
    ]);
    let sizes = BatchSizes::default();
    let mut tools = SleepyTools { timeline: timeline.clone(), sizes: sizes.clone() };
    let mut run = Run::root(
        RunId::from_name("probe"),
        SessionId::from_name("probe"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::default();
    let mut provenance = Provenance::default();
    let (mut s, mut g, mut k, mut c, mut cl, mut r) =
        (Nothing, Nothing, Nothing, Nothing, Nothing, Nothing);
    let mut ports = Ports {
        escalations: None,
        driver: &mut driver,
        summarizer: &mut s,
        tools: &mut tools,
        memory: None,
        approvals: &mut g,
        sink: &mut k,
        control: &mut c,
        clock: &mut cl,
        recorder: &mut r,
    };

    let t0 = Instant::now();
    let _ = engine.run(&mut run, &mut state, &mut provenance, &mut ports);
    let wall = t0.elapsed();

    let spans = timeline.0.lock().unwrap().clone();
    let executed = spans.len();

    // Max overlap: the largest number of executions in flight at any instant.
    let mut max_overlap = 0usize;
    for (s0, e0) in &spans {
        let n = spans.iter().filter(|(s1, e1)| s1 < e0 && e1 > s0).count();
        max_overlap = max_overlap.max(n);
    }

    let serial_prediction = SLEEP * executed as u32;
    let parallel_prediction = SLEEP;

    println!("== Are batched tool calls parallel? ==\n");
    println!("  batch size                 {CALLS}");
    println!("  executed                   {executed}");
    println!("  per-call sleep             {} ms", SLEEP.as_millis());
    println!("  observed wall              {} ms", wall.as_millis());
    println!("  max concurrent executions  {max_overlap}");
    println!("  batches delivered by loop  {:?}", sizes.0.lock().unwrap());
    println!();
    println!("  if SERIAL,   predicted     {} ms  (overlap 1)", serial_prediction.as_millis());
    println!("  if PARALLEL, predicted     ~{} ms  (overlap {CALLS})", parallel_prediction.as_millis());
    println!();
    let verdict = if max_overlap <= 1 { "SERIAL" } else { "PARALLEL" };
    println!("  VERDICT: {verdict}");
    println!(
        "\n  (observed wall is {:.2}x the serial prediction and {:.1}x the parallel one)",
        wall.as_secs_f64() / serial_prediction.as_secs_f64().max(1e-9),
        wall.as_secs_f64() / parallel_prediction.as_secs_f64().max(1e-9),
    );
}
