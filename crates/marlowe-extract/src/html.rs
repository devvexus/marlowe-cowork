//! The HTML extractor. **One streaming pass, no DOM.**
//!
//! # Why this is hand-rolled when `html5ever` was available
//!
//! Measured against this workspace's lockfile, `scraper`/`html5ever` costs **18 new crates** —
//! `cssparser`, `selectors`, `string_cache`, `tendril`, `servo_arc` — to build a spec-compliant,
//! interned DOM tree. Every one of those nodes is then thrown away, because what the caller wants
//! is a string.
//!
//! This module is the pass that produces the string directly. `memchr` finds the next `<` at
//! memory bandwidth, tag bodies are skipped without being materialised, attributes are parsed
//! **only** for the four elements whose attributes anyone reads (`a`, `html`, `meta`, `img`), and
//! text accumulates into a single buffer reserved once from the input length. There is no tree,
//! no interning, and no per-element allocation.
//!
//! So the dependency would have been both heavier *and* slower for this specific job. That is not
//! a general claim about `html5ever` — it is the right tool when you need the DOM. Here it is
//! strictly more work than the question requires.
//!
//! # What it still has to get right
//!
//! Real-world HTML is hostile, and the cases that bite a naive scanner are all handled explicitly:
//!
//! - **`>` inside a quoted attribute value** — `<a title="a > b">` does not end the tag.
//! - **Raw-text elements** — the contents of `<script>` are not markup, so `a < b` inside one must
//!   not open a tag. These are scanned for their literal closing tag instead.
//! - **Comments and CDATA** — `<!-- <div> -->` contains no element.
//! - **Unclosed and misnested tags** — the stack tolerates them rather than unwinding, because a
//!   text extractor has no structural obligation the way a renderer does.
//!
//! # Boilerplate removal, and the rule that governs it
//!
//! Stripping nav, headers, footers and link farms is what turns a 200 KB page into 6 KB of
//! readable prose. It is also **lossy**, and a boilerplate heuristic that eats the article is a
//! silent failure of exactly the kind this codebase is organised against.
//!
//! So the rule is: **the heuristic may only ever remove text, and never all of it.** If content
//! selection leaves too little behind, [`extract`] falls back to the unfiltered text and says so.
//! Returning a page with its nav bar attached is a small cost; returning an empty document that
//! reads like a page with nothing on it is not.

use crate::{charset, Document, ExtractError, Format, Heading, Link, Warning};

/// Elements whose *contents* are not prose and are dropped whole.
const SKIP_CONTENT: [&str; 10] = [
    "script", "style", "svg", "noscript", "iframe", "canvas", "template", "object", "embed",
    "math",
];

/// Elements that are raw text: markup inside them is not markup. Scanned to their literal close.
const RAW_TEXT: [&str; 4] = ["script", "style", "title", "textarea"];

/// Containers whose text is chrome rather than content. Not dropped outright — see the module
/// header's rule — but deprioritised during content selection.
const BOILERPLATE: [&str; 7] = ["nav", "footer", "header", "aside", "menu", "form", "figcaption"];

/// Elements that force a line break in the output.
const BLOCK: [&str; 26] = [
    "p", "div", "br", "hr", "li", "ul", "ol", "table", "tr", "td", "th", "h1", "h2", "h3", "h4",
    "h5", "h6", "section", "article", "main", "blockquote", "pre", "dd", "dt", "dl", "figure",
];

/// Containers that positively identify the main content.
const CONTENT: [&str; 2] = ["article", "main"];

/// One run of text with the context it was found in. Content selection works over these.
#[derive(Debug)]
struct TextBlock {
    text: String,
    /// Was this inside a `nav`/`footer`/`aside`/…?
    boilerplate: bool,
    /// Was this inside an `article`/`main`?
    content: bool,
    /// Characters of this block that were anchor text. High ratios are link farms.
    link_chars: usize,
}

pub fn extract(
    bytes: &[u8],
    content_type: Option<&str>,
    url: Option<&str>,
) -> Result<Document, ExtractError> {
    let mut warnings = Vec::new();
    let decoded = charset::decode(bytes, content_type, true, &mut warnings);
    let mut p = Parser::new(&decoded.text, url);
    p.run();

    let script_chars = p.script_chars;
    let title = p.title.take().map(|t| collapse(&decode_entities(&t))).filter(|t| !t.is_empty());
    let selected = select_content(&p.blocks, &mut warnings);
    let text = crate::normalize(&selected, &mut warnings);

    // **The client-rendered tell.** A page whose server HTML is mostly script and barely any prose
    // was assembled in a browser, and what we extracted is not what a reader would see. Saying so
    // is the difference between "this article is short" and "we cannot read this site".
    //
    // **The thresholds were raised after a false positive on a real page**, and the reason is the
    // standing rule: a warning that fires on a page which is merely *short* reads identically to
    // one that fires on an app shell, so it would be evidence about neither.
    // `doc.rust-lang.org/book/ch01-00` is a complete, server-rendered chapter index — 288
    // characters of genuine content against 2,418 of ordinary page script — and it tripped the
    // first version of this check. An actual shell has essentially *no* text (tens of characters)
    // against tens of kilobytes of bundle, so all three bounds must hold at once.
    if script_chars > 10_000 && text.len() < 500 && text.len() * 20 < script_chars {
        warnings.push(Warning::LikelyClientRendered {
            text_chars: text.len(),
            script_chars,
        });
    }

    Ok(Document {
        format: Format::Html,
        title,
        text,
        links: p.links,
        headings: p.headings,
        lang: p.lang,
        description: p.description,
        bytes_in: bytes.len(),
        encoding: decoded.encoding,
        warnings,
    })
}

/// Choose which blocks are the document.
///
/// Three tiers, tried in order, and **the last one cannot fail**: an `article`/`main` if the page
/// marked one and it carries enough text; otherwise everything that is not chrome; otherwise
/// everything. See the module header — the heuristic may remove text, never all of it.
fn select_content(blocks: &[TextBlock], warnings: &mut Vec<Warning>) -> String {
    let total: usize = blocks.iter().map(|b| b.text.len()).sum();
    if total == 0 {
        return String::new();
    }

    let marked: usize = blocks.iter().filter(|b| b.content).map(|b| b.text.len()).sum();
    // A `<main>` wrapping the whole page is common and tells us nothing; require it to be a
    // meaningful share but not the entire document before trusting it.
    if marked >= 200 && marked * 100 >= total * 15 {
        return join(blocks.iter().filter(|b| b.content && !is_link_farm(b)));
    }

    let kept = join(blocks.iter().filter(|b| !b.boilerplate && !is_link_farm(b)));
    if kept.trim().len() * 100 >= total * 10 && kept.trim().len() >= 120 {
        return kept;
    }

    // Everything, including the chrome. A page that is genuinely all navigation — a link index,
    // a sitemap — reaches here, and returning it whole is correct.
    if total >= 120 {
        warnings.push(Warning::Recovered {
            detail: "content selection found no clear article; returning the full page text"
                .to_string(),
        });
    }
    join(blocks.iter())
}

/// A block that is mostly anchor text is a menu or a related-links strip, whatever it sits inside.
fn is_link_farm(b: &TextBlock) -> bool {
    b.text.len() >= 40 && b.link_chars * 100 > b.text.len() * 65
}

fn join<'a>(it: impl Iterator<Item = &'a TextBlock>) -> String {
    let mut out = String::new();
    for b in it {
        if b.text.trim().is_empty() {
            continue;
        }
        out.push_str(&b.text);
        out.push('\n');
    }
    out
}

struct Parser<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    base: Option<&'a str>,

    blocks: Vec<TextBlock>,
    /// Characters retained across all of [`Parser::blocks`]. See [`Parser::flush_block`] — this
    /// bounds the two full-document copies that `normalize` was going to discard anyway.
    block_chars: usize,
    /// The block being accumulated.
    current: String,
    current_links: usize,

    /// Open-element stack. Bounded — a document that nests 4,000 divs is not going to be read
    /// more correctly by tracking all of them, and an unbounded stack is a memory target.
    stack: Vec<String>,
    boiler_depth: u32,
    content_depth: u32,
    skip_depth: u32,

    title: Option<String>,
    lang: Option<String>,
    description: Option<String>,
    links: Vec<Link>,
    headings: Vec<Heading>,
    script_chars: usize,

    /// Set while inside an `<a>`, so anchor text can be attributed for link-density scoring.
    anchor: Option<(String, usize)>,
    heading: Option<(u8, usize)>,
}

const MAX_STACK: usize = 512;
const MAX_LINKS: usize = 5_000;
/// Ceiling on the raw `<title>` slice. Titles are a line; anything beyond this is a malformed
/// document, not a title.
const MAX_TITLE_BYTES: usize = 4_096;
const MAX_HEADINGS: usize = 2_000;

impl<'a> Parser<'a> {
    fn new(text: &'a str, base: Option<&'a str>) -> Self {
        Self {
            src: text.as_bytes(),
            text,
            pos: 0,
            base,
            blocks: Vec::new(),
            block_chars: 0,
            current: String::with_capacity(1024),
            current_links: 0,
            stack: Vec::new(),
            boiler_depth: 0,
            content_depth: 0,
            skip_depth: 0,
            title: None,
            lang: None,
            description: None,
            links: Vec::new(),
            headings: Vec::new(),
            script_chars: 0,
            anchor: None,
            heading: None,
        }
    }

    fn run(&mut self) {
        while self.pos < self.src.len() {
            // `memchr` for the next `<`. This is the hot loop and it is why there is no DOM.
            let Some(rel) = memchr::memchr(b'<', &self.src[self.pos..]) else {
                self.push_text(self.pos, self.src.len());
                break;
            };
            let lt = self.pos + rel;
            if lt > self.pos {
                self.push_text(self.pos, lt);
            }
            self.pos = lt;
            self.consume_markup();
        }
        self.flush_block();
    }

    fn consume_markup(&mut self) {
        let rest = &self.src[self.pos..];
        if rest.starts_with(b"<!--") {
            self.pos = find_from(self.src, self.pos + 4, b"-->")
                .map(|i| i + 3)
                .unwrap_or(self.src.len());
            return;
        }
        if rest.starts_with(b"<![CDATA[") {
            let end = find_from(self.src, self.pos + 9, b"]]>");
            let start = self.pos + 9;
            let stop = end.unwrap_or(self.src.len());
            self.push_text(start, stop);
            self.pos = end.map(|i| i + 3).unwrap_or(self.src.len());
            return;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            self.pos = memchr::memchr(b'>', rest).map(|i| self.pos + i + 1).unwrap_or(self.src.len());
            return;
        }
        // A bare `<` that opens nothing is literal text — `a < b` outside a raw-text element.
        let after = rest.get(1).copied();
        let opens_tag = matches!(after, Some(c) if c.is_ascii_alphabetic() || c == b'/');
        if !opens_tag {
            self.push_text(self.pos, self.pos + 1);
            self.pos += 1;
            return;
        }

        let Some(tag_end) = self.tag_end(self.pos) else {
            // Unterminated tag at EOF. Everything left is markup we cannot close; stop.
            self.pos = self.src.len();
            return;
        };
        let raw = &self.text[self.pos..tag_end];
        let closing = raw.starts_with("</");
        let name = tag_name(raw);
        self.pos = tag_end;

        if name.is_empty() {
            return;
        }
        if closing {
            self.close(&name);
        } else {
            let self_closing = raw.trim_end().ends_with("/>") || is_void(&name);
            self.open(&name, raw, self_closing);
        }
    }

    /// Find the `>` that ends a tag, respecting quoted attribute values.
    ///
    /// This is the case a naive `memchr(b'>')` gets wrong: `<a title="a > b">` would end four
    /// characters early, and everything after it would be misparsed as text.
    fn tag_end(&self, start: usize) -> Option<usize> {
        let mut i = start + 1;
        let mut quote: Option<u8> = None;
        while i < self.src.len() {
            let c = self.src[i];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    b'"' | b'\'' => quote = Some(c),
                    b'>' => return Some(i + 1),
                    _ => {}
                },
            }
            i += 1;
        }
        None
    }

    fn open(&mut self, name: &str, raw: &str, self_closing: bool) {
        // Attributes are parsed for four elements only. Everything else pays nothing.
        match name {
            "a" => {
                if let Some(href) = attr(raw, "href") {
                    let resolved = resolve(self.base, &decode_entities(&href));
                    if let Some(u) = resolved {
                        self.anchor = Some((u, self.current.len()));
                    }
                }
            }
            "html" => {
                if self.lang.is_none() {
                    self.lang = attr(raw, "lang").map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
                }
            }
            "meta" => {
                let is_desc = attr(raw, "name").is_some_and(|n| n.eq_ignore_ascii_case("description"))
                    || attr(raw, "property")
                        .is_some_and(|p| p.eq_ignore_ascii_case("og:description"));
                if is_desc && self.description.is_none() {
                    self.description = attr(raw, "content")
                        .map(|c| collapse(&decode_entities(&c)))
                        .filter(|c| !c.is_empty());
                }
            }
            "img" => {
                // Alt text is real content — figures and diagrams often carry the only
                // description of what they show.
                if let Some(alt) = attr(raw, "alt") {
                    let alt = collapse(&decode_entities(&alt));
                    if alt.len() >= 3 && self.skip_depth == 0 {
                        self.current.push_str(&alt);
                        self.current.push(' ');
                    }
                }
            }
            _ => {}
        }

        if name.starts_with('h') && name.len() == 2 {
            if let Some(level) = name.as_bytes()[1].checked_sub(b'0').filter(|l| (1..=6).contains(l)) {
                self.heading = Some((level, self.current.len()));
            }
        }

        if is_block(name) {
            self.flush_block();
        }

        // Raw-text elements: their contents are not markup. Scan to the literal close so that
        // `if (a < b)` inside a script cannot open an element.
        if RAW_TEXT.contains(&name) && !self_closing {
            let close = format!("</{name}");
            // **A raw-text element ends at `</name` followed by a TERMINATOR, not at `</name`.**
            // Without the check, `<script>var s="</scriptX>";INSTRUCTIONS</script>` ended the
            // script early and the remainder was parsed as markup and emitted as prose -- inert
            // JavaScript in a browser, visible instructions here.
            let end = find_close_tag(self.src, self.pos, close.as_bytes());
            let body_end = end.unwrap_or(self.src.len());
            if name == "title" {
                if self.title.is_none() {
                    // **Capped.** With no `</title>`, `body_end` is end-of-input, so the entire
                    // document became the title -- copied again by `decode_entities` and again by
                    // `collapse`, stored permanently in the document store, and prepended to the
                    // text by `corpus::render`, bypassing `MAX_TEXT_CHARS` on every path.
                    let end = body_end.min(self.pos.saturating_add(MAX_TITLE_BYTES));
                    let end = crate::floor_char_boundary(self.text, end.min(self.text.len()));
                    self.title = Some(self.text[self.pos..end.max(self.pos)].to_string());
                }
            } else if name == "script" || name == "style" {
                self.script_chars += body_end - self.pos;
            } else if name == "textarea" {
                // Placeholder content is chrome; skipped deliberately.
            }
            self.pos = end
                .and_then(|i| self.tag_end(i).or(Some(self.src.len())))
                .unwrap_or(self.src.len());
            return;
        }

        // **Raw-text elements returned above without incrementing `skip_depth`**, so anything
        // that decrements it on close is decrementing a counter this branch never raised. See
        // `close`.
        if SKIP_CONTENT.contains(&name) {
            self.skip_depth += 1;
        }
        if BOILERPLATE.contains(&name) {
            self.boiler_depth += 1;
        }
        if CONTENT.contains(&name) {
            self.content_depth += 1;
        }

        if !self_closing && self.stack.len() < MAX_STACK {
            self.stack.push(name.to_string());
        }
    }

    fn close(&mut self, name: &str) {
        if name == "a" {
            if let Some((url, from)) = self.anchor.take() {
                let text = collapse(&self.current[self.mark(from)..]);
                self.current_links += text.len();
                if self.links.len() < MAX_LINKS && !url.is_empty() {
                    self.links.push(Link { url, text });
                }
            }
        }
        if let Some((level, from)) = self.heading.take() {
            let text = collapse(&self.current[self.mark(from)..]);
            if !text.is_empty() && self.headings.len() < MAX_HEADINGS {
                self.headings.push(Heading { level, text });
            }
        }

        if is_block(name) {
            self.flush_block();
        }
        // **Only decrement for a skip element this parser actually opened.**
        //
        // `script` and `style` are in both `RAW_TEXT` and `SKIP_CONTENT`, and `open`'s raw-text
        // branch returns before the increment -- so every `</script>` in a document decremented a
        // counter no `<script>` had raised, and `saturating_sub` hid it. One stray `</script>`
        // inside `<svg>` reopened the SVG body as ordinary text: content a human auditing the page
        // cannot see, which is exactly what makes it useful for shaping a summary.
        // **Keyed on the element stack, not on a counter.** A bare counter was not enough: `<svg>`
        // raised it and a stray `</script>` — a different name — spent it, so the SVG body still
        // reopened. The stack is the only structure that knows *which* element is open, and
        // raw-text elements never reach it (their branch returns first), so `</script>` cannot
        // find a `script` there and cannot decrement.
        if SKIP_CONTENT.contains(&name) && self.stack.iter().any(|s| s == name) {
            self.skip_depth = self.skip_depth.saturating_sub(1);
        }
        if BOILERPLATE.contains(&name) {
            self.boiler_depth = self.boiler_depth.saturating_sub(1);
        }
        if CONTENT.contains(&name) {
            self.content_depth = self.content_depth.saturating_sub(1);
        }

        // Unwind to the matching open tag if there is one, tolerating misnesting. If the name is
        // not on the stack at all it was a stray close, and dropping it is the recovery.
        if let Some(at) = self.stack.iter().rposition(|s| s == name) {
            self.stack.truncate(at);
        }
    }

    /// Resolve a start offset recorded when an inline element opened.
    ///
    /// **`.min(len)` was not enough, and that is subtle.** `from` is captured at open time and
    /// `flush_block` calls `self.current.clear()`, so the offset is stale immediately -- for a
    /// heading, within the same call, because `open` records it and then flushes. Clamping to the
    /// new length keeps the index in bounds but says nothing about it being a CHAR boundary, so
    /// `<p>a<h1>e-acute</h1>` panicked on a two-byte character. Clamp, then walk back to a
    /// boundary.
    fn mark(&self, from: usize) -> usize {
        crate::floor_char_boundary(&self.current, from.min(self.current.len()))
    }

    fn push_text(&mut self, from: usize, to: usize) {
        if self.skip_depth > 0 || to <= from {
            return;
        }
        let raw = &self.text[from..to];
        if raw.trim().is_empty() {
            // Whitespace between elements is still a word separator.
            if !self.current.ends_with(' ') && !self.current.is_empty() {
                self.current.push(' ');
            }
            return;
        }
        push_decoded(&mut self.current, raw);
    }

    fn flush_block(&mut self) {
        let text = collapse(&self.current);
        self.current.clear();
        let links = std::mem::take(&mut self.current_links);
        if text.is_empty() {
            return;
        }
        // **The block buffer is bounded, and this is where the HTML memory peak came from.**
        //
        // A 64 MB windows-1252 input holds four full-size copies at once, because every high byte
        // becomes 2–3 UTF-8 bytes and each stage rebuilds the whole document:
        //
        // | stage | holds | up to |
        // |---|---|---|
        // | caller's `bytes` | the raw input | 64 MB |
        // | `charset::decode` | decoded UTF-8 | ~192 MB |
        // | `self.blocks` | every text run, copied | ~192 MB |
        // | `select_content` | the chosen runs, joined | ~192 MB |
        // | `normalize` | the final text | 8 MiB — `MAX_TEXT_CHARS` |
        //
        // ~770 MB peak for a document that is capped at 8 MiB the moment it is finished. The last
        // stage discards 95% of what the two before it built, so bounding them costs nothing that
        // survives the function.
        //
        // **What it does change**, stated rather than hidden: for a document whose text exceeds
        // the bound, *which* 8 MiB survives can differ, because `select_content` now chooses from
        // a prefix of the blocks rather than all of them. The document was going to be truncated
        // either way; this decides where. `decoded.text` is the one copy that cannot be bounded
        // without a streaming decoder, so the peak floor is the input size and its expansion.
        if self.block_chars >= crate::MAX_TEXT_CHARS {
            return;
        }
        self.block_chars += text.len();
        self.blocks.push(TextBlock {
            text,
            boilerplate: self.boiler_depth > 0,
            content: self.content_depth > 0,
            link_chars: links,
        });
    }
}

/// Does this element end the current text block?
///
/// **Every container that changes the classification must be here, not just the ones that render
/// as blocks**, and that is the subtle part. A block is stamped with `boiler_depth`/`content_depth`
/// at the moment it is *flushed*. `<nav>` was absent from [`BLOCK`], so a nav's text stayed in the
/// buffer until the next block element opened — by which point the `</nav>` had already
/// decremented `boiler_depth`, and the chrome was stamped as content and survived filtering.
///
/// The rule: an element that moves a depth counter is a block boundary by definition, because
/// otherwise text from two different contexts shares one classification.
fn is_block(name: &str) -> bool {
    BLOCK.contains(&name) || BOILERPLATE.contains(&name) || CONTENT.contains(&name)
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr"
    )
}

fn tag_name(raw: &str) -> String {
    let s = raw.trim_start_matches('<').trim_start_matches('/');
    let end = s
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(s.len());
    s[..end].to_ascii_lowercase()
}

/// Read an attribute out of a raw tag string. Case-insensitive name, both quote styles, bare
/// values. Not a general parser — it serves the four elements listed in [`Parser::open`].
fn attr(raw: &str, name: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(name) {
        let at = from + rel;
        let before_ok = at > 0
            && lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '"' || c == '\'');
        let after = lower[at + name.len()..].trim_start();
        if before_ok {
            if let Some(rest) = after.strip_prefix('=') {
                // Index back into `raw`, not `lower`: values are case-sensitive (URLs are).
                let off = raw.len() - rest.len();
                let rest_raw = raw[off..].trim_start();
                let value = if let Some(r) = rest_raw.strip_prefix('"') {
                    r.split('"').next()
                } else if let Some(r) = rest_raw.strip_prefix('\'') {
                    r.split('\'').next()
                } else {
                    rest_raw.split([' ', '\t', '\n', '\r', '>']).next().map(|v| v.trim_end_matches('/'))
                };
                if let Some(v) = value {
                    return Some(v.to_string());
                }
            }
        }
        from = at + name.len();
    }
    None
}

/// Resolve `href` against the document URL. Textual only — this crate has no network and grants
/// no permission; a resolved URL is a string, not an authorisation to fetch it.
fn resolve(base: Option<&str>, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    // Non-navigational schemes are dropped rather than surfaced as places to go.
    for bad in ["javascript:", "data:", "vbscript:", "about:"] {
        if lower.starts_with(bad) {
            return None;
        }
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.to_string());
    }
    let base = base?;
    if href.starts_with("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return Some(format!("{scheme}:{href}"));
    }
    let scheme_end = base.find("://")? + 3;
    let authority_end = base[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(base.len());
    let origin = &base[..authority_end];

    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    if lower.contains(':') && !href.starts_with('.') {
        // Some other scheme (mailto:, tel:, ftp:). Not a page.
        let head = lower.split(':').next().unwrap_or("");
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-') {
            return None;
        }
    }
    // Relative to the directory of the base path.
    let path = &base[authority_end..];
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Some(format!("{origin}{dir}{href}"))
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    memchr::memchr_iter(needle[0], &hay[from..])
        .map(|i| from + i)
        .find(|i| hay[*i..].starts_with(needle))
}

/// Find a raw-text element's real closing tag: `</name` followed by whitespace, `/` or `>`.
fn find_close_tag(hay: &[u8], from: usize, needle_lower: &[u8]) -> Option<usize> {
    let mut at = from;
    loop {
        let hit = find_ci(hay, at, needle_lower)?;
        let after = hit + needle_lower.len();
        let terminated = match hay.get(after) {
            None => true,
            Some(b) => matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'/' | b'>'),
        };
        if terminated {
            return Some(hit);
        }
        at = hit + 1;
    }
}

/// Case-insensitive search, used for raw-text closing tags (`</SCRIPT>` is legal).
fn find_ci(hay: &[u8], from: usize, needle_lower: &[u8]) -> Option<usize> {
    if from >= hay.len() || needle_lower.is_empty() {
        return None;
    }
    let first_lower = needle_lower[0];
    let first_upper = first_lower.to_ascii_uppercase();
    let mut i = from;
    while i < hay.len() {
        let Some(rel) = memchr::memchr2(first_lower, first_upper, &hay[i..]) else {
            return None;
        };
        let at = i + rel;
        if hay.len() - at >= needle_lower.len()
            && hay[at..at + needle_lower.len()].eq_ignore_ascii_case(needle_lower)
        {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

// ── entities ─────────────────────────────────────────────────────────────────────────────────

/// The named entities that actually occur in prose, plus numeric forms.
///
/// Not the full WHATWG table of 2,231 names: that table is 100 KB of static data to serve a tail
/// nobody's research corpus contains. Anything absent is left verbatim, which is visibly wrong
/// rather than silently wrong.
const ENTITIES: [(&str, &str); 62] = [
    ("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'"), ("nbsp", " "),
    ("ndash", "–"), ("mdash", "—"), ("lsquo", "‘"), ("rsquo", "’"), ("ldquo", "“"),
    ("rdquo", "”"), ("hellip", "…"), ("copy", "©"), ("reg", "®"), ("trade", "™"),
    ("deg", "°"), ("plusmn", "±"), ("times", "×"), ("divide", "÷"), ("frac12", "½"),
    ("frac14", "¼"), ("frac34", "¾"), ("sup2", "²"), ("sup3", "³"), ("micro", "µ"),
    ("para", "¶"), ("sect", "§"), ("bull", "•"), ("dagger", "†"), ("Dagger", "‡"),
    ("permil", "‰"), ("prime", "′"), ("Prime", "″"), ("euro", "€"), ("pound", "£"),
    ("yen", "¥"), ("cent", "¢"), ("laquo", "«"), ("raquo", "»"), ("lsaquo", "‹"),
    ("rsaquo", "›"), ("larr", "←"), ("rarr", "→"), ("harr", "↔"), ("uarr", "↑"),
    ("darr", "↓"), ("alpha", "α"), ("beta", "β"), ("gamma", "γ"), ("delta", "δ"),
    ("epsilon", "ε"), ("lambda", "λ"), ("mu", "μ"), ("pi", "π"), ("sigma", "σ"),
    ("tau", "τ"), ("phi", "φ"), ("omega", "ω"), ("Omega", "Ω"), ("infin", "∞"),
    ("ne", "≠"),
];

fn push_decoded(out: &mut String, raw: &str) {
    if !raw.contains('&') {
        out.push_str(raw);
        return;
    }
    out.push_str(&decode_entities(raw));
}

pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if bytes[i] != b'&' {
            let next = memchr::memchr(b'&', &bytes[i..]).map(|r| i + r).unwrap_or(s.len());
            out.push_str(&s[i..next]);
            i = next;
            continue;
        }
        // An entity is short; a `&` with no `;` within 32 bytes is a literal ampersand.
        //
        // **`floor_char_boundary`, because `i + 32` is an arbitrary byte offset.** `i` is the
        // index of an ASCII `&`, but nothing makes `i + 32` a boundary: `&` followed by 30 ASCII
        // characters and then any multi-byte character puts byte 32 inside it, and the slice
        // panics. That panic then propagated out of `rayon`'s fan-out and, via the payload,
        // carried 256 bytes of the document to the orchestrator.
        let limit = crate::floor_char_boundary(s, (i + 32).min(s.len()));
        let Some(semi) = s[i..limit].find(';').map(|r| i + r) else {
            out.push('&');
            i += 1;
            continue;
        };
        let body = &s[i + 1..semi];
        if let Some(stripped) = body.strip_prefix('#') {
            let cp = if let Some(hex) = stripped.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                stripped.parse::<u32>().ok()
            };
            match cp.and_then(char::from_u32) {
                Some(c) => out.push(c),
                None => out.push_str(&s[i..=semi]),
            }
            i = semi + 1;
            continue;
        }
        match ENTITIES.iter().find(|(n, _)| *n == body) {
            Some((_, v)) => out.push_str(v),
            // Left verbatim on purpose: visibly wrong beats silently wrong.
            None => out.push_str(&s[i..=semi]),
        }
        i = semi + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        extract(html.as_bytes(), Some("text/html; charset=utf-8"), Some("https://ex.com/a/b.html"))
            .expect("html always extracts")
    }

    /// **The HTML memory peak, as an assertable bound rather than as arithmetic.**
    ///
    /// The fix bounds `Parser::blocks` — the third of four full-size copies of a large document.
    /// The tempting test is `d.text.len() <= MAX_TEXT_CHARS` on the finished `Document`, and it is
    /// worthless: `normalize` caps the text either way, so it passes identically on a build with
    /// no bound at all. It measures something adjacent to the property and moves with it. The
    /// intermediate buffer is the thing that changed, so the intermediate buffer is what is
    /// asserted.
    ///
    /// **Not peak RSS.** That is allocator- and platform-dependent and would be a flaky test
    /// asserting a number nobody can reproduce. This is a count, and it is exact.
    #[test]
    fn the_block_buffer_is_bounded_before_normalize_ever_sees_it() {
        const PARA_CHARS: usize = 1_000;
        // Comfortably past MAX_TEXT_CHARS (8 MiB) so the bound must engage.
        let paras = (crate::MAX_TEXT_CHARS / PARA_CHARS) * 3;
        let one = format!("<p>{}</p>", "x".repeat(PARA_CHARS));
        let src = one.repeat(paras);

        let mut p = Parser::new(&src, None);
        p.run();
        let retained: usize = p.blocks.iter().map(|b| b.text.len()).sum();

        // The bound overshoots by at most the block that crossed it.
        assert!(
            retained <= crate::MAX_TEXT_CHARS + PARA_CHARS,
            "blocks retained {retained} chars; the cap is {} and the input was {}",
            crate::MAX_TEXT_CHARS,
            paras * PARA_CHARS
        );
        // Vacuity control: it parsed, and it kept a real document's worth rather than stopping at
        // the first block. A bound that retained nothing would pass the assertion above.
        assert!(
            retained > crate::MAX_TEXT_CHARS / 2,
            "the parser retained almost nothing ({retained} chars) — bounded to uselessness"
        );
    }

    #[test]
    fn script_and_style_bodies_do_not_reach_the_text() {
        let d = doc("<html><body><style>.a{color:red}</style><p>Real text.</p><script>var x=1;</script></body></html>");
        assert_eq!(d.text, "Real text.");
    }

    #[test]
    fn a_less_than_inside_a_script_does_not_open_a_tag() {
        // The classic naive-scanner failure: `a < b` swallows the rest of the document.
        let d = doc("<html><body><script>if (a < b) { document.write('<p>ghost</p>'); }</script><p>Kept.</p></body></html>");
        assert_eq!(d.text, "Kept.");
        assert!(!d.text.contains("ghost"));
    }

    #[test]
    fn a_greater_than_inside_a_quoted_attribute_does_not_end_the_tag() {
        let d = doc(r#"<html><body><a href="https://ex.com/x" title="a > b">Link</a> after</body></html>"#);
        assert!(d.text.contains("Link"), "got {:?}", d.text);
        assert!(d.text.contains("after"), "got {:?}", d.text);
        assert_eq!(d.links.len(), 1);
    }

    #[test]
    fn comments_contain_no_elements() {
        let d = doc("<html><body><!-- <p>hidden</p> --><p>shown</p></body></html>");
        assert_eq!(d.text, "shown");
    }

    #[test]
    fn entities_decode_including_numeric_and_hex() {
        let d = doc("<html><body><p>caf&eacute;? no &mdash; caf&#233; &#x2014; ok &amp; fine</p></body></html>");
        assert!(d.text.contains("—"), "got {:?}", d.text);
        assert!(d.text.contains("café"), "got {:?}", d.text);
        assert!(d.text.contains("& fine"), "got {:?}", d.text);
        // Unknown entity left verbatim rather than silently dropped.
        assert!(d.text.contains("&eacute;"), "got {:?}", d.text);
    }

    #[test]
    fn relative_links_resolve_against_the_document_url() {
        let d = doc(r#"<html><body><a href="c.html">c</a><a href="/root">r</a><a href="//cdn/x">p</a></body></html>"#);
        let urls: Vec<&str> = d.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, vec!["https://ex.com/a/c.html", "https://ex.com/root", "https://cdn/x"]);
    }

    #[test]
    fn javascript_and_mailto_hrefs_are_not_offered_as_places_to_go() {
        let d = doc(r#"<html><body><a href="javascript:void(0)">x</a><a href="mailto:a@b.c">y</a></body></html>"#);
        assert!(d.links.is_empty(), "got {:?}", d.links);
    }

    #[test]
    fn nav_and_footer_are_stripped_when_an_article_carries_the_text() {
        let d = doc(
            "<html><body><nav><a href='/1'>Home</a><a href='/2'>About</a><a href='/3'>Contact</a></nav>\
             <article><p>The actual article body, which is substantially longer than the chrome \
             around it and is the thing a reader came for. It continues for a while.</p></article>\
             <footer><p>Copyright 2026 Someone</p></footer></body></html>",
        );
        assert!(d.text.contains("actual article body"));
        assert!(!d.text.contains("Copyright"), "footer survived: {:?}", d.text);
        assert!(!d.text.contains("About"), "nav survived: {:?}", d.text);
    }

    #[test]
    fn boilerplate_removal_never_returns_an_empty_document() {
        // A pure link index is ALL chrome. Returning nothing would read as "this page is blank".
        let d = doc("<html><body><nav><a href='/a'>Alpha</a><a href='/b'>Beta</a></nav></body></html>");
        assert!(d.has_text(), "content selection ate the whole document");
    }

    #[test]
    fn title_headings_lang_and_description_are_captured() {
        let d = doc(
            "<html lang=\"en-GB\"><head><title>The Title</title>\
             <meta name=\"description\" content=\"A summary.\"></head>\
             <body><h1>Top</h1><h2>Sub</h2><p>Body text here.</p></body></html>",
        );
        assert_eq!(d.title.as_deref(), Some("The Title"));
        assert_eq!(d.lang.as_deref(), Some("en-GB"));
        assert_eq!(d.description.as_deref(), Some("A summary."));
        assert_eq!(d.headings.len(), 2);
        assert_eq!(d.headings[0], Heading { level: 1, text: "Top".into() });
    }

    #[test]
    fn unclosed_and_misnested_tags_still_yield_their_text() {
        let d = doc("<html><body><div><p>one<div>two<span>three</div>four</body></html>");
        for want in ["one", "two", "three", "four"] {
            assert!(d.text.contains(want), "lost {want:?} in {:?}", d.text);
        }
    }

    #[test]
    fn a_client_rendered_page_says_so_rather_than_reading_as_empty() {
        let mut html = String::from("<html><body><div id=root></div><script>");
        html.push_str(&"var someBundledThing = 1; ".repeat(2_000));
        html.push_str("</script></body></html>");
        let d = doc(&html);
        assert!(
            d.warnings.iter().any(|w| matches!(w, Warning::LikelyClientRendered { .. })),
            "an app shell must not be reported as a page with nothing on it"
        );
    }

    /// **The negative control, and it came from a real false positive.**
    ///
    /// `doc.rust-lang.org/book/ch01-00` is a complete server-rendered chapter index: 288
    /// characters of real content beside 2,418 of ordinary page script. The first version of the
    /// heuristic flagged it. A warning that fires on a page which is merely *short* would read
    /// identically on a page that is genuinely unreadable, and would therefore be evidence about
    /// neither — so the short-but-complete case is asserted, not just the shell case.
    #[test]
    fn a_short_but_complete_page_is_not_reported_as_client_rendered() {
        let mut html = String::from(
            "<html><body><main><h1>Getting Started</h1><p>Let's start your journey. \
             There is a lot to learn, but every journey starts somewhere.</p></main><script>",
        );
        html.push_str(&"var nav = 1; ".repeat(180)); // ~2.3 KB, an ordinary amount
        html.push_str("</script></body></html>");
        let d = doc(&html);
        assert!(d.has_text());
        assert!(
            !d.warnings.iter().any(|w| matches!(w, Warning::LikelyClientRendered { .. })),
            "a short page that IS fully readable must not claim to be unreadable: {:?}",
            d.warnings
        );
    }

    #[test]
    fn image_alt_text_is_content() {
        let d = doc("<html><body><p><img alt=\"Figure 3: the decay curve\" src=\"x.png\"> caption</p></body></html>");
        assert!(d.text.contains("Figure 3"), "got {:?}", d.text);
    }

    #[test]
    fn block_elements_become_line_breaks_not_run_on_prose() {
        let d = doc("<html><body><p>One.</p><p>Two.</p><li>Three.</li></body></html>");
        assert_eq!(d.text, "One.\nTwo.\nThree.");
    }

    #[test]
    fn reduction_is_large_on_a_realistic_page() {
        let mut html = String::from("<html><head><style>");
        html.push_str(&".cls { margin: 0 } ".repeat(300));
        html.push_str("</style></head><body><nav>");
        html.push_str(&"<a href='/x'>Nav</a>".repeat(60));
        html.push_str("</nav><article><p>The real content of the page, which is short.</p></article></body></html>");
        let d = doc(&html);
        assert!(d.reduction() > 0.9, "only reduced {:.3}", d.reduction());
        assert!(d.text.contains("real content"));
    }
}
