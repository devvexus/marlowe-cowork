//! The daemon connection: one thread that connects, says Hello with the token, forwards every
//! envelope, and reconnects with `since_seq` so the daemon replays what was missed.

use std::io::{BufReader, BufWriter};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, select, unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_proto::{write_line, Envelope, Event, LineReader, Request};

use crate::state::ClientEvent;
use crate::ConnectInfo;

/// Finds the daemon to connect to. Called before every attempt, so a restarted daemon (new port,
/// new token) is picked up.
pub type Resolver = Box<dyn FnMut() -> Option<ConnectInfo> + Send>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WELCOME_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MS: [u64; 6] = [150, 300, 600, 1_200, 2_400, 4_000];

/// A handle to the connection thread. Dropping it ends the thread.
pub struct Client {
    requests: Sender<Request>,
    events: Receiver<ClientEvent>,
    last_seq: Arc<AtomicU64>,
}

impl Client {
    /// Starts connecting in the background.
    pub fn spawn(resolve: Resolver) -> Client {
        let (req_tx, req_rx) = unbounded::<Request>();
        let (ev_tx, ev_rx) = unbounded::<ClientEvent>();
        let last_seq = Arc::new(AtomicU64::new(0));
        let seq = last_seq.clone();
        thread::Builder::new()
            .name("marlowe-tui-conn".into())
            .spawn(move || connection(resolve, req_rx, ev_tx, seq))
            .expect("spawn connection thread");
        Client { requests: req_tx, events: ev_rx, last_seq }
    }

    /// Queues a request. Requests sent while offline are dropped.
    pub fn send(&self, r: Request) {
        let _ = self.requests.send(r);
    }

    pub fn events(&self) -> &Receiver<ClientEvent> {
        &self.events
    }

    /// Highest broadcast seq received.
    pub fn last_seq(&self) -> u64 {
        self.last_seq.load(Ordering::SeqCst)
    }
}

fn connection(
    mut resolve: Resolver,
    requests: Receiver<Request>,
    events: Sender<ClientEvent>,
    last_seq: Arc<AtomicU64>,
) {
    let mut attempt: u32 = 0;
    loop {
        let reason = match resolve() {
            None => "no daemon is running (daemon.json not found)".to_string(),
            Some(info) => match session(&info, &requests, &events, &last_seq) {
                Outcome::Closed => return,
                Outcome::Refused(msg) => {
                    let _ = events.send(ClientEvent::Refused(msg));
                    attempt = attempt.saturating_add(1);
                    if !wait(&requests, BACKOFF_MS[BACKOFF_MS.len() - 1]) {
                        return;
                    }
                    continue;
                }
                Outcome::Lost { reason, was_online } => {
                    if was_online {
                        attempt = 0;
                    }
                    reason
                }
            },
        };
        attempt = attempt.saturating_add(1);
        if events.send(ClientEvent::Disconnected { reason, attempt }).is_err() {
            return;
        }
        let delay = BACKOFF_MS[(attempt as usize - 1).min(BACKOFF_MS.len() - 1)];
        if !wait(&requests, delay) {
            return;
        }
    }
}

/// Sleeps for `ms`, discarding requests made while offline. False when the client is gone.
fn wait(requests: &Receiver<Request>, ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        match requests.recv_timeout(left) {
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

enum Outcome {
    /// The client handle was dropped.
    Closed,
    Refused(String),
    Lost { reason: String, was_online: bool },
}

fn session(
    info: &ConnectInfo,
    requests: &Receiver<Request>,
    events: &Sender<ClientEvent>,
    last_seq: &Arc<AtomicU64>,
) -> Outcome {
    let lost = |reason: String, was_online| Outcome::Lost { reason, was_online };
    let stream = match TcpStream::connect_timeout(&info.addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => return lost(format!("cannot reach the daemon at {}: {e}", info.addr), false),
    };
    let _ = stream.set_nodelay(true);
    let Ok(read_half) = stream.try_clone() else {
        return lost("socket clone failed".into(), false);
    };
    let mut writer = BufWriter::new(stream);
    let since = last_seq.load(Ordering::SeqCst);
    let hello = Request::Hello {
        token: info.token.clone(),
        client: "tui".into(),
        since_seq: (since > 0).then_some(since),
    };
    if let Err(e) = write_line(&mut writer, &hello) {
        return lost(format!("handshake failed: {e}"), false);
    }

    // The first line must be Welcome; anything else is a refusal.
    let _ = read_half.set_read_timeout(Some(WELCOME_TIMEOUT));
    let mut reader = LineReader::new(BufReader::new(read_half));
    let first: Envelope = match reader.next::<Envelope>() {
        Ok(Some(env)) => env,
        Ok(None) => return lost("the daemon closed the connection during the handshake".into(), false),
        Err(e) => return lost(format!("handshake failed: {e}"), false),
    };
    match &first.event {
        Event::Welcome { seq, .. } => {
            if *seq < last_seq.load(Ordering::SeqCst) {
                // A different daemon: its seqs start over.
                last_seq.store(*seq, Ordering::SeqCst);
            }
        }
        Event::Error { message, .. } => return Outcome::Refused(message.clone()),
        other => return Outcome::Refused(format!("unexpected first event: {other:?}")),
    }
    let _ = reader.get_ref().get_ref().set_read_timeout(None);
    if events.send(ClientEvent::Envelope(first)).is_err() {
        return Outcome::Closed;
    }

    // Reader: forwards envelopes; tells the writer loop when the stream ends.
    let (dead_tx, dead_rx) = bounded::<String>(1);
    let ev = events.clone();
    let seq = last_seq.clone();
    let spawned = thread::Builder::new().name("marlowe-tui-read".into()).spawn(move || {
        let reason = loop {
            match reader.next::<Envelope>() {
                Ok(Some(env)) => {
                    if env.seq != 0 {
                        seq.fetch_max(env.seq, Ordering::SeqCst);
                    }
                    if ev.send(ClientEvent::Envelope(env)).is_err() {
                        break "client closed".to_string();
                    }
                }
                Ok(None) => break "the daemon closed the connection".to_string(),
                Err(e) if e.is_fatal() => break format!("connection lost: {e}"),
                // A bad line is skipped; the stream stays usable.
                Err(_) => continue,
            }
        };
        let _ = dead_tx.send(reason);
    });
    if spawned.is_err() {
        return lost("could not start the reader thread".into(), true);
    }

    let reason = loop {
        select! {
            recv(requests) -> r => match r {
                Ok(req) => {
                    if let Err(e) = write_line(&mut writer, &req) {
                        break format!("connection lost: {e}");
                    }
                }
                Err(_) => {
                    let _ = writer.get_ref().shutdown(Shutdown::Both);
                    return Outcome::Closed;
                }
            },
            recv(dead_rx) -> r => break r.unwrap_or_else(|_| "connection lost".into()),
        }
    };
    let _ = writer.get_ref().shutdown(Shutdown::Both);
    lost(reason, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::SessionId;
    use std::io::BufRead;
    use std::net::TcpListener;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    fn env(seq: u64, event: Event) -> Envelope {
        Envelope { seq, at_ms: 0, event }
    }

    fn welcome(seq: u64) -> Envelope {
        env(0, Event::Welcome {
            daemon_version: "test".into(),
            proto_version: marlowe_proto::PROTO_VERSION,
            profile_home: "home".into(),
            seq,
        })
    }

    fn delta(seq: u64, text: &str) -> Envelope {
        env(seq, Event::TextDelta { session: SessionId::from("s"), turn: 1, text: text.into() })
    }

    fn next_env(c: &Client) -> Envelope {
        loop {
            match c.events().recv_timeout(Duration::from_secs(10)).expect("event") {
                ClientEvent::Envelope(e) => return e,
                ClientEvent::Disconnected { .. } => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn handshakes_forwards_and_replays_after_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resolve: Resolver = Box::new(move || Some(ConnectInfo { addr, token: TOKEN.into() }));
        let client = Client::spawn(resolve);

        // First connection: Hello with no since_seq, then two events, then the daemon drops us.
        let (s, _) = listener.accept().unwrap();
        let mut r = LineReader::new(std::io::BufReader::new(s.try_clone().unwrap()));
        let mut w = s;
        let hello: Request = r.next().unwrap().unwrap();
        assert_eq!(
            hello,
            Request::Hello { token: TOKEN.into(), client: "tui".into(), since_seq: None }
        );
        write_line(&mut w, &welcome(0)).unwrap();
        write_line(&mut w, &delta(1, "a")).unwrap();
        write_line(&mut w, &delta(2, "b")).unwrap();
        assert!(matches!(next_env(&client).event, Event::Welcome { .. }));
        assert_eq!(next_env(&client).seq, 1);
        assert_eq!(next_env(&client).seq, 2);

        // A request reaches the daemon.
        client.send(Request::Status);
        let got: Request = r.next().unwrap().unwrap();
        assert_eq!(got, Request::Status);
        drop(w);
        drop(r);

        // Reconnect: Hello now asks for a replay from seq 2.
        let (s, _) = listener.accept().unwrap();
        let mut r = LineReader::new(std::io::BufReader::new(s.try_clone().unwrap()));
        let mut w = s;
        let hello: Request = r.next().unwrap().unwrap();
        assert!(matches!(hello, Request::Hello { since_seq: Some(2), .. }), "{hello:?}");
        write_line(&mut w, &welcome(2)).unwrap();
        write_line(&mut w, &delta(3, "c")).unwrap();
        assert!(matches!(next_env(&client).event, Event::Welcome { .. }));
        assert_eq!(next_env(&client).seq, 3);
        assert_eq!(client.last_seq(), 3);
    }

    #[test]
    fn a_refusal_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Client::spawn(Box::new(move || Some(ConnectInfo { addr, token: "wrong".into() })));
        let (s, _) = listener.accept().unwrap();
        let mut r = std::io::BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let mut w = s;
        write_line(
            &mut w,
            &env(0, Event::Error { session: None, message: marlowe_proto::auth::REFUSED.into() }),
        )
        .unwrap();
        drop(w);
        match client.events().recv_timeout(Duration::from_secs(10)).unwrap() {
            ClientEvent::Refused(m) => assert_eq!(m, marlowe_proto::auth::REFUSED),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn no_daemon_reports_disconnected_and_keeps_trying() {
        let client = Client::spawn(Box::new(|| None));
        match client.events().recv_timeout(Duration::from_secs(10)).unwrap() {
            ClientEvent::Disconnected { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("{other:?}"),
        }
        match client.events().recv_timeout(Duration::from_secs(10)).unwrap() {
            ClientEvent::Disconnected { attempt, .. } => assert_eq!(attempt, 2),
            other => panic!("{other:?}"),
        }
    }
}
