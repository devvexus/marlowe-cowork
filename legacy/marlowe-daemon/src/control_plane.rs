//! The control plane — a **second listener**, so `/steer` from outside is real rather than queued.
//!
//! # Why this exists, in the daemon's own words
//!
//! `Daemon::serve` says *"One connection at a time — a second client is a M3 concern and pretending
//! to handle it now would be a concurrency story nobody tested"*, and `Request::Approve`'s arm says
//! *"Concurrency is M3."* This is M3, and this is that concurrency — deliberately the **smallest**
//! amount of it that makes the milestone's scope item true.
//!
//! M3-DESIGN §10.1 requires steering *"from outside"* — another terminal, no TUI, a script. On a
//! serial daemon a steer sent from a second terminal is not **read** until the turn it was meant
//! to change has already ended. Queuing it and calling that "mid-flight" would be a control that
//! looks like it works and cannot: the same shape as a banner that fires on every run.
//!
//! # What is shared, and what is emphatically not
//!
//! The thread started here **never touches `Daemon`**. It holds one `Arc<Mutex<ControlPlane>>`
//! containing exactly two things: the run table, and the [`DurableControl`]. The model, the
//! memory, the tool host, the session store and the journal-for-writing all stay behind the
//! daemon's own lock and are still served one connection at a time. So the concurrency added here
//! is bounded by a struct a reader can hold in their head, which is the only kind worth shipping
//! at this stage.
//!
//! The loop reaches the shared state through [`SharedControl`], which locks **per call** at
//! iteration boundaries — `take_steer` and `cancelled` are a lock, a `pop_front`, and an unlock.
//! Nothing holds the lock across a model call, which is what makes a steer arriving mid-turn
//! visible to the turn that is already running.
//!
//! # A second port, ADVERTISED rather than derived — and the first draft got this wrong
//!
//! One `TcpListener` cannot be accepted from two places without a thread per connection, and a
//! thread per connection means `Daemon: Send`, which it is not (ONNX sessions, `Rc`-free but not
//! audited for it). A second port is the change that does not require auditing the whole daemon
//! for thread-safety.
//!
//! **The first version derived it as `port + 1`, and that is a collision waiting for a second
//! daemon.** The default port is 11435, so a daemon deliberately started on 11436 lands on the
//! first one's control plane; a client then offers the second profile's token to the first
//! profile's listener and is refused, which reads as a mysterious auth failure and is not one.
//!
//! Two workspace tests found it within minutes of it being written — `socket_auth` and `split`
//! both call `free_port()`, and the OS handed one fixture the port another fixture's control
//! plane had just taken. That is the **defaults that make a mismatch unobservable** family with
//! the sign flipped: the derivation made a collision *silent* everywhere except where two daemons
//! happened to be adjacent.
//!
//! So the port is **bound at 0 — the OS picks a free one — and written to the profile root**, next
//! to `daemon.token` and protected by the same directory ACL. Discovery, not arithmetic. A client
//! reads the file; if it is absent or stale the request falls back to the main port, which is
//! slower and always correct.
//!
//! If the listener cannot start at all the daemon **still serves** and says the control plane is
//! unavailable — invariant 4: degrade visibly, name the remedy. A daemon that refused to start
//! because a secondary listener failed would be worse than one whose `/steer` says why it cannot
//! work.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_loop::{
    CheckpointStore, Control, DurableControl, JournalCheckpoints, OrphanPolicy, RunControl, RunId,
    SteerMessage, Urgency,
};

use crate::daemon::RunSummary;
use crate::protocol::{write_line, Event, Request, RunFrame};

/// Everything a control request may reach. Deliberately small — see the module header.
pub struct ControlPlane {
    /// The run table. **The daemon's state, which is the whole point of `/runs`.**
    ///
    /// `--status` reported the client's own default while describing a daemon running something
    /// else, because a fresh process resolved its own configuration instead of asking. `/runs` is
    /// the same question about a different noun, so it reads this and there is no second copy.
    pub runs: BTreeMap<String, RunSummary>,
    pub control: DurableControl<JournalCheckpoints>,
    /// What each run has **said**, for a window to render. Session F; ADR-055.
    ///
    /// # This is a third thing, and the module header says there are two
    ///
    /// That header's claim is that the concurrency here is *"bounded by a struct a reader can hold
    /// in their head"*, and the claim is worth keeping true rather than quietly outgrowing. So this
    /// field is bounded twice over: [`MAX_FRAMES`] entries per run, and **frames coalesce** — a run
    /// of `TextDelta`s appends to one frame instead of adding thousands, so the count is
    /// proportional to turns rather than to tokens.
    ///
    /// It lives here rather than behind the daemon's own lock for the reason everything else here
    /// does: the turn writes it while the control connection reads it, and a copy on each side
    /// would be two answers to one question.
    frames: BTreeMap<String, Frames>,
}

/// One run's output frames, and where the sequence is up to.
#[derive(Debug, Default)]
struct Frames {
    seq: Vec<(u64, RunFrame)>,
    next: u64,
    /// The oldest sequence still held. Moves when the ring drops one, so a window that has fallen
    /// behind can be **told** rather than shown a gap it cannot see.
    first: u64,
}

/// How many frames one run keeps. Frames coalesce, so this is turns-worth, not tokens-worth.
pub const MAX_FRAMES: usize = 2_000;

pub type Shared = Arc<Mutex<ControlPlane>>;

impl ControlPlane {
    pub fn new(control: DurableControl<JournalCheckpoints>) -> Shared {
        Arc::new(Mutex::new(Self { runs: BTreeMap::new(), control, frames: BTreeMap::new() }))
    }

    /// Fill the run table from the journal, so a run that survived a restart can be **found**.
    ///
    /// **Not "running".** These runs stopped when the process did, and calling them running would
    /// be the surface asserting a state nothing holds — `live_runs` feeds `--status`'s count and
    /// `Shutdown`'s refusal, and both would then be wrong forever. `interrupted` is the honest
    /// word: it says the work stopped and did not finish, which is exactly what a resume is for.
    ///
    /// A run whose last checkpoint is terminal is **not** seeded. It completed, failed or was
    /// cancelled; listing it as pending would offer a resume that `RunControl::resume` refuses.
    pub fn seed_from_journal(&mut self) -> usize {
        let seeded: Vec<_> = self
            .control
            .store()
            .latest_per_run()
            .into_iter()
            .filter(|cp| !cp.is_terminal())
            .collect();
        let n = seeded.len();
        for cp in seeded {
            let id = cp.run.to_string();
            self.runs.entry(id.clone()).or_insert_with(|| RunSummary {
                status: "interrupted".into(),
                tokens: cp.spent.tokens,
                depth: cp.budget.depth,
                spend_micros_usd: cp.spent.micros_usd,
                elapsed_ms: cp.spent.wall_ms,
                ..RunSummary::accepted(id)
            });
        }
        n
    }

    /// Append one frame of a run's output, coalescing consecutive prose of the same kind.
    ///
    /// **Coalescing is what bounds the memory.** A frame per token is unbounded growth in a process
    /// that must not die; a run of `TextDelta`s becomes one frame that grows, so the count tracks
    /// turns. The client re-reads the growing tail — see [`ControlPlane::frames_since`].
    pub fn push(&mut self, run: &str, frame: RunFrame) {
        let f = self.frames.entry(run.to_string()).or_insert_with(|| Frames {
            seq: Vec::new(),
            next: 1,
            first: 1,
        });

        let coalesced = match (&frame, f.seq.last_mut()) {
            (RunFrame::Text { delta }, Some((_, RunFrame::Text { delta: tail }))) => {
                tail.push_str(delta);
                true
            }
            // **The token count SUMS while the text concatenates.** Coalescing is what bounds this
            // ring's memory, and a count that was overwritten rather than added would make a
            // window's thinking line report whatever the last chunk happened to cost.
            (
                RunFrame::Reasoning { delta, tokens },
                Some((_, RunFrame::Reasoning { delta: tail, tokens: total })),
            ) => {
                tail.push_str(delta);
                *total = total.saturating_add(*tokens);
                true
            }
            // **A tool line REPLACES its own earlier frame rather than adding one.** §B6: the close
            // must replace the open line, not scroll a second one in beneath it — the same property
            // `quarantine_batch.rs` asserts for the quarantined reader's line.
            (RunFrame::Tool { id, .. }, _) => {
                let id = *id;
                match f.seq.iter_mut().find(|(_, x)| {
                    matches!(x, RunFrame::Tool { id: other, .. } if *other == id)
                }) {
                    Some((_, slot)) => {
                        *slot = frame.clone();
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if coalesced {
            return;
        }

        let seq = f.next;
        f.next += 1;
        f.seq.push((seq, frame));
        while f.seq.len() > MAX_FRAMES {
            f.seq.remove(0);
            f.first += 1;
        }
    }

    /// Frames from `since` onward, and whether anything older was dropped.
    ///
    /// **`seq >= since`, not `>`.** The tail frame is still growing, so a client asks from the
    /// highest sequence it holds and overwrites it. With `>` a live run's last paragraph would
    /// freeze at whatever it held on the poll that first saw it.
    pub fn frames_since(&self, run: &str, since: u64) -> (Vec<Event>, u64) {
        let Some(f) = self.frames.get(run) else { return (Vec::new(), 0) };
        let dropped = if since > 0 && since < f.first { f.first - since } else { 0 };
        let out = f
            .seq
            .iter()
            .filter(|(seq, _)| *seq >= since)
            .map(|(seq, frame)| Event::RunOutput { seq: *seq, frame: frame.clone() })
            .collect();
        (out, dropped)
    }

    pub fn live_runs(&self) -> usize {
        self.runs.values().filter(|r| r.status == "running").count()
    }

    /// What a person typed, resolved to a run: a **mnemonic**, a **UUID prefix**, or a full UUID.
    ///
    /// # Why a name needs a resolver at all
    ///
    /// `RunId::mnemonic` gives `daring-storm` and is derived, never stored — the argument is in its
    /// own header. That makes it free to display and useless to type, because until something
    /// resolves it back nothing accepts it. A name nobody can use is a longer id.
    ///
    /// # Ambiguity is REFUSED and the candidates are listed, git-style
    ///
    /// 4096 names means two live runs can share one, and a prefix can match several. The wrong
    /// answers here are both silent: picking the first match steers the wrong run, and reporting
    /// "not a run id" for a name the user can see on their own screen reads as a broken product.
    /// So an ambiguous token is refused **by name, with what it matched**, and the full id always
    /// works.
    ///
    /// **A full UUID resolves whether or not the table holds it.** The table is what this daemon
    /// remembers; the journal is what survived, and `Watch` on an id from a previous daemon is a
    /// real thing to want. Only the shorthands need a table to resolve against, because a
    /// shorthand is a search.
    pub fn resolve(&self, typed: &str) -> Result<RunId, String> {
        resolve_among(self.runs.keys().map(String::as_str), typed)
    }

    /// Every run, as wire frames. One definition, read by the main port and the control port.
    pub fn run_frames(&self) -> Vec<Event> {
        self.runs.values().map(RunSummary::to_frame).collect()
    }

    /// One run, in the detail §6.2 asks a window to render.
    ///
    /// **Including what a resume would resume from** — M3-DESIGN calls that *"the field that makes
    /// this a debugging instrument"*, and it is read from the checkpoint store rather than from the
    /// run table, because the table is what the daemon remembers and the store is what survived.
    pub fn detail(&self, run: RunId) -> Event {
        let summary = self.runs.get(&run.to_string());
        let cp = self.control.store().latest(run);
        Event::RunDetail {
            id: run.to_string(),
            status: summary.map_or_else(
                || cp.as_ref().map_or("unknown".into(), |c| format!("{:?}", c.status).to_lowercase()),
                |s| s.status.clone(),
            ),
            parent: cp.as_ref().and_then(|c| c.parent).map(|p| p.to_string()),
            // **Final when there is one, live otherwise.** `RunSummary::elapsed_ms` is written
            // once, when the turn ends; until then it is `0` and a window would report a run that
            // had been going for a minute as having taken no time at all. The daemon owns the
            // clock, so the daemon is what resolves the two — not the surface, which reads none.
            //
            // **`elapsed_ms > 0` alone is not enough, and the gap is a run that stopped without
            // spending wall time.** Zero does two jobs here — *not finished yet* and *finished
            // having taken almost none* — and the live branch turns the second into a number that
            // grows every time anybody looks. It could not arise while `ask_streaming_with` owned
            // every row, because a turn containing a model call never takes zero. A spawned child's
            // row is written by `roster.rs` from its last checkpoint, and a child that paused
            // before its first call has spent no wall time at all. So the status decides as well:
            // **a run that has stopped is never timed live**, whatever its final number was.
            elapsed_ms: summary.map_or(0, |s| {
                if s.elapsed_ms > 0 || crate::roster::is_terminal_word(&s.status) {
                    s.elapsed_ms
                } else {
                    // Through the fence — see `clock.rs`, which is the one file the determinism
                    // guard exempts for exactly this.
                    (marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock).max(0) as u64)
                        .saturating_sub(s.started_ms)
                }
            }),
            spend_micros_usd: summary.map_or(0, |s| s.spend_micros_usd),
            ceiling_micros_usd: cp.as_ref().map_or(0, |c| c.budget.micros_usd),
            spent_tokens: cp.as_ref().map_or(0, |c| c.spent.tokens),
            granted_tokens: cp.as_ref().map_or(0, |c| c.budget.tokens),
            depth: cp.as_ref().map_or(0, |c| c.budget.depth),
            // **A fact, never a roadmap** (§6.3). `None` means no checkpoint exists, and the
            // renderer says so in those words rather than inventing a step 0.
            last_checkpoint_step: cp.as_ref().map(|c| c.step),
            resumable: cp.as_ref().is_some_and(|c| !c.is_terminal()),
            orphan_policy: cp.as_ref().map_or_else(
                || "unknown".into(),
                |c| match c.orphan_policy {
                    OrphanPolicy::Adopt { by } => format!("adopt by {by}"),
                    OrphanPolicy::Detach => "detach".into(),
                    OrphanPolicy::Terminate => "terminate".into(),
                },
            ),
            pending_steers: self.control.pending_steers(run),
            subagents: self.children_of(run),
        }
    }

    /// The runs this one spawned, for §6.3's roster panel.
    ///
    /// **Read from the checkpoint store, not from the run table**, and for the same reason
    /// `parent` above is: the table is what this daemon remembers and the store is what survived.
    /// A child spawned before a restart is still this run's child, and a roster that forgot it
    /// would be a window disagreeing with the journal.
    ///
    /// The status still prefers the live table when it has a row, because that is the one that
    /// moves while a child works; the checkpoint is the fallback and never a guess.
    fn children_of(&self, parent: RunId) -> Vec<crate::protocol::RunChild> {
        self.control
            .store()
            .latest_per_run()
            .into_iter()
            .filter(|cp| cp.parent == Some(parent))
            .map(|cp| {
                let id = cp.run.to_string();
                let status = self
                    .runs
                    .get(&id)
                    .map(|s| s.status.clone())
                    .unwrap_or_else(|| format!("{:?}", cp.status).to_lowercase());
                crate::protocol::RunChild { id, status }
            })
            .collect()
    }
}

/// The loop's [`Control`] port, backed by the shared plane.
///
/// **A lock per call, never across one.** `Engine::drive_inner` asks `cancelled` and `take_steer`
/// once per iteration, before the model call; both are a lock, a lookup and an unlock. So a steer
/// written by the control thread while a model call is in flight is picked up by the very next
/// boundary — which is what "mid-flight, no restart" means.
pub struct SharedControl(pub Shared);

impl SharedControl {
    fn with<T>(&self, f: impl FnOnce(&mut ControlPlane) -> T) -> T {
        f(&mut self.0.lock().expect("the control plane lock was poisoned"))
    }
}

impl Control for SharedControl {
    fn cancelled(&self, run: RunId) -> bool {
        self.with(|p| p.control.cancelled(run))
    }

    fn take_steer(&mut self, run: RunId) -> Option<SteerMessage> {
        self.with(|p| p.control.take_steer(run))
    }

    fn take_interrupt(&mut self) -> Option<String> {
        None
    }
}

/// What a control request may be answered with, and what it may not.
///
/// **Anything needing a model goes on the main port**, and is refused here by name rather than
/// half-served. `Resume` is the one that surprises people: staging a checkpoint is cheap and this
/// plane could do it, but *running* it needs the engine, the driver and the tool host — all of
/// which live behind the daemon's own lock. A `Resume` that staged and did not drive would report
/// success for a run that never took another step.
pub(crate) fn answer(plane: &Shared, request: Request, on_event: &mut dyn FnMut(Event)) {
    let mut plane = plane.lock().expect("the control plane lock was poisoned");
    match request {
        Request::Runs => {
            for e in plane.run_frames() {
                on_event(e);
            }
        }
        // **The detail first, then the frames it belongs with.** One lock, one answer: a window
        // that asked twice could be told a run had finished and then handed frames from before it
        // did, which is the two-answers-to-one-question shape this plane exists to avoid.
        // **`daring-storm` works here, and so does `a1b2`.** See `ControlPlane::resolve`; the
        // refusal for an ambiguous one lists what it matched rather than picking.
        Request::Watch { run, since } => match plane.resolve(&run) {
            Ok(id) => {
                on_event(plane.detail(id));
                let (frames, dropped) = plane.frames_since(&id.0.to_string(), since);
                // **Dropping is reported.** A gap a reader cannot see is worse than a short
                // history — the journal is the record, this is a window.
                if dropped > 0 {
                    on_event(Event::Degraded {
                        what: format!("{dropped} earlier frames are no longer held"),
                        remedy: "the journal has the whole run; a window keeps the recent tail"
                            .into(),
                    });
                }
                for e in frames {
                    on_event(e);
                }
            }
            Err(detail) => on_event(Event::Error { detail }),
        },
        Request::Steer { run, text } => match plane.resolve(&run) {
            Ok(id) => {
                // ── THE ONE DOOR (ADR-054) ──────────────────────────────────────────────────
                //
                // **This used to build a `SteerMessage` here.** It sanitised — `sanitize_line`,
                // then a non-empty check — and that half was right and is unchanged in effect.
                // What it skipped is the rest of admission, and the omission mattered:
                //
                // * **No length cap.** `Provenance::attribute_user_message` inserts the whole
                //   message *and every whitespace-separated token* at `UserAsserted`, and
                //   `taint_for` reads that map **before** it reaches for the run's floor. So an
                //   unbounded steer is an unbounded budget of laundered targets in a run whose
                //   floor has already latched — the one channel that can still do that.
                // * **No `SteerOrigin`.** Authority was carried by nothing at all, so the type
                //   could not say that only a person may assert at this class.
                //
                // Found by `marlowe-loop/tests/steer_has_one_door.rs`, which greps the workspace
                // for `SteerMessage {` outside `steer.rs` and fails by name. It fired on this
                // line — a guard written in Session F catching a path merged from Session A, in a
                // session that was not looking for it.
                //
                // `admit` performs the sanitise, the cap and the emptiness check in that order,
                // and the refusal it returns names both numbers. Nothing is duplicated here.
                // ── A STEER FOR A RUN THAT HAS STOPPED IS REFUSED, NOT QUEUED ──────────────
                //
                // **Found by using it.** `--steer` against a completed run answered
                // `steers 2 queued` — which reads as success, and is a claim about a mechanism
                // that will never run. Nothing consumes a terminal run's queue, so the guidance
                // sits there forever.
                //
                // That is audit finding **E10's exact shape**: *"the user's correction vanished
                // with no error."* E10 was about a child eating a parent's steer; this is the same
                // failure reached from the other end, and the fix is the same one — say so.
                //
                // **`interrupted` is deliberately NOT terminal.** A run that stopped because the
                // daemon did is exactly what resume exists for, and guidance queued for it applies
                // when it resumes. Refusing that would remove a real capability to close a
                // different hole.
                let status = plane.runs.get(&id.to_string()).map(|r| r.status.clone());
                if let Some(status) = status.filter(|s| {
                    matches!(s.as_str(), "completed" | "failed" | "cancelled")
                }) {
                    on_event(Event::Error {
                        detail: format!(
                            "run {id} is {status}; a steer would queue behind a run that has stopped \
                             and would never be read. Nothing was queued"
                        ),
                    });
                    return;
                }

                let message = match marlowe_loop::steer::admit(
                    marlowe_loop::steer::SteerOrigin::Human,
                    &text,
                    Urgency::Advisory,
                ) {
                    Ok(m) => m,
                    Err(refused) => {
                        on_event(Event::Error { detail: refused.to_string() });
                        return;
                    }
                };
                plane.control.steer(id, message);
                // **The count, not an acknowledgement.** "queued" is a claim about a mechanism;
                // a number is a fact the next `/watch` can be checked against.
                on_event(plane.detail(id));
            }
            Err(detail) => on_event(Event::Error { detail }),
        },
        Request::Cancel { run } => match plane.resolve(&run) {
            Ok(id) => {
                plane.control.cancel(id);
                if let Some(s) = plane.runs.get_mut(&id.to_string()) {
                    // Not "cancelled" — the run has been *asked* to stop and stops at its next
                    // iteration boundary. Marking it done here would be the surface reporting an
                    // outcome it does not have.
                    s.status = "cancelling".into();
                }
                on_event(plane.detail(id));
            }
            Err(detail) => on_event(Event::Error { detail }),
        },
        other => on_event(Event::Error {
            detail: format!(
                "the control plane answers runs / watch / steer / cancel. `{}` needs the model, \
                 the memory or the session store, so it is served on the main port",
                request_name(&other)
            ),
        }),
    }
}

/// The resolution itself, over a bare list of ids.
///
/// **Separated from [`ControlPlane`] so it can be tested at all.** A `ControlPlane` owns a
/// `DurableControl<JournalCheckpoints>`, so exercising this through the struct means standing up a
/// journal on disk — and a rule this session proved four times over is that a function which needs
/// a daemon to test is a function nobody tests. The same move as `keyburst::classify` and
/// `watch::age_notice`: the decision is pure, the plumbing is thin, and the decision is what has
/// the behaviour worth pinning.
pub(crate) fn resolve_among<'a>(
    ids: impl Iterator<Item = &'a str>,
    typed: &str,
) -> Result<RunId, String> {
    let typed = typed.trim();
    if let Ok(u) = typed.parse::<uuid::Uuid>() {
        return Ok(RunId(u));
    }
    let want = typed.to_ascii_lowercase();
    // **Four characters before a prefix is a prefix.** Below that, `a` matches a sixteenth of every
    // id in the table and the "candidates" listing is the whole table — which is not an answer.
    let long_enough = want.len() >= 4;
    let mut hits: Vec<String> = ids
        .filter(|id| {
            let lower = id.to_ascii_lowercase();
            (long_enough && lower.starts_with(&want))
                || marlowe_loop::run::sayable(id).eq_ignore_ascii_case(&want)
        })
        .map(str::to_string)
        .collect();
    hits.sort();
    hits.dedup();
    match hits.len() {
        1 => hits[0]
            .parse::<uuid::Uuid>()
            .map(RunId)
            .map_err(|_| unknown_run(typed)),
        0 => Err(unknown_run(typed)),
        _ => Err(ambiguous_run(typed, &hits)),
    }
}

fn unknown_run(run: &str) -> String {
    format!(
        "`{}` names no run here. `/runs` lists them; a run is addressed by its name \
         (`daring-storm`), by the start of its id, or by the whole id",
        marlowe_contract::text::sanitize_line(run)
    )
}

/// **The refusal that lists what it matched.** A resolver that guessed would steer, cancel or
/// watch the wrong run and say nothing about it; one that only said "ambiguous" would leave the
/// user with no way to be more specific except by finding the ids themselves.
fn ambiguous_run(run: &str, hits: &[String]) -> String {
    let mut out = format!(
        "`{}` matches {} runs:",
        marlowe_contract::text::sanitize_line(run),
        hits.len()
    );
    for id in hits {
        out.push_str(&format!("\n  {}  {}", marlowe_loop::run::sayable(id), id));
    }
    out.push_str("\nName one of them by its id. Names are derived from the id and 4096 of them \
                  exist, so two runs can share one");
    out
}

fn request_name(r: &Request) -> &'static str {
    match r {
        Request::Status => "status",
        Request::Ask { .. } => "ask",
        Request::Runs => "runs",
        Request::Watch { .. } => "watch",
        Request::Steer { .. } => "steer",
        Request::Cancel { .. } => "cancel",
        Request::Resume { .. } => "resume",
        Request::Approve { .. } => "approve",
        Request::Replay { .. } => "replay",
        Request::SetModel { .. } => "set_model",
        Request::SetProvider { .. } => "set_provider",
        Request::Shutdown => "shutdown",
    }
}

/// Where the control port is advertised. Beside the token, under the same directory ACL.
pub fn port_path(profile_root: &std::path::Path) -> std::path::PathBuf {
    profile_root.join("control.port")
}

/// The control port a daemon for this profile advertised, if one is running.
///
/// **`None` is the honest answer for a stale file too.** A leftover file from a daemon that died
/// points at a port that is either closed or now belongs to something else; the caller falls back
/// to the main port, where the answer is slower and cannot be wrong.
pub fn advertised_port(profile_root: &std::path::Path) -> Option<u16> {
    std::fs::read_to_string(port_path(profile_root)).ok()?.trim().parse().ok()
}

/// Start the control listener. Returns the reason it could not start, for the daemon to announce.
///
/// **The token is the same one the main port checks**, read once and held — a token file replaced
/// under a running daemon must not change who it will serve, on either port.
pub fn spawn(
    plane: Shared,
    profile_root: &std::path::Path,
    token: String,
    shutdown: Arc<AtomicBool>,
) -> Result<u16, String> {
    // **Port 0: the OS picks.** See the module header for why this is not `port + 1`.
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(|e| {
        format!(
            "the control plane could not bind a loopback port ({e}); `/steer`, `/watch` and \
             `/runs` from another terminal are unavailable, and `--runs` falls back to the main \
             port behind whatever turn is running"
        )
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("the control plane bound a port it cannot name: {e}"))?
        .port();
    // **Advertised before the thread starts.** A client that connected in the window between
    // binding and advertising would fall back to the main port -- correct, just slower -- so the
    // ordering costs nothing and closes the other direction, where the file names a port nothing
    // is listening on yet.
    std::fs::create_dir_all(profile_root)
        .and_then(|()| std::fs::write(port_path(profile_root), port.to_string()))
        .map_err(|e| {
            format!(
                "the control plane is listening on {port} and could not advertise it in the \
                 profile root ({e}); `--steer` and `--watch` will fall back to the main port"
            )
        })?;
    // A short accept timeout is what lets the thread notice a shutdown. `incoming()` blocks
    // forever otherwise, and a control thread that outlives its daemon holds the port against the
    // next one — which presents as "the control plane could not bind" on every restart.
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            // LOOP-EXEMPT: an accept loop, not an agent loop.
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = incoming else { continue };
            // See the note on the main port's accept loop: a child spawned while this connection
            // is open inherits its handle on Windows, so dropping our copy does not give the peer
            // an EOF. `shutdown` acts on the connection, not on a handle count.
            let hangup = stream.try_clone().ok();
            let _ = serve_one(&plane, &token, stream);
            if let Some(h) = hangup {
                let _ = h.shutdown(std::net::Shutdown::Both);
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }
    });
    Ok(port)
}

fn serve_one(plane: &Shared, token: &str, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    reader.get_ref().set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();

    // Same preamble, same rule: the CONNECTION is authenticated, not the request.
    let mut preamble = String::new();
    let offered = match reader.read_line(&mut preamble) {
        Ok(0) | Err(_) => return Ok(()),
        Ok(_) => preamble.trim().to_string(),
    };
    if !crate::auth::matches(token, &offered) {
        write_line(&mut writer, &Event::Error { detail: crate::auth::refusal() })?;
        // Drain before closing. On Windows a close with unread inbound data sends RST, which
        // discards the refusal that was just written — see `Daemon::serve_one`, where this was
        // found, and where the same three lines say the same thing.
        let mut sink = [0u8; 4096];
        let mut drained = 0usize;
        reader.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(200))).ok();
        while drained < 64 * 1024 {
            // LOOP-EXEMPT: draining a socket before close.
            match reader.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
        return Ok(());
    }

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => {
            let mut err = None;
            answer(plane, request, &mut |e| {
                if err.is_none() {
                    err = write_line(&mut writer, &e).err();
                }
            });
        }
        Err(e) => {
            write_line(&mut writer, &Event::Error { detail: format!("malformed request: {e}") })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    /// Two ids whose mnemonics collide, found by walking `from_name` — the collision is the point
    /// of the ambiguity arm and inventing one by hand would not prove it can happen.
    fn colliding_pair() -> (String, String, String) {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for i in 0..20_000 {
            let id = marlowe_loop::RunId::from_name(&format!("run-{i}"));
            let name = id.mnemonic();
            if let Some(first) = seen.insert(name.clone(), id.0.to_string()) {
                return (first, id.0.to_string(), name);
            }
        }
        panic!("no collision in 20000 ids, which contradicts 4096 names");
    }

    fn id_of(name: &str) -> String {
        marlowe_loop::RunId::from_name(name).0.to_string()
    }

    #[test]
    fn a_full_uuid_resolves_even_when_this_daemon_has_never_heard_of_it() {
        // The table is what this daemon remembers; the journal is what survived. `--watch` on an id
        // from a previous daemon is a real thing to want, so only the SHORTHANDS need a table.
        let id = id_of("a-run-nobody-listed");
        let got = resolve_among(std::iter::empty(), &id).expect("a full id must always resolve");
        assert_eq!(got.0.to_string(), id);
    }

    #[test]
    fn a_mnemonic_resolves_to_its_run() {
        let id = id_of("some-run");
        let name = marlowe_loop::RunId::from_name("some-run").mnemonic();
        assert_eq!(
            resolve_among([id.as_str()].into_iter(), &name).unwrap().0.to_string(),
            id
        );
        // ...and case does not matter, because a name is for saying out loud.
        assert!(resolve_among([id.as_str()].into_iter(), &name.to_uppercase()).is_ok());
    }

    #[test]
    fn an_id_prefix_resolves_from_four_characters_and_not_from_three() {
        let id = id_of("prefix-run");
        assert!(resolve_among([id.as_str()].into_iter(), &id[..4]).is_ok());

        // **The floor is the whole reason there is one.** Three hex characters match a sixteenth of
        // every id there could be, so the "candidates" would be the table and the answer would be
        // no answer.
        let err = resolve_among([id.as_str()].into_iter(), &id[..3]).unwrap_err();
        assert!(err.contains("names no run"), "{err}");
    }

    #[test]
    fn an_ambiguous_name_is_refused_by_name_and_lists_what_it_matched() {
        // **The arm that exists because 4096 names collide.** The two wrong answers are both
        // silent: picking the first match steers the wrong run, and reporting "not a run id" for a
        // name the user can see on their own screen reads as a broken product.
        let (a, b, name) = colliding_pair();
        let err = resolve_among([a.as_str(), b.as_str()].into_iter(), &name).unwrap_err();

        assert!(err.contains(&name), "the refusal does not name what was typed: {err}");
        assert!(err.contains(&a) && err.contains(&b), "both candidates must be listed: {err}");
        assert!(err.contains("by its id"), "a refusal that names no way forward: {err}");

        // The control: with only one of them present the same token resolves, so the refusal is
        // about ambiguity rather than about the name being unusable.
        assert!(resolve_among([a.as_str()].into_iter(), &name).is_ok());
    }

    #[test]
    fn an_unknown_token_says_how_a_run_is_addressed() {
        let err = resolve_among(std::iter::empty(), "not-a-run").unwrap_err();
        assert!(err.contains("names no run"), "{err}");
        assert!(err.contains("daring-storm"), "the refusal must show the shape of a name: {err}");
    }

    #[test]
    fn a_typed_token_cannot_carry_an_escape_sequence_into_the_refusal() {
        // The refusal echoes what was typed, and it is printed to a terminal.
        let err = resolve_among(std::iter::empty(), "\u{1b}[2Jwiped").unwrap_err();
        assert!(!err.contains('\u{1b}'), "an escape reached a refusal: {err:?}");
    }
}
