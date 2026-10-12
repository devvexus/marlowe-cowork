//! JSON-RPC 2.0 over any [`Wire`]: request/response correlation, server-initiated requests and
//! notifications, timeouts and cancellation. The wires are `stdio` (a child process) and `http`
//! (streamable HTTP); both feed one inbound channel and send through [`Wire::send`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use serde_json::{json, Value};

use crate::http_wire::{NEEDS_AUTH_CODE, SESSION_GONE_CODE};
use crate::McpError;

/// One transport. Sends are non-blocking in the sense that a slow reply never holds `send`.
pub trait Wire: Send + Sync {
    /// Sends one JSON-RPC message (request, notification or response).
    fn send(&self, msg: &Value) -> Result<(), McpError>;
    /// Stops the transport (kills the child, ends streams). Idempotent.
    fn close(&self);
    /// Why the transport ended, when it did (a child's exit status and stderr tail).
    fn exit_note(&self) -> Option<String> {
        None
    }
    /// The protocol version `initialize` settled on (HTTP sends it as a header from then on).
    fn set_protocol(&self, _version: &str) {}
    /// `initialized` was sent: the server may now push (HTTP opens its GET stream).
    fn started(&self) {}
}

/// An error a server-initiated request answers with.
#[derive(Debug, Clone)]
pub struct RpcFailure {
    pub code: i64,
    pub message: String,
}

impl RpcFailure {
    pub fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("method not found: {method}") }
    }
}

/// What the client answers to the server's own requests and notifications.
pub trait Inbound: Send + Sync {
    fn notification(&self, method: &str, params: &Value);
    fn request(&self, method: &str, params: &Value) -> Result<Value, RpcFailure>;
}

type Reply = Result<Value, McpError>;

struct Shared {
    server: String,
    wire: Box<dyn Wire>,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, Sender<Reply>>>,
    closed: AtomicBool,
}

/// A live JSON-RPC peer. Cheap to clone.
#[derive(Clone)]
pub struct Peer {
    shared: Arc<Shared>,
}

impl Peer {
    /// Starts the dispatcher over `inbound` (everything the wire reads).
    pub fn start(server: &str, wire: Box<dyn Wire>, inbound: Receiver<Value>, handler: Arc<dyn Inbound>) -> Peer {
        let shared = Arc::new(Shared {
            server: server.to_string(),
            wire,
            next: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name(format!("mcp-{server}"))
            .spawn(move || dispatch(&s, &inbound, handler.as_ref()))
            .ok();
        Peer { shared }
    }

    pub fn server(&self) -> &str {
        &self.shared.server
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    pub fn exit_note(&self) -> Option<String> {
        self.shared.wire.exit_note()
    }

    /// Sends a request and waits up to `timeout`; `cancelled` is polled every 50 ms. A timeout or
    /// cancel sends `notifications/cancelled` for the request.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Value, McpError> {
        let s = &self.shared;
        if self.is_closed() {
            return Err(self.closed_error());
        }
        let id = s.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = bounded::<Reply>(1);
        lock(&s.pending).insert(id, tx);
        let sent = s.wire.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if let Err(e) = sent {
            lock(&s.pending).remove(&id);
            return Err(e);
        }
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                lock(&s.pending).remove(&id);
                self.cancel_remote(id, "timed out");
                return Err(McpError::Timeout { server: s.server.clone(), method: method.to_string(), secs: timeout.as_secs() });
            }
            match rx.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        lock(&s.pending).remove(&id);
                        self.cancel_remote(id, "cancelled");
                        return Err(McpError::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.closed_error()),
            }
        }
    }

    pub fn set_protocol(&self, version: &str) {
        self.shared.wire.set_protocol(version);
    }

    pub fn started(&self) {
        self.shared.wire.started();
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.shared.wire.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn cancel_remote(&self, id: i64, reason: &str) {
        let _ = self.notify("notifications/cancelled", json!({"requestId": id, "reason": reason}));
    }

    fn closed_error(&self) -> McpError {
        McpError::Closed {
            server: self.shared.server.clone(),
            detail: self.shared.wire.exit_note().unwrap_or_else(|| "the connection closed".into()),
        }
    }

    /// Closes the transport and fails every waiting request.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.wire.close();
        fail_all(&self.shared);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn fail_all(s: &Shared) {
    let waiting: Vec<_> = lock(&s.pending).drain().collect();
    for (_, tx) in waiting {
        let _ = tx.try_send(Err(McpError::Closed {
            server: s.server.clone(),
            detail: s.wire.exit_note().unwrap_or_else(|| "the connection closed".into()),
        }));
    }
}

fn dispatch(s: &Arc<Shared>, inbound: &Receiver<Value>, handler: &dyn Inbound) {
    while let Ok(msg) = inbound.recv() {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.request(method, &params))) {
                    Ok(r) => r,
                    Err(_) => Err(RpcFailure { code: -32603, message: "internal error".into() }),
                };
                let body = match reply {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(f) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": f.code, "message": f.message}}),
                };
                let _ = s.wire.send(&body);
            }
            (Some(method), None) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.notification(method, &params)));
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64().or_else(|| id.as_str().and_then(|t| t.parse().ok())) else { continue };
                let Some(tx) = lock(&s.pending).remove(&id) else { continue };
                let reply = match msg.get("error") {
                    Some(e) => {
                        let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let message = e.get("message").and_then(Value::as_str).unwrap_or("no message").to_string();
                        Err(match code {
                            NEEDS_AUTH_CODE => McpError::NeedsAuth { server: s.server.clone(), detail: message },
                            SESSION_GONE_CODE => McpError::Closed { server: s.server.clone(), detail: message },
                            _ => McpError::Remote { server: s.server.clone(), code, message },
                        })
                    }
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.try_send(reply);
            }
            (None, None) => {}
        }
    }
    s.closed.store(true, Ordering::SeqCst);
    fail_all(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    struct Loop {
        out: Mutex<Option<Sender<Value>>>,
        sent: Mutex<Vec<Value>>,
        echo: bool,
    }
    impl Loop {
        /// The reader thread of a real wire ends like this: its sender goes away.
        fn hangup(&self) {
            self.out.lock().unwrap().take();
        }
    }
    impl Wire for Arc<Loop> {
        fn send(&self, msg: &Value) -> Result<(), McpError> {
            self.sent.lock().unwrap().push(msg.clone());
            if self.echo {
                if let (Some(id), Some(m), Some(out)) = (msg.get("id"), msg.get("method"), &*self.out.lock().unwrap()) {
                    let _ = out.send(json!({"jsonrpc": "2.0", "id": id, "result": {"echo": m}}));
                }
            }
            Ok(())
        }
        fn close(&self) {}
    }

    struct Quiet;
    impl Inbound for Quiet {
        fn notification(&self, _: &str, _: &Value) {}
        fn request(&self, method: &str, _: &Value) -> Result<Value, RpcFailure> {
            if method == "ping" {
                Ok(json!({}))
            } else {
                Err(RpcFailure::method_not_found(method))
            }
        }
    }

    fn peer(echo: bool) -> (Peer, Arc<Loop>, Sender<Value>) {
        let (tx, rx) = unbounded();
        let wire = Arc::new(Loop { out: Mutex::new(Some(tx.clone())), sent: Mutex::new(Vec::new()), echo });
        (Peer::start("t", Box::new(wire.clone()), rx, Arc::new(Quiet)), wire, tx)
    }

    #[test]
    fn a_reply_is_matched_by_id_and_interleaved_notifications_are_skipped() {
        let (p, _w, tx) = peer(true);
        tx.send(json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}})).unwrap();
        let r = p.request("tools/list", json!({}), Duration::from_secs(5), &|| false).unwrap();
        assert_eq!(r["echo"], "tools/list");
    }

    #[test]
    fn an_error_reply_names_the_server_and_code() {
        let (p, _w, tx) = peer(false);
        let t = std::thread::spawn({
            let p = p.clone();
            move || p.request("x", json!({}), Duration::from_secs(5), &|| false)
        });
        std::thread::sleep(Duration::from_millis(100));
        tx.send(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "boom"}})).unwrap();
        match t.join().unwrap() {
            Err(McpError::Remote { server, code, message }) => assert_eq!((server.as_str(), code, message.as_str()), ("t", -32000, "boom")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_silent_server_times_out_and_the_request_is_cancelled_remotely() {
        let (p, w, _tx) = peer(false);
        let e = p.request("slow", json!({}), Duration::from_millis(120), &|| false).unwrap_err();
        assert!(matches!(e, McpError::Timeout { .. }), "{e:?}");
        let sent = w.sent.lock().unwrap();
        assert!(sent.iter().any(|m| m["method"] == "notifications/cancelled" && m["params"]["requestId"] == 1), "{sent:?}");
    }

    #[test]
    fn cancelling_returns_promptly() {
        let (p, _w, _tx) = peer(false);
        let flag = Arc::new(AtomicBool::new(false));
        let f = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            f.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let e = p.request("slow", json!({}), Duration::from_secs(30), &|| flag.load(Ordering::SeqCst)).unwrap_err();
        assert!(matches!(e, McpError::Cancelled), "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn server_requests_are_answered_and_unknown_ones_fail_cleanly() {
        let (_p, w, tx) = peer(false);
        tx.send(json!({"jsonrpc": "2.0", "id": 7, "method": "ping"})).unwrap();
        tx.send(json!({"jsonrpc": "2.0", "id": 8, "method": "sampling/createMessage"})).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let sent = w.sent.lock().unwrap();
        assert!(sent.iter().any(|m| m["id"] == 7 && m["result"] == json!({})), "{sent:?}");
        assert!(sent.iter().any(|m| m["id"] == 8 && m["error"]["code"] == -32601), "{sent:?}");
    }

    #[test]
    fn a_dead_wire_fails_waiting_requests_instead_of_hanging() {
        let (p, _w, tx) = peer(false);
        let t = std::thread::spawn({
            let p = p.clone();
            move || p.request("x", json!({}), Duration::from_secs(30), &|| false)
        });
        std::thread::sleep(Duration::from_millis(100));
        _w.hangup();
        drop(tx);
        let e = t.join().unwrap().unwrap_err();
        assert!(matches!(e, McpError::Closed { .. }), "{e:?}");
        assert!(p.is_closed());
    }
}
