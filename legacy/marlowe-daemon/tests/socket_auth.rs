//! **The daemon socket is authenticated.** Audit finding 4.
//!
//! Loopback is per-machine, not per-user. Before this, any process that could open a TCP connection
//! to 127.0.0.1 could drive an agent holding the owning user's filesystem and shell access — on a
//! shared box, an RDP host or a terminal server, that is a cross-user privilege escalation.
//!
//! # What is asserted, and why it is not a proxy
//!
//! The tempting assertion is "a wrong token produces an error event". That is true on a build where
//! the check does nothing *and the request also ran*, because a `Shutdown` answers with events
//! either way. So the wrong-token case here sends **`Shutdown`** — the one request with an
//! observable side effect — and then proves the daemon is **still serving** afterwards. If the
//! refusal happened after dispatch, or not at all, the daemon would be gone and the second half
//! could not answer.
//!
//! The negative control is the other half of the same test: the identical exchange with the
//! **correct** token must produce a `Status`. Without it, a daemon that refused everything would
//! pass.

#[path = "../../marlowe-loop/tests/common/exclusive.rs"]
mod exclusive;
use exclusive::exclusive;

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use marlowe_daemon::protocol::Event;
use marlowe_daemon::{auth, Client, Daemon, DaemonConfig};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Fixture {
    root: PathBuf,
    port: u16,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Fixture {
    fn start(name: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-auth-wire-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let port = free_port();

        let mut config = DaemonConfig::new(root.join("profile"), workspace);
        config.port = port;
        let daemon = Daemon::open(config).expect("the daemon opens");
        let shutdown = daemon.shutdown_handle();
        thread::spawn(move || {
            let _ = daemon.serve();
        });

        let fx = Self { root, port, shutdown };
        fx.wait_until_listening();
        fx
    }

    fn profile(&self) -> PathBuf {
        self.root.join("profile")
    }

    fn client(&self) -> Client {
        Client::new("t").with_port(self.port).with_profile_root(self.profile())
    }

    fn wait_until_listening(&self) {
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the daemon never bound {}", self.port);
    }

    /// One exchange at the byte level, so the test controls exactly what is offered.
    fn raw(&self, preamble: &str, request: &str) -> Vec<String> {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("the daemon accepts");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        write!(stream, "{preamble}\n{request}\n").expect("write");
        stream.flush().expect("flush");
        let reader = BufReader::new(stream);
        reader.lines().map_while(Result::ok).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The whole finding, in one test.
///
/// **`Shutdown` is the request under refusal on purpose.** Any other request's refusal is
/// indistinguishable from its failure; this one's is not, because a dispatched `Shutdown` stops the
/// daemon and a refused one does not. The assertion that the daemon still answers afterwards is
/// what makes this a statement about *dispatch* rather than about *events*.
#[test]
fn a_wrong_token_is_refused_before_the_request_is_dispatched() {
    let fx = Fixture::start("wrong");

    let lines = fx.raw(
        &"0".repeat(64), // Well-formed, correct length, wrong value.
        r#"{"op":"shutdown"}"#,
    );

    assert_eq!(lines.len(), 1, "a refusal is one line and then a close: {lines:?}");
    assert!(
        lines[0].contains(auth::REFUSED),
        "the refusal must be recognisable to the client: {}",
        lines[0]
    );

    // **The daemon is still here.** If the check ran after dispatch — or not at all — the shutdown
    // above would have taken it, and this would fail. Nothing else in this file can tell those
    // cases apart.
    let events = fx.client().status().expect("the daemon is still serving");
    assert!(
        events.iter().any(|e| matches!(e, Event::Status(_))),
        "the refused shutdown must not have run: {events:?}"
    );
}

/// The negative control. Without it, a daemon that refused every connection would pass above.
#[test]
fn the_right_token_is_served() {
    let fx = Fixture::start("right");
    let token = auth::read_token(&fx.profile()).expect("the daemon minted one");
    let lines = fx.raw(&token, r#"{"op":"status"}"#);

    assert!(
        lines.iter().any(|l| l.contains("\"status\"")),
        "the correct token must be served: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(auth::REFUSED)),
        "a served connection must not also be refused: {lines:?}"
    );
}

/// Every near miss is a refusal, and each of these is a distinct way to get it wrong.
#[test]
fn an_empty_a_truncated_and_a_one_character_wrong_token_are_all_refused() {
    let fx = Fixture::start("nearmiss");
    let token = auth::read_token(&fx.profile()).expect("minted");

    let mut wrong = token.clone();
    wrong.pop();
    wrong.push(if token.ends_with('a') { 'b' } else { 'a' });

    for (name, offered) in [
        ("empty", String::new()),
        ("truncated", token[..32].to_string()),
        ("one character out", wrong),
        ("the request itself", r#"{"op":"status"}"#.to_string()),
    ] {
        let lines = fx.raw(&offered, r#"{"op":"status"}"#);
        assert!(
            lines.iter().any(|l| l.contains(auth::REFUSED)),
            "{name} must be refused, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("\"status\"")),
            "{name} must not be served: {lines:?}"
        );
    }
}

/// A client pointed at another profile's daemon is refused, and the error says which failure it is.
///
/// This is the path a real user hits: two profiles, one port. It must not read as "the daemon is
/// down" — that sends them to restart something that is already running.
#[test]
fn a_client_from_another_profile_gets_a_named_refusal_not_a_silence() {
    let fx = Fixture::start("otherprofile");
    let other = fx.root.join("someone-elses-profile");
    auth::ensure_token(&other).expect("mint a second profile's token");

    let stranger = Client::new("t").with_port(fx.port).with_profile_root(&other);
    let err = stranger.status().expect_err("another profile's token must not be served");

    assert!(
        matches!(err, marlowe_daemon::ClientError::Refused { .. }),
        "a refusal must be typed as one, not as a closed connection: {err}"
    );
    let text = err.to_string();
    assert!(text.contains("refused"), "{text}");
    assert!(
        !text.contains("no daemon"),
        "a refusal must not read as an absent daemon — that sends the user to restart a daemon \
         that is already running: {text}"
    );

    // And the real client still works, so the refusal is about the token and not about the port.
    assert!(fx.client().status().is_ok());
}

/// A peer that connects and says nothing must not hold the daemon.
///
/// `serve` is serial — one connection at a time — so an unbounded read on the preamble would let
/// any local process wedge every other client by opening a socket and waiting. This is the read a
/// port scanner reaches first.
#[test]
fn a_silent_peer_does_not_wedge_the_daemon() {
    // **Ports are a machine resource and cargo runs test binaries concurrently.** This test
    // needs specific adjacent ports and no other daemon competing for them; it passed alone
    // and failed under `--workspace` until this. See `common/exclusive.rs`.
    let _ports = exclusive("daemon-ports");
    let fx = Fixture::start("silent");

    let idle = TcpStream::connect(("127.0.0.1", fx.port)).expect("accepted");
    // Held open, saying nothing, for longer than a client would wait.
    let holder = thread::spawn(move || {
        // **10, not 7.** See the assertion below: the holder must be irrelevant to the outcome,
        // and it cannot be if it lets go anywhere near the deadline being tested. 10 puts its
        // release 4 s clear of the 6 s threshold while keeping the join -- and so the suite -- cheap.
        thread::sleep(Duration::from_secs(10));
        drop(idle);
    });

    // The preamble deadline is 5 s, so this must be answered well before the holder lets go.
    let started = std::time::Instant::now();
    let events = fx.client().status().expect("the daemon is still reachable");
    assert!(
        events.iter().any(|e| matches!(e, Event::Status(_))),
        "a silent peer held the daemon: {events:?}"
    );
    // **6 s, and the number is the whole point of this assertion.**
    //
    // It was 7 s, which is exactly how long the holder sleeps — so "the 5 s preamble deadline
    // freed the daemon" and "the holder let go and freed it" were indistinguishable at the
    // boundary, and a daemon that only ever unwedged because the peer left would have passed at
    // 6.99 s. The threshold has to sit strictly BETWEEN the mechanism under test and the
    // alternative explanation, or passing proves nothing about the mechanism.
    //
    // Observed 2026-08-30 failing at 7.067 s inside a `--workspace` run and passing 3/3 in
    // isolation at ~7.6 s total — a flake produced by a threshold with no margin, not by the
    // daemon. With the holder at 10 s the window is 5 → 6 s and the peer cannot decide the result.
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the silent peer was still holding the daemon after {:?} — the preamble deadline is 5 s,          so anything past 6 s means the deadline did not free it",
        started.elapsed()
    );
    holder.join().ok();
}
