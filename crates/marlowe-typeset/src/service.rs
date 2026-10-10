//! **The render worker and its cache.** Typesetting never runs on the UI thread.
//!
//! The UI asks [`Service::get`] for a formula every frame it is on screen. The first ask queues
//! it and answers [`Slot::Pending`]; the worker has it rendered (in the shipped build by the
//! `marlowe-typeset.exe` helper, [`crate::helper`]), and stores the strips; the UI is woken and
//! the next frame gets [`Slot::Ready`]. Every later frame, scrolling included, is a lookup: the
//! bytes are encoded once and only ever re-sent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::fit;
use crate::sixel::Coverage;

/// What the terminal reported (or its defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub cell_w: u16,
    pub cell_h: u16,
    /// The terminal's default foreground: the colour of maths drawn in the default weight.
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

/// One formula as it will be drawn: the markup, its colour, and the box of cells it must fill,
/// which is the box its character-grid layout reserved. Fitting the image into that box is what
/// keeps the rows around it still when it lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Job {
    pub markup: String,
    pub fg: [u8; 3],
    pub cols: u16,
    pub rows: u16,
}

/// A typeset formula: one Sixel strip per cell row, top to bottom. An empty strip is a row the
/// glyphs do not reach, which is never sent.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub cols: u16,
    pub strips: Vec<Arc<[u8]>>,
}

#[derive(Debug, Clone)]
pub enum Slot {
    Pending,
    Ready(Arc<Image>),
    /// Typst refused it, or it would not fit its box legibly. It stays on the character grid.
    Failed,
}

/// Images held at once. Past this the finished entries are dropped and re-rendered on demand.
const CACHE_CAP: usize = 512;

/// Turns (markup, em in pixels, colour) into a coverage plane, in process. Tests use stand-ins;
/// Typst itself lives only in the helper binary.
pub type Typesetter = Box<dyn FnMut(&str, f32, [u8; 3]) -> Result<Coverage, String> + Send>;

/// Turns a job into strips: the helper process ([`crate::helper::renderer`]), or a
/// [`Typesetter`] run through [`crate::fit::image`].
pub type Renderer = Box<dyn FnMut(&Job, Caps) -> Result<Image, String> + Send>;

pub struct Service {
    caps: Caps,
    cache: Arc<Mutex<HashMap<Job, Slot>>>,
    tx: Sender<Job>,
}

impl Service {
    /// Starts the worker with an in-process typesetter. `wake` is sent `()` each time a job
    /// finishes.
    pub fn start(caps: Caps, mut typeset: Typesetter, wake: Sender<()>) -> Service {
        Service::with_renderer(caps, Box::new(move |job, caps| fit::image(job, caps, &mut typeset)), wake)
    }

    /// Starts the worker with any renderer. `wake` is sent `()` each time a job finishes.
    pub fn with_renderer(caps: Caps, mut render: Renderer, wake: Sender<()>) -> Service {
        let cache: Arc<Mutex<HashMap<Job, Slot>>> = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx): (Sender<Job>, Receiver<Job>) = unbounded();
        let store = Arc::clone(&cache);
        thread::Builder::new()
            .name("marlowe-typeset".into())
            .spawn(move || {
                for job in rx {
                    // A typesetter panic must not leave the formula pending forever behind its
                    // placeholder: it becomes a failure, and the formula stays on the grid.
                    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&job, caps)));
                    let slot = match made {
                        Ok(Ok(img)) if img.strips.len() == usize::from(job.rows) => Slot::Ready(Arc::new(img)),
                        _ => Slot::Failed,
                    };
                    if let Ok(mut map) = store.lock() {
                        map.insert(job, slot);
                    }
                    let _ = wake.send(());
                }
            })
            .expect("spawn the typeset worker");
        Service { caps, cache, tx }
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// The formula's state. Queues it the first time it is asked for.
    pub fn get(&self, job: &Job) -> Slot {
        let Ok(mut map) = self.cache.lock() else { return Slot::Failed };
        if let Some(s) = map.get(job) {
            return s.clone();
        }
        if map.len() >= CACHE_CAP {
            map.retain(|_, s| matches!(s, Slot::Pending));
        }
        map.insert(job.clone(), Slot::Pending);
        if self.tx.send(job.clone()).is_err() {
            map.insert(job.clone(), Slot::Failed);
            return Slot::Failed;
        }
        Slot::Pending
    }
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// Installs the process-wide service. Only the first call has an effect.
pub fn install(service: Service) -> &'static Service {
    SERVICE.get_or_init(|| service)
}

/// The installed service, if images are on.
pub fn installed() -> Option<&'static Service> {
    SERVICE.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn caps() -> Caps {
        Caps { cell_w: 10, cell_h: 20, fg: [0xcd, 0xc9, 0xd6], bg: [0x0f, 0x0e, 0x14] }
    }

    /// A stand-in typesetter: a solid block `w` by `h` pixels at the requested em of 16.
    fn block(w: usize, h: usize) -> Typesetter {
        Box::new(move |_, em, _| {
            let s = em / 16.0;
            let (w, h) = ((w as f32 * s) as usize, (h as f32 * s) as usize);
            Ok(Coverage { w, h, alpha: vec![255; w * h] })
        })
    }

    fn job(cols: u16, rows: u16) -> Job {
        Job { markup: "frac(a, b)".into(), fg: [0xcd, 0xc9, 0xd6], cols, rows }
    }

    fn wait(s: &Service, j: &Job) -> Slot {
        for _ in 0..200 {
            match s.get(j) {
                Slot::Pending => std::thread::sleep(Duration::from_millis(5)),
                other => return other,
            }
        }
        panic!("the worker never finished");
    }

    #[test]
    fn a_job_is_queued_once_rendered_off_thread_and_then_a_lookup() {
        let (wake_tx, wake) = unbounded();
        let s = Service::start(caps(), block(30, 30), wake_tx);
        let j = job(10, 3);
        assert!(matches!(s.get(&j), Slot::Pending));
        let Slot::Ready(img) = wait(&s, &j) else { panic!("not rendered") };
        assert!(wake.try_recv().is_ok(), "the UI was not woken");
        assert_eq!(img.strips.len(), 3, "one strip per reserved row");
        assert_eq!(img.cols, 3);
        // Vertically centred: 30 px in a 60 px box leaves the first and last 15 px empty.
        assert!(!img.strips[0].is_empty() && !img.strips[1].is_empty() && !img.strips[2].is_empty());
        // The second ask is the same image, not a re-render.
        let Slot::Ready(again) = s.get(&j) else { panic!() };
        assert!(Arc::ptr_eq(&img, &again));
    }

    #[test]
    fn a_typesetter_that_panics_leaves_the_formula_on_the_grid_not_pending() {
        let (wake_tx, _wake) = unbounded();
        let s = Service::start(caps(), Box::new(|_: &str, _: f32, _: [u8; 3]| -> Result<Coverage, String> { panic!("typst fell over") }), wake_tx);
        assert!(matches!(wait(&s, &job(10, 3)), Slot::Failed));
        // And the worker is still there for the next one.
        assert!(matches!(wait(&s, &job(11, 3)), Slot::Failed));
    }

    #[test]
    fn a_formula_that_cannot_fit_its_box_legibly_stays_on_the_grid() {
        let (wake_tx, _wake) = unbounded();
        let s = Service::start(caps(), block(400, 30), wake_tx);
        assert!(matches!(wait(&s, &job(10, 3)), Slot::Failed));
        // Slightly too wide is shrunk to fit instead.
        let s = Service::start(caps(), block(110, 30), unbounded().0);
        let Slot::Ready(img) = wait(&s, &job(10, 3)) else { panic!("should have been shrunk") };
        assert!(img.cols <= 10);
    }
}
