//! **Typst, as a maths rasteriser.** One equation in, a coverage plane out.
//!
//! The world is as small as Typst allows: one in-memory source, two embedded fonts (New Computer
//! Modern and its maths companion, GUST Font License, `fonts/`), no files, no packages, no clock.
//! The markup comes from `marlowe_surface::math`, which escapes everything a model wrote, so
//! nothing here evaluates model text as code.

use std::sync::OnceLock;

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime};
use typst::layout::PagedDocument;
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

use marlowe_typeset::sixel::Coverage;

static MATH_FONT: &[u8] = include_bytes!("../fonts/NewCMMath-Regular.otf");
static TEXT_FONT: &[u8] = include_bytes!("../fonts/NewCM10-Regular.otf");

/// What every render shares: built once, on the worker thread.
pub struct Engine {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: FileId,
}

impl Engine {
    pub fn new() -> Engine {
        let fonts: Vec<Font> = [MATH_FONT, TEXT_FONT]
            .into_iter()
            .flat_map(|data| Font::iter(Bytes::new(data)))
            .collect();
        Engine {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            main: main_id(),
        }
    }

    /// Typesets `markup` (an equation body) with an em of `em_px` pixels, in `fg`, on nothing.
    pub fn render(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Coverage, String> {
        let pixmap = self.pixmap(markup, em_px, fg)?;
        let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
        let alpha: Vec<u8> = pixmap.data().chunks_exact(4).map(|p| p[3]).collect();
        Ok(Coverage { w, h, alpha }.trimmed())
    }

    /// The same render as a PNG, for looking at.
    pub fn png(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Vec<u8>, String> {
        self.pixmap(markup, em_px, fg)?.encode_png().map_err(|e| e.to_string())
    }

    fn pixmap(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<tiny_skia::Pixmap, String> {
        let [r, g, b] = fg;
        let text = format!(
            "#set page(width: auto, height: auto, margin: (x: 0.2em, y: 0.15em), fill: none)\n\
             #set text(font: \"New Computer Modern\", size: {em_px:.2}pt, fill: rgb({r}, {g}, {b}))\n\
             #show math.equation: set text(font: \"New Computer Modern Math\")\n\
             #let fg = rgb({r}, {g}, {b})\n\
             $ {markup} $\n"
        );
        let world = OneShot { engine: self, source: Source::new(self.main, text) };
        let doc: PagedDocument = typst::compile(&world)
            .output
            .map_err(|errs| errs.iter().map(|e| e.message.to_string()).collect::<Vec<_>>().join("; "))?;
        // Bound comemo's memo: formulas are many and each is rendered once.
        typst::comemo::evict(30);
        let page = doc.pages.first().ok_or("no page")?;
        if doc.pages.len() > 1 {
            return Err("the formula broke across pages".into());
        }
        // `size` is in points and one point is one pixel here, so the em is `em_px` pixels.
        Ok(typst_render::render(page, 1.0))
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// One interned id for every render: `FileId::new_fake` would intern a new one each time, and
/// the interner is bounded.
fn main_id() -> FileId {
    static ID: OnceLock<FileId> = OnceLock::new();
    *ID.get_or_init(|| FileId::new(None, VirtualPath::new("/maths.typ")))
}

struct OneShot<'a> {
    engine: &'a Engine,
    source: Source,
}

impl World for OneShot<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.engine.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.engine.book
    }

    fn main(&self) -> FileId {
        self.engine.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.engine.main {
            Ok(self.source.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn file(&self, _id: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.engine.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        None
    }
}
