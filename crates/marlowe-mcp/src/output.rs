//! Tool output is capped before it reaches the model: 25,000 tokens by default (about four
//! characters each), matching Claude Code's MCP limit. The text above the cap is cut at a line or
//! character boundary and, when a spill directory exists, the whole output is saved there so the
//! model can `read` the rest.

use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_TOKENS: usize = 25_000;
const CHARS_PER_TOKEN: usize = 4;

/// What [`cap`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capped {
    pub text: String,
    pub truncated: bool,
    /// Where the full output was saved, when it was.
    pub saved: Option<PathBuf>,
}

pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Cuts `text` to `max_tokens`. `spill` is a directory to save the full text in.
pub fn cap(text: &str, max_tokens: usize, label: &str, spill: Option<&Path>) -> Capped {
    let max_chars = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    if text.chars().count() <= max_chars {
        return Capped { text: text.to_string(), truncated: false, saved: None };
    }
    let cut_at = text.char_indices().nth(max_chars).map_or(text.len(), |(i, _)| i);
    let head = &text[..cut_at];
    // Back up to the last line break when one is near the end, so a line is not cut in half.
    let head = match head.rfind('\n') {
        Some(i) if i > head.len() / 2 => &head[..i],
        _ => head,
    };
    let total = estimate_tokens(text);
    let saved = spill.and_then(|dir| {
        std::fs::create_dir_all(dir).ok()?;
        let safe: String = label.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let path = dir.join(format!("{safe}-{}.txt", crate::util::random_token(6)));
        std::fs::write(&path, text).ok()?;
        Some(path)
    });
    let footer = match &saved {
        Some(p) => format!(
            "\n\n[output truncated: about {total} tokens, showing the first {max_tokens}. The full output is in {} (read it in pieces with offset and limit).]",
            p.display()
        ),
        None => format!("\n\n[output truncated: about {total} tokens, showing the first {max_tokens}.]"),
    };
    Capped { text: format!("{head}{footer}"), truncated: true, saved }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passes_through() {
        let c = cap("hello", 10, "x", None);
        assert_eq!((c.text.as_str(), c.truncated), ("hello", false));
    }

    #[test]
    fn large_output_is_cut_at_a_line_and_saved_in_full() {
        let dir = std::env::temp_dir().join(format!("marlowe-mcp-out-{}", crate::util::random_token(6)));
        let body: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let c = cap(&body, 100, "mcp__fs__big", Some(&dir));
        assert!(c.truncated);
        assert!(c.text.starts_with("line 0\n") && c.text.contains("output truncated"));
        let head = c.text.split("\n\n[output truncated").next().unwrap();
        assert!(head.chars().count() <= 400 && head.ends_with(|ch: char| ch.is_ascii_digit()), "cut at a line end: {head:?}");
        assert_eq!(std::fs::read_to_string(c.saved.unwrap()).unwrap(), body);
    }

    #[test]
    fn multibyte_text_is_cut_on_a_character_boundary() {
        let s = "é".repeat(1000);
        let c = cap(&s, 10, "x", None);
        assert!(c.truncated && c.text.starts_with("éé"));
    }
}
