//! Images: PutBlob. A client's image (a paste, a dropped file) is decoded, capped at the store's
//! long edge and kept in `<home>\blobs\<blake3>` (the store `read` and the providers share); the
//! answer is `Event::BlobStored`, whose reference an `Ask` carries in `images`. Decoding runs off
//! the client reader.

use std::sync::Arc;

use marlowe_model::image::{base64_decode, ingest, FsBlobStore, MAX_INPUT_BYTES, STORE_LONG_EDGE};
use marlowe_proto::{Event, Request};

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let Request::PutBlob { mime: _, base64 } = req else {
        return crate::client::not_yet(core, client, &req, "G");
    };
    let core = Arc::clone(core);
    let spawned = std::thread::Builder::new().name("marlowe-put-blob".into()).spawn(move || {
        let event = match store(&core, &base64) {
            Ok(r) => Event::BlobStored { blob: r.blob, mime: r.mime, w: r.w, h: r.h },
            Err(message) => Event::Error { session: None, message },
        };
        core.bus.reply(client, event);
    });
    if let Err(e) = spawned {
        eprintln!("marlowe: an image could not be stored: {e}");
    }
}

fn store(core: &Core, base64: &str) -> Result<marlowe_model::ImageRef, String> {
    // Base64 is 4/3 the size of what it carries.
    if base64.len() / 4 * 3 > MAX_INPUT_BYTES {
        return Err("the image is too large to attach".into());
    }
    let bytes = base64_decode(base64.trim()).ok_or("the image is not valid base64")?;
    let blobs = FsBlobStore::new(core.home.join("blobs"));
    ingest(&blobs, &bytes, STORE_LONG_EDGE).map_err(|e| format!("the image could not be stored: {e}"))
}
