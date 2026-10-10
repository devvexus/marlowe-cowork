//! arXiv e-print sources: gzip / tar / single `.tex` in, a [`Paper`] out.
//!
//! The converter is a reader, not a TeX engine: it keeps prose, section structure, inline and
//! display math as TeX, figure and table captions, list items and verbatim code; it drops layout
//! commands and resolves `\input`/`\include` inside the archive only.

use std::io::Read;

use super::{reference_from_text, sections_from_markdown, tar, Paper, PaperSource, Reference, MAX_REFERENCES};
use crate::ExtractError;

/// Nesting limit for `\input`, and a cap on the inlined source.
const MAX_INPUT_DEPTH: usize = 8;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Commands whose arguments are layout, not prose: dropped with their arguments.
const DROP: [&str; 40] = [
    "label",
    "vspace",
    "hspace",
    "includegraphics",
    "bibliographystyle",
    "bibliography",
    "usepackage",
    "newcommand",
    "renewcommand",
    "providecommand",
    "def",
    "setlength",
    "addtolength",
    "setcounter",
    "addcontentsline",
    "pagestyle",
    "thispagestyle",
    "documentclass",
    "input",
    "include",
    "graphicspath",
    "definecolor",
    "hypersetup",
    "newtheorem",
    "DeclareMathOperator",
    "newenvironment",
    "linewidth",
    "maketitle",
    "centering",
    "noindent",
    "newpage",
    "clearpage",
    "small",
    "footnotesize",
    "tiny",
    "large",
    "Large",
    "normalsize",
    "tableofcontents",
    "appendix",
];

/// Environments kept only for their captions.
const FLOATS: [&str; 6] = ["figure", "figure*", "table", "table*", "algorithm", "wrapfigure"];
const MATH_ENVS: [&str; 10] = [
    "equation",
    "equation*",
    "align",
    "align*",
    "gather",
    "gather*",
    "multline",
    "multline*",
    "eqnarray",
    "displaymath",
];
const VERBATIM: [&str; 4] = ["verbatim", "lstlisting", "minted", "Verbatim"];

/// Reads an e-print: gzip and tar are unwrapped; a bare `.tex` is read as is.
pub fn from_source(bytes: &[u8]) -> Result<Paper, ExtractError> {
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .take(crate::MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| ExtractError::Malformed { detail: format!("gzip: {e}") })?;
        if out.len() > crate::MAX_INPUT_BYTES {
            return Err(ExtractError::TooLarge { bytes: out.len() });
        }
        out
    } else {
        bytes.to_vec()
    };
    let files: Vec<(String, Vec<u8>)> =
        if tar::is_tar(&raw) { tar::entries(&raw) } else { vec![("main.tex".to_string(), raw)] };
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let tex: Vec<(String, String)> = files
        .iter()
        .filter(|(n, _)| n.ends_with(".tex") || n.ends_with(".ltx"))
        .map(|(n, b)| (n.clone(), text(b)))
        .collect();
    let main = tex
        .iter()
        .filter(|(_, t)| t.contains("\\begin{document}"))
        .max_by_key(|(n, t)| (n.ends_with("main.tex") || n.ends_with("ms.tex"), t.len()))
        .or_else(|| tex.iter().max_by_key(|(_, t)| t.len()))
        .ok_or_else(|| ExtractError::Unsupported { format: "latex", detail: "no .tex file in the source".into() })?;
    let source = inline_inputs(&strip_comments(&main.1), &tex, 0);
    let bbl = files.iter().find(|(n, _)| n.ends_with(".bbl")).map(|(_, b)| strip_comments(&text(b)));
    let bib: Vec<String> = files.iter().filter(|(n, _)| n.ends_with(".bib")).map(|(_, b)| text(b)).collect();
    Ok(from_tex(&source, bbl.as_deref(), &bib))
}

/// Removes `%` comments (not `\%`).
pub fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let mut prev = '\0';
        let mut cut = line.len();
        for (i, c) in line.char_indices() {
            if c == '%' && prev != '\\' {
                cut = i;
                break;
            }
            prev = c;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

fn inline_inputs(src: &str, files: &[(String, String)], depth: usize) -> String {
    if depth >= MAX_INPUT_DEPTH {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find(['\\']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let cmd = ["\\input{", "\\include{", "\\subfile{"].iter().find(|c| tail.starts_with(**c));
        match cmd.and_then(|c| tail[c.len()..].find('}').map(|e| (c.len(), e))) {
            Some((start, end)) => {
                let name = tail[start..start + end].trim();
                let found = files.iter().find(|(n, _)| {
                    let n = n.trim_start_matches("./");
                    n == name
                        || n == format!("{name}.tex")
                        || n.ends_with(&format!("/{name}.tex"))
                        || n.ends_with(&format!("/{name}"))
                });
                if let Some((_, body)) = found {
                    if out.len() + body.len() <= MAX_SOURCE_BYTES {
                        out.push_str(&inline_inputs(&strip_comments(body), files, depth + 1));
                    }
                }
                rest = &tail[start + end + 1..];
            }
            None => {
                out.push('\\');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The `{...}` group starting at `s[0] == '{'`: (content, bytes consumed).
fn group(s: &str) -> Option<(&str, usize)> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut prev = '\0';
    for (i, c) in s.char_indices() {
        match c {
            '{' if prev != '\\' => depth += 1,
            '}' if prev != '\\' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[1..i], i + 1));
                }
            }
            _ => {}
        }
        prev = if prev == '\\' && c == '\\' { '\0' } else { c };
    }
    None
}

/// Skips one `[...]` optional argument at the start of `s`.
fn skip_opt(s: &str) -> usize {
    if !s.starts_with('[') {
        return 0;
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    0
}

/// Converts a TeX source to a [`Paper`].
pub fn from_tex(src: &str, bbl: Option<&str>, bib: &[String]) -> Paper {
    let preamble_end = src.find("\\begin{document}").unwrap_or(0);
    let title = find_arg(&src[..preamble_end.max(1).min(src.len())], "\\title")
        .or_else(|| find_arg(src, "\\title"))
        .map(|t| squash(&to_text(t)));
    let body_start = src.find("\\begin{document}").map_or(0, |i| i + "\\begin{document}".len());
    let body_end = src.find("\\end{document}").unwrap_or(src.len()).max(body_start);
    let body = &src[body_start..body_end];
    let abstract_text = env_body(body, "abstract").map(|a| squash(&to_text(a)));
    let mut references = Vec::new();
    let mut text_src = body.to_string();
    if let Some(b) = env_span(body, "thebibliography") {
        references = bibitems(&body[b.clone()]);
        text_src.replace_range(b, "");
    }
    if let Some(a) = env_span(&text_src, "abstract") {
        text_src.replace_range(a, "");
    }
    if references.is_empty() {
        if let Some(b) = bbl {
            references = bibitems(b);
        }
    }
    if references.is_empty() {
        references = bib.iter().flat_map(|b| bibtex(b)).take(MAX_REFERENCES).collect();
    }
    let text = tidy(&to_text(&text_src));
    let sections = sections_from_markdown(&text);
    Paper { source: PaperSource::Latex, title, abstract_text, text, sections, references }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapses runs of blank lines and trims line ends.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    let mut fence = false;
    for line in s.lines() {
        let l = if fence { line.trim_end() } else { line.trim() };
        if l.starts_with("```") {
            fence = !fence;
        }
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

fn find_arg<'a>(s: &'a str, cmd: &str) -> Option<&'a str> {
    let i = s.find(cmd)?;
    let rest = &s[i + cmd.len()..];
    let rest = &rest[skip_opt(rest)..];
    let rest = rest.trim_start();
    group(rest).map(|(c, _)| c)
}

fn env_span(s: &str, env: &str) -> Option<std::ops::Range<usize>> {
    let open = format!("\\begin{{{env}}}");
    let close = format!("\\end{{{env}}}");
    let a = s.find(&open)?;
    let b = s[a..].find(&close).map(|j| a + j + close.len())?;
    Some(a..b)
}

fn env_body<'a>(s: &'a str, env: &str) -> Option<&'a str> {
    let r = env_span(s, env)?;
    let open = format!("\\begin{{{env}}}");
    let close = format!("\\end{{{env}}}");
    Some(&s[r.start + open.len()..r.end - close.len()])
}

/// `\bibitem[opt]{key} text` entries.
fn bibitems(s: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    let parts = s.split("\\bibitem").skip(1);
    for p in parts {
        if out.len() >= MAX_REFERENCES {
            break;
        }
        let p = p.split("\\end{thebibliography}").next().unwrap_or(p);
        let o = skip_opt(p.trim_start());
        let p = p.trim_start()[o..].trim_start();
        let (key, used) = group(p).map(|(k, n)| (Some(k.trim().to_string()), n)).unwrap_or((None, 0));
        let body = to_text(&p[used..].replace("\\newblock", " "));
        let mut r = reference_from_text(&squash(&body), &[]);
        r.label = key;
        if r.text.len() >= 10 {
            out.push(r);
        }
    }
    out
}

/// Entries of a `.bib` file.
fn bibtex(s: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    for entry in s.split('@').skip(1) {
        let Some(brace) = entry.find('{') else { continue };
        let kind = entry[..brace].trim().to_ascii_lowercase();
        if matches!(kind.as_str(), "comment" | "string" | "preamble") {
            continue;
        }
        let Some((inner, _)) = group(&entry[brace..]) else { continue };
        let key = inner.split(',').next().unwrap_or("").trim().to_string();
        let field = |name: &str| bib_field(inner, name).map(|v| squash(&to_text(&v)));
        let (title, author, year) = (field("title"), field("author"), field("year"));
        let venue = field("journal").or_else(|| field("booktitle"));
        let mut text = String::new();
        for part in [author.clone(), title.clone(), venue, year.clone()].into_iter().flatten() {
            text.push_str(&part);
            text.push_str(". ");
        }
        let mut r = reference_from_text(text.trim(), &[]);
        r.label = Some(key);
        r.title = title.or(r.title);
        if let Some(d) = field("doi") {
            r.doi = Some(d.trim_start_matches("https://doi.org/").to_ascii_lowercase());
        }
        if let Some(e) = field("eprint").filter(|e| e.contains('.') || e.contains('/')) {
            r.arxiv_id = Some(e);
        }
        r.url = field("url").or(r.url);
        r.year = year.and_then(|y| y.trim().parse().ok()).or(r.year);
        out.push(r);
        if out.len() >= MAX_REFERENCES {
            break;
        }
    }
    out
}

fn bib_field(inner: &str, name: &str) -> Option<String> {
    let lower = inner.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = lower[at + name.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let v = inner[inner.len() - after.len() + 1..].trim_start();
            return if v.starts_with('{') {
                group(v).map(|(c, _)| c.to_string())
            } else if let Some(q) = v.strip_prefix('"') {
                q.find('"').map(|e| q[..e].to_string())
            } else {
                Some(v.split([',', '}', '\n']).next().unwrap_or("").trim().to_string())
            };
        }
        from = at + name.len();
    }
    None
}

/// TeX to readable markdown-ish text.
pub fn to_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = b[i];
        match c {
            b'\\' => i += command(&s[i..], &mut out),
            b'{' | b'}' => i += 1,
            b'~' => {
                out.push(' ');
                i += 1;
            }
            b'$' => {
                // Math stays TeX: `$...$` and `$$...$$`.
                let display = s[i..].starts_with("$$");
                let open = if display { 2 } else { 1 };
                let close = if display { "$$" } else { "$" };
                match s[i + open..].find(close) {
                    Some(e) => {
                        out.push_str(&s[i..i + open + e + close.len()]);
                        i += open + e + close.len();
                    }
                    None => {
                        out.push('$');
                        i += 1;
                    }
                }
            }
            _ => {
                let next = s[i..].find(['\\', '{', '}', '~', '$']).map_or(s.len(), |j| i + j);
                out.push_str(
                    &s[i..next].replace("``", "\"").replace("''", "\"").replace("---", "—").replace("--", "–"),
                );
                i = next;
            }
        }
    }
    out
}

/// Handles one command at `s[0] == '\\'`, appending its rendering; returns bytes consumed.
fn command(s: &str, out: &mut String) -> usize {
    let name_len = s[1..].find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len() - 1);
    if name_len == 0 {
        // Control symbol: \\, \&, \%, \_, \#, \$, \{, \}, \,, \ (space).
        let sym = s[1..].chars().next().unwrap_or(' ');
        match sym {
            '\\' => out.push('\n'),
            '&' | '%' | '_' | '#' | '$' | '{' | '}' => out.push(sym),
            _ => out.push(' '),
        }
        return 1 + sym.len_utf8();
    }
    let name = &s[1..1 + name_len];
    let mut at = 1 + name_len;
    let star = s[at..].starts_with('*');
    if star {
        at += 1;
    }
    let arg = |from: usize| -> Option<(&str, usize)> {
        let rest = &s[from..];
        let o = skip_opt(rest);
        let r = &rest[o..];
        let ws = r.len() - r.trim_start().len();
        group(r.trim_start()).map(|(c, n)| (c, from + o + ws + n))
    };
    match name {
        "section" | "subsection" | "subsubsection" | "paragraph" | "chapter" => {
            let level = match name {
                "chapter" => 1,
                "section" => 2,
                "subsection" => 3,
                "subsubsection" => 4,
                _ => 5,
            };
            if let Some((t, end)) = arg(at) {
                let t = squash(&to_text(t));
                if name == "paragraph" {
                    out.push_str(&format!("\n\n**{t}** "));
                } else {
                    out.push_str(&format!("\n\n{} {t}\n\n", "#".repeat(level)));
                }
                return end;
            }
            at
        }
        "begin" => {
            let Some((env, end)) = arg(at) else { return at };
            let env = env.trim();
            let close = format!("\\end{{{env}}}");
            let body_end = s[end..].find(&close).map(|j| end + j);
            let after = body_end.map_or(s.len(), |j| j + close.len());
            let body = &s[end..body_end.unwrap_or(s.len())];
            if FLOATS.contains(&env) {
                if let Some(cap) = find_arg(body, "\\caption") {
                    let label = if env.starts_with("table") { "Table" } else { "Figure" };
                    out.push_str(&format!("\n\n{label}: {}\n\n", squash(&to_text(cap))));
                }
            } else if MATH_ENVS.contains(&env) {
                out.push_str(&format!("\n\n$$\n{}\n$$\n\n", body.trim()));
            } else if VERBATIM.contains(&env) {
                let code = body.trim_start_matches(['[', '{']).trim_matches('\n');
                out.push_str(&format!("\n\n```\n{}\n```\n\n", code.trim_end()));
            } else if env == "itemize" || env == "enumerate" || env == "description" {
                out.push('\n');
                out.push_str(&to_text(body));
                out.push('\n');
            } else {
                out.push_str(&to_text(body));
            }
            after
        }
        "end" => arg(at).map_or(at, |(_, e)| e),
        "item" => {
            out.push_str("\n- ");
            let at = at + skip_opt(&s[at..]);
            at + (s[at..].len() - s[at..].trim_start().len())
        }
        "cite" | "citep" | "citet" | "citealp" | "citeauthor" | "citeyear" | "autocite" | "parencite" | "textcite" => {
            match arg(at) {
                Some((keys, end)) => {
                    out.push_str(&format!("[{}]", keys.split(',').map(str::trim).collect::<Vec<_>>().join(", ")));
                    end
                }
                None => at,
            }
        }
        "ref" | "eqref" | "autoref" | "cref" | "Cref" | "pageref" => {
            out.push_str("[ref]");
            arg(at).map_or(at, |(_, e)| e)
        }
        "url" => match arg(at) {
            Some((u, end)) => {
                out.push_str(u.trim());
                end
            }
            None => at,
        },
        "href" => match arg(at) {
            Some((u, e1)) => match arg(e1) {
                Some((t, e2)) => {
                    out.push_str(&format!("{} ({})", to_text(t).trim(), u.trim()));
                    e2
                }
                None => e1,
            },
            None => at,
        },
        "footnote" => match arg(at) {
            Some((t, end)) => {
                out.push_str(&format!(" ({})", squash(&to_text(t))));
                end
            }
            None => at,
        },
        "caption" => match arg(at) {
            Some((t, end)) => {
                out.push_str(&format!("\n\nCaption: {}\n\n", squash(&to_text(t))));
                end
            }
            None => at,
        },
        n if DROP.contains(&n) => {
            let mut end = at + skip_opt(&s[at..]);
            // Definitions take a name and a body; everything else in DROP takes one argument.
            let groups =
                if matches!(n, "newcommand" | "renewcommand" | "providecommand" | "newenvironment") { 2 } else { 1 };
            for _ in 0..groups {
                match arg(end) {
                    Some((_, e)) => end = e,
                    None => break,
                }
            }
            end
        }
        // Formatting and unknown commands: the command goes, its arguments' text stays.
        _ => at,
    }
}
