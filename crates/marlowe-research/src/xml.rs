//! A small element tree over quick-xml for the XML channels (arXiv Atom, RSS/Atom feeds, PubMed
//! efetch). Names are local (prefix dropped). Tolerant of sloppy feeds: mismatched end tags close
//! the nearest open element of that name, unknown entities pass through verbatim, and a document
//! cut off mid-way keeps what was read.

use quick_xml::events::Event;
use quick_xml::Reader;

/// Bodies above this are refused rather than parsed.
pub const MAX_XML_BYTES: usize = 16 * 1024 * 1024;
/// Elements nested deeper than this are flattened into their parent (a hostile feed cannot
/// make the tree arbitrarily deep).
const MAX_DEPTH: usize = 64;

/// One element. The document itself is a root node with an empty name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    /// This element's own text and CDATA, concatenated (children's text excluded).
    pub text: String,
    /// All text at and below this element, in document order.
    pub all: String,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The first descendant (depth first, self excluded) named `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(n) = c.find(name) {
                return Some(n);
            }
        }
        None
    }

    /// Every descendant named `name`, document order.
    pub fn find_all<'a>(&'a self, name: &str) -> Vec<&'a Node> {
        let mut out = Vec::new();
        fn walk<'a>(n: &'a Node, name: &str, out: &mut Vec<&'a Node>) {
            for c in &n.children {
                if c.name == name {
                    out.push(c);
                }
                walk(c, name, out);
            }
        }
        walk(self, name, &mut out);
        out
    }

    /// All text below this element, document order.
    pub fn text_deep(&self) -> String {
        self.all.clone()
    }

    /// `text_deep` with whitespace collapsed.
    pub fn clean_text(&self) -> String {
        collapse(&self.text_deep())
    }

    /// The collapsed text of the first child named `name`.
    pub fn child_text(&self, name: &str) -> Option<String> {
        self.child(name).map(Node::clean_text)
    }
}

/// Collapses whitespace runs to one space and trims.
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn local(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, l)) => l.to_string(),
        None => s.into_owned(),
    }
}

/// Text goes to the innermost open element and, in order, to every element enclosing it.
fn push_text(stack: &mut [Node], s: &str) {
    if let Some(top) = stack.last_mut() {
        top.text.push_str(s);
    }
    for n in stack.iter_mut().skip(1) {
        n.all.push_str(s);
    }
}

/// Parses `xml` into a tree. Errors only on an oversized body or a document with no element.
pub fn parse(xml: &str) -> Result<Node, String> {
    if xml.len() > MAX_XML_BYTES {
        return Err(format!("XML body is {} bytes, above the {MAX_XML_BYTES} byte cap", xml.len()));
    }
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut saw_element = false;
    let open = |e: &quick_xml::events::BytesStart<'_>| Node {
        name: local(e.name().as_ref()),
        attrs: e
            .attributes()
            .flatten()
            .map(|a| {
                let v = a
                    .unescape_value()
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| decode_entities(&String::from_utf8_lossy(&a.value)));
                (local(a.key.as_ref()), v)
            })
            .collect(),
        ..Node::default()
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                saw_element = true;
                if stack.len() > MAX_DEPTH {
                    continue;
                }
                stack.push(open(&e));
            }
            Ok(Event::Empty(e)) => {
                saw_element = true;
                let n = open(&e);
                if let Some(top) = stack.last_mut() {
                    top.children.push(n);
                }
            }
            Ok(Event::End(e)) => {
                let name = local(e.name().as_ref());
                // Close up to the nearest open element of this name; a stray end tag is ignored.
                if let Some(pos) = stack.iter().rposition(|n| n.name == name) {
                    if pos == 0 {
                        continue;
                    }
                    while stack.len() > pos {
                        let n = stack.pop().unwrap_or_default();
                        if let Some(top) = stack.last_mut() {
                            top.children.push(n);
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let s = t
                    .unescape()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| decode_entities(&String::from_utf8_lossy(t.as_ref())));
                push_text(&mut stack, &s);
            }
            Ok(Event::CData(c)) => push_text(&mut stack, &String::from_utf8_lossy(c.as_ref())),
            Ok(Event::Eof) => break,
            Ok(_) => {}
            // Malformed input: keep what was read so far.
            Err(_) => break,
        }
    }
    while stack.len() > 1 {
        let n = stack.pop().unwrap_or_default();
        if let Some(top) = stack.last_mut() {
            top.children.push(n);
        }
    }
    if !saw_element {
        return Err("no XML element found".into());
    }
    Ok(stack.pop().unwrap_or_default())
}

/// XML/HTML entity decoding for text quick-xml would not unescape: the five XML entities,
/// numeric references and the handful of HTML entities feeds actually use.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = tail.find(';').filter(|e| *e <= 10).and_then(|e| {
            let ent = &tail[1..e];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "ndash" => Some('–'),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                "rsquo" => Some('’'),
                "lsquo" => Some('‘'),
                "rdquo" => Some('”'),
                "ldquo" => Some('“'),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, e + 1))
        });
        match decoded {
            Some((ch, n)) => {
                out.push(ch);
                rest = &tail[n..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tags stripped and entities decoded: for HTML fragments inside JSON or feeds (snippets,
/// abstracts in JATS). Never used to render; the result is plain text.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse(&decode_entities(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_use_local_names_and_keep_text_and_cdata() {
        let x = r#"<?xml version="1.0"?><rss xmlns:dc="x"><channel><item><title><![CDATA[A & B]]></title>
            <dc:creator>Ada</dc:creator><link>https://e.org/a?x=1&amp;y=2</link><bad>&nbsp;&#233;</bad></item>
            </channel></rss>"#;
        let root = parse(x).unwrap();
        let item = root.find("item").unwrap();
        assert_eq!(item.child_text("title").as_deref(), Some("A & B"));
        assert_eq!(item.child_text("creator").as_deref(), Some("Ada"));
        assert_eq!(item.child_text("link").as_deref(), Some("https://e.org/a?x=1&y=2"));
        assert_eq!(item.child("bad").unwrap().text, "\u{a0}é");
    }

    #[test]
    fn mixed_content_keeps_document_order() {
        let mixed = parse("<t>A <i>trial</i> of <b>many</b> things.</t>").unwrap();
        assert_eq!(mixed.child_text("t").as_deref(), Some("A trial of many things."));
    }

    #[test]
    fn sloppy_documents_still_parse() {
        let root = parse("<a><b>one</c><b>two</b></a>").unwrap();
        assert_eq!(root.find_all("b").len(), 2);
        let cut = parse("<feed><entry><title>kept</title><entry><title>half").unwrap();
        assert_eq!(cut.find_all("title").len(), 2);
        assert!(parse("just text").is_err());
        let deep = "<a>".repeat(500) + "x";
        assert!(parse(&deep).is_ok());
    }

    #[test]
    fn fragments_strip_to_text() {
        assert_eq!(strip_tags("<p>Rank <b>8</b> &amp; <span class=\"x\">64</span></p>"), "Rank 8 & 64");
        assert_eq!(decode_entities("&#x41;&bogus;&"), "A&bogus;&");
    }
}
