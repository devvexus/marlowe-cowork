//! The Message box beyond typing: sending (slash commands, `!` shell, queued input, image
//! attachments), the `/` and `@` completion popups, history and Ctrl+R search, and paste.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::Request;

use crate::attach::{self, Attachment, Upload};
use crate::coding::{Effect, ImageSource, Search};
use crate::slash::{self, Outcome};
use crate::state::{Entry, Focus, NoteTone, Overlay, State, View};

/// Rows the `@` popup lists.
pub const MENTION_ROWS: usize = 8;
/// Rows the history search lists.
pub const SEARCH_ROWS: usize = 6;

/// A completion list above the Message box.
#[derive(Debug, Clone, PartialEq)]
pub enum Popup {
    Slash { rows: Vec<slash::Row> },
    Mention { query: String, matches: Vec<String>, indexing: bool },
    Search { query: String, matches: Vec<String>, sel: usize },
}

impl Popup {
    pub fn len(&self) -> usize {
        match self {
            Popup::Slash { rows } => rows.len(),
            Popup::Mention { matches, .. } | Popup::Search { matches, .. } => matches.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How well `q` matches `path` (higher is better), or `None`. A name that contains the query
/// beats a path that does; letters in order, with gaps, come last.
pub fn fuzzy(q: &str, path: &str) -> Option<i32> {
    let p = path.to_lowercase();
    let q = q.to_lowercase();
    if q.is_empty() {
        return Some(-(p.len() as i32));
    }
    let base = p.rsplit('/').next().unwrap_or(&p);
    if base.starts_with(&q) {
        return Some(3_000 - base.len() as i32);
    }
    if base.contains(&q) {
        return Some(2_000 - base.len() as i32);
    }
    if p.contains(&q) {
        return Some(1_000 - p.len() as i32);
    }
    let mut it = p.chars();
    let mut gaps = 0;
    for c in q.chars() {
        let mut skipped = 0;
        loop {
            match it.next() {
                Some(x) if x == c => break,
                Some(_) => skipped += 1,
                None => return None,
            }
        }
        gaps += skipped;
    }
    Some(500 - gaps - p.len() as i32)
}

/// The best `limit` of `files` for `q`.
pub fn best_matches(q: &str, files: &[String], limit: usize) -> Vec<String> {
    let mut scored: Vec<(i32, &String)> = files.iter().filter_map(|f| fuzzy(q, f).map(|s| (s, f))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}

/// `@partial` before the cursor: its byte range in the text and what follows the `@`.
fn mention_at(text: &str, cursor: usize) -> Option<(usize, String)> {
    let before = &text[..cursor];
    let start = before.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
    let tok = &before[start..];
    let rest = tok.strip_prefix('@')?;
    Some((start, rest.trim_start_matches('"').to_string()))
}

impl State {
    /// The completion list open above the Message box, if any.
    pub fn popup(&self) -> Option<Popup> {
        if let Some(s) = &self.code.search {
            let matches = self.code.history.search(&s.query).into_iter().take(SEARCH_ROWS * 4).map(str::to_string).collect();
            return Some(Popup::Search { query: s.query.clone(), matches, sel: s.sel });
        }
        if self.view != View::Conversation || self.overlay.is_some() || self.focus != Focus::Message || !self.approvals.is_empty() {
            return None;
        }
        let text = self.composer.text();
        if self.code.popup_dismissed.as_deref() == Some(text) {
            return None;
        }
        if text.starts_with('/') && !text.contains(char::is_whitespace) && self.composer.cursor() == text.len() {
            let rows = slash::rows(&text[1..], &self.code.commands);
            return (!rows.is_empty()).then_some(Popup::Slash { rows });
        }
        if let Some((_, query)) = mention_at(text, self.composer.cursor()) {
            return match &self.code.file_index {
                Some(files) => {
                    let matches = best_matches(&query, files, MENTION_ROWS);
                    (!matches.is_empty()).then_some(Popup::Mention { query, matches, indexing: false })
                }
                None => Some(Popup::Mention { query, matches: Vec::new(), indexing: true }),
            };
        }
        None
    }

    /// Called after the composer's text changed: the popup starts at its first row, and an `@`
    /// asks for the file list once.
    pub(crate) fn composer_edited(&mut self) {
        self.code.popup_sel = 0;
        self.code.history.stop_walking();
        let text = self.composer.text();
        if self.code.file_index.is_none() && self.code.index_root.is_none() && mention_at(text, self.composer.cursor()).is_some() {
            if let Some(root) = self.shown_workspace().map(str::to_string) {
                self.code.index_root = Some(root.clone());
                self.code.effects.push(Effect::IndexFiles { root });
            }
        }
    }

    /// Keys for the open popup. True when it took the key.
    pub(crate) fn popup_key(&mut self, popup: &Popup, k: KeyEvent, out: &mut Vec<Request>) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Popup::Search { .. } = popup {
            return self.search_key(k);
        }
        let n = popup.len();
        match k.code {
            KeyCode::Up => {
                self.code.popup_sel = self.code.popup_sel.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.code.popup_sel = (self.code.popup_sel + 1).min(n.saturating_sub(1));
                true
            }
            KeyCode::Esc => {
                self.code.popup_dismissed = Some(self.composer.text().to_string());
                true
            }
            KeyCode::Tab | KeyCode::Enter if !ctrl && n > 0 && !k.modifiers.contains(KeyModifiers::SHIFT) => {
                let sel = self.code.popup_sel.min(n - 1);
                match popup {
                    Popup::Slash { rows } => {
                        let row = &rows[sel];
                        if k.code == KeyCode::Enter && !row.needs_args {
                            let line = format!("/{}", row.name);
                            self.composer.clear();
                            self.finish_line(&line, out);
                        } else {
                            self.composer.set(&format!("/{} ", row.name));
                            self.composer_edited();
                        }
                    }
                    Popup::Mention { matches, .. } => {
                        let path = matches[sel].clone();
                        self.insert_mention(&path, out);
                    }
                    Popup::Search { .. } => {}
                }
                true
            }
            _ => false,
        }
    }

    /// Replaces the `@partial` under the cursor with the chosen file. An image becomes an
    /// attachment instead.
    fn insert_mention(&mut self, path: &str, _out: &mut Vec<Request>) {
        let text = self.composer.text().to_string();
        let cursor = self.composer.cursor();
        let Some((start, _)) = mention_at(&text, cursor) else { return };
        if attach::image_mime(path).is_some() {
            self.composer.replace_range(start, cursor, "");
            let abs = match self.shown_workspace() {
                Some(ws) => format!("{}/{}", ws.trim_end_matches(['/', '\\']), path),
                None => path.to_string(),
            };
            self.code.effects.push(Effect::ReadImage { path: abs });
            return;
        }
        let shown = if path.contains(' ') { format!("@\"{path}\" ") } else { format!("@{path} ") };
        self.composer.replace_range(start, cursor, &shown);
        self.composer_edited();
    }

    // ---- Ctrl+R ------------------------------------------------------------------------------------

    /// Ctrl+R: opens the search, or, with it open, goes to the next older match.
    pub(crate) fn open_search(&mut self) {
        match self.code.search.as_mut() {
            None => {
                self.code.search = Some(Search { query: String::new(), sel: 0, original: self.composer.text().to_string() });
            }
            Some(s) => {
                let n = self.code.history.search(&s.query).len();
                s.sel = (s.sel + 1).min(n.saturating_sub(1));
            }
        }
    }

    fn search_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(s) = self.code.search.as_mut() else { return false };
        let matches = self.code.history.search(&s.query);
        match k.code {
            KeyCode::Esc => {
                let original = s.original.clone();
                self.code.search = None;
                self.composer.set(&original);
            }
            KeyCode::Char('c') if ctrl => {
                let original = s.original.clone();
                self.code.search = None;
                self.composer.set(&original);
            }
            KeyCode::Char('r') if ctrl => s.sel = (s.sel + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Up => s.sel = (s.sel + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Down => s.sel = s.sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Tab => {
                let pick = matches.get(s.sel).map(|m| m.to_string());
                let original = s.original.clone();
                self.code.search = None;
                self.composer.set(&pick.unwrap_or(original));
                self.composer_edited();
            }
            KeyCode::Backspace => {
                s.query.pop();
                s.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                s.query.push(c);
                s.sel = 0;
            }
            _ => {}
        }
        true
    }

    // ---- sending -----------------------------------------------------------------------------------

    /// Enter in the Message box.
    pub(crate) fn send(&mut self, out: &mut Vec<Request>) {
        if self.composer.is_blank() {
            if self.library.known && !self.library.has_models() {
                self.view = View::Library;
                self.library.start_add();
            }
            return;
        }
        let line = self.composer.text().trim_end().to_string();
        // Commands that need no model and no connection run offline too.
        let local_command = slash::parse(&line).is_some_and(|(n, _)| slash::builtin(&n).is_some());
        if !self.conn.is_online() && !local_command {
            self.flash("offline — your message is still here; it will send once reconnected", NoteTone::Amber);
            return;
        }
        if !local_command && self.library.known && !self.library.has_models() {
            self.flash("add a model first — opening the library", NoteTone::Amber);
            self.view = View::Library;
            self.library.start_add();
            return;
        }
        self.composer.clear();
        self.finish_line(&line, out);
    }

    /// A line the user sent: a `/command`, a `!shell`, or a message.
    fn finish_line(&mut self, line: &str, out: &mut Vec<Request>) {
        if slash::parse(line).is_some() {
            match self.run_slash(line, out) {
                Outcome::Handled => {
                    self.code.history.push(line);
                    return;
                }
                Outcome::Unknown(name) => {
                    self.composer.set(line);
                    self.flash(format!("unknown command /{} · /help lists them", crate::text::clean(&name)), NoteTone::Amber);
                    return;
                }
                Outcome::Forward => {}
            }
        } else if let Some(cmd) = line.strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
            self.code.history.push(line);
            self.user_shell(cmd, out);
            return;
        }
        self.code.history.push(line);
        if self.is_busy() && self.session.is_some() {
            // Held until the model finishes its step; Up takes it back.
            self.code.queue.push(line.to_string());
            self.scroll = 0;
            return;
        }
        self.ask(line.to_string(), out);
    }

    /// Sends `text` as a message, with the attachments it still names.
    pub(crate) fn ask(&mut self, text: String, out: &mut Vec<Request>) {
        let atts: Vec<Attachment> = self.code.attachments.iter().filter(|a| text.contains(&a.marker())).cloned().collect();
        self.code.attachments.clear();
        if !atts.is_empty() {
            let (up, reqs) = Upload::start(self.session.clone(), text.clone(), self.default_workspace.clone().filter(|_| self.session.is_none()), &atts);
            if self.session.is_none() {
                self.expect_new_session(&text);
            }
            self.code.upload = Some(up);
            out.extend(reqs);
            self.scroll = 0;
            return;
        }
        match self.session.clone() {
            Some(s) => {
                out.push(Request::Ask { session: Some(s), text, workspace: None, images: Vec::new(), options: None });
            }
            None => {
                self.expect_new_session(&text);
                out.push(Request::Ask {
                    session: None,
                    text,
                    workspace: self.default_workspace.clone(),
                    images: Vec::new(),
                    options: None,
                });
            }
        }
        self.scroll = 0;
    }

    /// The queued messages go in at a step boundary (a tool finished) as steers, or, once the turn
    /// is over, one at a time as new turns.
    pub(crate) fn flush_queue(&mut self, out: &mut Vec<Request>) {
        let Some(session) = self.session.clone() else { return };
        if self.code.queue.is_empty() {
            return;
        }
        if self.is_busy() {
            for text in std::mem::take(&mut self.code.queue) {
                self.entries.push(Entry::User { text: text.clone(), steer: true });
                out.push(Request::Ask { session: Some(session.clone()), text, workspace: None, images: Vec::new(), options: None });
            }
        } else {
            let text = self.code.queue.remove(0);
            self.ask(text, out);
        }
    }

    /// Up with queued messages and an empty box: the newest comes back for editing.
    pub(crate) fn unqueue(&mut self) -> bool {
        if self.composer.is_blank() {
            if let Some(t) = self.code.queue.pop() {
                self.composer.set(&t);
                return true;
            }
        }
        false
    }

    /// `!cmd`: runs as the user, its output into the context.
    fn user_shell(&mut self, cmd: &str, out: &mut Vec<Request>) {
        match self.session.clone() {
            Some(session) => out.push(Request::UserShell { session, command: cmd.to_string() }),
            None => {
                self.code.pending_shell = Some(cmd.to_string());
                self.expect_new_session(&format!("! {cmd}"));
                out.push(Request::NewSession { workspace: self.default_workspace.clone() });
            }
        }
        self.scroll = 0;
    }

    /// Ctrl+V and Alt+P: look for an image on the clipboard.
    pub(crate) fn paste_clipboard_image(&mut self) {
        self.code.effects.push(Effect::ClipboardImage);
    }

    /// Text pasted into the Message box. A dropped image path is an attachment; an empty paste
    /// (an image-only clipboard in some terminals) looks for an image.
    pub(crate) fn paste_into_composer(&mut self, text: &str) {
        self.focus = Focus::Message;
        if text.is_empty() {
            self.paste_clipboard_image();
            return;
        }
        if let Some(p) = attach::dropped_image_path(text) {
            self.code.effects.push(Effect::ReadImage { path: p });
            return;
        }
        self.composer.insert_str(text);
        self.composer_edited();
    }

    /// An image arrived (or did not): it becomes a `[Image #n]` marker in the box.
    pub(crate) fn image_read(&mut self, source: ImageSource, result: Result<(String, Vec<u8>), String>) {
        match result {
            Ok((mime, bytes)) => {
                if bytes.len() > attach::MAX_IMAGE_BYTES {
                    self.flash("that image is over 12 MB", NoteTone::Amber);
                    return;
                }
                self.code.next_image += 1;
                let label = match &source {
                    ImageSource::Clipboard => "clipboard".to_string(),
                    ImageSource::Path(p) => p.clone(),
                };
                let a = Attachment::new(self.code.next_image, &mime, bytes, &label);
                self.composer.insert_str(&format!("{} ", a.marker()));
                self.flash(a.chip(), NoteTone::Dim);
                self.code.attachments.push(a);
                self.focus = Focus::Message;
            }
            Err(e) => match source {
                ImageSource::Clipboard => {
                    let why = if e.trim().is_empty() { "no image on the clipboard".to_string() } else { crate::text::clean(&e) };
                    self.flash(why, NoteTone::Amber);
                }
                // Not an image after all: what was pasted or picked goes in as text.
                ImageSource::Path(p) => {
                    self.composer.insert_str(&format!("{p} "));
                    self.composer_edited();
                }
            },
        }
    }

    /// Shows the open popup's top-level overlay state for the footer: true while one is up.
    pub fn popup_open(&self) -> bool {
        self.overlay != Some(Overlay::Panel) && self.popup().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_beats_a_path_beats_scattered_letters() {
        let files: Vec<String> =
            ["src/render/mod.rs", "src/model.rs", "docs/old-model-notes.md", "crates/x/src/mod_rs.txt", "README.md"].iter().map(|s| s.to_string()).collect();
        assert_eq!(best_matches("model", &files, 3)[0], "src/model.rs");
        assert_eq!(best_matches("mod", &files, 5)[0], "src/render/mod.rs");
        assert!(best_matches("zzz", &files, 5).is_empty());
        assert_eq!(best_matches("rndmod", &files, 5), ["src/render/mod.rs"], "letters in order with gaps");
    }

    #[test]
    fn the_mention_is_the_word_under_the_cursor() {
        assert_eq!(mention_at("look at @src/ma", 15), Some((8, "src/ma".into())));
        assert_eq!(mention_at("@", 1), Some((0, String::new())));
        assert_eq!(mention_at("mail me@x.com", 13), None);
    }
}
