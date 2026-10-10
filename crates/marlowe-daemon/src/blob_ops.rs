//! Images (track G): PutBlob.
//!
//! Seam stub: `client.rs` already routes these requests here; until track G lands every one is
//! answered with an `Error` naming the track. The blob store itself is
//! `marlowe-model/src/image.rs` (`<home>\blobs\<hex>`); the answer is `Event::BlobStored`, whose
//! reference an `Ask` carries in `images`. Replace the body of [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "G");
}
