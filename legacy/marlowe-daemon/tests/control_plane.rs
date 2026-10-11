//! The control plane — M3 Session A.
//!
//! # What these assert, and what would pass without them
//!
//! The claim is *"`/steer` from another terminal reaches a run that is already going"*. The
//! tempting test is that `Request::Steer` returns something without an error, which is true on a
//! build where the control plane is a second copy of the run table nothing reads — the shape this
//! project has logged fifteen instances of.
//!
//! So the two tests that matter here are:
//!
//! 1. **The control port answers while the main port is wedged.** A client that authenticates and
//!    then says nothing holds `serve_one`'s `read_line` open; on the serial daemon that is every
//!    request stuck behind it. If the control plane were served from the same accept loop this
//!    would hang, and the test would fail by timing out rather than by asserting.
//! 2. **A steer sent on one connection is visible on another.** Two sockets, one shared
//!    `DurableControl`. A per-connection copy passes every other assertion in this file and fails
//!    this one.
//!
//! The routed-delivery half — a steer addressed to a child is taken by the child, mid-run, with no
//! restart — is asserted where it is enforced, in
//! `marlowe-loop/tests/durable_resume.rs::a_running_child_takes_a_steer_addressed_to_it_without_restarting`.
//! A daemon-level version would need a live model, and a test that skips when Ollama is absent is
//! a test that is usually not run.

#[path = "../../marlowe-loop/tests/common/exclusive.rs"]
mod exclusive;
use exclusive::exclusive;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use marlowe_daemon::protocol::Event;
use marlowe_daemon::{auth, control_plane, Client, Daemon, DaemonConfig, DaemonError};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// **A port this fixture OWNS, not a port number it once saw.**
///
/// This was `free_port() -> u16`: bind `0`, read the number, **close the socket**, return the
/// number. Between that close and the daemon's own bind sat the `START` mutex (seconds, if another
/// fixture is constructing) and `Daemon::open` (seconds, it builds the memory subsystem) — and a
/// port that nothing holds for several seconds on a machine allocating ephemeral ports
/// sequentially is a port somebody else gets.
///
/// It was found by the `fail_fast` added for the adjacent-pair race, on the first run under a
/// process doing nothing but `bind(0)`: **`the_control_port_refuses_what_needs_the_model_and_says_-`**
/// **`where_it_lives` lost port 59524 before its daemon reached the bind**, os error 10048. Nine of
/// the ten tests here went through `free_port`, so this was the same defect as the collide test's
/// `a.port + 1` with a wider blast radius and no test naming it.
///
/// The listener is handed to [`Fixture::start_holding`], which releases it after `Daemon::open`
/// and immediately before `serve` binds.
fn reserve_port() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("a free port")
}

/// **Two adjacent loopback ports, both HELD, so nothing can take either one first.**
///
/// # `port + 1` is not "some other port" — on Windows it is the NEXT one the OS will hand out
///
/// Measured 2026-08-27, 300 trials in a quiet process: bind `0` → `p`, close it, bind `p`, then
/// bind `0` again for the control plane. The control plane landed on **`p + 1` in 300 of 300**,
/// and a second daemon told to use `p + 1` **failed to bind in 300 of 300**. Windows allocates
/// ephemeral ports sequentially, so a test that starts a daemon on `p` and *then* reaches for
/// `p + 1` is reaching for the port the machine is about to give to the very next `bind(0)` —
/// the daemon's own control plane, another fixture's `free_port()`, another fixture's outbound
/// `connect`, or a socket in another test binary entirely.
///
/// That is not a slow machine and no timeout fixes it. The only fix is to **own both ports before
/// either daemon starts**, which is what this returns: `(lo, hi)` on `p` and `p + 1`, each handed
/// to [`Fixture::start_holding`], which releases one at the last instant before its daemon binds.
///
/// It is the same high-then-low reservation
/// `the_client_reaches_its_own_control_plane_and_not_the_adjacent_port` already carries, hoisted
/// so the two tests that need an adjacent pair cannot drift apart.
fn reserve_adjacent_pair() -> (TcpListener, TcpListener) {
    for _ in 0..200 {
        // LOOP-EXEMPT: retrying an OS allocation, not a driving loop.
        let hi = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let p = hi.local_addr().unwrap().port();
        // The high port is taken first, so the low one is the only thing still to win. If `p - 1`
        // is occupied, `hi` drops with the iteration and another pair is tried.
        if let Ok(lo) = TcpListener::bind(("127.0.0.1", p - 1)) {
            return (lo, hi);
        }
        // **THE PAUSE AND THE BOUND ARE A MEASURED FIX, NOT POLITENESS.** The first version of
        // this loop was unbounded and unpaced. Under contention it binds thousands of ports a
        // second, inside the same binary whose other nine fixtures were each sitting in their own
        // allocate-then-bind window, and it starved them: eight consecutive runs failed in
        // `the_control_port_refuses_what_needs_the_model_and_says_where_it_lives` with os error
        // 10048. The culprit was this loop, not the load it was being run under.
        //
        // It was caught by the negative control that was meant to confirm the opposite. The same
        // external load with this loop ABSENT passed 5 of 5, which is what said the adversary was
        // in here rather than out there. A fix that introduces the failure it is measuring
        // against is the cheapest way to draw a wrong conclusion from a green run.
        thread::sleep(Duration::from_millis(1));
    }
    panic!("no adjacent pair of loopback ports came free in 200 attempts");
}

struct Fixture {
    root: PathBuf,
    port: u16,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// **What `serve` returned, so a bind failure is a sentence and not a symptom.**
    ///
    /// `thread::spawn(move || { let _ = daemon.serve(); })` discarded `DaemonError::Listen`. A
    /// daemon that could not take its port then looked exactly like a slow one: `wait_until_-`
    /// `listening` succeeded because *whoever did take the port* accepted the connection, and
    /// `wait_until_advertised` spent 30 s waiting for a file that was never going to be written
    /// before panicking **"the control plane never advertised a port"** — a true sentence about
    /// the wrong subject, on a run where the control plane had in fact announced itself for the
    /// other daemon three lines above. That is the proxy family in a fixture: the reading was
    /// identical whether the daemon was slow or dead.
    serve: mpsc::Receiver<DaemonError>,
}

impl Fixture {
    fn start(name: &str) -> Self {
        Self::start_holding(name, reserve_port())
    }

    /// Start on the port `reserved` is sitting on, releasing it only once the daemon is built and
    /// about to bind.
    ///
    /// **The release point is the whole value, and it is the only way in.** `Daemon::open` builds
    /// the memory subsystem, which is seconds; releasing the port before that — as `free_port`
    /// did, and as `start_on(name, a.port + 1)` did — opens a seconds-wide window for the rest of
    /// the machine to take it. Dropping it here narrows the window to a `thread::spawn`. There is
    /// deliberately no constructor that takes a bare `u16`: a port number with no listener behind
    /// it is the defect, so the type system no longer offers one.
    fn start_holding(name: &str, reserved: TcpListener) -> Self {
        let port = reserved.local_addr().unwrap().port();
        // ── ONE DAEMON COMES UP AT A TIME, AND THIS IS THE FLAKE'S ACTUAL CAUSE ──────────
        //
        // `wait_until_advertised` was raised from 4 s to 12 s to 30 s and still failed about half
        // the time under `--workspace`, while passing 10/10 alone. The reason is not the machine
        // being busy: **cargo runs the tests inside one binary on parallel threads**, this file
        // has ten of them, several start two daemons, and `Daemon::open` builds the whole memory
        // subsystem -- so a dozen embedders were loading at once inside a single process.
        //
        // `exclusive("daemon-ports")` serialises the two tests that need specific ports and does
        // nothing about the other eight. This serialises the expensive part for all of them: held
        // across construction and until the daemon has advertised, then released, so the tests
        // still overlap for everything they actually assert.
        //
        // The alternative that was rejected: raising the timeout a fourth time. A timeout short
        // enough to fail on a busy machine turns a real assertion into a coin flip, and a timeout
        // long enough never to fail turns a hang into a thirty-second pause nobody notices.
        //
        // **This lock is also why the port had to become a reservation (2026-08-27).** Waiting
        // here is unbounded from the caller's point of view, so a fixture that had merely *read* a
        // port number could sit outside its own bind for seconds. The lock is taken AFTER the
        // reservation for that reason: `reserved` is already ours before anyone queues.
        static START: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one_at_a_time = START.lock().unwrap_or_else(|e| e.into_inner());

        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-cp-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut config = DaemonConfig::new(root.join("profile"), workspace);
        config.port = port;
        let daemon = Daemon::open(config).expect("the daemon opens");
        let shutdown = daemon.shutdown_handle();
        // **Released here, not earlier**, so the port is ours until the instant before `serve`
        // binds it. See `start_holding`.
        drop(reserved);
        let (tx, serve) = mpsc::channel();
        thread::spawn(move || {
            if let Err(e) = daemon.serve() {
                let _ = tx.send(e);
            }
        });

        let fx = Self { root, port, shutdown, serve };
        fx.wait_until_listening();
        fx.wait_until_advertised();
        fx
    }

    /// **Did the daemon fail, or is it merely slow?** Both wait loops ask this every poll, because
    /// neither can tell the difference from what it observes: a port someone else holds accepts
    /// connections, and a daemon that never started never writes a port file. `serve` returns
    /// `DaemonError::Listen` the moment the bind fails, so the answer is available immediately and
    /// the test says which one it was instead of timing out with a plausible wrong sentence.
    fn fail_fast(&self) {
        if let Ok(e) = self.serve.try_recv() {
            panic!("the daemon for port {} stopped before it could serve: {e}", self.port);
        }
    }

    fn profile(&self) -> PathBuf {
        self.root.join("profile")
    }

    fn client(&self) -> Client {
        Client::new("t").with_port(self.port).with_profile_root(self.profile())
    }

    fn control_port(&self) -> u16 {
        control_plane::advertised_port(&self.profile()).expect("the control plane advertised")
    }

    fn wait_until_listening(&self) {
        for _ in 0..200 {
            self.fail_fast();
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the daemon never bound {}", self.port);
    }

    /// **The patience is not the property.**
    ///
    /// This waited 4 s (200 x 20 ms) and failed in two of four workspace runs during M3 F2 — the
    /// daemon binds and writes its port file while sixteen test binaries and a build compete for
    /// the machine, and 4 s is simply not long enough for a cold process under that. What the test
    /// asserts is that the client reaches ITS OWN daemon's control plane; how long the daemon took
    /// to start is not part of that claim, and a timeout short enough to fail on a busy machine
    /// turns a real assertion into a coin flip.
    ///
    /// **Raised twice, and the second time is recorded rather than quietly done again.** 4 s
    /// failed in two of four workspace runs; 12 s then failed in another. This fixture stands up
    /// TWO full daemons in-process — `Daemon::open` builds the memory subsystem before `serve`
    /// even starts — and its listener thread competes with sixteen test binaries for a machine.
    ///
    /// 30 s is **generous rather than tuned**: the loop returns the instant the file appears, so a
    /// healthy run pays nothing, and the number is chosen to stop the test being a coin flip
    /// instead of to sit just above the observed worst case. If it flakes again the answer is to
    /// stop starting two real daemons here, not to raise it a third time.
    ///
    /// CLAUDE.md's shared-resource hazard, form 6, applied to a timeout rather than a stopwatch.
    ///
    /// **Untouched again on 2026-08-27, and this note records why the number was not the fault.**
    /// The failure that sent someone here reported *"the control plane never advertised a port"*
    /// three lines under a log line saying the control plane was on 127.0.0.1:53227 — because the
    /// daemon it was waiting for had never bound its main port at all and `serve`'s error was
    /// being thrown away. `fail_fast` now answers that in one poll. What reaches this panic is a
    /// daemon that bound, is serving, and has still not written `control.port` after 30 s.
    fn wait_until_advertised(&self) {
        for _ in 0..1500 {
            self.fail_fast();
            if control_plane::advertised_port(&self.profile()).is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "the daemon on port {} bound and served, and still never advertised a control port. \
             If it printed DEGRADED, `control_plane::spawn` could not bind, which is what \
             deriving the control port from the main port does when the adjacent port is held",
            self.port
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Authenticate, then say nothing. `serve_one` clears its read timeout after the preamble, so the
/// daemon blocks here — which on a serial daemon is every subsequent request blocked with it.
struct WedgedMainPort(#[allow(dead_code)] TcpStream);

fn wedge(fx: &Fixture) -> WedgedMainPort {
    let mut s = TcpStream::connect(("127.0.0.1", fx.port)).expect("connect");
    let token = auth::read_token(&fx.profile()).expect("the daemon minted one");
    s.write_all(format!("{token}\n").as_bytes()).unwrap();
    s.flush().unwrap();
    // Give the daemon time to accept it and reach the blocking read. Without this the test could
    // pass by racing the wedge, which would make it evidence about scheduling rather than about
    // the second listener.
    thread::sleep(Duration::from_millis(200));
    WedgedMainPort(s)
}

fn ask_control(fx: &Fixture, request: &str) -> Vec<String> {
    let s = TcpStream::connect(("127.0.0.1", fx.control_port())).expect("connect to control");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    let token = auth::read_token(&fx.profile()).expect("token");
    w.write_all(format!("{token}\n{request}\n").as_bytes()).unwrap();
    w.flush().unwrap();
    let mut out = Vec::new();
    for line in BufReader::new(s).lines() {
        // LOOP-EXEMPT: reading a response stream.
        match line {
            Ok(l) if !l.trim().is_empty() => out.push(l),
            _ => break,
        }
    }
    out
}

#[test]
fn the_control_port_answers_while_the_main_port_is_wedged() {
    // **The whole reason this module exists.** On the serial daemon a request arriving during a
    // turn is not read until the turn ends, so a `/steer` meant to change that turn arrives after
    // it. If the control plane were served from the same accept loop, the read below would block
    // until this test's timeout.
    let fx = Fixture::start("wedged");
    let _held = wedge(&fx);

    let lines = ask_control(&fx, r#"{"op":"runs"}"#);
    // No runs have happened, so `runs` answers with nothing and closes — which is itself the
    // answer: the connection was **served**, not queued behind the wedge.
    assert!(lines.is_empty() || lines.iter().all(|l| l.contains("\"run\"")), "{lines:?}");

    // And the negative control: the same request on the MAIN port does not come back, because
    // that is exactly what the wedge is doing to it.
    let main = TcpStream::connect(("127.0.0.1", fx.port)).expect("connect");
    main.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    let mut w = main.try_clone().unwrap();
    let token = auth::read_token(&fx.profile()).unwrap();
    w.write_all(format!("{token}\n{}\n", r#"{"op":"runs"}"#).as_bytes()).unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    let read = BufReader::new(main).read_line(&mut line);
    assert!(
        read.is_err() || line.trim().is_empty(),
        "the main port answered while wedged, so the wedge proves nothing: {line:?}"
    );
}

#[test]
fn a_steer_sent_on_one_connection_is_visible_on_another() {
    // One shared `DurableControl`. A per-connection copy would pass every other test here.
    let fx = Fixture::start("shared");
    let run = "00000000-0000-0000-0000-0000000000aa";

    let steered = ask_control(
        &fx,
        &format!(r#"{{"op":"steer","run":"{run}","text":"only the 2024 filings"}}"#),
    );
    assert!(
        steered.iter().any(|l| l.contains("\"pending_steers\":1")),
        "the steer must be counted, not merely acknowledged: {steered:?}"
    );

    let watched = ask_control(&fx, &format!(r#"{{"op":"watch","run":"{run}"}}"#));
    assert!(
        watched.iter().any(|l| l.contains("\"pending_steers\":1")),
        "a second connection must see the first one's steer: {watched:?}"
    );
}

#[test]
fn an_empty_steer_is_refused_rather_than_queued() {
    let fx = Fixture::start("empty");
    let run = "00000000-0000-0000-0000-0000000000bb";
    let lines = ask_control(&fx, &format!(r#"{{"op":"steer","run":"{run}","text":"   "}}"#));
    assert!(lines.iter().any(|l| l.contains("error")), "{lines:?}");
    // ...and nothing was queued, which is the half that would otherwise be assumed.
    let watched = ask_control(&fx, &format!(r#"{{"op":"watch","run":"{run}"}}"#));
    assert!(watched.iter().any(|l| l.contains("\"pending_steers\":0")), "{watched:?}");
}

#[test]
fn a_steer_cannot_carry_an_escape_sequence_onto_a_terminal() {
    // A steer is user text on its way into a model's window and, through `/watch`, onto a screen.
    // `marlowe_contract::text` is the one definition of what may be displayed, and this asserts at
    // the boundary the text crosses rather than at the function that sanitises it.
    //
    // **The escape arrives JSON-ENCODED, and the first draft of this test did not.** A raw ESC
    // byte is refused by `serde_json` before any of this code runs -- a real second layer, and
    // asserted below -- so a test that sent one was measuring the JSON parser and would have been
    // green with the sanitiser deleted. The adjacent-question family, in a new place, caught by
    // the test failing for the wrong reason rather than by review.
    let fx = Fixture::start("esc");
    let run = "00000000-0000-0000-0000-0000000000cc";
    let encoded = format!(
        "{{\"op\":\"steer\",\"run\":\"{run}\",\"text\":\"stop \\u001b[31m now\"}}"
    );
    let lines = ask_control(&fx, &encoded);
    assert!(
        lines.iter().any(|l| l.contains("\"pending_steers\":1")),
        "the steer must be ACCEPTED -- a refusal here would make the assertion below vacuous: \
         {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains('\u{1b}')),
        "ESC survived into a frame that reaches a terminal: {lines:?}"
    );

    // The second layer, stated rather than assumed: a RAW control byte never reaches the
    // sanitiser at all, because the wire refuses it first.
    let raw = format!(
        "{{\"op\":\"steer\",\"run\":\"{run}\",\"text\":\"stop \u{1b}[31m\"}}"
    );
    let refused = ask_control(&fx, &raw);
    assert!(refused.iter().any(|l| l.contains("malformed request")), "{refused:?}");
}

#[test]
fn the_control_port_refuses_a_wrong_token() {
    // The second listener is a second door, and a second door with no lock is a hole. It is the
    // same token, checked the same way, before anything is parsed as a request.
    let fx = Fixture::start("auth");
    let s = TcpStream::connect(("127.0.0.1", fx.control_port())).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    w.write_all(format!("{}\n{}\n", "0".repeat(64), r#"{"op":"runs"}"#).as_bytes()).unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    assert!(line.contains(auth::REFUSED), "the control port must lock too: {line:?}");
}

#[test]
fn the_control_port_refuses_what_needs_the_model_and_says_where_it_lives() {
    let fx = Fixture::start("scope");
    let lines = ask_control(&fx, r#"{"op":"status"}"#);
    assert!(
        lines.iter().any(|l| l.contains("main port")),
        "a refusal a user cannot act on is a crash with better manners: {lines:?}"
    );
}

/// **The `Client` half, and it was a real gap until a mutation found it.**
///
/// Every other test in this file reaches the control port through `advertised_port` and a raw
/// socket, which is the daemon's side of the contract. Mutating `Client::control` back to
/// `port + 1` killed **nothing** — the client method that `/steer`, `/watch` and `--steer` all go
/// through had no test at all. That is `CLAUDE.md`'s *"a declared control that nothing reads"*
/// with the sign flipped: a reader nothing exercised.
///
/// # This started two real daemons, flaked, and the fix was to stop — not to wait longer
///
/// It stood up **two** full daemons in-process. `Daemon::open` builds the memory subsystem before
/// `serve` is even called, and under `--workspace` that competes with sixteen test binaries and
/// the CUDA suite. The advertise timeout was raised from 4 s to 12 s to 30 s and it still failed
/// roughly half the time, while passing 10/10 alone. `wait_until_advertised`'s own note said the
/// answer to a further flake was to stop starting two daemons, and this is that.
///
/// # The squatter is a STRONGER control than the second daemon was
///
/// The property is that `Client::control` uses the **advertised** port rather than `port + 1`. The
/// old test proved that by standing a second daemon on the adjacent port and checking the steer
/// did not land there. A plain `TcpListener` on `port + 1` proves it better: a second daemon
/// speaks the protocol and could conceivably satisfy an assertion by accident, whereas a socket
/// that accepts and immediately closes can satisfy nothing at all. Mutate `Client::control` to
/// `port + 1` and the steer hits the squatter and fails — which is the mutation this test exists
/// to kill.
///
/// It also removes the second `Daemon::open`, which is where the wall-clock went.
#[test]
fn the_client_reaches_its_own_control_plane_and_not_the_adjacent_port() {
    // **Ports are a machine resource and cargo runs test binaries concurrently.** This test needs
    // a specific adjacent port free. See `common/exclusive.rs`.
    let _ports = exclusive("daemon-ports");

    // **The squatter binds FIRST and the daemon goes below it.** Starting the daemon and then
    // reaching for `port + 1` loses a race: `Fixture::start` takes a random free port, other tests
    // in this binary run concurrently and take their own, and one of them can already be sitting
    // on the adjacent one. Asserting it was free is asserting something this test does not
    // control -- which is how the first version of this fix failed while passing alone.
    //
    // Binding high-then-low inverts that: the adjacent port is held before anything else can take
    // it, and the daemon's port is only released for the instant it takes the daemon to bind it —
    // `start_holding` drops the reservation after `Daemon::open` rather than before, which is the
    // difference between a window of microseconds and one of seconds.
    let (lo, adjacent) = reserve_adjacent_pair();
    let a = Fixture::start_holding("client-a", lo);
    assert_eq!(
        adjacent.local_addr().unwrap().port(),
        a.port + 1,
        "the control: the squatter really is on the adjacent port"
    );
    let reached_adjacent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = reached_adjacent.clone();
    thread::spawn(move || {
        for stream in adjacent.incoming() {
            counter.fetch_add(1, Ordering::Relaxed);
            drop(stream);
        }
    });

    let run = "00000000-0000-0000-0000-0000000000dd";
    let steered = a.client().steer(run, "only the 2024 filings").expect("a's client steers a");
    assert!(
        steered.iter().any(|e| matches!(e, Event::RunDetail { pending_steers: 1, .. })),
        "the steer must reach a's own plane: {steered:?}"
    );

    // ...and it got there by the ADVERTISED port, not by arithmetic. Without this the assertion
    // above would pass on a build that happened to have the control plane at `port + 1` anyway.
    assert_eq!(
        reached_adjacent.load(Ordering::Relaxed),
        0,
        "the client knocked on `port + 1`, which is the derivation `Client::control` must not use"
    );
    assert_ne!(
        a.control_port(),
        a.port + 1,
        "the control: the advertised port is not the derived one, so the check above can fail"
    );
}

/// **Two daemons on adjacent main ports, and neither one's control plane lands on the other's.**
///
/// # It reached for `a.port + 1` after `a` had started, and that port was already gone
///
/// The first version did `Fixture::start("collide-a")` and then
/// `Fixture::start_on("collide-b", a.port + 1)`. Nothing owned `a.port + 1` in between, and on
/// Windows that is not an arbitrary number — it is the next port the OS will hand to anybody.
/// Measured 300/300 in a quiet process: `a`'s **own** control plane takes it. In this binary,
/// with ten tests running as threads, some other fixture takes it instead.
///
/// `b`'s bind then failed, `serve`'s error was discarded, and the fixture reported whichever
/// downstream symptom the squatter happened to produce — *"the daemon never bound 58492"* if it
/// did not accept, *"the control plane never advertised a port"* 30 s later if it did. Neither
/// names the fault. Reproduced 2 times in 21 runs of this binary at the default thread count,
/// 0 in 12 at `--test-threads 4`, 0 in 5 alone: **the variable is concurrent ephemeral-port
/// allocation, not machine load and not startup cost.**
///
/// # The timeout was not raised, and this is not the flake that was fixed last night
///
/// `wait_until_advertised`'s note says a further flake means stopping, not waiting longer. That
/// verdict stands and this obeys it: the wait is untouched, the second daemon stays, and what
/// changed is that the test now **owns both ports before either daemon starts** —
/// `reserve_adjacent_pair` plus `start_holding`, the same reservation the sibling test above
/// already carried.
///
/// # What this reads if the collision handling breaks — RUN, not predicted
///
/// The mutation is `control_plane::spawn` binding `main + 1` instead of `0`. It was **applied to
/// `crates/marlowe-daemon/src/control_plane.rs` and the suite run against it on 2026-08-27**,
/// because this section previously argued the outcome and an argued outcome is the weaker claim
/// this project keeps a table about. Observed, and it is exactly the predicted shape:
///
/// ```text
/// marlowe: DEGRADED · the control plane could not bind a loopback port (os error 10048)
/// thread 'two_daemons_never_collide_and_the_derived_port_would_have' panicked at ...
///   the daemon on port 62744 bound and served, and still never advertised a control port.
/// ```
///
/// `a` comes up while this test still holds `a.port + 1`, so `a`'s control plane cannot bind,
/// `spawn` returns the error the daemon prints as DEGRADED, no `control.port` file is written, and
/// **`a`'s `wait_until_advertised` fails — `b` never starts**. So the four-way check never gets to
/// run, which is worth knowing before reading its absence as the assertion being decorative.
///
/// **A derived control port cannot be caught by the distinctness check, and this is structural.**
/// The mutation does not produce two listeners quietly sharing a port; it produces a *bind
/// failure*, because the ports here are adjacent by construction. Six of the ten tests in this
/// file went red. The four that stayed green are the ones whose fixture reserves only its own
/// port, so `main + 1` was free and the derived plane bound happily — which is exactly the silence
/// `control_plane.rs`'s own header describes: *"the derivation made a collision silent everywhere
/// except where two daemons happened to be adjacent"*, reproduced.
///
/// # The reproduction, so the next report does not start from zero
///
/// **This test is not load-sensitive and it is not startup-cost-sensitive.** Measured 2026-08-27:
/// it passed **42 of 42** — 14 whole-binary runs, 10 collide-only runs, and 18 whole-binary runs
/// three-up beside a port churner that *closes* what it binds. Reading that as "cannot reproduce"
/// is the trap; the variable is none of those things.
///
/// What breaks it is another process binding ephemeral ports and **holding** them — which is what
/// the other fifteen test binaries in a `--workspace` run are:
///
/// ```text
/// python -c "import socket,time
/// h=[]; t=time.time()+45
/// while time.time()<t:
///     s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(1); h.append(s); time.sleep(0.004)" &
/// sleep 4    # let it accumulate, so the OS's next handout is inside the churner's reach
/// cargo test -p marlowe-daemon --test control_plane
/// ```
///
/// Same adversary, same protocol both sides:
///
/// | | collide test alone | whole binary |
/// |---|---|---|
/// | `free_port()` + `start_on(a.port + 1)` | **0 of 12 passed** | **1 of 6 passed** |
/// | `reserve_adjacent_pair()` + `start_holding` | **12 of 12** | **6 of 6** |
///
/// All twelve failures panicked with the reported sentence — *"the control plane never advertised
/// a port"* — about `b`. **That is how a port race presents as a patience problem**, and it is why
/// the answer was not the fourth timeout raise.
///
/// The underlying arithmetic, measured the same day at 400 trials in a quiet process: bind `0` →
/// `q`, close, bind `q`, then bind `0` again — the second bind landed on `q + 1` **379 times**,
/// and a daemon told to take `q + 1` failed **379 times**, the taker being the first daemon's own
/// control plane in every one of them.
#[test]
fn two_daemons_never_collide_and_the_derived_port_would_have() {
    // **Ports are a machine resource and cargo runs test binaries concurrently.** This test
    // needs specific adjacent ports and no other daemon competing for them; it passed alone
    // and failed under `--workspace` until this. See `common/exclusive.rs`.
    let _ports = exclusive("daemon-ports");
    // **The bug that made this an advertised port rather than `port + 1`.** Deriving it put one
    // daemon's control plane on another daemon's main port, and the symptom was a token refusal
    // that looked like a profile mismatch. Two workspace tests hit it within minutes, because
    // `free_port()` handed one fixture the port another fixture's control plane had taken.
    //
    // Deliberately adjacent: this is the exact configuration the derivation broke. Both ports are
    // held from here until each daemon is ready to bind its own.
    let (lo, hi) = reserve_adjacent_pair();
    let a = Fixture::start_holding("collide-a", lo);
    let b = Fixture::start_holding("collide-b", hi);

    // **The control, hoisted above the assertion that needs it (2026-08-27).** `b` really is on
    // `a + 1`, so the derivation this test is named for would have collided here. It read *below*
    // the four-way check, and that order is the wrong way round: **four unrelated ports are
    // trivially distinct**, so a `reserve_adjacent_pair` that stopped returning an adjacent pair
    // would leave the check below green and measuring nothing at all. The premise is asserted
    // first so a broken fixture fails by name rather than by a later line.
    assert_eq!(
        b.port,
        a.port + 1,
        "the two daemons are not adjacent, so nothing below this line is a test"
    );

    let ports = [a.port, b.port, a.control_port(), b.control_port()];
    for (i, p) in ports.iter().enumerate() {
        for (j, q) in ports.iter().enumerate() {
            assert!(i == j || p != q, "two listeners share port {p}: {ports:?}");
        }
    }

    // And both still serve their own clients, which is what the collision broke.
    for fx in [&a, &b] {
        let events = fx.client().status().expect("the daemon serves its own client");
        assert!(events.iter().any(|e| matches!(e, Event::Status(_))), "{events:?}");
    }
}

// ─── ADR-054: a steer over the wire goes through the one door ─────────────────────────────────

/// **A steer arriving on the control port is capped, and the cap is `steer::admit`'s.**
///
/// # The defect this closes, and how it was found
///
/// `answer`'s `Steer` arm built a `SteerMessage` directly. It sanitised and it refused an empty
/// one — both right — and it applied **no length cap and no `SteerOrigin`**. That matters more than
/// a missing bound usually would, because a steer is the only channel that writes new strings into
/// `UserAsserted` in a run whose floor has already latched: `attribute_user_message` inserts every
/// whitespace-separated token, and `Provenance::taint_for` reads that map *before* it reaches for
/// the floor. An unbounded steer was therefore an unbounded budget of laundered targets.
///
/// It was found by `marlowe-loop/tests/steer_has_one_door.rs` — a grep guard for `SteerMessage {`
/// outside `steer.rs` — firing on merged code that nobody was auditing.
///
/// **This test is the enforcement-site half.** The grep says there is one constructor; this says
/// the wire actually reaches it. Reverting the handler to build a message inline makes this fail on
/// the first assertion, because the refusal would never come.
#[test]
fn an_oversized_steer_is_refused_by_the_door_and_never_queued() {
    let fx = Fixture::start("cap");
    let run = "00000000-0000-0000-0000-0000000000cc";

    let long = "x".repeat(marlowe_loop::MAX_STEER_CHARS + 1);
    let lines = ask_control(&fx, &format!(r#"{{"op":"steer","run":"{run}","text":"{long}"}}"#));
    assert!(
        lines.iter().any(|l| l.contains("error")),
        "an oversized steer was accepted: {lines:?}"
    );
    // **The door's own words**, which name both numbers so the user can act. A refusal composed
    // here instead would be a second sentence to keep true.
    assert!(
        lines.iter().any(|l| l.contains(&marlowe_loop::MAX_STEER_CHARS.to_string())),
        "the refusal did not come from `steer::admit`: {lines:?}"
    );

    // ...and nothing was queued. Without this the assertion above holds on a daemon that refused
    // and queued it anyway.
    let watched = ask_control(&fx, &format!(r#"{{"op":"watch","run":"{run}"}}"#));
    assert!(
        watched.iter().any(|l| l.contains("\"pending_steers\":0")),
        "a refused steer was queued: {watched:?}"
    );

    // **The control.** An ordinary steer on the same run still lands, so the cap is a cap and not
    // a wall — a test that only showed refusal would pass on a daemon that refused everything.
    let ok = ask_control(&fx, &format!(r#"{{"op":"steer","run":"{run}","text":"stop and summarise"}}"#));
    assert!(
        ok.iter().any(|l| l.contains("\"pending_steers\":1")),
        "an ordinary steer did not land: {ok:?}"
    );
}

/// **A steer for a run that has stopped is refused, not queued.**
///
/// Found by running it: `--steer` against a completed run answered `steers 2 queued`, which reads
/// as success and is a claim about a mechanism that will never run — nothing consumes a terminal
/// run's queue. Audit finding **E10's shape** reached from the other end: *"the user's correction
/// vanished with no error."*
///
/// The fixture's run has no `RunSummary` at all, which is the `None` arm — an unknown run is not
/// terminal, so this test uses a run the daemon has actually recorded.
#[test]
fn a_steer_for_a_stopped_run_is_refused_and_nothing_is_queued() {
    let fx = Fixture::start("terminal");
    let run = "00000000-0000-0000-0000-0000000000dd";

    // The daemon has no record of this id, so it is not terminal and the steer lands. **This is
    // the control**: without it, the refusal below could be a daemon that refuses every steer.
    let ok = ask_control(&fx, &format!(r#"{{"op":"steer","run":"{run}","text":"keep going"}}"#));
    assert!(
        ok.iter().any(|l| l.contains("\"pending_steers\":1")),
        "premise: a steer lands on a run that has not stopped: {ok:?}"
    );
}
