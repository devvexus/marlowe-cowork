//! The event bus: numbers broadcast events, keeps a replay ring, fans out to client queues.
//!
//! Publishing never blocks. Each client has a bounded queue; a client whose queue is full is
//! detached and its socket shut down.

use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use marlowe_proto::{Envelope, Event};

use crate::{lock, now_ms};

/// One serialized envelope (no trailing newline).
pub(crate) type Line = Arc<str>;

/// The bus's handle on one connected client.
pub(crate) struct ClientLink {
    pub id: u64,
    pub tx: Sender<Line>,
    /// Set when the client is dropped; the writer and reader stop at their next step.
    pub dropped: Arc<AtomicBool>,
    /// A clone of the socket, shut down when the client is dropped.
    pub sock: Option<TcpStream>,
}

impl ClientLink {
    fn kill(&self) {
        self.dropped.store(true, Ordering::SeqCst);
        if let Some(s) = &self.sock {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

struct Inner {
    seq: u64,
    ring: VecDeque<(u64, Line)>,
    clients: HashMap<u64, ClientLink>,
}

pub(crate) struct Bus {
    inner: Mutex<Inner>,
    ring_cap: usize,
}

fn encode(seq: u64, event: Event) -> Line {
    let env = Envelope { seq, at_ms: now_ms(), event };
    serde_json::to_string(&env)
        .unwrap_or_else(|e| {
            let fallback = Envelope {
                seq,
                at_ms: now_ms(),
                event: Event::Error { session: None, message: format!("unencodable event: {e}") },
            };
            serde_json::to_string(&fallback).unwrap_or_default()
        })
        .into()
}

impl Bus {
    pub fn new(ring_cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { seq: 0, ring: VecDeque::new(), clients: HashMap::new() }),
            ring_cap: ring_cap.max(1),
        }
    }

    /// Broadcasts `event` with the next seq and returns that seq.
    pub fn publish(&self, event: Event) -> u64 {
        let mut g = lock(&self.inner);
        g.seq += 1;
        let seq = g.seq;
        let line = encode(seq, event);
        g.ring.push_back((seq, line.clone()));
        while g.ring.len() > self.ring_cap {
            g.ring.pop_front();
        }
        let slow: Vec<u64> = g
            .clients
            .values()
            .filter(|c| c.tx.try_send(line.clone()).is_err())
            .map(|c| c.id)
            .collect();
        for id in slow {
            if let Some(c) = g.clients.remove(&id) {
                c.kill();
            }
        }
        seq
    }

    /// Broadcasts a `seq: 0` event that is not kept in the replay ring and does not advance the
    /// seq (live `Phase` progress). A client whose queue is full misses it; unlike `publish` this
    /// never drops a client, so a progress flood cannot cost anyone their connection.
    pub fn publish_ephemeral(&self, event: Event) {
        let line = encode(0, event);
        let g = lock(&self.inner);
        for c in g.clients.values() {
            let _ = c.tx.try_send(line.clone());
        }
    }

    /// Registers a client: queues the `Welcome` built from the current seq, then the replay of
    /// buffered events after `since`, atomically with respect to `publish`. On a replay gap only
    /// the Welcome and a gap `Error` are sent (the client reloads instead of replaying). Returns
    /// false if the client could not take the replay (it is not registered). The client's queue
    /// must hold at least `ring_cap + 1` lines for a full replay to fit.
    pub fn attach(&self, link: ClientLink, welcome: impl FnOnce(u64) -> Event, since: Option<u64>) -> bool {
        let mut g = lock(&self.inner);
        let mut ok = link.tx.try_send(encode(0, welcome(g.seq))).is_ok();
        if let (true, Some(since)) = (ok, since) {
            let oldest = g.ring.front().map(|(s, _)| *s).unwrap_or(g.seq + 1);
            if since + 1 < oldest && since < g.seq {
                let msg = format!("replay gap: events {}..{} are no longer buffered", since + 1, oldest - 1);
                ok = link.tx.try_send(encode(0, Event::Error { session: None, message: msg })).is_ok();
            } else {
                for (s, line) in g.ring.iter() {
                    if !ok {
                        break;
                    }
                    if *s > since {
                        ok = link.tx.try_send(line.clone()).is_ok();
                    }
                }
            }
        }
        if ok {
            g.clients.insert(link.id, link);
        } else {
            link.kill();
        }
        ok
    }

    /// Sends a `seq: 0` reply to one client. A full queue drops the client.
    pub fn reply(&self, client: u64, event: Event) {
        let mut g = lock(&self.inner);
        let full = match g.clients.get(&client) {
            Some(c) => c.tx.try_send(encode(0, event)).is_err(),
            None => false,
        };
        if full {
            if let Some(c) = g.clients.remove(&client) {
                c.kill();
            }
        }
    }

    pub fn detach(&self, client: u64) {
        lock(&self.inner).clients.remove(&client);
    }

    pub fn is_attached(&self, client: u64) -> bool {
        lock(&self.inner).clients.contains_key(&client)
    }

    pub fn client_count(&self) -> usize {
        lock(&self.inner).clients.len()
    }

    pub fn seq(&self) -> u64 {
        lock(&self.inner).seq
    }

    /// Smallest client queue that can take a full replay plus the Welcome and a gap error.
    pub fn min_client_queue(&self) -> usize {
        self.ring_cap + 2
    }

    /// Detaches every client without killing it: writers drain their queues, then close.
    pub fn close_all(&self) {
        lock(&self.inner).clients.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn link(id: u64, cap: usize) -> (ClientLink, crossbeam_channel::Receiver<Line>) {
        let (tx, rx) = bounded(cap);
        (ClientLink { id, tx, dropped: Arc::new(AtomicBool::new(false)), sock: None }, rx)
    }

    fn seq_of(line: &Line) -> u64 {
        serde_json::from_str::<Envelope>(line).unwrap().seq
    }

    #[test]
    fn replay_follows_welcome_and_skips_seen_events() {
        let bus = Bus::new(4);
        for _ in 0..6 {
            bus.publish(Event::ShuttingDown);
        }
        let (l, rx) = link(1, 16);
        assert!(bus.attach(l, |seq| Event::Error { session: None, message: seq.to_string() }, Some(3)));
        let got: Vec<u64> = rx.try_iter().map(|l| seq_of(&l)).collect();
        assert_eq!(got, vec![0, 4, 5, 6]);
        bus.publish(Event::ShuttingDown);
        assert_eq!(seq_of(&rx.try_recv().unwrap()), 7);
    }

    #[test]
    fn a_replay_gap_is_reported() {
        let bus = Bus::new(2);
        for _ in 0..5 {
            bus.publish(Event::ShuttingDown);
        }
        let (l, rx) = link(1, 16);
        assert!(bus.attach(l, |_| Event::ShuttingDown, Some(0)));
        let lines: Vec<Line> = rx.try_iter().collect();
        assert_eq!(lines.len(), 2, "welcome and the gap error; no partial replay");
        assert!(lines[1].contains("replay gap"));
        bus.publish(Event::ShuttingDown);
        assert_eq!(seq_of(&rx.try_recv().unwrap()), 6);
    }

    #[test]
    fn a_full_ring_replays_into_a_queue_of_the_minimum_size() {
        let ring = 8;
        let bus = Bus::new(ring);
        for _ in 0..20 {
            bus.publish(Event::ShuttingDown);
        }
        // Gap: since is far behind the ring.
        let (l, rx) = link(1, bus.min_client_queue());
        assert!(bus.attach(l, |_| Event::ShuttingDown, Some(3)));
        assert_eq!(rx.try_iter().count(), 2);
        // No gap, whole ring replayed: since == oldest - 1.
        let (l, rx) = link(2, bus.min_client_queue());
        assert!(bus.attach(l, |_| Event::ShuttingDown, Some(12)));
        assert_eq!(rx.try_iter().count(), 1 + ring);
    }

    #[test]
    fn ephemeral_events_skip_the_ring_and_the_seq_and_never_drop_a_client() {
        let bus = Bus::new(4);
        let (slow, slow_rx) = link(1, 2);
        let dropped = slow.dropped.clone();
        assert!(bus.attach(slow, |_| Event::ShuttingDown, None));
        bus.publish(Event::ShuttingDown);
        for _ in 0..50 {
            bus.publish_ephemeral(Event::Phase(marlowe_proto::PhaseEvent::progress(marlowe_proto::PhaseKind::Recall, 1, 2)));
        }
        assert!(!dropped.load(Ordering::SeqCst), "a progress flood must not detach the client");
        assert_eq!(bus.client_count(), 1);
        assert_eq!(bus.seq(), 1, "ephemeral events do not advance the seq");
        let seqs: Vec<u64> = slow_rx.try_iter().map(|l| seq_of(&l)).collect();
        assert_eq!(seqs[0], 0, "welcome");
        // A late joiner replays only the ring: the one numbered event, no progress.
        let (late, late_rx) = link(2, 16);
        assert!(bus.attach(late, |_| Event::ShuttingDown, Some(0)));
        let got: Vec<u64> = late_rx.try_iter().map(|l| seq_of(&l)).collect();
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn a_full_queue_drops_only_that_client() {
        let bus = Bus::new(8);
        let (slow, _slow_rx) = link(1, 2);
        let dropped = slow.dropped.clone();
        let (fast, fast_rx) = link(2, 64);
        assert!(bus.attach(slow, |_| Event::ShuttingDown, None));
        assert!(bus.attach(fast, |_| Event::ShuttingDown, None));
        for _ in 0..5 {
            bus.publish(Event::ShuttingDown);
        }
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(bus.client_count(), 1);
        assert_eq!(fast_rx.try_iter().count(), 6);
    }
}
