//! Watch, notice and voice requests, and the threads that forward Sentinel and Voice events to
//! the bus. Without a Sentinel or Voice the requests answer with an error.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use marlowe_proto::{Event, Request};
use marlowe_sentinel::SentinelEvent;
use marlowe_voice::VoiceEvent;

use crate::daemon::Core;
use crate::work::Work;

const NO_SENTINEL: &str = "the Sentinel is not running";
const NO_VOICE: &str = "voice is not available";

/// Handles a watch / notice / voice request from `client`.
pub(crate) fn dispatch(core: &Arc<Core>, client: u64, client_name: &str, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let refuse = |message: String| reply(Event::Error { session: None, message });
    match req {
        Request::Brief => crate::brief::reply(core, client),
        Request::WatchList | Request::WatchAdd { .. } | Request::WatchRemove { .. } | Request::WatchCandidates
        | Request::ListNotices { .. } | Request::NoticeSeen { .. } => {
            let Some(s) = core.sentinel.clone() else { return refuse(NO_SENTINEL.into()) };
            match req {
                Request::WatchList => reply(Event::WatchState(s.state())),
                Request::WatchAdd { path } => match s.add_root(Path::new(&path)) {
                    Ok(state) => {
                        core.bus.publish(Event::WatchState(state));
                    }
                    Err(e) => refuse(e.to_string()),
                },
                Request::WatchRemove { path } => match s.remove_root(Path::new(&path)) {
                    Ok(state) => {
                        core.bus.publish(Event::WatchState(state));
                    }
                    Err(e) => refuse(e.to_string()),
                },
                Request::WatchCandidates => {
                    // A bounded directory walk: off the reader thread.
                    let core = core.clone();
                    let _ = std::thread::Builder::new().name("marlowe-discover".into()).spawn(move || {
                        core.bus.reply(client, Event::WatchCandidates { candidates: s.candidates() });
                    });
                }
                Request::ListNotices { limit, unseen_only } => match s.notices(client_name, limit as usize, unseen_only) {
                    Ok(list) => reply(Event::Notices { list }),
                    Err(e) => refuse(e.to_string()),
                },
                Request::NoticeSeen { id } => match s.mark_seen(client_name, &id) {
                    Ok(true) => {}
                    Ok(false) => refuse(format!("no notice {id}")),
                    Err(e) => refuse(e.to_string()),
                },
                _ => unreachable!("matched above"),
            }
        }
        Request::Speak { .. } | Request::StopSpeaking | Request::VoiceSettings { .. } => {
            let Some(v) = core.voice.clone() else { return refuse(NO_VOICE.into()) };
            match req {
                Request::Speak { text } => {
                    if let Err(e) = v.speak(&text) {
                        refuse(e.to_string());
                    }
                }
                Request::StopSpeaking => v.stop(),
                Request::VoiceSettings { enabled, voice } => match v.configure(enabled, voice) {
                    Ok(status) => {
                        core.bus.publish(Event::VoiceState(status));
                    }
                    Err(e) => refuse(e.to_string()),
                },
                _ => unreachable!("matched above"),
            }
        }
        other => refuse(format!("not a watch request: {other:?}")),
    }
}

/// Forwards Sentinel events to the bus until the Sentinel's sink closes, speaking urgent notices
/// outside quiet hours.
pub(crate) fn forward_sentinel(core: Arc<Core>, events: Receiver<SentinelEvent>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("marlowe-sentinel-fwd".into()).spawn(move || {
        while let Ok(ev) = events.recv() {
            match ev {
                SentinelEvent::Notice(n) => {
                    crate::brief::speak_notice(&core, &n);
                    core.bus.publish(Event::Notice(n))
                }
                SentinelEvent::State(s) => core.bus.publish(Event::WatchState(s)),
            };
        }
    })
}

/// Forwards Voice events to the bus as `VoiceState` until the voice's channel closes. A
/// degraded engine (the Chatterbox clone falling back to WinRT) is said once as an error line;
/// the voice logged it already.
pub(crate) fn forward_voice(core: Arc<Core>, events: Receiver<VoiceEvent>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("marlowe-voice-fwd".into()).spawn(move || {
        while let Ok(ev) = events.recv() {
            let Some(voice) = core.voice.as_ref() else { break };
            let mut status = voice.status();
            match ev {
                VoiceEvent::Degraded { message } => {
                    core.bus.publish(Event::Error { session: None, message: format!("voice: {message}") });
                    continue;
                }
                VoiceEvent::Speaking { envelope, .. } => {
                    status.speaking = true;
                    status.envelope = envelope;
                }
                VoiceEvent::Spoken { .. } => status.speaking = false,
                VoiceEvent::Failed { message, .. } => {
                    status.speaking = false;
                    core.bus.publish(Event::Error { session: None, message: format!("voice: {message}") });
                }
            }
            core.bus.publish(Event::VoiceState(status));
        }
    })
}

/// Shows the voice engine's model loading as background work while it does (the Chatterbox sidecar
/// takes tens of seconds and holds GPU memory): the work is announced when the load begins and
/// ends when the engine says it is ready, fails, or exits. Ends with the daemon.
pub(crate) fn watch_voice_load(core: Arc<Core>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("marlowe-voice-load".into()).spawn(move || {
        let mut work: Option<Work> = None;
        while !core.is_shutting_down() {
            let loading = core.voice.as_ref().and_then(|v| v.loading_since_ms()).is_some();
            match (loading, work.is_some()) {
                (true, false) => work = Some(Work::start(&core, "voice", "loading the voice model")),
                (false, true) => work = None,
                _ => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    })
}
