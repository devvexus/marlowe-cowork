//! Notifications and sent files (track N): NotifySettings, OpenFile; the `notify`, `send_file` and
//! `notices` tools' daemon side (toast and spoken line, the outbox, `Notified` and `FileSent`).
//!
//! Seam stub: `client.rs` already routes these requests here; until track N lands every one is
//! answered with an `Error` naming the track. Replace the body of [`dispatch`]; keep its
//! signature. `stack.rs` calls [`services`] for every session and `boot.rs` calls [`on_boot`].

use std::sync::Arc;

use marlowe_agent::{Conversation, Services};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "N");
}

/// N's services for one session (the notifier, the outbox).
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}

/// Loads the notification switches. Must not block.
pub(crate) fn on_boot(_core: &Arc<Core>) {}
