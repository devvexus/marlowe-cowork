//! Clickable `file:line` and URLs (U2): OSC 8 hyperlinks, sent after the frame.
//!
//! Rendering stays a pure function of the state and the buffer holds only what is visible. After
//! a frame is drawn, [`scan`] reads the visible rows for file references and URLs, and
//! [`LinkEmitter::frame`] re-prints those cells wrapped in `ESC ] 8 ; ; url ST` ... `ESC ] 8 ; ; ST`.
//! The text the user sees is untouched; a terminal without OSC 8 ignores the wrapper.
//!
//! A file reference opens through the `editor_url` setting, a template with `{path}`, `{line}` and
//! `{col}` (`vscode://file/{path}:{line}:{col}`); without one it is a `file:///` URL. Only text
//! already on screen is linked, and the URL is percent-encoded, so nothing a model wrote can put
//! an escape sequence in the link.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

/// Where links go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkConfig {
    /// `editor_url` from the settings.
    pub editor_url: Option<String>,
    /// Relative references resolve against this.
    pub workspace: Option<String>,
}

/// One linked run of cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRegion {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub url: String,
}

/// What a token on screen refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Url(String),
    File { path: String, line: Option<u32>, col: Option<u32> },
}

/// A reference found in a string: char offsets `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub start: usize,
    pub end: usize,
    pub what: Reference,
}

const KNOWN_EXT: &[&str] = &[
    "rs", "toml", "md", "py", "js", "ts", "tsx", "jsx", "json", "yaml", "yml", "c", "h", "cc", "cpp", "hpp", "go", "java",
    "kt", "cs", "rb", "sh", "ps1", "txt", "html", "css", "lock", "ini", "cfg", "sql", "ipynb", "log", "csv", "toml",
    "mjs", "cjs", "vue", "swift", "lua", "bat", "cmd", "xml", "gradle", "tf", "proto", "zig",
];

fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`' | ',' | '|')
}

/// File references and URLs in `s`.
pub fn find(s: &str) -> Vec<Found> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_sep(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !is_sep(chars[i]) {
            i += 1;
        }
        let mut end = i;
        // Trailing sentence punctuation is not part of the reference.
        while end > start && matches!(chars[end - 1], '.' | ';' | ':' | '!' | '?' | ',' | '*') {
            end -= 1;
        }
        let tok: String = chars[start..end].iter().collect();
        if let Some(what) = reference(&tok) {
            out.push(Found { start, end, what });
        }
    }
    out
}

fn reference(tok: &str) -> Option<Reference> {
    if tok.starts_with("http://") || tok.starts_with("https://") {
        let rest = tok.split_once("://")?.1;
        return (!rest.is_empty()).then(|| Reference::Url(tok.to_string()));
    }
    // `path`, `path:12`, `path:12:3`. A drive letter's colon stays in the path.
    let (head, tail) = split_drive(tok);
    let mut parts: Vec<&str> = tail.split(':').collect();
    let mut nums: Vec<u32> = Vec::new();
    while parts.len() > 1 && nums.len() < 2 {
        match parts.last().filter(|p| !p.is_empty() && p.len() <= 7 && p.bytes().all(|b| b.is_ascii_digit())) {
            Some(p) => {
                nums.insert(0, p.parse().ok()?);
                parts.pop();
            }
            None => break,
        }
    }
    if parts.len() != 1 {
        return None;
    }
    let path = format!("{head}{}", parts[0]);
    if !plausible_path(&path) {
        return None;
    }
    Some(Reference::File { path, line: nums.first().copied(), col: nums.get(1).copied() })
}

fn split_drive(tok: &str) -> (&str, &str) {
    let b = tok.as_bytes();
    if b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        (&tok[..2], &tok[2..])
    } else {
        ("", tok)
    }
}

fn plausible_path(p: &str) -> bool {
    if p.is_empty() || p.contains(['*', '?', '#']) || p.ends_with(['/', '\\']) {
        return false;
    }
    let has_sep = p.contains(['/', '\\']);
    let rooted = p.starts_with(['/', '\\']) || p.starts_with("./") || p.starts_with("../") || p.starts_with(".\\") || p.as_bytes().get(1) == Some(&b':');
    let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
    let ext = name.rsplit_once('.').map(|(stem, e)| (stem, e));
    match ext {
        Some((stem, e)) if !stem.is_empty() && !e.is_empty() => {
            let known = KNOWN_EXT.iter().any(|k| k.eq_ignore_ascii_case(e));
            let alpha = e.chars().any(|c| c.is_ascii_alphabetic()) && e.chars().all(|c| c.is_ascii_alphanumeric()) && e.len() <= 8;
            known || (has_sep && alpha)
        }
        // A dotfile or a bare name is a path only when it is spelled as one.
        _ => rooted && has_sep && name.chars().any(|c| c.is_ascii_alphanumeric()),
    }
}

/// Schemes an `editor_url` may use.
const EDITOR_SCHEMES: &[&str] = &["vscode", "vscode-insiders", "cursor", "windsurf", "idea", "pycharm", "zed", "file", "http", "https"];

/// An `editor_url` template is used only when it has an allowed scheme and no control byte: it is
/// written into an OSC 8 sequence, and an escape or BEL in it would end the link and start another
/// sequence (an OSC 52 clipboard write, say).
fn editor_template_ok(t: &str) -> bool {
    let Some((scheme, _)) = t.split_once(':') else { return false };
    EDITOR_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) && !t.bytes().any(|b| b < 0x20 || b == 0x7F)
}

/// The URL a reference opens.
pub fn url_for(what: &Reference, cfg: &LinkConfig) -> Option<String> {
    url_for_inner(what, cfg).filter(|u| !u.bytes().any(|b| b < 0x20 || b == 0x7F))
}

fn url_for_inner(what: &Reference, cfg: &LinkConfig) -> Option<String> {
    match what {
        Reference::Url(u) => Some(encode(u, true)),
        Reference::File { path, line, col } => {
            let abs = absolute(path, cfg.workspace.as_deref())?;
            let enc = encode(&abs, false);
            match cfg.editor_url.as_deref().map(str::trim).filter(|t| !t.is_empty()).filter(|t| editor_template_ok(t)) {
                Some(t) => Some(
                    t.replace("{path}", &enc)
                        .replace("{line}", &line.unwrap_or(1).to_string())
                        .replace("{col}", &col.unwrap_or(1).to_string()),
                ),
                // Without an editor to hand it to, the terminal opens a file:// link with the
                // system's default handler, which runs a script. Only text to read is linked.
                None => viewable(&abs).then(|| format!("file:///{}", enc.trim_start_matches('/'))),
            }
        }
    }
}

/// Extensions a `file:///` link may open: source and text a default handler shows, never one that
/// executes (scripts, binaries, installers, shortcuts, registry files) or runs script in a browser.
const VIEWABLE_EXT: &[&str] = &[
    "rs", "toml", "md", "txt", "json", "yaml", "yml", "lock", "ini", "cfg", "log", "csv", "tsv", "c", "h", "cc", "cpp", "hpp", "java",
    "kt", "cs", "go", "swift", "css", "sql", "ipynb", "proto", "tf", "gradle", "zig", "ts", "tsx", "jsx", "vue",
];

/// True when `path` ends in a viewable-text extension.
pub fn viewable(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && VIEWABLE_EXT.iter().any(|k| k.eq_ignore_ascii_case(ext)))
}

/// An absolute path with forward slashes, or `None` for a relative one with no workspace.
fn absolute(path: &str, workspace: Option<&str>) -> Option<String> {
    let p = path.replace('\\', "/");
    let rooted = p.starts_with('/') || p.as_bytes().get(1) == Some(&b':');
    if rooted {
        return Some(p);
    }
    let ws = workspace?.replace('\\', "/");
    let rel = p.trim_start_matches("./");
    let mut parts: Vec<&str> = ws.trim_end_matches('/').split('/').collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Percent-encodes everything outside the URL-safe set. A URL keeps its own `:/?#&=%` syntax.
fn encode(s: &str, is_url: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':')
            || (is_url && matches!(b, b'?' | b'#' | b'&' | b'=' | b'%' | b'+' | b'@' | b'!' | b'$' | b'*' | b',' | b';'));
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The links in the visible rows of `area`.
pub fn scan(buf: &Buffer, area: Rect, cfg: &LinkConfig) -> Vec<LinkRegion> {
    let mut out = Vec::new();
    for y in area.top()..area.bottom().min(buf.area.bottom()) {
        let mut row = String::new();
        let mut cols: Vec<u16> = Vec::new();
        let mut x = area.left();
        while x < area.right().min(buf.area.right()) {
            let Some(cell) = buf.cell((x, y)) else { break };
            let sym = cell.symbol();
            let w = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            for c in sym.chars() {
                row.push(c);
                cols.push(x);
            }
            if sym.is_empty() {
                row.push(' ');
                cols.push(x);
            }
            x += w;
        }
        for f in find(&row) {
            let Some(url) = url_for(&f.what, cfg) else { continue };
            let from = cols[f.start];
            let last = cols[f.end - 1];
            let last_w = buf.cell((last, y)).map_or(1, |c| unicode_width::UnicodeWidthStr::width(c.symbol()).max(1) as u16);
            out.push(LinkRegion { x: from, y, cols: last + last_w - from, url });
        }
    }
    out
}

/// Sends the link wrappers a frame needs, as one write after ratatui has flushed it.
#[derive(Default)]
pub struct LinkEmitter {
    shown: Vec<LinkRegion>,
    prev: Option<Buffer>,
}

impl LinkEmitter {
    pub fn frame(&mut self, buf: &Buffer, plan: &[LinkRegion]) -> Vec<u8> {
        // A resize repaints everything without links.
        if self.prev.as_ref().is_some_and(|p| p.area != buf.area) {
            self.shown.clear();
            self.prev = None;
        }
        let mut bytes: Vec<u8> = Vec::new();
        for r in plan {
            let same_cells = |a: &Buffer| (r.x..r.x + r.cols).all(|x| a.cell((x, r.y)) == buf.cell((x, r.y)));
            let still = self.shown.contains(r) && self.prev.as_ref().is_some_and(same_cells);
            if still {
                continue;
            }
            bytes.extend_from_slice(&paint(buf, r));
        }
        self.shown = plan.to_vec();
        self.prev = Some(buf.clone());
        bytes
    }
}

/// One link's cells, re-printed inside the wrapper at their own position.
fn paint(buf: &Buffer, r: &LinkRegion) -> Vec<u8> {
    let mut s = String::new();
    s.push_str(&format!("\x1b7\x1b[{};{}H", r.y + 1, r.x + 1));
    // The URL is written into an OSC sequence: no control byte may ride in it.
    let url: String = r.url.chars().filter(|c| !c.is_control()).collect();
    s.push_str(&format!("\x1b]8;;{url}\x1b\\"));
    let mut x = r.x;
    let mut last: Option<(Color, Modifier)> = None;
    while x < r.x + r.cols {
        let Some(c) = buf.cell((x, r.y)) else { break };
        // The style is sent when it changes, not for every cell.
        if last != Some((c.fg, c.modifier)) {
            s.push_str(&sgr(c.fg, c.modifier));
            last = Some((c.fg, c.modifier));
        }
        // A cell never carries a control character into the stream.
        if c.symbol().chars().any(char::is_control) {
            s.push(' ');
        } else {
            s.push_str(c.symbol());
        }
        x += unicode_width::UnicodeWidthStr::width(c.symbol()).max(1) as u16;
    }
    s.push_str("\x1b]8;;\x1b\\\x1b[0m\x1b8");
    s.into_bytes()
}

fn sgr(fg: Color, m: Modifier) -> String {
    let mut p: Vec<String> = vec!["0".into()];
    for (flag, code) in [(Modifier::BOLD, "1"), (Modifier::DIM, "2"), (Modifier::ITALIC, "3"), (Modifier::UNDERLINED, "4")] {
        if m.contains(flag) {
            p.push(code.into());
        }
    }
    match fg {
        Color::Reset => {}
        Color::Rgb(r, g, b) => p.push(format!("38;2;{r};{g};{b}")),
        Color::Indexed(n) => p.push(format!("38;5;{n}")),
        Color::Black => p.push("30".into()),
        Color::Red => p.push("31".into()),
        Color::Green => p.push("32".into()),
        Color::Yellow => p.push("33".into()),
        Color::Blue => p.push("34".into()),
        Color::Magenta => p.push("35".into()),
        Color::Cyan => p.push("36".into()),
        Color::Gray => p.push("37".into()),
        Color::DarkGray => p.push("90".into()),
        Color::LightRed => p.push("91".into()),
        Color::LightGreen => p.push("92".into()),
        Color::LightYellow => p.push("93".into()),
        Color::LightBlue => p.push("94".into()),
        Color::LightMagenta => p.push("95".into()),
        Color::LightCyan => p.push("96".into()),
        Color::White => p.push("97".into()),
    }
    format!("\x1b[{}m", p.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(s: &str) -> Vec<Reference> {
        find(s).into_iter().map(|f| f.what).collect()
    }

    fn file(path: &str, line: Option<u32>, col: Option<u32>) -> Reference {
        Reference::File { path: path.into(), line, col }
    }

    #[test]
    fn finds_paths_with_lines_and_columns() {
        assert_eq!(refs("error at src/main.rs:42:7: expected"), vec![file("src/main.rs", Some(42), Some(7))]);
        assert_eq!(refs("see C:\\work\\a.py:7."), vec![file("C:\\work\\a.py", Some(7), None)]);
        assert_eq!(refs("(crates/x/lib.rs)"), vec![file("crates/x/lib.rs", None, None)]);
        assert_eq!(refs("edit `Cargo.toml` now"), vec![file("Cargo.toml", None, None)]);
    }

    #[test]
    fn prose_is_not_a_path() {
        for s in ["e.g. this", "pi is 3.14", "v1.2.3 released", "and/or maybe", "i.e.", "Mr. Smith", "foo.bar baz"] {
            assert!(refs(s).is_empty(), "{s}: {:?}", refs(s));
        }
    }

    #[test]
    fn urls_keep_their_query_and_lose_sentence_punctuation() {
        assert_eq!(refs("docs: https://example.com/a?b=1&c=2."), vec![Reference::Url("https://example.com/a?b=1&c=2".into())]);
    }

    #[test]
    fn a_file_opens_through_the_editor_template_or_as_a_file_url() {
        let cfg = LinkConfig { editor_url: None, workspace: Some("C:/work/val iance".into()) };
        let r = file("src/main.rs", Some(42), None);
        assert_eq!(url_for(&r, &cfg).unwrap(), "file:///C:/work/val%20iance/src/main.rs");
        let cfg = LinkConfig { editor_url: Some("vscode://file/{path}:{line}:{col}".into()), ..cfg };
        assert_eq!(url_for(&r, &cfg).unwrap(), "vscode://file/C:/work/val%20iance/src/main.rs:42:1");
        assert_eq!(url_for(&file("../x.rs", None, None), &cfg).unwrap(), "vscode://file/C:/work/x.rs:1:1");
        assert!(url_for(&r, &LinkConfig::default()).is_none(), "a relative path with no workspace has no URL");
    }

    #[test]
    fn without_an_editor_only_text_to_read_gets_a_file_url() {
        let cfg = LinkConfig { editor_url: None, workspace: Some("C:/w".into()) };
        for bad in [
            "C:/x/run.bat", "C:/Users/m/Downloads/x.exe", "fix.cmd", "tools/a.ps1", "x.js", "x.sh", "a.py", "a.vbs", "a.msi", "a.lnk", "a.reg", "a.hta",
            "a.html", "a.svg", "C:/x/readme.txt.exe", "a.RB", "Makefile",
        ] {
            assert!(url_for(&file(bad, None, None), &cfg).is_none(), "{bad} must not become a file link");
        }
        for good in ["src/a.rs", "NOTES.MD", "Cargo.toml", "C:/x/log.txt", "a/b.json"] {
            assert!(url_for(&file(good, None, None), &cfg).is_some(), "{good}");
        }
        // An editor template takes anything: the editor opens it, nothing runs it.
        let cfg = LinkConfig { editor_url: Some("vscode://file/{path}:{line}".into()), workspace: Some("C:/w".into()) };
        assert_eq!(url_for(&file("C:/x/run.bat", Some(2), None), &cfg).unwrap(), "vscode://file/C:/x/run.bat:2");
    }

    #[test]
    fn nothing_can_put_an_escape_in_a_url() {
        let cfg = LinkConfig { editor_url: None, workspace: Some("C:/w".into()) };
        let u = url_for(&file("a\u{1b}]8;;evil.rs", None, None), &cfg).unwrap();
        assert!(!u.contains('\u{1b}') && !u.contains('\u{7}'), "{u:?}");
    }

    #[test]
    fn an_editor_url_cannot_smuggle_an_escape_or_an_odd_scheme() {
        let r = file("src/a.rs", Some(3), None);
        // Script and diagnostic-tool schemes are spelled from pieces: whole in a test binary an
        // antivirus string scan reads them as an exploit.
        let script = ["java", "script:alert({path})"].concat();
        let tool = ["ms", "-ms", "dt:{path}"].concat();
        for bad in ["vscode://file/{path}\u{1b}]52;c;AAAA\u{7}", "vscode://x\u{7}{path}", script.as_str(), "no-scheme/{path}", tool.as_str()] {
            let cfg = LinkConfig { editor_url: Some(bad.into()), workspace: Some("C:/w".into()) };
            let u = url_for(&r, &cfg).unwrap();
            assert_eq!(u, "file:///C:/w/src/a.rs", "{bad:?} falls back to a file URL");
        }
        let cfg = LinkConfig { editor_url: Some("Cursor://file/{path}:{line}".into()), workspace: Some("C:/w".into()) };
        assert_eq!(url_for(&r, &cfg).unwrap(), "Cursor://file/C:/w/src/a.rs:3");
    }

    #[test]
    fn a_link_region_with_an_escape_in_its_url_paints_without_it() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        buf.set_string(0, 0, "a.rs", ratatui::style::Style::default());
        let region = LinkRegion { x: 0, y: 0, cols: 4, url: "vscode://x\u{1b}]52;c;AAAA\u{7}".into() };
        let bytes = String::from_utf8(paint(&buf, &region)).unwrap();
        let url_part = bytes.split("\u{1b}]8;;").nth(1).unwrap().split("\u{1b}\\").next().unwrap();
        assert!(!url_part.chars().any(char::is_control), "{url_part:?}");
        assert!(!bytes.contains('\u{7}'), "{bytes:?}");
    }

    #[test]
    fn the_emitter_wraps_exactly_the_linked_cells_and_resends_only_on_change() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        buf.set_string(2, 1, "see src/a.rs:9 ok", ratatui::style::Style::default().fg(Color::Rgb(1, 2, 3)));
        let cfg = LinkConfig { editor_url: None, workspace: Some("C:/w".into()) };
        let plan = scan(&buf, buf.area, &cfg);
        assert_eq!(plan.len(), 1);
        assert_eq!((plan[0].x, plan[0].y, plan[0].cols), (6, 1, 10));
        let mut e = LinkEmitter::default();
        let bytes = String::from_utf8(e.frame(&buf, &plan)).unwrap();
        assert!(bytes.contains("\x1b]8;;file:///C:/w/src/a.rs\x1b\\"), "{bytes:?}");
        assert!(bytes.contains("src/a.rs:9"), "{bytes:?}");
        assert!(bytes.ends_with("\x1b]8;;\x1b\\\x1b[0m\x1b8"), "{bytes:?}");
        assert!(e.frame(&buf, &plan).is_empty(), "a still screen sends nothing");
        // ratatui rewrote a cell in the link: wrap again.
        let mut changed = buf.clone();
        changed.set_style(Rect::new(8, 1, 1, 1), ratatui::style::Style::default().fg(Color::Red));
        let plan2 = scan(&changed, changed.area, &cfg);
        assert_eq!(plan2, plan);
        assert!(!e.frame(&changed, &plan2).is_empty(), "the link is wrapped again after its cells changed");
    }
}
