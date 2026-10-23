//! The run loop (design 3.11, as built on Starlark).
//!
//! Starlark has no coroutines, so concurrency comes from deterministic replay. The script runs
//! from the top; an `agent()` whose result is not known yet is issued to the host and raises a
//! "waiting" signal that unwinds the script (`parallel` and `pipeline` catch it per thunk or item,
//! so every independent branch issues its next agent before the run waits). When agents finish,
//! the script runs again from the top on a fresh heap: finished calls return their results at
//! once, so control flow reaches the next unknown call. Scripts are deterministic (no clock, no
//! randomness, no I/O), each call has a stable id from its position in the script, and every
//! re-run starts from scratch, so nothing a script does is ever applied twice.
//!
//! The same mechanism gives resume: a journal of finished calls seeds the results, so a resumed run
//! replays to where the last one stopped without a model call.
//!
//! Every pass is bounded, so a script always ends and never exhausts the machine: statements,
//! comprehension iterations and lambda calls share one count (neither of the last two runs a
//! statement, so the source is rewritten to tick, [`validate::guarded_source`]); each tick checks
//! the pass's heap ceiling and its time limit; `*`, `+` and `<<` go through a size check before
//! they build anything; `range` is capped; a prompt and a log line have a size limit; and a panic
//! inside the interpreter ends the run as an error. Nothing in a script runs before it is approved:
//! [`check`] resolves every name without running a statement.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::Value as Json;
use starlark::any::ProvidesStaticType;
use starlark::codemap::FileSpanRef;
use starlark::docs::{DocItem, DocMember};
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{BeforeStmtFuncDyn, Evaluator};
use starlark::syntax::AstModule;
use starlark::values::dict::AllocDict;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, Value};

use crate::host::{AgentCall, AgentHost, AgentOpts, AgentResult, AgentStatus, Completion, Control, Observer, WfEvent};
use crate::journal::Resume;
use crate::schema;
use crate::validate::{self, Diagnostic, Validated};

/// At most this many agents per run (WF2).
pub const MAX_AGENTS: u32 = 1000;
/// At most this many items in one `parallel` or `pipeline` call (WF2).
pub const MAX_ITEMS: usize = 4096;
/// At most this many concurrent agents, before the CPU and model-slot limits (WF2).
pub const MAX_CONCURRENCY: usize = 16;
/// Statements (and comprehension iterations and lambda calls) one pass of the script may execute.
pub const MAX_STATEMENTS: u64 = 5_000_000;
/// Bytes one pass's heap may hold.
pub const MAX_HEAP_BYTES: usize = 256 << 20;
/// How long one pass may run.
pub const MAX_PASS_TIME: Duration = Duration::from_secs(120);
/// The largest value `*` or `+` may build (string bytes, or 8 per list or tuple element).
pub const MAX_VALUE_BYTES: u128 = 16 << 20;
/// The longest prompt `agent()` takes (about 250k tokens, past any context window).
pub const MAX_PROMPT_BYTES: usize = 1 << 20;
/// The longest `log` line kept; longer ones are cut.
const MAX_LOG_CHARS: usize = 4000;
/// The most numbers `range()` gives.
pub const MAX_RANGE: i64 = 100_000;
/// The longest integer (in decimal digits) `*` and `<<` may work on.
const MAX_INT_DIGITS: usize = 10_000;
/// The largest shift `<<` takes.
const MAX_SHIFT: i64 = 4096;
/// Starlark call depth.
const MAX_STACK: usize = 100;
/// The message a waiting call unwinds the script with (never shown to the model).
const WAITING: &str = "workflow: waiting for agents";
/// What stops [`check`]'s pass once every name is resolved (never shown to the model).
const COMPILED: &str = "workflow: compiled";

/// The concurrency for this machine: `min(16, CPUs - 2, model slots)`, at least 1.
pub fn default_concurrency(model_slots: usize) -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    MAX_CONCURRENCY.min(cpus.saturating_sub(2)).min(model_slots).max(1)
}

/// The conductor's own pool, which a linked [`Budget`] reads and charges (F3's `TokenPool`).
pub trait PoolLink: Send + Sync + std::fmt::Debug {
    /// The armed target; `None` when the user set none.
    fn total(&self) -> Option<u64>;
    fn spent(&self) -> u64;
    fn charge(&self, tokens: u64);
}

/// One token pool shared by the main loop and every workflow of a session (WF8): `+500k`.
/// Standalone ([`Budget::new`]) it counts on its own; linked ([`Budget::linked`]) every reading
/// and charge goes to the conductor's pool, so the main loop and its workflows spend one total.
#[derive(Debug, Default)]
pub struct Budget {
    total: Option<u64>,
    spent: AtomicU64,
    link: Option<Arc<dyn PoolLink>>,
}

impl Budget {
    pub fn new(total: Option<u64>) -> Arc<Budget> {
        Arc::new(Budget { total, spent: AtomicU64::new(0), link: None })
    }

    pub fn linked(link: Arc<dyn PoolLink>) -> Arc<Budget> {
        Arc::new(Budget { total: None, spent: AtomicU64::new(0), link: Some(link) })
    }

    pub fn total(&self) -> Option<u64> {
        match &self.link {
            Some(l) => l.total(),
            None => self.total,
        }
    }

    pub fn spent(&self) -> u64 {
        match &self.link {
            Some(l) => l.spent(),
            None => self.spent.load(Ordering::SeqCst),
        }
    }

    pub fn add(&self, tokens: u64) {
        match &self.link {
            Some(l) => l.charge(tokens),
            None => {
                self.spent.fetch_add(tokens, Ordering::SeqCst);
            }
        }
    }

    pub fn remaining(&self) -> Option<u64> {
        self.total().map(|t| t.saturating_sub(self.spent()))
    }
}

/// Finds a saved workflow for `workflow(name)` inside a script.
pub trait ScriptResolver: Send + Sync {
    /// `(filename, source)` of `name_or_path`.
    fn resolve(&self, name_or_path: &str) -> Result<(String, String), String>;
}

/// How a run is set up.
#[derive(Clone)]
pub struct RunConfig {
    /// Shown in errors (`code-review.star`).
    pub filename: String,
    /// The JSON the script gets as `args` and `main(args)`.
    pub args: Json,
    pub max_concurrency: usize,
    pub max_agents: u32,
    pub max_items: usize,
    pub max_statements: u64,
    pub max_heap_bytes: usize,
    pub max_pass_time: Duration,
    pub budget: Option<Arc<Budget>>,
    pub resume: Option<Resume>,
    pub resolver: Option<Arc<dyn ScriptResolver>>,
}

impl RunConfig {
    pub fn new(filename: impl Into<String>, args: Json) -> Self {
        RunConfig {
            filename: filename.into(),
            args,
            max_concurrency: default_concurrency(MAX_CONCURRENCY),
            max_agents: MAX_AGENTS,
            max_items: MAX_ITEMS,
            max_statements: MAX_STATEMENTS,
            max_heap_bytes: MAX_HEAP_BYTES,
            max_pass_time: MAX_PASS_TIME,
            budget: None,
            resume: None,
            resolver: None,
        }
    }
}

/// How a finished run went.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// `main`'s return value.
    pub value: Json,
    /// Agents issued (live and cached).
    pub agents: u32,
    /// Of those, served from the resume journal.
    pub cached: u32,
    /// Output tokens the live agents spent.
    pub tokens: u64,
    /// Some agent read untrusted content.
    pub tainted: bool,
}

/// Why a run did not finish.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WorkflowError {
    /// Found before anything ran.
    #[error("the script is not valid:\n{}", validate::render(.0))]
    Invalid(Vec<Diagnostic>),
    /// The script raised an error (or `fail()`ed) while running.
    #[error("the script failed: {0}")]
    Script(Diagnostic),
    /// Stopped by the user; the journal has every finished agent, so it can resume.
    #[error("the team task was stopped")]
    Stopped,
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------- state

/// A finished call, kept across passes.
struct Done {
    result: AgentResult,
}

/// Run state that survives from one pass of the script to the next.
#[derive(Default)]
struct State {
    results: HashMap<String, Done>,
    /// Issued and not finished: id -> index.
    inflight: HashMap<String, u32>,
    queue: VecDeque<AgentCall>,
    next_index: u32,
    /// Readings a pass must see the same way every time (`budget.spent()`), by mark id.
    memo: HashMap<String, Json>,
    /// Marks already emitted (`log`, `phase`), by mark id.
    emitted: HashSet<String>,
    events: Vec<WfEvent>,
    resume: Option<Resume>,
    /// The prefix rule tripped: every call from here on runs live.
    live: bool,
    completion: u32,
    tokens: u64,
    cached: u32,
    tainted: bool,
    stopped: bool,
    /// Validated child scripts, by name.
    children: HashMap<String, (String, String)>,
}

/// One frame of call positions: `prefix` + a counter of agent-bearing ops, and one of marks.
struct Frame {
    prefix: String,
    ops: u32,
    marks: u32,
}

/// State of one pass.
struct Pass {
    frames: Vec<Frame>,
    pending: bool,
    fatal: Option<String>,
    phase: Option<String>,
    in_main: bool,
    depth: u32,
    statements: u64,
    started: Instant,
    /// [`check`]: the module's names are resolved, then the first statement stops it.
    compile_only: bool,
}

impl Pass {
    fn new() -> Pass {
        Pass {
            frames: vec![Frame { prefix: String::new(), ops: 0, marks: 0 }],
            pending: false,
            fatal: None,
            phase: None,
            in_main: false,
            depth: 0,
            statements: 0,
            started: Instant::now(),
            compile_only: false,
        }
    }

    fn next_op(&mut self) -> String {
        let f = self.frames.last_mut().expect("a frame");
        let id = format!("{}{}", f.prefix, f.ops);
        f.ops += 1;
        id
    }

    fn next_mark(&mut self, kind: char) -> String {
        let f = self.frames.last_mut().expect("a frame");
        let id = format!("{}{kind}{}", f.prefix, f.marks);
        f.marks += 1;
        id
    }

    fn push(&mut self, prefix: String) {
        self.frames.push(Frame { prefix, ops: 0, marks: 0 });
    }

    fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }
}

/// What the host API reaches through `eval.extra`. (The derive emits an `unsafe impl`, the only
/// unsafe code in the crate, hence the module.)
#[allow(unsafe_code)]
mod context {
    use super::*;

    #[derive(ProvidesStaticType)]
    pub(super) struct Ctx {
        pub(super) st: RefCell<State>,
        pub(super) pass: RefCell<Pass>,
        pub(super) cfg: RunConfig,
    }
}
use context::Ctx;

/// Why a host function did not return a value.
enum Halt {
    /// An agent it needs is not finished: unwind and wait.
    Waiting,
    /// The run must end (stopped, a limit): `parallel` does not swallow it.
    Fatal(String),
    /// A script error (catchable by `parallel`/`pipeline` as `None`).
    Throw(String),
}

impl Ctx {
    fn halt(&self, h: Halt) -> anyhow::Error {
        match h {
            Halt::Waiting => {
                self.pass.borrow_mut().pending = true;
                anyhow::anyhow!(WAITING)
            }
            Halt::Fatal(m) => {
                self.pass.borrow_mut().fatal.get_or_insert(m.clone());
                anyhow::anyhow!(m)
            }
            Halt::Throw(m) => anyhow::anyhow!(m),
        }
    }

    /// One statement, comprehension iteration or lambda call: counts it, then [`Ctx::limits`].
    /// `Err` is fatal (the pass and the run end with it).
    fn tick(&self, heap: &Heap) -> Result<(), String> {
        {
            let mut p = self.pass.borrow_mut();
            p.statements += 1;
            if p.statements > self.cfg.max_statements && p.fatal.is_none() {
                p.fatal = Some(format!("the script ran more than {} statements in one pass; bound its loops", self.cfg.max_statements));
            }
        }
        self.limits(heap)
    }

    /// The pass's heap ceiling and time limit (and [`check`]'s stop before the first statement).
    fn limits(&self, heap: &Heap) -> Result<(), String> {
        let mut p = self.pass.borrow_mut();
        if p.fatal.is_none() {
            if p.compile_only {
                p.fatal = Some(COMPILED.into());
            } else if heap.allocated_bytes() > self.cfg.max_heap_bytes {
                p.fatal = Some(format!("the script holds more than {} MB of values; keep only what it needs", self.cfg.max_heap_bytes >> 20));
            } else if p.statements % 256 == 0 && p.started.elapsed() > self.cfg.max_pass_time {
                p.fatal = Some(format!("one pass of the script ran longer than {} s; bound its loops", self.cfg.max_pass_time.as_secs()));
            }
        }
        match &p.fatal {
            Some(f) => Err(f.clone()),
            None => Ok(()),
        }
    }

    fn require_main(&self, what: &str) -> anyhow::Result<()> {
        if self.pass.borrow().in_main {
            Ok(())
        } else {
            Err(anyhow::anyhow!("call {what}() from inside `def main(args):`, not at the top level of the script"))
        }
    }

    fn emit_mark(&self, kind: char, ev: WfEvent) {
        let id = self.pass.borrow_mut().next_mark(kind);
        let mut st = self.st.borrow_mut();
        if st.emitted.insert(id) {
            st.events.push(ev);
        }
    }

    /// The result of the call at the next position, or why there is none yet.
    fn agent_call(&self, prompt: String, opts: AgentOpts) -> Result<AgentResult, Halt> {
        let id = self.pass.borrow_mut().next_op();
        let mut st = self.st.borrow_mut();
        if let Some(done) = st.results.get(&id) {
            return Ok(done.result.clone());
        }
        if st.inflight.contains_key(&id) {
            return Err(Halt::Waiting);
        }
        let key = call_key(&id, &prompt, &opts);
        let call_index = st.next_index;
        // Served from the journal of an earlier run?
        let mut cached = None;
        if let Some(r) = &st.resume {
            if !r.entries.is_empty() {
                let hit = r.entries.get(&id).filter(|(k, _)| *k == key).map(|(_, res)| res.clone());
                match hit {
                    Some(res) if !(r.prefix_rule && st.live) => cached = Some(res),
                    _ if r.prefix_rule => st.live = true,
                    _ => {}
                }
            }
        }
        if let Some(result) = cached {
            st.next_index += 1;
            st.cached += 1;
            st.completion += 1;
            let completion = st.completion;
            st.tainted |= result.tainted;
            let call = AgentCall { index: call_index, id: id.clone(), key, prompt, opts };
            st.events.push(WfEvent::Queued { call: call.clone() });
            st.events.push(WfEvent::Finished { call, result: result.clone(), cached: true, completion });
            st.results.insert(id, Done { result: result.clone() });
            return Ok(result);
        }
        if st.stopped {
            return Err(Halt::Fatal("the team task was stopped".into()));
        }
        if st.next_index >= self.cfg.max_agents {
            return Err(Halt::Throw(format!("a team task runs at most {} agents", self.cfg.max_agents)));
        }
        if let Some(b) = &self.cfg.budget {
            if let Some(total) = b.total() {
                if b.spent() >= total {
                    return Err(Halt::Throw(format!(
                        "the token budget is spent ({} of {total} output tokens); check budget.remaining() before calling agent()",
                        b.spent()
                    )));
                }
            }
        }
        st.next_index += 1;
        let call = AgentCall { index: call_index, id: id.clone(), key, prompt, opts };
        st.inflight.insert(id, call_index);
        st.events.push(WfEvent::Queued { call: call.clone() });
        st.queue.push_back(call);
        Err(Halt::Waiting)
    }
}

/// `wf:<id>:<blake3 of the canonical call>[..16]`. The label and phase are display only and do
/// not change the key, so relabelling a call keeps its cached result.
pub fn call_key(id: &str, prompt: &str, opts: &AgentOpts) -> String {
    let canonical = serde_json::json!({
        "prompt": prompt,
        "schema": opts.schema,
        "model": opts.model,
        "effort": opts.effort,
        "isolation": opts.isolation,
        "agent_type": opts.agent_type,
    });
    let digest = blake3::hash(canonical.to_string().as_bytes()).to_hex();
    format!("wf:{id}:{}", &digest[..16])
}

fn ctx<'a>(eval: &Evaluator<'_, 'a, '_>) -> &'a Ctx {
    eval.extra.and_then(|e| e.downcast_ref::<Ctx>()).expect("the team task context")
}

// ---------------------------------------------------------------- values

/// A JSON value on a Starlark heap.
pub(crate) fn to_value<'v>(heap: &'v Heap, v: &Json) -> Value<'v> {
    match v {
        Json::Null => Value::new_none(),
        Json::Bool(b) => Value::new_bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => heap.alloc(i),
            None => heap.alloc(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => heap.alloc(s.as_str()),
        Json::Array(a) => heap.alloc(AllocList(a.iter().map(|x| to_value(heap, x)).collect::<Vec<_>>())),
        Json::Object(o) => heap.alloc(AllocDict(o.iter().map(|(k, x)| (k.as_str(), to_value(heap, x))).collect::<Vec<_>>())),
    }
}

/// Positional parameters a callable takes (`usize::MAX` with `*args`).
fn arity(v: Value) -> Option<usize> {
    match v.documentation() {
        DocItem::Member(DocMember::Function(f)) => {
            if f.params.args.is_some() {
                Some(usize::MAX)
            } else {
                Some(f.params.pos_only.len() + f.params.pos_or_named.len())
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- the host API

/// Counts statements and stops a pass that runs away or fills its heap.
struct StmtGuard;

impl<'a, 'e: 'a> BeforeStmtFuncDyn<'a, 'e> for StmtGuard {
    fn call<'v>(&mut self, _span: FileSpanRef, eval: &mut Evaluator<'v, 'a, 'e>) -> starlark::Result<()> {
        ctx(eval).tick(eval.heap()).map_err(|m| starlark::Error::new_other(anyhow::anyhow!(m)))
    }
}

/// Bytes a sequence `*` would repeat: string bytes, 8 per list or tuple element.
fn seq_units(v: Value) -> Option<u128> {
    if let Some(s) = v.unpack_str() {
        return Some(s.len() as u128);
    }
    if let Some(l) = ListRef::from_value(v) {
        return Some(l.len() as u128 * 8);
    }
    TupleRef::from_value(v).map(|t| t.len() as u128 * 8)
}

/// `Some(Ok(n))` for an int that fits 32 bits, `Some(Err(digits))` for a bigger one, `None` for
/// anything else.
fn int_of(v: Value) -> Option<Result<i64, usize>> {
    if v.get_type() != "int" {
        return None;
    }
    Some(match v.unpack_i32() {
        Some(n) => Ok(n as i64),
        None => Err(v.to_repr().len()),
    })
}

/// Whether `a * b` stays within the limits, before anything is built.
fn mul_fits<'v>(a: Value<'v>, b: Value<'v>) -> Result<(), String> {
    for (x, y) in [(a, b), (b, a)] {
        if let (Some(units), Some(n)) = (seq_units(x), int_of(y)) {
            let n = match n {
                Ok(n) => n.max(0) as u128,
                Err(_) => u128::MAX,
            };
            if units.saturating_mul(n) > MAX_VALUE_BYTES {
                return Err(too_big("*", x, units.saturating_mul(n)));
            }
        }
    }
    if let (Some(x), Some(y)) = (int_of(a), int_of(b)) {
        let digits = |r: Result<i64, usize>| r.map(|n| n.unsigned_abs().to_string().len()).unwrap_or_else(|d| d);
        if digits(x) + digits(y) > MAX_INT_DIGITS {
            return Err(format!("`*` would build an integer of more than {MAX_INT_DIGITS} digits"));
        }
    }
    Ok(())
}

/// The error for an operation that would build a value over [`MAX_VALUE_BYTES`].
fn too_big(op: &str, v: Value, bytes: u128) -> String {
    format!("`{op}` would build a {} of about {} MB; values in a team task stay under {} MB", v.get_type(), bytes >> 20, MAX_VALUE_BYTES >> 20)
}

/// Whether `a + b` stays within the limits: two strings, lists or tuples joined.
fn add_fits<'v>(a: Value<'v>, b: Value<'v>) -> Result<(), String> {
    if a.get_type() != b.get_type() {
        return Ok(());
    }
    if let (Some(x), Some(y)) = (seq_units(a), seq_units(b)) {
        if x + y > MAX_VALUE_BYTES {
            return Err(too_big("+", a, x + y));
        }
    }
    Ok(())
}

/// Whether `a << b` stays within the limits.
fn shl_fits(a: Value, b: Value) -> Result<(), String> {
    match int_of(b) {
        Some(Ok(n)) if n <= MAX_SHIFT => {}
        Some(_) => return Err(format!("`<<` shifts by at most {MAX_SHIFT} bits in a team task")),
        None => return Ok(()),
    }
    match int_of(a) {
        Some(Err(d)) if d > MAX_INT_DIGITS => Err(format!("`<<` on an integer of more than {MAX_INT_DIGITS} digits")),
        _ => Ok(()),
    }
}

/// The engine's guards and the capped `range` (bound over the standard library's).
#[starlark::starlark_module]
fn guard_api(builder: &mut GlobalsBuilder) {
    /// One comprehension iteration (inserted by the engine): counted like a statement.
    fn _wf_tick<'v>(#[starlark(require = pos)] x: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.tick(eval.heap()).map_err(|m| c.halt(Halt::Fatal(m)))?;
        Ok(x)
    }

    /// `a + b`, size-checked first.
    fn _wf_add<'v>(
        #[starlark(require = pos)] a: Value<'v>,
        #[starlark(require = pos)] b: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.limits(eval.heap()).and_then(|_| add_fits(a, b)).map_err(|m| c.halt(Halt::Fatal(m)))?;
        a.add(b, eval.heap()).map_err(|e| e.into_anyhow())
    }

    /// `a * b`, size-checked first.
    fn _wf_mul<'v>(
        #[starlark(require = pos)] a: Value<'v>,
        #[starlark(require = pos)] b: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.limits(eval.heap()).and_then(|_| mul_fits(a, b)).map_err(|m| c.halt(Halt::Fatal(m)))?;
        a.mul(b, eval.heap()).map_err(|e| e.into_anyhow())
    }

    /// `a << b`, size-checked first.
    fn _wf_shl<'v>(
        #[starlark(require = pos)] a: Value<'v>,
        #[starlark(require = pos)] b: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.limits(eval.heap()).and_then(|_| shl_fits(a, b)).map_err(|m| c.halt(Halt::Fatal(m)))?;
        a.left_shift(b, eval.heap()).map_err(|e| e.into_anyhow())
    }

    /// `range(stop)`, `range(start, stop)`, `range(start, stop, step)` as a list, at most
    /// 100000 numbers.
    fn range<'v>(
        #[starlark(require = pos)] a: i32,
        #[starlark(require = pos)] b: Option<i32>,
        #[starlark(require = pos)] step: Option<i32>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let (start, stop) = match b {
            Some(b) => (a as i64, b as i64),
            None => (0, a as i64),
        };
        let step = step.unwrap_or(1) as i64;
        if step == 0 {
            return Err(anyhow::anyhow!("range() step must not be zero"));
        }
        let n = if step > 0 {
            if stop > start { (stop - start + step - 1) / step } else { 0 }
        } else if start > stop {
            (start - stop - step - 1) / -step
        } else {
            0
        };
        if n > MAX_RANGE {
            let c = ctx(eval);
            return Err(c.halt(Halt::Fatal(format!("range() gives at most {MAX_RANGE} numbers in a team task; this one would give {n}"))));
        }
        let heap = eval.heap();
        Ok(heap.alloc(AllocList((0..n).map(|i| heap.alloc(start + i * step)).collect::<Vec<_>>())))
    }
}

/// Parses a script for running: the comprehension and lambda ticks inserted, and `*`, `+` and `<<`
/// through the size-checked guards.
fn parse_guarded(filename: &str, source: &str) -> starlark::Result<AstModule> {
    let mut ast = AstModule::parse(filename, validate::guarded_source(filename, source), &validate::dialect())?;
    let ops: HashMap<String, String> = [("*", "_wf_mul"), ("+", "_wf_add"), ("<<", "_wf_shl")].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    ast.replace_binary_operators(&ops);
    Ok(ast)
}

/// A panic's message.
fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

fn opt_str(v: Option<Value>, what: &str) -> anyhow::Result<Option<String>> {
    match v {
        None => Ok(None),
        Some(v) if v.is_none() => Ok(None),
        Some(v) => v.unpack_str().map(|s| Some(s.to_string())).ok_or_else(|| anyhow::anyhow!("agent({what}=...) takes a string")),
    }
}

/// A `log` line: the parts joined by spaces, cut at [`MAX_LOG_CHARS`].
fn log_line(parts: &[Value]) -> String {
    let mut line = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        match p.unpack_str() {
            Some(s) => line.push_str(s),
            None => line.push_str(&p.to_str()),
        }
        if line.len() > MAX_LOG_CHARS {
            break;
        }
    }
    if line.len() > MAX_LOG_CHARS {
        let mut end = MAX_LOG_CHARS;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str(" [...]");
    }
    line
}

#[starlark::starlark_module]
fn host_api(builder: &mut GlobalsBuilder) {
    /// Runs one agent on `prompt`; returns its final text, the object it answered with when
    /// `schema` is given, or `None` when it failed or was skipped.
    fn agent<'v>(
        #[starlark(require = pos)] prompt: Value<'v>,
        #[starlark(require = named)] label: Option<Value<'v>>,
        #[starlark(require = named)] phase: Option<Value<'v>>,
        #[starlark(require = named)] schema: Option<Value<'v>>,
        #[starlark(require = named)] model: Option<Value<'v>>,
        #[starlark(require = named)] effort: Option<Value<'v>>,
        #[starlark(require = named)] isolation: Option<Value<'v>>,
        #[starlark(require = named)] agent_type: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.require_main("agent")?;
        let prompt = prompt
            .unpack_str()
            .ok_or_else(|| anyhow::anyhow!("agent(prompt): the prompt is a string (build it with f\"...\" or +)"))?
            .to_string();
        if prompt.trim().is_empty() {
            return Err(anyhow::anyhow!("agent(prompt): the prompt is empty"));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(anyhow::anyhow!(
                "agent(prompt): the prompt is {} KB; keep it under {} KB (pass a summary or a file path, not whole results)",
                prompt.len() >> 10,
                MAX_PROMPT_BYTES >> 10
            ));
        }
        let schema = match schema {
            Some(s) if !s.is_none() => {
                let j = s.to_json_value().map_err(|e| anyhow::anyhow!("agent(schema=...) must be JSON-like: {e}"))?;
                schema::check_shape(&j).map_err(|e| anyhow::anyhow!("agent(schema=...): {e}"))?;
                Some(j)
            }
            _ => None,
        };
        let isolation = opt_str(isolation, "isolation")?;
        if let Some(i) = &isolation {
            if i != "worktree" {
                return Err(anyhow::anyhow!("agent(isolation=...) is \"worktree\" or omitted"));
            }
        }
        let effort = opt_str(effort, "effort")?;
        if let Some(e) = &effort {
            if !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
                return Err(anyhow::anyhow!("agent(effort=...) is one of low, medium, high, xhigh, max"));
            }
        }
        let phase = opt_str(phase, "phase")?.or_else(|| c.pass.borrow().phase.clone());
        let opts = AgentOpts {
            label: opt_str(label, "label")?,
            phase,
            schema,
            model: opt_str(model, "model")?,
            effort,
            isolation,
            agent_type: opt_str(agent_type, "agent_type")?,
        };
        match c.agent_call(prompt, opts) {
            Ok(r) => Ok(to_value(eval.heap(), &r.value)),
            Err(h) => Err(c.halt(h)),
        }
    }

    /// Calls every function in `thunks` (no arguments) so their agents run at the same time;
    /// returns their results in order once all are done. A thunk that fails gives `None`. An item
    /// that is a prompt string stands for `lambda: agent(prompt)`.
    fn parallel<'v>(#[starlark(require = pos)] thunks: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.require_main("parallel")?;
        let items: Vec<Value<'v>> = thunks
            .iterate(eval.heap())
            .map_err(|_| anyhow::anyhow!("parallel() takes a list of functions: parallel([lambda: agent(...), ...])"))?
            .take(c.cfg.max_items + 1)
            .collect();
        if items.len() > c.cfg.max_items {
            return Err(anyhow::anyhow!("parallel() takes at most {} items", c.cfg.max_items));
        }
        for t in &items {
            if let Some(p) = t.unpack_str() {
                if p.trim().is_empty() || p.len() > MAX_PROMPT_BYTES {
                    return Err(anyhow::anyhow!("parallel([\"prompt\", ...]): each prompt is a non-empty string under {} KB", MAX_PROMPT_BYTES >> 10));
                }
            } else if arity(*t).is_none() {
                return Err(anyhow::anyhow!(
                    "parallel() takes prompt strings or functions, not values: parallel([\"prompt a\", \"prompt b\"]) or parallel([lambda: agent(...), ...]); got a {}",
                    t.get_type()
                ));
            }
        }
        let id = c.pass.borrow_mut().next_op();
        let mut out = Vec::with_capacity(items.len());
        let mut waiting = false;
        for (i, t) in items.into_iter().enumerate() {
            c.pass.borrow_mut().push(format!("{id}.{i}/"));
            // A prompt string is `agent(prompt)` without the lambda around it.
            let r = match t.unpack_str() {
                Some(prompt) => {
                    let opts = AgentOpts { phase: c.pass.borrow().phase.clone(), ..AgentOpts::default() };
                    match c.agent_call(prompt.to_string(), opts) {
                        Ok(r) => Ok(to_value(eval.heap(), &r.value)),
                        Err(h) => Err(c.halt(h).into()),
                    }
                }
                None => eval.eval_function(t, &[], &[]),
            };
            let pending = std::mem::take(&mut c.pass.borrow_mut().pending);
            let fatal = c.pass.borrow().fatal.clone();
            // The failure line is marked in the item's own frame, so its id names the item
            // whatever else failed on this pass.
            if let (Err(e), false, None) = (&r, pending, &fatal) {
                c.emit_mark('E', WfEvent::Log(format!("parallel item {i} failed: {}", validate::from_starlark(e))));
            }
            c.pass.borrow_mut().pop();
            if let Some(f) = fatal {
                return Err(anyhow::anyhow!(f));
            }
            match r {
                _ if pending => {
                    waiting = true;
                    out.push(Value::new_none());
                }
                Ok(v) => out.push(v),
                Err(_) => out.push(Value::new_none()),
            }
        }
        if waiting {
            return Err(c.halt(Halt::Waiting));
        }
        Ok(eval.heap().alloc(AllocList(out)))
    }

    /// Runs each item through `stages` in order, every item independently (no barrier between
    /// stages). A stage is called as `stage(prev, item, index)` (it may take fewer parameters);
    /// `prev` is the item for the first stage. A stage that fails gives `None` for that item and
    /// skips its remaining stages. Returns the last stage's results in item order.
    fn pipeline<'v>(
        #[starlark(require = pos)] items: Value<'v>,
        #[starlark(args)] stages: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let c = ctx(eval);
        c.require_main("pipeline")?;
        let stages = stages.items;
        let items: Vec<Value<'v>> = items
            .iterate(eval.heap())
            .map_err(|_| anyhow::anyhow!("pipeline(items, stage, ...): items is a list"))?
            .take(c.cfg.max_items + 1)
            .collect();
        if items.len() > c.cfg.max_items {
            return Err(anyhow::anyhow!("pipeline() takes at most {} items", c.cfg.max_items));
        }
        if stages.is_empty() {
            return Err(anyhow::anyhow!("pipeline(items, stage, ...) needs at least one stage function"));
        }
        let mut arities = Vec::new();
        for s in &stages {
            arities.push(arity(*s).ok_or_else(|| anyhow::anyhow!("pipeline() stages are functions: lambda prev, item, index: ..."))?);
        }
        let id = c.pass.borrow_mut().next_op();
        let mut out = Vec::with_capacity(items.len());
        let mut waiting = false;
        for (i, item) in items.into_iter().enumerate() {
            let mut prev = item;
            let mut item_out = Value::new_none();
            let mut finished = true;
            for (s, stage) in stages.iter().enumerate() {
                c.pass.borrow_mut().push(format!("{id}.{i}.{s}/"));
                let all = [prev, item, eval.heap().alloc(i as i64)];
                let n = arities[s].min(3);
                let r = eval.eval_function(*stage, &all[..n], &[]);
                let pending = std::mem::take(&mut c.pass.borrow_mut().pending);
                let fatal = c.pass.borrow().fatal.clone();
                if let (Err(e), false, None) = (&r, pending, &fatal) {
                    c.emit_mark('E', WfEvent::Log(format!("pipeline item {i} stage {s} failed: {}", validate::from_starlark(e))));
                }
                c.pass.borrow_mut().pop();
                if let Some(f) = fatal {
                    return Err(anyhow::anyhow!(f));
                }
                if pending {
                    waiting = true;
                    finished = false;
                    break;
                }
                match r {
                    Ok(v) => prev = v,
                    Err(_) => {
                        finished = false;
                        break;
                    }
                }
            }
            if finished {
                item_out = prev;
            }
            out.push(item_out);
        }
        if waiting {
            return Err(c.halt(Halt::Waiting));
        }
        Ok(eval.heap().alloc(AllocList(out)))
    }

    /// Enters a phase: later agents are grouped under it in the progress view.
    fn phase<'v>(#[starlark(require = pos)] name: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<starlark::values::none::NoneType> {
        let c = ctx(eval);
        c.pass.borrow_mut().phase = Some(name.to_string());
        c.emit_mark('P', WfEvent::Phase(name.to_string()));
        Ok(starlark::values::none::NoneType)
    }

    /// A progress line for the user (shown in the workflow view, kept in the journal).
    fn log<'v>(#[starlark(args)] parts: UnpackTuple<Value<'v>>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<starlark::values::none::NoneType> {
        ctx(eval).emit_mark('L', WfEvent::Log(log_line(&parts.items)));
        Ok(starlark::values::none::NoneType)
    }

    /// The same as `log`.
    fn print<'v>(#[starlark(args)] parts: UnpackTuple<Value<'v>>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<starlark::values::none::NoneType> {
        ctx(eval).emit_mark('L', WfEvent::Log(log_line(&parts.items)));
        Ok(starlark::values::none::NoneType)
    }

    /// Runs a saved team task (by name or path) with `args` and returns its result. One level
    /// deep: a team task started this way cannot start another. It shares this run's limits and
    /// budget.
    fn team<'v>(
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos)] args: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        nested(name, args, eval, "team")
    }

    /// The old name of `team`: saved scripts that call it keep running.
    fn workflow<'v>(
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos)] args: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        nested(name, args, eval, "workflow")
    }
}

/// `team(name, args)` / `workflow(name, args)`: `who` is the spelling the script used.
fn nested<'v>(name: &str, args: Option<Value<'v>>, eval: &mut Evaluator<'v, '_, '_>, who: &str) -> anyhow::Result<Value<'v>> {
    let c = ctx(eval);
    c.require_main(who)?;
    if c.pass.borrow().depth >= 1 {
        return Err(anyhow::anyhow!("{who}() is one level deep: a team task started by {who}() cannot start another"));
    }
    let args_json = args.unwrap_or_else(Value::new_none).to_json_value().map_err(|e| anyhow::anyhow!("{who}(name, args): args must be JSON-like: {e}"))?;
    let (filename, source) = c.child(name)?;
    let id = c.pass.borrow_mut().next_op();
    {
        let mut p = c.pass.borrow_mut();
        p.push(format!("{id}/"));
        p.depth += 1;
    }
    let r = run_child(c, &filename, &source, &args_json);
    {
        let mut p = c.pass.borrow_mut();
        p.depth -= 1;
        p.pop();
    }
    match r {
        Ok(v) => Ok(to_value(eval.heap(), &v)),
        Err(ChildErr::Waiting) => Err(c.halt(Halt::Waiting)),
        Err(ChildErr::Fatal(m)) => Err(c.halt(Halt::Fatal(m))),
        Err(ChildErr::Throw(m)) => Err(anyhow::anyhow!("{who}(\"{name}\") failed: {m}")),
    }
}

#[starlark::starlark_module]
fn budget_api(builder: &mut GlobalsBuilder) {
    /// Output tokens spent so far in this session's pool (main loop and workflows).
    fn spent<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let v = ctx(eval).reading('B', |b| b.map(|b| b.spent()).unwrap_or(0).into());
        Ok(to_value(eval.heap(), &v))
    }

    /// Tokens left before `agent()` refuses, or `None` when no budget was set.
    fn remaining<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let v = ctx(eval).reading('R', |b| b.and_then(|b| b.remaining()).map(Json::from).unwrap_or(Json::Null));
        Ok(to_value(eval.heap(), &v))
    }
}

impl Ctx {
    /// A host reading, the same on every pass of the run.
    fn reading(&self, kind: char, f: impl FnOnce(Option<&Budget>) -> Json) -> Json {
        let id = self.pass.borrow_mut().next_mark(kind);
        let mut st = self.st.borrow_mut();
        if let Some(v) = st.memo.get(&id) {
            return v.clone();
        }
        let v = f(self.cfg.budget.as_deref());
        st.memo.insert(id, v.clone());
        v
    }

    fn child(&self, name: &str) -> anyhow::Result<(String, String)> {
        if let Some(c) = self.st.borrow().children.get(name) {
            return Ok(c.clone());
        }
        let resolver = self.cfg.resolver.clone().ok_or_else(|| anyhow::anyhow!("team(\"{name}\"): no saved team tasks are available here"))?;
        let (filename, source) = resolver.resolve(name).map_err(|e| anyhow::anyhow!("team(\"{name}\"): {e}"))?;
        validate::static_checks(&filename, &source)
            .map_err(|d| anyhow::anyhow!("team(\"{name}\") is not valid:\n{}", validate::render(&d)))?;
        self.st.borrow_mut().children.insert(name.to_string(), (filename.clone(), source.clone()));
        Ok((filename, source))
    }
}

enum ChildErr {
    Waiting,
    Fatal(String),
    Throw(String),
}

fn run_child(c: &Ctx, filename: &str, source: &str, args: &Json) -> Result<Json, ChildErr> {
    let globals = globals(&c.cfg);
    let module = Module::new();
    let ast = parse_guarded(filename, source).map_err(|e| ChildErr::Throw(e.to_string()))?;
    let args_v = to_value(module.heap(), &script_args(args));
    module.set("args", args_v);
    let mut eval = Evaluator::new(&module);
    eval.extra = Some(c);
    let _ = eval.set_max_callstack_size(MAX_STACK);
    let hook: Box<dyn BeforeStmtFuncDyn> = Box::new(StmtGuard);
    eval.before_stmt_for_dap(hook.into());
    let in_main = std::mem::replace(&mut c.pass.borrow_mut().in_main, false);
    let r = eval.eval_module(ast, &globals).and_then(|_| {
        c.pass.borrow_mut().in_main = true;
        let main = module.get("main").ok_or_else(|| starlark::Error::new_other(anyhow::anyhow!("the team task has no main()")))?;
        let n = arity(main).unwrap_or(0).min(1);
        eval.eval_function(main, &[args_v][..n], &[])
    });
    c.pass.borrow_mut().in_main = in_main;
    let pass = c.pass.borrow();
    if pass.pending {
        return Err(ChildErr::Waiting);
    }
    if let Some(f) = &pass.fatal {
        return Err(ChildErr::Fatal(f.clone()));
    }
    match r {
        Ok(v) => v.to_json_value().map_err(|e| ChildErr::Throw(format!("its result is not JSON-like: {e}"))),
        Err(e) => Err(ChildErr::Throw(validate::from_starlark(&e).to_string())),
    }
}

/// The globals every workflow script sees: the standard library, `json`, `struct`, `map`,
/// `filter`, and the host API. No `print` to stdout, no `load`, no clock or randomness.
fn globals(cfg: &RunConfig) -> Globals {
    let total = cfg.budget.as_ref().and_then(|b| b.total());
    GlobalsBuilder::extended_by(&[LibraryExtension::Json, LibraryExtension::StructType, LibraryExtension::Map, LibraryExtension::Filter])
        .with(host_api)
        .with(guard_api)
        .with_namespace("budget", move |b| {
            budget_api(b);
            match total {
                Some(t) => b.set("total", t as i64),
                None => b.set("total", starlark::values::none::NoneType),
            }
        })
        .build()
}

// ---------------------------------------------------------------- passes

enum PassEnd {
    Done(Json),
    Waiting,
    Fatal(String),
    Failed(Diagnostic),
}

/// One pass; a panic inside the interpreter (a size it asserts on) ends it as a fatal error.
fn one_pass(ctx: &Ctx, source: &str, globals: &Globals, compile_only: bool) -> PassEnd {
    match catch_unwind(AssertUnwindSafe(|| pass_inner(ctx, source, globals, compile_only))) {
        Ok(end) => end,
        Err(p) => PassEnd::Fatal(format!("the script stopped the interpreter: {}", panic_text(&p))),
    }
}

fn pass_inner(ctx: &Ctx, source: &str, globals: &Globals, compile_only: bool) -> PassEnd {
    *ctx.pass.borrow_mut() = Pass { compile_only, ..Pass::new() };
    let ast = match parse_guarded(&ctx.cfg.filename, source) {
        Ok(a) => a,
        Err(e) => return PassEnd::Failed(validate::from_starlark(&e)),
    };
    let module = Module::new();
    let args_v = to_value(module.heap(), &script_args(&ctx.cfg.args));
    module.set("args", args_v);
    let mut eval = Evaluator::new(&module);
    eval.extra = Some(ctx);
    let _ = eval.set_max_callstack_size(MAX_STACK);
    let hook: Box<dyn BeforeStmtFuncDyn> = Box::new(StmtGuard);
    eval.before_stmt_for_dap(hook.into());
    if let Err(e) = eval.eval_module(ast, globals) {
        return end_of(ctx, Err(e));
    }
    if compile_only {
        return PassEnd::Done(Json::Null);
    }
    let Some(main) = module.get("main") else {
        return PassEnd::Failed(Diagnostic::new(None, "the script has no `def main(args):`"));
    };
    ctx.pass.borrow_mut().in_main = true;
    let n = arity(main).unwrap_or(0).min(1);
    let r = eval.eval_function(main, &[args_v][..n], &[]);
    end_of(ctx, r)
}

/// What a script sees as `args`: the JSON the caller passed, or an empty object when it passed none
/// (a saved team task started by name has no arguments, and `args.get("x", default)` must work).
fn script_args(args: &Json) -> Json {
    if args.is_null() { Json::Object(Default::default()) } else { args.clone() }
}

fn end_of(ctx: &Ctx, r: starlark::Result<Value>) -> PassEnd {
    let pass = ctx.pass.borrow();
    if let Some(f) = &pass.fatal {
        return PassEnd::Fatal(f.clone());
    }
    if pass.pending {
        return PassEnd::Waiting;
    }
    match r {
        Ok(v) => match v.to_json_value() {
            Ok(j) => PassEnd::Done(j),
            Err(e) => PassEnd::Failed(Diagnostic::new(None, format!("main() returned something that is not JSON-like: {e}"))),
        },
        Err(e) => PassEnd::Failed(validate::from_starlark(&e)),
    }
}

/// Validates `source` completely before anything runs: the static checks, then the module is
/// compiled, which resolves every name it uses (a misspelt name is an error here). Nothing in the
/// script runs: this is called before approval, on the conductor's thread, so the first statement
/// stops the pass.
pub fn check(filename: &str, source: &str) -> Result<Validated, Vec<Diagnostic>> {
    check_raw(filename, source).map_err(|d| validate::with_hints(source, d))
}

fn check_raw(filename: &str, source: &str) -> Result<Validated, Vec<Diagnostic>> {
    let v = validate::static_checks(filename, source)?;
    let cfg = RunConfig::new(filename, Json::Object(Default::default()));
    let ctx = Ctx { st: RefCell::new(State::default()), pass: RefCell::new(Pass::new()), cfg };
    match one_pass(&ctx, source, &globals(&ctx.cfg), true) {
        PassEnd::Done(_) => Ok(v),
        PassEnd::Fatal(m) if m == COMPILED => Ok(v),
        PassEnd::Failed(d) => Err(vec![d]),
        PassEnd::Fatal(m) => Err(vec![Diagnostic::new(None, m)]),
        PassEnd::Waiting => Err(vec![Diagnostic::new(None, "the top level of the script must not wait on agents")]),
    }
}

/// Runs a script to its end on this thread. Agents run on the host; `obs` sees every event in
/// order; `ctl` carries skips and stop. The caller has validated the script ([`check`]).
pub fn run(source: &str, cfg: RunConfig, host: Arc<dyn AgentHost>, obs: &mut dyn Observer, ctl: &Receiver<Control>) -> Result<RunOutcome, WorkflowError> {
    if let Err(d) = check(&cfg.filename, source) {
        return Err(WorkflowError::Invalid(d));
    }
    let globals = globals(&cfg);
    let state = State { resume: cfg.resume.clone(), ..State::default() };
    let ctx = Ctx { st: RefCell::new(state), pass: RefCell::new(Pass::new()), cfg };
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Completion>();
    let mut running: HashMap<u32, AgentCall> = HashMap::new();
    loop {
        let end = one_pass(&ctx, source, &globals, false);
        flush(&ctx, obs);
        match end {
            PassEnd::Done(value) => {
                let st = ctx.st.borrow();
                return Ok(RunOutcome { value, agents: st.next_index, cached: st.cached, tokens: st.tokens, tainted: st.tainted });
            }
            PassEnd::Failed(d) => {
                cancel_all(&host, &mut running);
                return Err(WorkflowError::Script(d));
            }
            PassEnd::Fatal(m) => {
                cancel_all(&host, &mut running);
                return Err(if ctx.st.borrow().stopped { WorkflowError::Stopped } else { WorkflowError::Script(Diagnostic::new(None, m)) });
            }
            PassEnd::Waiting => {}
        }
        launch(&ctx, &host, &done_tx, &mut running);
        flush(&ctx, obs);
        if running.is_empty() {
            return Err(WorkflowError::Internal("the script waits on agents but none is queued or running".into()));
        }
        crossbeam_channel::select! {
            recv(done_rx) -> c => if let Ok(c) = c { finish(&ctx, &mut running, c) },
            recv(ctl) -> m => match m {
                Ok(m) => control(&ctx, &host, &mut running, m),
                // No controller: wait on agents alone.
                Err(_) => if let Ok(c) = done_rx.recv() { finish(&ctx, &mut running, c) },
            },
        }
        while let Ok(c) = done_rx.try_recv() {
            finish(&ctx, &mut running, c);
        }
        while let Ok(m) = ctl.try_recv() {
            control(&ctx, &host, &mut running, m);
        }
        flush(&ctx, obs);
        if ctx.st.borrow().stopped {
            cancel_all(&host, &mut running);
            return Err(WorkflowError::Stopped);
        }
    }
}

fn flush(ctx: &Ctx, obs: &mut dyn Observer) {
    let events = std::mem::take(&mut ctx.st.borrow_mut().events);
    for ev in &events {
        obs.event(ev);
    }
}

fn launch(ctx: &Ctx, host: &Arc<dyn AgentHost>, done: &Sender<Completion>, running: &mut HashMap<u32, AgentCall>) {
    let limit = ctx.cfg.max_concurrency.max(1);
    while running.len() < limit {
        let Some(call) = ctx.st.borrow_mut().queue.pop_front() else { break };
        running.insert(call.index, call.clone());
        ctx.st.borrow_mut().events.push(WfEvent::Started { index: call.index });
        host.start(call, done.clone());
    }
}

fn record(ctx: &Ctx, call: AgentCall, mut result: AgentResult) {
    if result.status == AgentStatus::Done {
        if let Some(s) = &call.opts.schema {
            let errors = schema::validate(&result.value, s);
            if !errors.is_empty() {
                result = AgentResult {
                    status: AgentStatus::Failed,
                    value: Json::Null,
                    tokens: result.tokens,
                    tainted: result.tainted,
                    note: Some(format!("the answer did not match the schema: {}", errors.join("; "))),
                };
            }
        }
    }
    if let Some(b) = &ctx.cfg.budget {
        b.add(result.tokens);
    }
    let mut st = ctx.st.borrow_mut();
    st.tokens += result.tokens;
    st.tainted |= result.tainted;
    st.inflight.remove(&call.id);
    st.completion += 1;
    let completion = st.completion;
    if result.status == AgentStatus::Failed {
        let why = result.note.clone().unwrap_or_else(|| "it gave no answer".into());
        st.events.push(WfEvent::Log(format!("agent {} ({}) failed: {why}", call.index, call.display_label())));
    }
    st.events.push(WfEvent::Finished { call: call.clone(), result: result.clone(), cached: false, completion });
    st.results.insert(call.id, Done { result });
}

fn finish(ctx: &Ctx, running: &mut HashMap<u32, AgentCall>, c: Completion) {
    // A skipped call's late completion is ignored.
    if let Some(call) = running.remove(&c.index) {
        record(ctx, call, c.result);
    }
}

fn control(ctx: &Ctx, host: &Arc<dyn AgentHost>, running: &mut HashMap<u32, AgentCall>, m: Control) {
    match m {
        Control::Skip(index) => {
            if let Some(call) = running.remove(&index) {
                host.cancel(index);
                record(ctx, call, AgentResult::skipped());
                return;
            }
            let queued = {
                let mut st = ctx.st.borrow_mut();
                let pos = st.queue.iter().position(|c| c.index == index);
                pos.and_then(|p| st.queue.remove(p))
            };
            if let Some(call) = queued {
                record(ctx, call, AgentResult::skipped());
            }
        }
        Control::Stop => {
            let mut st = ctx.st.borrow_mut();
            st.stopped = true;
            st.queue.clear();
        }
    }
}

fn cancel_all(host: &Arc<dyn AgentHost>, running: &mut HashMap<u32, AgentCall>) {
    for (index, _) in running.drain() {
        host.cancel(index);
    }
}
