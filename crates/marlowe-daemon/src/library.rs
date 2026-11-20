//! Library requests, delegated to `marlowe-brains` on a worker thread (detection and catalogs
//! touch the network; a client's other requests must not wait on them).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use marlowe_brains::BrainsError;
use marlowe_model::ProviderKind;
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
        Request::AddModelDir { path } => {
            add_model_dir(core, &path)?;
            Done::Reply(Event::Catalog { provider: ProviderKind::Ollama, models: b.catalog(ProviderKind::Ollama)? })
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

/// Remembers a model folder (or one `.gguf`) in `[models] dirs` of the user's settings and has the
/// engine search it from now on.
fn add_model_dir(core: &Core, path: &str) -> Result<(), BrainsError> {
    let path = path.trim().trim_matches('"').trim();
    let p = std::path::Path::new(path);
    let gguf = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf"));
    if !(p.is_dir() || (p.is_file() && gguf)) {
        return Err(BrainsError::Invalid(format!("{path} is not a folder or a .gguf file")));
    }
    let file = core.home.join("settings.toml");
    let mut dirs = marlowe_settings::Settings::load(&core.home, None, None).model_dirs();
    if !dirs.iter().any(|d| d == path) {
        dirs.push(path.to_string());
        marlowe_settings::edit::set_key(&file, "models.dirs", &serde_json::json!(dirs)).map_err(|e| BrainsError::Invalid(format!("could not save {}: {e}", file.display())))?;
    }
    if let Some(hw) = core.brains.factory().hardware() {
        hw.models.add_model_dir(p).map_err(BrainsError::Invalid)?;
    }
    Ok(())
}
