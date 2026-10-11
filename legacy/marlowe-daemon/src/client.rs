//! The thin client. ARCHITECTURE §6.
//!
//! > **`marlowe`** — thin client. Holds no run state. Renders. Auto-spawns the daemon if absent.
//!
//! # It holds no run state, and that is checkable rather than asserted
//!
//! [`Client`] has three fields: an address, a session name, and a connect timeout. There is no
//! run table, no transcript, no checkpoint. `tests/split.rs` asserts the type's whole serialized
//! surface, because "the client is thin" is the kind of claim that stays true until someone adds
//! a cache.
//!
//! # Auto-spawn, and why it is not silent
//!
//! §5's zero-config first run means the user types `marlowe` and it works. So a client that finds
//! no daemon **starts one** — and says so, because a process appearing on a machine without the
//! user being told is exactly what a well-behaved tool does not do.

use std::io::{BufRead, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::{Event, Request};
use crate::DEFAULT_DAEMON_PORT;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("no daemon on 127.0.0.1:{port}: {detail}")]
    NoDaemon { port: u16, detail: String },
    #[error("the daemon closed the connection: {detail}")]
    Closed { detail: String },
    #[error("could not start a daemon: {detail}")]
    Spawn { detail: String },
    /// The daemon on this port belongs to a different profile — or a different user.
    #[error("the daemon on 127.0.0.1:{port} refused this connection: {detail}")]
    Refused { port: u16, detail: String },
}

/// Everything the client holds. **Four fields, none of them run state.**
///
/// The fourth is the profile root, which is not run state either: it is where the socket token
/// lives (see [`crate::auth`]). The client reads it, it never writes it, and it holds nothing
/// derived from it between calls.
#[derive(Clone)]
pub struct Client {
    port: u16,
    session: String,
    connect_timeout: Duration,
    profile_root: PathBuf,
}

/// The value is redacted, the FIELD NAMES are not.
///
/// `the_client_holds_no_run_state` scans this dump for `run`, `transcript`, `cache` and so on —
/// that assertion is about the client's *shape*, so it needs every field named. It does not need
/// the user's home directory in it, and a derived `Debug` would put it there: on a machine whose
/// user directory happened to contain one of those words the test would fail for a reason that has
/// nothing to do with the property. Redacting the path keeps the assertion about the type.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("port", &self.port)
            .field("session", &self.session)
            .field("connect_timeout", &self.connect_timeout)
            .field("profile_root", &"<redacted>")
            .finish()
    }
}

impl Client {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            port: DEFAULT_DAEMON_PORT,
            session: session.into(),
            // §B13 budgets 150 ms to FIRST FRAME, and §6 says the split is what buys it. A
            // 400 ms connect probe spent the whole budget discovering there was no daemon — the
            // first measured version of this did exactly that. Loopback either answers at once
            // or is not there, so the probe is short by design.
            connect_timeout: Duration::from_millis(40),
            profile_root: crate::default_profile_root(),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Point the client at a non-default profile — which is what decides **which token it offers**.
    ///
    /// A client aimed at a daemon serving another profile is refused rather than served, and the
    /// error says so. That is a default that makes a mismatch *observable*: the failing path still
    /// exists and it is named.
    pub fn with_profile_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.profile_root = root.into();
        self
    }

    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The bound the first frame is protected by. Exposed so a test can assert the **configured**
    /// property rather than time a connection: a wall-clock assertion would be flaky on a loaded
    /// machine, and a parallel build inflating it is exactly the failure the shared-checkout
    /// ledger records as its sixth form.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Connect **and authenticate**.
    ///
    /// # The token goes here, not at the call sites
    ///
    /// Every request path in this file goes through `connect`, so putting the preamble here is the
    /// same rule as a validating constructor: there is no way to open a served connection without
    /// offering the token. A version that wrote it in `send` and `send_streaming_approving`
    /// separately would be two places to forget it, and the one that got forgotten would still
    /// work against a daemon — until the daemon started checking.
    ///
    /// # It does NOT wait for an acknowledgement
    ///
    /// The daemon serves one connection at a time (`serve`), so a read here would block behind any
    /// turn already in flight — and this is on the path §B13 budgets 150 ms to. The token is
    /// written and the client proceeds; a refusal arrives as the daemon's first and only event,
    /// which [`Self::check_refusal`] turns into [`ClientError::Refused`].
    fn connect(&self) -> Result<TcpStream, ClientError> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, self.connect_timeout)
            .map_err(|e| ClientError::NoDaemon { port: self.port, detail: e.to_string() })?;
        // An absent token is offered as the empty string rather than skipped. The daemon reads a
        // preamble line unconditionally, so writing nothing would make it read the REQUEST as the
        // token — refusing correctly, but for a reason nobody could diagnose.
        let token = crate::auth::read_token(&self.profile_root).unwrap_or_default();
        stream
            .write_all(format!("{token}\n").as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| ClientError::Closed { detail: e.to_string() })?;
        Ok(stream)
    }

    /// Turn the daemon's refusal event into a typed error.
    ///
    /// The marker is a shared constant rather than a sentence matched by eye — one definition, read
    /// on both sides, so rewording the message cannot silently stop the client recognising it.
    fn check_refusal(&self, event: &Event) -> Option<ClientError> {
        match event {
            Event::Error { detail } if detail.starts_with(crate::auth::REFUSED) => {
                Some(ClientError::Refused { port: self.port, detail: detail.clone() })
            }
            _ => None,
        }
    }

    /// Send one request and hand each event to `on_event` **as it arrives**.
    ///
    /// **Layer 4 of the streaming path.** `send` reads to EOF and returns a `Vec`, which is
    /// correct for `Status` and fatal for a turn: it re-buffers everything the daemon streamed.
    /// This is the same loop with the accumulation replaced by a callback, so a caller can put
    /// tokens on screen at the rate they land.
    pub fn send_streaming(
        &self,
        request: &Request,
        mut on_event: impl FnMut(Event),
    ) -> Result<(), ClientError> {
        // A caller that supplies no decider cannot approve anything, and the honest answer for
        // that is a refusal rather than a default. `false` here is the same fail-closed rule the
        // daemon's gate applies to a client that hangs up.
        self.send_streaming_approving(request, &mut |_| (false, None), &mut on_event)
    }

    /// As [`Self::send_streaming`], answering approval prompts with `decide`.
    ///
    /// # The answer goes back on the SAME connection
    ///
    /// The daemon is serial: the turn that raised the prompt is holding the only connection being
    /// served, so a `Request::Approve` opened on a second socket is read only after the decision
    /// it answers has already been denied. The exchange therefore happens inline, in this loop —
    /// which is also why `decide` blocks: the daemon is waiting on the read.
    ///
    /// **`decision: 0` is not answered.** That is the loop's render-only announcement that it is
    /// about to ask (`TurnEvent::ApprovalPrompt`); the gate's own prompt carries an id from 1.
    /// Answering both would put two replies on the wire for one question, and the second would be
    /// read as the answer to whatever came next.
    pub fn send_streaming_approving(
        &self,
        request: &Request,
        decide: &mut dyn FnMut(&Event) -> (bool, Option<String>),
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), ClientError> {
        let stream = self.connect()?;
        // No whole-turn deadline. A model that takes two minutes is working, and a deadline here
        // would kill exactly the long turns streaming exists to make bearable. The per-read
        // timeout is what distinguishes slow from dead.
        stream.set_read_timeout(Some(Duration::from_secs(600))).ok();
        let mut writer = stream
            .try_clone()
            .map_err(|e| ClientError::Closed { detail: e.to_string() })?;
        crate::protocol::write_line(&mut writer, request)
            .map_err(|e| ClientError::Closed { detail: e.to_string() })?;

        let mut reader = BufReader::new(stream);
        loop {
            // LOOP-EXEMPT: reading a response stream, not a driving loop.
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Event>(line.trim()) {
                        Ok(e) => {
                            if let Some(refused) = self.check_refusal(&e) {
                                return Err(refused);
                            }
                            if let Event::Approval { decision, .. } = &e {
                                let decision = *decision;
                                if decision != 0 {
                                    // Show it before asking — the decider is a human, and §B9's
                                    // whole point is that they see the blast radius first.
                                    on_event(e.clone());
                                    let (granted, reason) = decide(&e);
                                    // The reason rides with the decline rather than being sent
                                    // separately: two messages for one answer is a second thing
                                    // that can be lost.
                                    let reply = Request::Approve { decision, granted, reason };
                                    crate::protocol::write_line(&mut writer, &reply).map_err(
                                        |e| ClientError::Closed { detail: e.to_string() },
                                    )?;
                                    continue;
                                }
                            }
                            on_event(e)
                        }
                        Err(e) => return Err(ClientError::Closed { detail: e.to_string() }),
                    }
                }
                Err(e) => return Err(ClientError::Closed { detail: e.to_string() }),
            }
        }
        Ok(())
    }

    /// Send one request, read every event until the daemon closes.
    pub fn send(&self, request: &Request) -> Result<Vec<Event>, ClientError> {
        let stream = self.connect()?;
        stream.set_read_timeout(Some(Duration::from_secs(600))).ok();
        let mut writer = stream.try_clone().map_err(|e| ClientError::Closed {
            detail: e.to_string(),
        })?;
        crate::protocol::write_line(&mut writer, request)
            .map_err(|e| ClientError::Closed { detail: e.to_string() })?;

        let mut reader = BufReader::new(stream);
        let mut events = Vec::new();
        loop {
            // LOOP-EXEMPT: reading a response stream, not a driving loop.
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Event>(line.trim()) {
                        Ok(e) => {
                            if let Some(refused) = self.check_refusal(&e) {
                                return Err(refused);
                            }
                            events.push(e)
                        }
                        Err(e) => {
                            return Err(ClientError::Closed { detail: e.to_string() });
                        }
                    }
                }
                Err(e) => return Err(ClientError::Closed { detail: e.to_string() }),
            }
        }
        Ok(events)
    }

    pub fn ask(&self, message: &str) -> Result<Vec<Event>, ClientError> {
        self.send(&Request::Ask { session: self.session.clone(), message: message.to_string() })
    }

    /// Ask, streaming each event to the callback as it arrives.
    pub fn ask_streaming(
        &self,
        message: &str,
        on_event: impl FnMut(Event),
    ) -> Result<(), ClientError> {
        self.send_streaming(
            &Request::Ask { session: self.session.clone(), message: message.to_string() },
            on_event,
        )
    }

    /// A turn whose approval prompts are answered by `decide`.
    pub fn ask_streaming_approving(
        &self,
        message: &str,
        decide: &mut dyn FnMut(&Event) -> (bool, Option<String>),
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), ClientError> {
        self.send_streaming_approving(
            &Request::Ask { session: self.session.clone(), message: message.to_string() },
            decide,
            on_event,
        )
    }

    pub fn status(&self) -> Result<Vec<Event>, ClientError> {
        self.send(&Request::Status)
    }

    /// The same client aimed at the **control port**, if this profile's daemon advertised one.
    ///
    /// # Why the control commands do not use the main port
    ///
    /// The daemon serves the main port one connection at a time, so a `/steer` sent while a turn
    /// is running is not *read* until that turn has ended — and a steer delivered after the turn
    /// it was meant to change is not steering, it is a note. `crate::control_plane` listens on
    /// `port + 1` and shares only the run table and the `DurableControl` with the turn in flight,
    /// which is what makes M3-DESIGN §10.1's *"from outside"* true rather than declared.
    ///
    /// **The fall-back is the main port, and it is a fall-back rather than a preference.** An
    /// older daemon, or one whose `port + 1` was taken, still answers these four requests on the
    /// main port — behind whatever turn is running. `control_or_main` is where that is decided, in
    /// one place, so a caller cannot get it right for `/runs` and wrong for `/steer`.
    pub fn control(&self) -> Option<Self> {
        // **Read, never derived.** The port is whatever the OS gave the daemon; deriving it as
        // `port + 1` collided with a second daemon's main port, which presents as an
        // unexplainable auth refusal. See `control_plane`'s module header.
        let port = crate::control_plane::advertised_port(&self.profile_root)?;
        Some(Self { port, ..self.clone() })
    }

    /// The control port when there is one, the main port otherwise.
    ///
    /// **The fall-back is a fall-back, not a preference.** On the main port these requests wait
    /// behind whatever turn is running, which for `/steer` means arriving after the turn it was
    /// meant to change. It is still the right answer when there is no control plane: a slow
    /// correct reply beats a refusal.
    fn control_or_main(&self, request: &Request) -> Result<Vec<Event>, ClientError> {
        let Some(control) = self.control() else {
            return self.send(request);
        };
        match control.send(request) {
            Ok(events) => Ok(events),
            // Only a missing listener falls back — a stale `control.port` file from a daemon that
            // died. A refusal is a token mismatch and means the same thing on both ports; retrying
            // would turn one clear error into two.
            Err(ClientError::NoDaemon { .. }) => self.send(request),
            Err(e) => Err(e),
        }
    }

    /// Every run the **daemon** owns. Not a client-side list — see `--status`, which reported a
    /// fresh process's own defaults while describing a daemon running something else.
    pub fn runs(&self) -> Result<Vec<Event>, ClientError> {
        self.control_or_main(&Request::Runs)
    }

    /// One run in full: state, checkpoint, spend against ceiling, orphan policy on cancel.
    pub fn watch(&self, run: &str, since: u64) -> Result<Vec<Event>, ClientError> {
        self.control_or_main(&Request::Watch { run: run.to_string(), since })
    }

    /// Guidance for a run that is already going. Delivered at its next iteration boundary — never
    /// mid-tool-call, and never as a restart.
    pub fn steer(&self, run: &str, text: &str) -> Result<Vec<Event>, ClientError> {
        self.control_or_main(&Request::Steer { run: run.to_string(), text: text.to_string() })
    }

    /// Ask a run to stop at its next iteration boundary.
    pub fn cancel(&self, run: &str) -> Result<Vec<Event>, ClientError> {
        self.control_or_main(&Request::Cancel { run: run.to_string() })
    }

    /// Continue a run from its last completed checkpoint. **Main port** — it needs the engine, and
    /// a resume that staged without driving would report success for a run that never moved.
    pub fn resume(&self, run: &str) -> Result<Vec<Event>, ClientError> {
        self.send(&Request::Resume { run: run.to_string() })
    }

    /// Re-fetch this session's turns, so a reconnecting client shows what the model can see.
    pub fn replay(&self) -> Result<Vec<Event>, ClientError> {
        self.send(&Request::Replay { session: self.session.clone() })
    }

    /// Ask the daemon to stop.
    ///
    /// **Refused while a run is in flight** — that is the case invariant 6 exists for. An idle
    /// daemon protects nothing, and one that could not be stopped meant a closed window left a
    /// process listening that the next launch silently reconnected to. Three times this session a
    /// fixed build was tested against a stale one that way.
    pub fn shutdown(&self) -> Result<Vec<Event>, ClientError> {
        self.send(&Request::Shutdown)
    }

    /// Whether a daemon is already listening. Cheap, and does not start one.
    pub fn daemon_is_up(&self) -> bool {
        self.connect().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_holds_no_run_state() {
        // §2.14 and §6: surfaces hold no state the daemon lacks, because if the client owned the
        // run then closing the terminal would end it. The assertion is on the type's whole
        // surface — a `Debug` of every field — rather than on a comment, because "thin" is the
        // kind of claim that stays true until somebody adds a cache.
        let c = Client::new("s");
        let debug = format!("{c:?}");
        for forbidden in ["run", "transcript", "checkpoint", "history", "context", "cache"] {
            assert!(
                !debug.to_lowercase().contains(forbidden),
                "the client holds `{forbidden}`: {debug}"
            );
        }
        assert!(debug.contains("port") && debug.contains("session"));
    }

    #[test]
    fn an_absent_daemon_is_a_named_error_not_a_hang() {
        // Port 1: nothing listens. The client must fail fast and say what is missing, because
        // this is the path a first run takes before a daemon exists.
        let c = Client::new("s").with_port(1);
        assert!(!c.daemon_is_up());
        let e = c.status().unwrap_err();
        assert!(matches!(e, ClientError::NoDaemon { .. }), "{e}");
        assert!(e.to_string().contains("no daemon"), "{e}");
    }
}
