//! **The approval exchange, both ends, over a real socket.**
//!
//! `daemon.rs`'s `SocketApprovals` tests prove the gate asks and fails closed. They say nothing
//! about whether any client answers — and a gate nothing crosses is the shape CLAUDE.md keeps
//! recording: a mechanism with green tests and no traffic.
//!
//! This drives `Client::ask_streaming_approving` against a **fake daemon** that speaks the real
//! wire format: it reads the request, sends an `Event::Approval`, and reads the reply. What it
//! does not do is run a model, so this is not the live end-to-end run — that still has to be
//! observed on a real turn. What it does prove is that the two halves agree about the protocol.

use marlowe_daemon::protocol::{Event, Request};
use marlowe_daemon::Client;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};

/// A daemon that asks once, records the answer, and finishes the turn.
fn fake_daemon(decision: u64) -> (u16, std::sync::mpsc::Receiver<Option<Request>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);

        // **The auth preamble comes first.** A fake that skipped it would read the token AS the
        // request and this file's whole claim — "the two halves agree about the protocol" — would
        // be false in the one place it is asserted. The fake does not check the value; whether a
        // wrong token is refused is `socket_auth.rs`'s subject, not this one's.
        let mut preamble = String::new();
        reader.read_line(&mut preamble).expect("preamble");
        assert!(
            !preamble.trim().starts_with('{'),
            "the client must offer a token line before its request, and this looks like the \
             request: {preamble}"
        );

        // The client's request.
        let mut line = String::new();
        reader.read_line(&mut line).expect("request");

        // §B9's prompt, exactly as `SocketApprovals` sends it.
        let prompt = Event::Approval {
            decision,
            verb: "web".into(),
            scope: "https://example.com/".into(),
            reversible: true,
            novelty: None,
        };
        writeln!(writer, "{}", serde_json::to_string(&prompt).expect("json")).expect("write");
        writer.flush().expect("flush");

        // **A bounded wait, because "no answer" is one of the outcomes under test.**
        //
        // The render-only case (decision 0) is *supposed* to produce no reply. Without a timeout
        // this thread blocks on a read that will never complete while the client blocks waiting
        // for the turn to finish — a deadlock in the test, which is a worse way to express "the
        // client did not answer" than simply not hearing one.
        reader
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_millis(750)))
            .expect("timeout");
        let mut reply = String::new();
        let got = match reader.read_line(&mut reply) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str::<Request>(reply.trim()).ok(),
        };
        let _ = tx.send(got);

        let done = Event::Done {
            outcome: "ok".into(),
            detail: String::new(),
            spend_micros_usd: 0,
            elapsed_ms: 0,
        };
        let _ = writeln!(writer, "{}", serde_json::to_string(&done).expect("json"));
        let _ = writer.flush();
    });

    (port, rx)
}

#[test]
fn the_client_answers_the_prompt_on_the_same_connection_and_names_the_decision() {
    let (port, rx) = fake_daemon(7);
    let client = Client::new("test").with_port(port);

    let mut shown = Vec::new();
    let _ = client.ask_streaming_approving(
        "fetch something",
        &mut |_event| (true, None),
        &mut |event| shown.push(event),
    );

    let reply = rx.recv().expect("the daemon read a reply");
    assert_eq!(
        reply,
        Some(Request::Approve { decision: 7, granted: true, reason: None }),
        "the answer must name the decision it answers — a reply that did not could approve \
         whatever happens to be pending next"
    );

    // **The human saw it before it was answered.** §B9's whole point is the blast radius reaching
    // a person; a client that replied without surfacing the prompt would satisfy the protocol and
    // defeat the requirement.
    assert!(
        shown.iter().any(|e| matches!(e, Event::Approval { scope, .. } if scope.contains("example.com"))),
        "the prompt must be surfaced to the caller, not silently answered: {shown:?}"
    );
}

#[test]
fn a_declined_prompt_is_sent_as_declined_rather_than_dropped() {
    let (port, rx) = fake_daemon(1);
    let client = Client::new("test").with_port(port);
    let _ = client.ask_streaming_approving("fetch something", &mut |_| (false, Some("not this host".to_string())), &mut |_| {});

    assert_eq!(
        rx.recv().expect("the daemon read a reply"),
        Some(Request::Approve {
            decision: 1,
            granted: false,
            reason: Some("not this host".to_string()),
        }),
        "a decline is an answer and must be sent — silence would leave the daemon blocked until \
         its read timed out"
    );
}

/// **`decision: 0` is the loop's render-only announcement and must NOT be answered.**
///
/// Two replies to one question puts a spare answer on the wire, and the daemon reads it as the
/// answer to whatever it asks next — approving something nobody was shown.
#[test]
fn the_render_only_prompt_is_not_answered() {
    let (port, rx) = fake_daemon(0);
    let client = Client::new("test").with_port(port);
    let mut shown = Vec::new();
    let _ = client.ask_streaming_approving("go", &mut |_| (true, None), &mut |e| shown.push(e));

    assert_eq!(
        rx.recv().expect("the daemon finished reading"),
        None,
        "decision 0 announces that the loop is about to ask; answering it would leave a spare \
         approval on the wire for the next question"
    );
    assert!(
        shown.iter().any(|e| matches!(e, Event::Approval { .. })),
        "it is still shown — it is render-only, not invisible"
    );
}
