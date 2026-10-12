//! The message composer: a small multi-line text buffer with a cursor.

use unicode_width::UnicodeWidthChar;

/// Most lines the composer grows to before it scrolls.
pub const MAX_VISIBLE_LINES: usize = 6;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    /// Takes the text out, leaving the composer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    /// Replaces the bytes `[start, end)` with `s` and puts the cursor after it.
    pub fn replace_range(&mut self, start: usize, end: usize, s: &str) {
        let end = end.min(self.text.len());
        let start = start.min(end);
        self.text.replace_range(start..end, s);
        self.cursor = start + s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    /// Deletes the word before the cursor.
    pub fn backspace_word(&mut self) {
        while self.text[..self.cursor].ends_with(' ') {
            self.backspace();
        }
        while let Some(c) = self.text[..self.cursor].chars().next_back() {
            if c == ' ' || c == '\n' {
                break;
            }
            self.backspace();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor += self.text[self.cursor..].find('\n').unwrap_or(self.text.len() - self.cursor);
    }

    /// Moves to the previous line. Returns false when already on the first line.
    pub fn up(&mut self) -> bool {
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        if line_start == 0 {
            return false;
        }
        let col = self.text[line_start..self.cursor].chars().count();
        let prev_start = self.text[..line_start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = advance(&self.text, prev_start, line_start - 1, col);
        true
    }

    /// Moves to the next line. Returns false when already on the last line.
    pub fn down(&mut self) -> bool {
        let Some(nl) = self.text[self.cursor..].find('\n').map(|i| i + self.cursor) else {
            return false;
        };
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let col = self.text[line_start..self.cursor].chars().count();
        let next_start = nl + 1;
        let next_end = self.text[next_start..].find('\n').map_or(self.text.len(), |i| i + next_start);
        self.cursor = advance(&self.text, next_start, next_end, col);
        true
    }

    /// Visual lines after wrapping to `width`, and the cursor's (row, col) within them.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut col = 0;
        let mut cur = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cur = (lines.len() - 1, col);
            }
            if c == '\n' {
                lines.push(String::new());
                col = 0;
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if col + cw > width {
                lines.push(String::new());
                col = 0;
            }
            lines.last_mut().expect("non-empty").push(c);
            col += cw;
        }
        if self.cursor >= self.text.len() {
            if col >= width {
                lines.push(String::new());
                col = 0;
            }
            cur = (lines.len() - 1, col);
        }
        (lines, cur)
    }
}

/// Byte offset `col` chars into the line `[start, end)`, clamped to its end.
fn advance(text: &str, start: usize, end: usize, col: usize) -> usize {
    text[start..end].char_indices().nth(col).map_or(end, |(i, _)| start + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_moves() {
        let mut c = Composer::default();
        for ch in "hello".chars() {
            c.insert(ch);
        }
        c.left();
        c.left();
        c.insert('X');
        assert_eq!(c.text(), "helXlo");
        c.backspace();
        c.delete();
        assert_eq!(c.text(), "hello"[..3].to_string() + "o");
        c.end();
        c.insert_str(" world\r\nnext");
        assert_eq!(c.text(), "helo world\nnext");
        assert!(c.up());
        assert!(!c.up());
        assert!(c.down());
        c.home();
        assert_eq!(&c.text()[c.cursor()..], "next");
        c.end();
        c.backspace_word();
        assert_eq!(c.text(), "helo world\n");
        assert_eq!(c.take(), "helo world\n");
        assert!(c.is_blank());
    }

    #[test]
    fn layout_wraps_and_tracks_cursor() {
        let mut c = Composer::default();
        c.set("abcdef\ngh");
        let (lines, cur) = c.layout(4);
        assert_eq!(lines, vec!["abcd", "ef", "gh"]);
        assert_eq!(cur, (2, 2));
        c.set("abcd");
        let (lines, cur) = c.layout(4);
        assert_eq!(lines, vec!["abcd", ""]);
        assert_eq!(cur, (1, 0));
    }
}
