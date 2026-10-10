//! **The render worker and its cache.** Typesetting never runs on the UI thread.
//!
//! The UI asks [`Service::get`] for a formula every frame it is on screen. The first ask queues
//! it and answers [`Slot::Pending`]; the worker has it rendered (in the shipped build by the
//! `marlowe-typeset.exe` helper, [`crate::helper`]), and stores the strips; the UI is woken and
//! the next frame gets [`Slot::Ready`]. Every later frame, scrolling included, is a lookup: the
//! bytes are encoded once and only ever re-sent.
//!
//! **A resize is a lookup too.** The cache is keyed on the formula, its colour and its rows, not
//! on the width it may fill: an image that still fits the new width is the same image, so a
//! drag-resize re-renders only a formula that no longer fits. The worker takes the newest job
//! first, so what is on screen now renders before anything asked for at an earlier width.

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

/// One formula as it will be drawn: the markup, its colour, and the box of cells it must fit: the
/// rows its character-grid layout reserved, so the rows around it stay still when it lands, and
/// the columns it may use.
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

/// What the cache is keyed on: everything in a [`Job`] but its width.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    markup: String,
    fg: [u8; 3],
    rows: u16,
}

impl Key {
    fn of(job: &Job) -> Key {
        Key { markup: job.markup.clone(), fg: job.fg, rows: job.rows }
    }
}

/// A cache entry: its state and the width it was (or is being) rendered for.
type Entry = (u16, Slot);

/// Turns (markup, em in pixels, colour) into a coverage plane, in process. Tests use stand-ins;
/// Typst itself lives only in the helper binary.
pub type Typesetter = Box<dyn FnMut(&str, f32, [u8; 3]) -> Result<Coverage, String> + Send>;

/// Turns a job into strips: the helper process ([`crate::helper::renderer`]), or a
/// [`Typesetter`] run through [`crate::fit::image`].
pub type Renderer = Box<dyn FnMut(&Job, Caps) -> Result<Image, String> + Send>;

pub struct Service {
    caps: Caps,
    cache: Arc<Mutex<HashMap<Key, Entry>>>,
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
        let cache: Arc<Mutex<HashMap<Key, Entry>>> = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx): (Sender<Job>, Receiver<Job>) = unbounded();
        let store = Arc::clone(&cache);
        thread::Builder::new()
            .name("marlowe-typeset".into())
            .spawn(move || {
                // Newest first: during a resize the jobs for the width on screen now overtake the
                // ones queued at the widths dragged through.
                let mut stack: Vec<Job> = Vec::new();
                loop {
                    stack.extend(rx.try_iter());
                    let job = match stack.pop() {
                        Some(j) => j,
                        None => match rx.recv() {
                            Ok(j) => j,
                            Err(_) => break,
                        },
                    };
                    let key = Key::of(&job);
                    // Asked for again at another width since this was queued: the newer job renders it.
                    let current = store.lock().ok().is_some_and(|m| m.get(&key).is_some_and(|(w, s)| *w == job.cols && matches!(s, Slot::Pending)));
                    if !current {
                        continue;
                    }
                    // A typesetter panic must not leave the formula pending forever behind its
                    // placeholder: it becomes a failure, and the formula stays on the grid.
                    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&job, caps)));
                    let slot = match made {
                        Ok(Ok(img)) if img.strips.len() == usize::from(job.rows) => Slot::Ready(Arc::new(img)),
                        _ => Slot::Failed,
                    };
                    if let Ok(mut map) = store.lock() {
                        map.insert(key, (job.cols, slot));
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

    /// The formula's state. Queues it the first time it is asked for, and again when the width
    /// it may use has changed enough to matter: narrower than its image, or (for one that would
    /// not fit) an eighth wider than when it was tried.
    pub fn get(&self, job: &Job) -> Slot {
        let Ok(mut map) = self.cache.lock() else { return Slot::Failed };
        let key = Key::of(job);
        if let Some((width, slot)) = map.get(&key) {
            let stale = match slot {
                Slot::Ready(img) => img.cols > job.cols,
                Slot::Failed => job.cols >= width.saturating_add(width.div_ceil(8)),
                Slot::Pending => false,
            };
            if !stale {
                return slot.clone();
            }
        } else if map.len() >= CACHE_CAP {
            map.retain(|_, (_, s)| matches!(s, Slot::Pending));
        }
        map.insert(key.clone(), (job.cols, Slot::Pending));
        if self.tx.send(job.clone()).is_err() {
            map.insert(key, (job.cols, Slot::Failed));
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
        // 30 px at a 16 px em is 32 px at the 17.2 px em of a 20 px cell: four 10 px columns.
        assert_eq!(img.cols, 4);
        // Vertically centred: 32 px in a 60 px box leaves the first and last 14 px empty.
        assert!(!img.strips[0].is_empty() && !img.strips[1].is_empty() && !img.strips[2].is_empty());
        // The second ask is the same image, not a re-render.
        let Slot::Ready(again) = s.get(&j) else { panic!() };
        assert!(Arc::ptr_eq(&img, &again));
    }

    #[test]
    fn a_resize_reuses_an_image_that_still_fits_and_re_renders_one_that_does_not() {
        let (wake_tx, _wake) = unbounded();
        let renders = Arc::new(Mutex::new(0usize));
        let n = Arc::clone(&renders);
        let mut inner = block(30, 30);
        let s = Service::start(
            caps(),
            Box::new(move |m, em, fg| {
                *n.lock().unwrap() += 1;
                inner(m, em, fg)
            }),
            wake_tx,
        );
        let Slot::Ready(img) = wait(&s, &job(40, 3)) else { panic!("not rendered") };
        assert_eq!(img.cols, 4);
        // Every width a drag-resize passes through that still holds four columns: one render.
        for cols in [39, 30, 12, 4, 60, 90] {
            let Slot::Ready(again) = s.get(&job(cols, 3)) else { panic!("{cols} columns re-rendered") };
            assert!(Arc::ptr_eq(&img, &again));
        }
        assert_eq!(*renders.lock().unwrap(), 1);
        // Narrower than the image: rendered again, to fit.
        assert!(matches!(s.get(&job(3, 3)), Slot::Pending));
        assert!(matches!(wait(&s, &job(3, 3)), Slot::Ready(_) | Slot::Failed));
        // (Typeset at full size, then again shrunk to fit.)
        assert!(*renders.lock().unwrap() > 1);
    }

    #[test]
    fn the_newest_job_renders_first() {
        let (wake_tx, _wake) = unbounded();
        let (gate_tx, gate) = unbounded::<()>();
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&order);
        let mut inner = block(30, 30);
        let s = Service::start(
            caps(),
            Box::new(move |m, em, fg| {
                if m == "first" {
                    let _ = gate.recv();
                }
                seen.lock().unwrap().push(m.to_string());
                inner(m, em, fg)
            }),
            wake_tx,
        );
        let named = |m: &str| Job { markup: m.into(), ..job(10, 3) };
        // The worker is busy with the first while three more are asked for.
        assert!(matches!(s.get(&named("first")), Slot::Pending));
        std::thread::sleep(Duration::from_millis(100));
        for m in ["a", "b", "c"] {
            assert!(matches!(s.get(&named(m)), Slot::Pending));
        }
        gate_tx.send(()).unwrap();
        wait(&s, &named("a"));
        assert_eq!(*order.lock().unwrap(), vec!["first", "c", "b", "a"]);
    }

    #[test]
    fn a_typesetter_that_panics_leaves_the_formula_on_the_grid_not_pending() {
        let (wake_tx, _wake) = unbounded();
        let s = Service::start(caps(), Box::new(|_: &str, _: f32, _: [u8; 3]| -> Result<Coverage, String> { panic!("typst fell over") }), wake_tx);
        assert!(matches!(wait(&s, &job(10, 3)), Slot::Failed));
        // And the worker is still there for the next one.
        let next = Job { markup: "x^2".into(), ..job(10, 3) };
        assert!(matches!(wait(&s, &next), Slot::Failed));
    }

    #[test]
    fn a_formula_that_cannot_fit_its_box_legibly_stays_on_the_grid() {
        let (wake_tx, _wake) = unbounded();
        let s = Service::start(caps(), block(400, 30), wake_tx);
        assert!(matches!(wait(&s, &job(10, 3)), Slot::Failed));
        // Slightly too wide is shrunk to fit instead (107 px into 100), down to 0.9x.
        let s = Service::start(caps(), block(100, 30), unbounded().0);
        let Slot::Ready(img) = wait(&s, &job(10, 3)) else { panic!("should have been shrunk") };
        assert!(img.cols <= 10);
        // Needing more than that, it stays on the grid rather than set smaller than the text.
        let s = Service::start(caps(), block(110, 30), unbounded().0);
        assert!(matches!(wait(&s, &job(10, 3)), Slot::Failed));
    }
}
