//! **`marlowe-typeset.exe`'s engine**: Typst as a maths rasteriser, behind the JSON-lines loop in
//! `main.rs` ([`marlowe_typeset::proto::serve`]).
//!
//! A separate process so that `marlowe.exe` never links Typst. The client side (starting this
//! helper, the cache, putting strips on screen) is `marlowe-typeset`.

mod raster;

use std::sync::{Mutex, OnceLock};

use marlowe_typeset::sixel::Coverage;

/// The typesetter the helper serves with: (markup, em in pixels, colour) to a coverage plane.
/// The engine (library, fonts) is built on the first call.
pub fn typesetter() -> impl FnMut(&str, f32, [u8; 3]) -> Result<Coverage, String> {
    let mut engine: Option<raster::Engine> = None;
    move |markup: &str, em: f32, fg: [u8; 3]| engine.get_or_insert_with(raster::Engine::new).render(markup, em, fg)
}

fn shared_engine() -> &'static Mutex<raster::Engine> {
    static ENGINE: OnceLock<Mutex<raster::Engine>> = OnceLock::new();
    ENGINE.get_or_init(|| Mutex::new(raster::Engine::new()))
}

/// Typesets one equation body to a coverage plane, in this process: for tests and timing.
pub fn typeset_once(markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Coverage, String> {
    shared_engine().lock().map_err(|_| "poisoned".to_string())?.render(markup, em_px, fg)
}

/// [`typeset_once`] as a PNG, for looking at a render.
pub fn typeset_png(markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Vec<u8>, String> {
    shared_engine().lock().map_err(|_| "poisoned".to_string())?.png(markup, em_px, fg)
}
