//! ARCHITECTURE §6's split, exercised over a real socket.
//!
//! The claims under test are the two §6 makes, and neither is about plumbing:
//!
//! 1. **The daemon owns the run.** A client connects, asks, disconnects — and the daemon still
//!    holds the record. That is invariant 6's *first* half: a run outlives its starter. The
//!    second half — outliving the *daemon* — is M3 and K5, and is deliberately not claimed here.
//! 2. **The client can render before the daemon exists.** §B13 budgets 150 ms to first frame and
//!    §6 names the split as what buys it.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use marlowe_daemon::protocol::Event;
use marlowe_daemon::{Client, Daemon, DaemonConfig, Request};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A free loopback port, so parallel tests do not collide.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    port: u16,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-daemon-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("notes.md"), "alpha\n").unwrap();
        Self { root, workspace, port: free_port() }
    }

    fn config(&self) -> DaemonConfig {
        let mut c = DaemonConfig::new(self.root.join("profile"), self.workspace.clone());
        c.port = self.port;
        c
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_client_can_render_before_a_daemon_exists() {
    // §B13's 150 ms budget, and §6's stated reason for the split.
    //
    // The assertion is on the CONFIGURED bound, not a timed observation. Timing this would be
    // flaky on a loaded machine — and a parallel build inflating a timed assertion is exactly the
    // sixth form in CLAUDE.md's shared-checkout ledger. It also keeps a real clock out of a test,
    // which `marlowe/tests/determinism_guard.rs` enforces across the workspace.
    let client = Client::new("s").with_port(free_port());

    assert!(
        client.connect_timeout() <= Duration::from_millis(50),
        "the client's connect probe is {:?}, which would eat the 150 ms first-frame budget just \
         discovering there is no daemon. The first measured version of this was 400 ms.",
        client.connect_timeout()
    );
    assert!(
        !client.daemon_is_up(),
        "no daemon should be listening on a freshly-bound-then-dropped port"
    );
}

#[test]
fn a_status_request_answers_without_touching_a_model() {
    // The first frame's data. It must not depend on Ollama being up, or a machine with no model
    // could not render a status band that says so.
    let fx = Fixture::new("status");
    let daemon = Daemon::open(fx.config()).expect("the daemon opens");
    let shutdown = daemon.shutdown_handle();
    let port = fx.port;
    let handle = thread::spawn(move || {
        let _ = daemon.serve();
    });

    // Give the listener a moment to bind.
    let client = Client::new("s").with_port(port).with_profile_root(fx.root.join("profile"));
    let mut ready = false;
    for _ in 0..50 {
        if client.daemon_is_up() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "the daemon never came up");

    let events = client.status().expect("status answers");
    let Some(Event::Status(report)) = events.first() else {
        panic!("expected a status frame, got {events:?}");
    };

    assert_eq!(report.workspace, fx.workspace.display().to_string());
    assert_eq!(report.model, marlowe_provider::DEFAULT_MODEL);
    // ADR-028 requirement 2: the disclosure carries its denominator.
    assert!(report.model_disclosure.contains("12/12"), "{}", report.model_disclosure);
    // ADR-029: the provider is announced, and the honest value when nothing has stamped one is
    // that nothing has — not a guess.
    assert_eq!(report.rerank_provider, "not-loaded");

    shutdown.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
}

#[test]
fn the_daemon_owns_the_run_across_a_client_disconnect() {
    // Invariant 6's first half, and the reason §6 splits the binary at all: the client that
    // started the work is gone, and the record is still the daemon's.
    //
    // The turn itself degrades (no model is required to be running in CI), and that is the
    // point — the RUN still exists either way, which is what the split buys.
    let fx = Fixture::new("owns");
    let daemon = Daemon::open(fx.config()).expect("the daemon opens");
    let shutdown = daemon.shutdown_handle();
    let port = fx.port;
    let handle = thread::spawn(move || {
        let _ = daemon.serve();
    });

    let client = Client::new("s1").with_port(port).with_profile_root(fx.root.join("profile"));
    for _ in 0..50 {
        if client.daemon_is_up() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let events = client.ask("say hello").expect("the turn returns events");
    assert!(
        events.iter().any(|e| matches!(e, Event::Done { .. })),
        "every turn ends in a Done frame: {events:?}"
    );

    // The client is dropped here — the connection it used is closed.
    drop(client);

    // A DIFFERENT client, on a new connection, still sees the daemon's runs.
    let second = Client::new("s2").with_port(port).with_profile_root(fx.root.join("profile"));
    let runs = second.send(&Request::Runs).expect("runs answers");
    assert!(
        runs.iter().any(|e| matches!(e, Event::Run { .. })),
        "the run vanished when its starting client disconnected, which is invariant 6 failing: \
         {runs:?}"
    );

    shutdown.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
}

#[test]
fn a_turn_with_no_model_degrades_with_a_remedy_rather_than_failing() {
    // Invariant 4 end to end: the daemon points at a port nothing serves, and the client gets a
    // declared state naming the fix — not a crash, and not silence.
    let fx = Fixture::new("degrade");
    let mut config = fx.config();
    config.model = "definitely-not-pulled:0b".to_string();
    let daemon = Daemon::open(config).expect("the daemon opens");
    let shutdown = daemon.shutdown_handle();
    let port = fx.port;
    let handle = thread::spawn(move || {
        let _ = daemon.serve();
    });

    let client = Client::new("s").with_port(port).with_profile_root(fx.root.join("profile"));
    for _ in 0..50 {
        if client.daemon_is_up() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let events = client.ask("anything").expect("the turn answers");
    let degraded = events.iter().find_map(|e| match e {
        Event::Degraded { remedy, .. } => Some(remedy.clone()),
        _ => None,
    });
    match degraded {
        Some(remedy) => assert!(
            remedy.contains("ollama") || remedy.contains("pull"),
            "the remedy must be actionable: {remedy}"
        ),
        // If a model IS pulled and serving on this machine the turn may proceed; the assertion
        // is then that it did not crash. Named rather than silently tolerated.
        None => assert!(
            events.iter().any(|e| matches!(e, Event::Done { .. })),
            "neither degraded nor done: {events:?}"
        ),
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
}
