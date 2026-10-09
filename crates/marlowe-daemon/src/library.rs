//! Library requests, delegated to `marlowe-brains` on a worker thread (detection and catalogs
//! touch the network; a client's other requests must not wait on them).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use marlowe_brains::BrainsError;
use marlowe_proto::{Event, Request};

use crate::daemon::Core;

enum Done {
    /// Answer only the asking client.
    Reply(Event),
    /// The library changed: broadcast the new state.
    Changed,
}

pub(crate) fn spawn(core: Arc<Core>, client: u64, req: Request) {
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-library".into()).spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| handle(&c, req)));
        match outcome {
            Ok(Ok(Done::Reply(ev))) => c.bus.reply(client, ev),
            Ok(Ok(Done::Changed)) => match catch_unwind(AssertUnwindSafe(|| c.brains.state())) {
                Ok(Ok(state)) => {
                    c.bus.publish(state);
                }
                Ok(Err(e)) => c.bus.reply(client, error(e.to_string())),
                Err(_) => c.bus.reply(client, error("the library failed".into())),
            },
            Ok(Err(e)) => c.bus.reply(client, error(e.to_string())),
            Err(_) => c.bus.reply(client, error("the library failed".into())),
        }
    });
    if spawned.is_err() {
        core.bus.reply(client, error("the daemon is out of threads".into()));
    }
}

fn error(message: String) -> Event {
    Event::Error { session: None, message }
}

fn handle(core: &Core, req: Request) -> Result<Done, BrainsError> {
    let b = &core.brains;
    Ok(match req {
        Request::LibraryState | Request::DetectProviders => Done::Reply(b.state()?),
        Request::ListCatalog { provider } => Done::Reply(Event::Catalog { provider, models: b.catalog(provider)? }),
        Request::AddModel { provider, model_id } => {
            b.add_model(provider, &model_id)?;
            Done::Changed
        }
        Request::RemoveModel { model } => {
            b.remove_model(&model)?;
            Done::Changed
        }
        Request::SetSlot { slot, model, effort } => {
            b.set_slot(&slot, model.as_ref(), effort)?;
            Done::Changed
        }
        Request::SetCredential { provider, secret } => {
            b.set_credential(provider, secret.expose())?;
            Done::Changed
        }
        _ => return Err(BrainsError::Invalid("not a library request".into())),
    })
}
