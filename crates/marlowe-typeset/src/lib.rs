//! **Display maths as typeset images**, for terminals that can show them.
//!
//! The character grid (`marlowe_surface::math`) renders every formula on every terminal and is
//! complete on its own. This crate is the step above it: where the terminal speaks Sixel
//! (Windows Terminal 1.22 and later), a display formula is laid out by Typst's maths engine and
//! drawn as an image in exactly the cells its grid layout reserved, so swapping one for the other
//! moves nothing.
//!
//! **Typst is not linked here.** It runs in a helper process, `marlowe-typeset.exe` (the
//! `marlowe-typeset-helper` crate), built by default and shipped beside `marlowe.exe`. This crate
//! is the client side and the pieces both sides share:
//!
//! - [`probe`]: what the terminal can show, from its answers to four queries.
//! - [`sixel`]: a one-colour encoder with a transparent background. No filled box, ever.
//! - [`fit`]: fitting a typeset formula into its reserved box and cutting it into strips.
//! - [`proto`]: the helper's JSON-lines wire format, and the helper's serve loop.
//! - [`helper`]: starting the helper lazily, restarting it once, and falling back to the grid.
//! - [`service`]: the worker thread and the cache. Rendering never runs on the UI thread, and a
//!   formula is encoded once.

pub mod fit;
pub mod helper;
pub mod probe;
pub mod proto;
pub mod service;
pub mod sixel;

pub use service::{install, installed, Caps, Image, Job, Renderer, Service, Slot};
