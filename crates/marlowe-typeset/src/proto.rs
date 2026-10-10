//! **The helper's wire format**: one JSON object per line over the helper's stdin and stdout.
//!
//! A request is one formula to render into one box; the reply carries its Sixel strips or an
//! error. Replies come back in order, one per request, with the request's `id`.
//!
//! The strips go straight to the terminal, so the client checks every one ([`valid_strip`])
//! before keeping it: a Sixel image and nothing else, whatever the process on the other end wrote.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::service::{Caps, Image, Job};
use crate::sixel::Coverage;

/// Bumped when a field changes meaning. A helper answers a request of another version with an
/// error, and the client then leaves every formula on the grid.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    /// The equation body as Typst math markup, produced by `marlowe_surface::math` from the
    /// parsed formula with every character escaped. Never LaTeX, never model text as code.
    pub markup: String,
    /// The glyph colour.
    pub fg: [u8; 3],
    /// The terminal background, for blending anti-aliased edges.
    pub bg: [u8; 3],
    /// One cell, in pixels: (width, height).
    pub cell: [u16; 2],
    /// The box the grid layout reserved, in cells.
    pub cols: u16,
    pub rows: u16,
}

impl Request {
    pub fn new(id: u64, job: &Job, caps: Caps) -> Request {
        Request {
            v: VERSION,
            id,
            markup: job.markup.clone(),
            fg: job.fg,
            bg: caps.bg,
            cell: [caps.cell_w, caps.cell_h],
            cols: job.cols,
            rows: job.rows,
        }
    }

    pub fn job(&self) -> Job {
        Job { markup: self.markup.clone(), fg: self.fg, cols: self.cols, rows: self.rows }
    }

    pub fn caps(&self) -> Caps {
        Caps { cell_w: self.cell[0], cell_h: self.cell[1], fg: self.fg, bg: self.bg }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    /// Columns the image covers, when it rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// One Sixel image per reserved row, top to bottom; an empty string is a row with no ink.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strips: Vec<String>,
    /// Why it did not render. The formula stays on the grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, img: &Image) -> Response {
        let strips = img.strips.iter().map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        Response { id, cols: Some(img.cols), strips, error: None }
    }

    pub fn err(id: u64, e: impl Into<String>) -> Response {
        Response { id, cols: None, strips: Vec::new(), error: Some(e.into()) }
    }

    /// The image, if this is a well-formed answer for a box `rows` high: one strip per row, each
    /// a valid strip.
    pub fn image(&self, rows: u16) -> Result<Image, String> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let cols = self.cols.ok_or("no columns")?;
        if self.strips.len() != usize::from(rows) {
            return Err(format!("{} strips for {rows} rows", self.strips.len()));
        }
        let mut strips = Vec::with_capacity(self.strips.len());
        for s in &self.strips {
            if !s.is_empty() && !valid_strip(s.as_bytes()) {
                return Err("a strip that is not a plain Sixel image".into());
            }
            strips.push(std::sync::Arc::<[u8]>::from(s.as_bytes()));
        }
        Ok(Image { cols, strips })
    }
}

/// `ESC P … q … ESC \` with only printable ASCII between: a Sixel image, and no other control
/// sequence can ride along to the terminal.
pub fn valid_strip(b: &[u8]) -> bool {
    let Some(body) = b.strip_prefix(b"\x1bP").and_then(|r| r.strip_suffix(b"\x1b\\")) else {
        return false;
    };
    body.contains(&b'q') && body.iter().all(|c| (0x20..=0x7e).contains(c))
}

/// The helper's side: answers each request line on `input` with one reply line on `out`, until
/// `input` ends. `typeset` is Typst in the helper binary. A request that panics the typesetter is
/// an error reply, not a dead helper.
pub fn serve(
    input: impl BufRead,
    mut out: impl Write,
    typeset: &mut dyn FnMut(&str, f32, [u8; 3]) -> Result<Coverage, String>,
) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(0, format!("bad request: {e}")),
            Ok(r) if r.v != VERSION => Response::err(r.id, format!("protocol {} (this helper speaks {VERSION})", r.v)),
            Ok(r) => {
                let (job, caps) = (r.job(), r.caps());
                let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::fit::image(&job, caps, typeset)));
                match made {
                    Ok(Ok(img)) => Response::ok(r.id, &img),
                    Ok(Err(e)) => Response::err(r.id, e),
                    Err(_) => Response::err(r.id, "the typesetter panicked"),
                }
            }
        };
        let text = serde_json::to_string(&resp).map_err(io::Error::other)?;
        out.write_all(text.as_bytes())?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Caps {
        Caps { cell_w: 10, cell_h: 20, fg: [1, 2, 3], bg: [4, 5, 6] }
    }

    #[test]
    fn a_request_round_trips_as_one_line() {
        let job = Job { markup: "frac(a, b) \\\n \"x\"".into(), fg: [7, 8, 9], cols: 12, rows: 3 };
        let r = Request::new(5, &job, caps());
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains('\n'), "{line}");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.job(), job);
        assert_eq!(back.caps().bg, [4, 5, 6]);
    }

    #[test]
    fn only_plain_sixel_reaches_the_terminal() {
        let good = "\x1bP0;1;0q\"1;1;4;6#1;2;0;0;0#1!4~\x1b\\";
        assert!(valid_strip(good.as_bytes()));
        // Another escape sequence smuggled inside the image, or a title set after it.
        assert!(!valid_strip(b"\x1bP0;1;0q\x1b]0;pwned\x07\x1b\\"));
        assert!(!valid_strip(b"\x1b]0;pwned\x1b\\"));
        assert!(!valid_strip(b"\x1bP0;1;0q~~\x1b\\\x1b[2J"));
        let resp = Response { id: 1, cols: Some(4), strips: vec![good.into(), String::new()], error: None };
        let img = resp.image(2).unwrap();
        assert_eq!(img.strips.len(), 2);
        assert!(resp.image(3).is_err(), "a strip per reserved row, or nothing");
        let bad = Response { strips: vec!["\x1b[2J".into(), String::new()], ..resp };
        assert!(bad.image(2).is_err());
        assert_eq!(Response::err(2, "no").image(1).unwrap_err(), "no");
    }

    #[test]
    fn the_helper_loop_answers_each_line_in_order_and_survives_a_panic() {
        let job = Job { markup: "ok".into(), fg: [200, 200, 200], cols: 4, rows: 2 };
        let boom = Job { markup: "boom".into(), ..job.clone() };
        let mut input = String::new();
        for (id, j) in [(1, &job), (2, &boom), (3, &job)] {
            input.push_str(&serde_json::to_string(&Request::new(id, j, caps())).unwrap());
            input.push('\n');
        }
        input.push_str("{not json\n");
        let mut out: Vec<u8> = Vec::new();
        let mut ts = |m: &str, em: f32, _: [u8; 3]| -> Result<Coverage, String> {
            assert!(m != "boom", "typst fell over");
            let (w, h) = ((em * 2.0) as usize, em as usize);
            Ok(Coverage { w, h, alpha: vec![255; w * h] })
        };
        serve(input.as_bytes(), &mut out, &mut ts).unwrap();
        let replies: Vec<Response> =
            String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3, 0]);
        assert_eq!(replies[0].image(2).unwrap().strips.len(), 2);
        assert!(replies[1].error.as_deref().unwrap().contains("panicked"));
        assert!(replies[2].image(2).is_ok(), "the helper is still serving after a panic");
        assert!(replies[3].error.is_some());
    }
}
