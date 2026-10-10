//! **What the terminal can show**, read from its answers to four queries.
//!
//! | query | answer | used for |
//! |---|---|---|
//! | `OSC 10 ; ?` | the default foreground | maths drawn in the terminal's own foreground weight |
//! | `OSC 11 ; ?` | the background colour | blending glyph edges, so the image reads as transparent |
//! | `CSI 16 t` | the cell size in pixels | sizing the image to whole cells |
//! | `CSI c` (DA1) | attribute `4` if Sixel is supported | whether images are possible at all |
//!
//! DA1 goes last because every terminal answers it: once its reply is in, nothing else is coming.
//! The I/O is the caller's (`marlowe_os::console::query`); this module only writes and reads text.

/// The queries, in the order they must be sent.
pub const QUERIES: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[16t\x1b[c";

/// The background Marlowe configures for its Windows Terminal profile, used when the terminal
/// does not say.
pub const DEFAULT_BG: [u8; 3] = [0x0f, 0x0e, 0x14];
/// The foreground that profile sets: the palette's normal weight.
pub const DEFAULT_FG: [u8; 3] = [0xcd, 0xc9, 0xd6];
/// Windows Terminal lays Sixel out on a 10×20 pixel cell, the VT340's, whatever the font.
pub const DEFAULT_CELL: (u16, u16) = (10, 20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub sixel: bool,
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    /// (width, height) of one cell, in pixels.
    pub cell: Option<(u16, u16)>,
}

/// Whether the DA1 answer has arrived, which ends the exchange.
pub fn complete(s: &str) -> bool {
    da1(s).is_some()
}

pub fn parse(s: &str) -> Reply {
    Reply {
        sixel: da1(s).is_some_and(|attrs| attrs.iter().skip(1).any(|a| *a == "4")),
        fg: osc_colour(s, "10"),
        bg: osc_colour(s, "11"),
        cell: cell_size(s),
    }
}

/// What a reply means for images: the terminal's caps with defaults for what it did not say, or
/// `None` when it cannot show Sixel (unless `force`, for a terminal that can without saying so).
pub fn caps(reply: &str, force: bool) -> Option<crate::Caps> {
    let r = parse(reply);
    if !r.sixel && !force {
        return None;
    }
    let (cell_w, cell_h) = r.cell.unwrap_or(DEFAULT_CELL);
    Some(crate::Caps { cell_w, cell_h, fg: r.fg.unwrap_or(DEFAULT_FG), bg: r.bg.unwrap_or(DEFAULT_BG) })
}

/// `ESC [ ? 61 ; 4 ; 6 c` as its attributes.
fn da1(s: &str) -> Option<Vec<&str>> {
    let start = s.find("\x1b[?")? + 3;
    let rest = &s[start..];
    let end = rest.find('c')?;
    let body = &rest[..end];
    if !body.chars().all(|c| c.is_ascii_digit() || c == ';') {
        return None;
    }
    Some(body.split(';').collect())
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` followed by `ESC \` or BEL (`10` for the foreground). Each
/// channel has one to four hex digits and is scaled to eight bits.
fn osc_colour(s: &str, which: &str) -> Option<[u8; 3]> {
    let head = format!("\x1b]{which};rgb:");
    let start = s.find(&head)? + head.len();
    let rest = &s[start..];
    let end = rest.find(['\x1b', '\x07'])?;
    let mut out = [0u8; 3];
    let parts: Vec<&str> = rest[..end].split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(p, 16).ok()?;
        let max = (1u32 << (4 * p.len())) - 1;
        out[i] = ((v * 255 + max / 2) / max) as u8;
    }
    Some(out)
}

/// `ESC [ 6 ; height ; width t`.
fn cell_size(s: &str) -> Option<(u16, u16)> {
    let start = s.find("\x1b[6;")? + 4;
    let rest = &s[start..];
    let end = rest.find('t')?;
    let mut it = rest[..end].split(';');
    let h: u16 = it.next()?.parse().ok()?;
    let w: u16 = it.next()?.parse().ok()?;
    (w >= 4 && h >= 6 && w <= 200 && h <= 400).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_windows_terminal_answer() {
        let s = "\x1b]10;rgb:cdcd/c9c9/d6d6\x1b\\\x1b]11;rgb:0f0f/0e0e/1414\x1b\\\x1b[6;20;10t\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c";
        assert!(complete(s));
        let want = Reply { sixel: true, fg: Some([0xcd, 0xc9, 0xd6]), bg: Some([0x0f, 0x0e, 0x14]), cell: Some((10, 20)) };
        assert_eq!(parse(s), want);
    }

    #[test]
    fn a_terminal_without_sixel_or_the_optional_answers() {
        let s = "\x1b[?62;22c";
        assert_eq!(parse(s), Reply { sixel: false, fg: None, bg: None, cell: None });
        // `4` as the device class is not the Sixel attribute.
        assert!(!parse("\x1b[?4;22c").sixel);
        assert!(!complete("\x1b]11;rgb:00/00/00\x07"));
    }

    #[test]
    fn colour_channels_of_any_width_scale_to_eight_bits() {
        assert_eq!(osc_colour("\x1b]11;rgb:f/8/0\x07", "11"), Some([255, 136, 0]));
        assert_eq!(osc_colour("\x1b]11;rgb:ffff/8080/0000\x1b\\", "11"), Some([255, 128, 0]));
        assert_eq!(osc_colour("\x1b]11;rgb:zz/00/00\x07", "11"), None);
        assert_eq!(osc_colour("\x1b]11;rgb:ff/00/00\x07", "10"), None, "the background is not the foreground");
    }
}
