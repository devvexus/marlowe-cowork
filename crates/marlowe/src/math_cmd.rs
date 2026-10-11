//! `marlowe math [--grid | --images] [--width N]`: the maths gallery, for eye-checking a real
//! terminal.
//!
//! Each formula in [`math::GALLERY`] goes through the same path as a reply in the conversation
//! pane: `markdown::render_prose_blocks` at the pane's text width, then, where the terminal speaks
//! Sixel, `marlowe-typeset.exe` renders the block into the rows its grid layout reserved and the
//! strips are placed one per row, as the TUI places them. By default both are shown, the grid
//! first; `--grid` or `--images` shows one. Anything the image path cannot do is said, and that
//! formula is shown on the grid.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use marlowe_surface::markdown::{self, MathBlock};
use marlowe_surface::{math, Theme};
use marlowe_typeset::helper::{self, Helper};
use marlowe_typeset::{probe, Caps, Job};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

const USAGE: &str = "marlowe math [--grid | --images] [--width N]";

pub fn run(args: &[String]) -> Result<i32, String> {
    let (mut grid, mut images, mut width) = (true, true, None::<usize>);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--grid" => images = false,
            "--images" => grid = false,
            "--width" => {
                let v = it.next().ok_or("--width needs a value")?;
                width = Some(v.parse().map_err(|_| format!("--width {v:?} is not a number"))?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            other => return Err(format!("unknown option {other:?}\n{USAGE}")),
        }
    }
    if !grid && !images {
        return Err(format!("--grid and --images together show nothing\n{USAGE}"));
    }
    let cols = width.or_else(|| crossterm::terminal::size().ok().map(|(w, _)| usize::from(w))).unwrap_or(100);
    // The conversation pane's text column: two columns of padding, a gap and the scrollbar.
    let tw = cols.saturating_sub(4).max(24);
    let theme = Theme::default_truecolor();
    let base = Style::default().fg(theme.speech());
    let dim = Style::default().fg(theme.speech()).add_modifier(Modifier::DIM);

    let mut out = io::stdout().lock();
    let mut img: Option<(Helper, Caps)> = None;
    if images {
        match image_setup() {
            Ok((h, caps, reply)) => {
                // What the terminal said, raw, and what was read from it: Windows Terminal should
                // answer `16 t` with its 10x20 Sixel cell whatever the font.
                let said = format!(
                    "terminal: cell {}x{} px, fg #{:02x}{:02x}{:02x}, bg #{:02x}{:02x}{:02x}; reply {}",
                    caps.cell_w, caps.cell_h, caps.fg[0], caps.fg[1], caps.fg[2], caps.bg[0], caps.bg[1], caps.bg[2],
                    reply.escape_debug()
                );
                line(&mut out, &Line::styled(said, dim))?;
                img = Some((h, caps));
            }
            Err(why) => {
                line(&mut out, &Line::styled(format!("images off: {why}. Showing the grid."), dim))?;
                grid = true;
            }
        }
    }
    let mut timings: Vec<f64> = Vec::new();
    for (k, (title, src)) in math::GALLERY.iter().enumerate() {
        line(&mut out, &Line::default())?;
        line(&mut out, &Line::styled(format!("{:>2}. {title}", k + 1), dim))?;
        let (lines, blocks) = markdown::render_prose_blocks(&format!("$$\n{src}\n$$"), tw, &theme, base, false);
        if grid {
            for l in &lines {
                line(&mut out, l)?;
            }
        }
        let Some((h, caps)) = img.as_mut() else { continue };
        let note = match blocks.first() {
            None => Some("no image: at this width it is set as lines".to_string()),
            Some(b) => {
                let t = Instant::now();
                match render(h, *caps, b, tw) {
                    Ok(strips) => {
                        timings.push(t.elapsed().as_secs_f64() * 1e3);
                        if grid {
                            line(&mut out, &Line::default())?;
                        }
                        place(&mut out, &lines, b, &strips)?;
                        None
                    }
                    Err(e) => Some(format!("stays on the grid: {e}")),
                }
            }
        };
        if let Some(n) = note {
            if !grid {
                for l in &lines {
                    line(&mut out, l)?;
                }
            }
            line(&mut out, &Line::styled(format!("    ({n})"), dim))?;
        }
    }
    if !timings.is_empty() {
        let cold = timings[0];
        let mut warm = timings[1..].to_vec();
        warm.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = warm.get(warm.len() / 2).copied().unwrap_or(cold);
        line(&mut out, &Line::default())?;
        line(
            &mut out,
            &Line::styled(
                format!("helper: first formula {cold:.0} ms (start included), then p50 {p50:.1} ms; {} images", timings.len()),
                dim,
            ),
        )?;
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

/// The helper, the terminal's caps and its raw reply to the probe, or why there are no images.
fn image_setup() -> Result<(Helper, Caps, String), String> {
    let setting = std::env::var("MARLOWE_MATH_IMAGES").unwrap_or_default();
    if setting == "0" {
        return Err("MARLOWE_MATH_IMAGES=0".into());
    }
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err("not a terminal".into());
    }
    let path = helper::locate().ok_or_else(|| format!("no {} beside this executable", helper::HELPER_NAME))?;
    let reply = marlowe_os::console::query(probe::QUERIES, probe::complete, Duration::from_millis(400))
        .map_err(|e| format!("the terminal did not answer ({e})"))?;
    let caps = probe::caps(&reply, setting == "1").ok_or("the terminal does not report Sixel (DA1 has no 4)")?;
    Ok((Helper::new(path), caps, reply))
}

/// The block's strips, one per reserved row, from the helper. The box is the block's rows and the
/// text column's width from the block's column, as in the pane.
fn render(h: &mut Helper, caps: Caps, b: &MathBlock, tw: usize) -> Result<Vec<std::sync::Arc<[u8]>>, String> {
    let markup = b.formula.typst.clone().ok_or("no Typst form")?;
    // The grid's colour, or the terminal's own foreground for the default weight: as the pane.
    let fg = match b.fg {
        Color::Rgb(r, g, bl) => [r, g, bl],
        _ => caps.fg,
    };
    let job = Job { markup, fg, cols: tw.saturating_sub(b.col) as u16, rows: b.rows as u16 };
    Ok(h.render(&job, caps)?.strips)
}

/// The rendered lines with the block's rows replaced by its strips, each placed at the block's
/// column with the cursor saved and restored around it, as the TUI's emitter does.
fn place(out: &mut impl Write, lines: &[Line<'static>], b: &MathBlock, strips: &[std::sync::Arc<[u8]>]) -> Result<(), String> {
    for (k, l) in lines.iter().enumerate() {
        if k < b.line || k >= b.line + b.rows {
            line(out, l)?;
            continue;
        }
        let strip = &strips[k - b.line];
        let mut bytes: Vec<u8> = Vec::new();
        if !strip.is_empty() {
            bytes.extend_from_slice(format!("\x1b7\x1b[{}G", b.col + 1).as_bytes());
            bytes.extend_from_slice(strip);
            bytes.extend_from_slice(b"\x1b8");
        }
        bytes.push(b'\n');
        out.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One styled line as ANSI: truecolour foregrounds and the modifiers the pane uses, no
/// background.
fn line(out: &mut impl Write, l: &Line<'_>) -> Result<(), String> {
    let mut s = String::new();
    for span in &l.spans {
        let st = l.style.patch(span.style);
        let mut codes: Vec<String> = Vec::new();
        if let Some(Color::Rgb(r, g, b)) = st.fg {
            codes.push(format!("38;2;{r};{g};{b}"));
        }
        for (m, c) in [(Modifier::BOLD, "1"), (Modifier::DIM, "2"), (Modifier::ITALIC, "3"), (Modifier::UNDERLINED, "4")] {
            if st.add_modifier.contains(m) {
                codes.push(c.into());
            }
        }
        if codes.is_empty() {
            s.push_str(&span.content);
        } else {
            s.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.content));
        }
    }
    writeln!(out, "{}", s.trim_end()).map_err(|e| e.to_string())
}
