//! The `web_fetch{prompt}` digest (design T20): the question is answered from the page by the
//! Quick slot's model, or the conductor's, whichever is hosted or already loaded. Nothing is
//! loaded for it (a digest never evicts the conductor from the GPU); with neither available the
//! tool returns the page itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use marlowe_agent::web_digest::{digest_prompt, DIGEST_SYSTEM};
use marlowe_agent::{Conversation, DigestRequest, Services, WebDigest};
use marlowe_model::{CancelToken, ChatRequest, Effort, Message, SystemBlock};
use marlowe_proto::Slot;

use crate::daemon::Core;

/// Longest a digest may take before the tool falls back to the page.
const DIGEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest answer.
const DIGEST_MAX_OUTPUT: u32 = 1_500;

/// The session's [`WebDigest`].
pub(crate) fn services(core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    let weak = Arc::downgrade(core);
    services.with(WebDigest::new(move |req: &DigestRequest<'_>, cancel: &CancelToken| digest(&weak, req, cancel)))
}

fn digest(core: &Weak<Core>, req: &DigestRequest<'_>, cancel: &CancelToken) -> Result<String, String> {
    let core = core.upgrade().ok_or("the daemon is stopping")?;
    let (model, effort) = core.brains.background_model(&Slot::Quick).map_err(|e| format!("no model is loaded to answer it: {e}"))?;
    let (provider, info, _) = core.brains.model_parts(&model, effort).map_err(|e| e.to_string())?;
    let mut chat = ChatRequest::new(info.id.clone(), vec![Message::user(digest_prompt(req))]);
    chat.system = vec![SystemBlock { text: DIGEST_SYSTEM.into(), cache: false }];
    chat.max_output_tokens = Some(DIGEST_MAX_OUTPUT);
    chat.effort = Effort::Off;

    // Stops on the turn's interrupt or at the timeout, whichever comes first.
    let local = CancelToken::new();
    let (watch, parent, done) = (local.clone(), cancel.clone(), Arc::new(AtomicBool::new(false)));
    let finished = done.clone();
    std::thread::spawn(move || {
        let until = Instant::now() + DIGEST_TIMEOUT;
        while !finished.load(Ordering::SeqCst) {
            if parent.is_cancelled() || Instant::now() >= until {
                watch.cancel();
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let out = provider.stream(&chat, &local, &mut |_| {});
    done.store(true, Ordering::SeqCst);
    let text = out.map_err(|e| e.to_string())?.text;
    if local.is_cancelled() {
        return Err(if cancel.is_cancelled() { "interrupted".into() } else { "the model took too long".into() });
    }
    let text = text.trim();
    if text.is_empty() {
        return Err("the model gave no answer".into());
    }
    Ok(format!("{text}\n\n(answered by {})", info.model_ref()))
}
