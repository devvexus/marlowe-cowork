//! The `paper` tool: a paper by arXiv id or https URL, read as structure (numbered sections, a
//! parsed reference list with DOIs and arXiv ids) and text.
//!
//! For an arXiv id the order is: arXiv's HTML (`arxiv.org/html/<id>`), then the e-print LaTeX
//! source, then the PDF. Every page fetch passes the process-wide host gate (robots.txt and
//! per-host spacing); arxiv.org's robots.txt disallows `/e-print` and asks for a 15 s crawl delay,
//! so the LaTeX step is normally refused (and says so) and arxiv.org requests are spaced. Title and
//! authors come from the arXiv API. GROBID is not used (an optional sidecar; see
//! docs/research/grobid.md). Everything returned is untrusted content.

use marlowe_agent::ToolOutcome;
use marlowe_extract::paper::{self as parse, Paper, PaperSource};
use marlowe_extract::{Format, Input};
use marlowe_model::CancelToken;
use marlowe_research::HostGate;
use serde_json::Value;

use crate::research::{arxiv_id, paper_url, HttpGet, HttpResponse, ResearchConfig, PAPER_WINDOW_CHARS};
use crate::text::floor_boundary;
use crate::{int_arg, str_arg};

/// A reading shorter than this is not the paper (an error page, an abstract-only stub).
const MIN_PAPER_CHARS: usize = 2_000;

/// exec's URL port as the research port, for the gate's robots.txt fetch.
struct Via<'a>(&'a dyn HttpGet);

impl marlowe_research::HttpGet for Via<'_> {
    fn get(
        &self,
        req: &marlowe_research::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<marlowe_research::HttpResponse, String> {
        let r = self.0.get(&req.url, cancel)?;
        Ok(marlowe_research::HttpResponse {
            status: r.status,
            content_type: r.content_type,
            bytes: r.bytes,
            final_url: r.final_url,
            retry_after: None,
        })
    }
}

/// Why one step of the reading order produced nothing.
enum Miss {
    Refused(String),
    Failed(String),
}

impl Miss {
    fn note(&self, what: &str) -> String {
        match self {
            Miss::Refused(why) => format!("{what}: not fetched ({why})"),
            Miss::Failed(why) => format!("{what}: {why}"),
        }
    }
}

/// One gated GET: robots.txt and host spacing first, then the request.
fn get(cfg: &ResearchConfig, gate: &HostGate, url: &str, cancel: &CancelToken) -> Result<HttpResponse, Miss> {
    gate.admit(url, &Via(cfg.http.as_ref()), cancel).map_err(|r| Miss::Refused(r.to_string()))?;
    // Transport errors can quote a redirect target, which is server-chosen text: keep none of it.
    let quiet = |e: String| if e.contains("://") || e.len() > 160 { "could not be reached".to_string() } else { e };
    let r = cfg.http.get(url, cancel).map_err(|e| Miss::Failed(quiet(e)))?;
    if r.status >= 400 {
        return Err(Miss::Failed(format!("HTTP {}", r.status)));
    }
    Ok(r)
}

/// Long enough to be the paper, or clearly structured (arXiv's "no HTML" page has no sections).
fn is_paper(p: &Paper) -> bool {
    let chars = p.text.chars().count();
    chars >= MIN_PAPER_CHARS || (p.sections.len() >= 3 && chars >= 500)
}

/// Reads fetched bytes as a paper, by what they are.
fn read(res: &HttpResponse) -> Result<Paper, String> {
    let ct = res.content_type.as_deref();
    let gz_or_tar =
        res.bytes.starts_with(&[0x1f, 0x8b]) || ct.is_some_and(|c| c.contains("x-eprint") || c.contains("x-tar"));
    if gz_or_tar {
        return parse::latex::from_source(&res.bytes).map_err(|e| e.kind().to_string());
    }
    match marlowe_extract::sniff::detect(&res.bytes, ct, Some(&res.final_url)) {
        Format::Html => {
            let (doc, blocks, _) = marlowe_extract::html::extract_structured(&res.bytes, ct, Some(&res.final_url));
            let arxiv = res.final_url.contains("arxiv.org/html/");
            Ok(parse::from_html(&doc, &blocks, arxiv))
        }
        _ => {
            let doc = marlowe_extract::extract(&Input::new(&res.bytes).content_type(ct).url(Some(&res.final_url)))
                .map_err(|e| e.kind().to_string())?;
            let mut p = parse::from_text(&doc.text, doc.title.clone().filter(|t| !t.trim().is_empty()));
            if doc.format != Format::Pdf {
                p.source = PaperSource::Html;
            }
            if let Some(w) = doc.warnings.iter().find(|w| matches!(w, marlowe_extract::Warning::NoTextLayer { .. })) {
                return if p.text.trim().is_empty() { Err(w.to_string()) } else { Ok(p) };
            }
            Ok(p)
        }
    }
}

/// arXiv: HTML, then LaTeX, then PDF. Returns the paper, the URL read and what was skipped.
fn read_arxiv(
    cfg: &ResearchConfig,
    gate: &HostGate,
    id: &str,
    cancel: &CancelToken,
) -> Result<(Paper, String, Vec<String>), String> {
    let mut notes = Vec::new();
    let steps = [
        ("arXiv HTML", format!("https://arxiv.org/html/{id}")),
        ("LaTeX source", format!("https://arxiv.org/e-print/{id}")),
        ("PDF", format!("https://arxiv.org/pdf/{id}")),
    ];
    let last = steps.len() - 1;
    for (i, (what, url)) in steps.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err("interrupted".into());
        }
        match get(cfg, gate, url, cancel) {
            Ok(res) => match read(&res) {
                Ok(p) if is_paper(&p) || i == last => {
                    return Ok((p, res.final_url, notes));
                }
                Ok(_) => notes.push(format!("{what}: too short to be the paper")),
                Err(e) => notes.push(format!("{what}: unreadable ({e})")),
            },
            Err(m) => {
                if i == last {
                    return Err(format!("{url}: {}", m.note(what)));
                }
                notes.push(m.note(what));
            }
        }
    }
    Err("no readable version".into())
}

pub(crate) fn paper(cfg: &ResearchConfig, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let input = crate::research_input(args);
    let url = match paper_url(input) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::error(e),
    };
    let offset = int_arg(args, "offset").unwrap_or(0).max(0) as usize;
    let references = str_arg(args, "part").is_some_and(|p| p.trim().eq_ignore_ascii_case("references"));
    let arxiv = arxiv_id(input);
    let gate = crate::corpus::shared_gate();
    let outcome = match &arxiv {
        Some(id) => read_arxiv(cfg, &gate, id, cancel),
        None => match get(cfg, &gate, &url, cancel) {
            Ok(res) => read(&res).map(|p| (p, res.final_url, Vec::new())).map_err(|e| {
                format!("{url} returned {} bytes that could not be turned into text ({e}).", res.bytes.len())
            }),
            Err(Miss::Failed(e)) if e.starts_with("HTTP ") => Err(format!("{url} returned {e}.")),
            Err(m) => Err(format!("could not fetch {}", m.note(&url))),
        },
    };
    let (mut p, final_url, notes) = match outcome {
        Ok(r) => r,
        Err(e) => return ToolOutcome::error(e),
    };
    // arXiv ids get title and authors from the API (cached; a failure is not fatal).
    let meta = arxiv.as_ref().and_then(|id| {
        crate::research::feed_for(cfg, &crate::research::arxiv_url(id, 1), cancel)
            .ok()
            .and_then(|(f, _)| f.entries.into_iter().next())
    });
    let authors = meta.as_ref().map(|m| m.authors.clone()).unwrap_or_default();
    if let Some(m) = &meta {
        p.title = Some(m.title.clone());
    }
    render(&final_url, &p, &authors, &notes, offset, references)
}

fn window(text: &str, offset: usize) -> (String, usize, usize) {
    let total = text.chars().count();
    let start = text.char_indices().nth(offset).map_or(text.len(), |(i, _)| i);
    let rest = &text[start..];
    let end = rest.char_indices().nth(PAPER_WINDOW_CHARS).map_or(rest.len(), |(i, _)| i);
    let w = &rest[..floor_boundary(rest, end)];
    (w.to_string(), w.chars().count(), total)
}

fn reference_line(r: &parse::Reference) -> String {
    let mut line = match &r.label {
        Some(l) => format!("[{l}] {}", r.text),
        None => r.text.clone(),
    };
    let ids: Vec<String> =
        [r.doi.as_ref().map(|d| format!("doi:{d}")), r.arxiv_id.as_ref().map(|a| format!("arXiv:{a}"))]
            .into_iter()
            .flatten()
            .collect();
    if !ids.is_empty() {
        line.push_str(&format!(" ({})", ids.join(", ")));
    }
    line
}

fn render(url: &str, p: &Paper, authors: &[String], notes: &[String], offset: usize, refs: bool) -> ToolOutcome {
    let mut out = format!("paper {url} · {}\n", p.source.as_str());
    if let Some(t) = &p.title {
        out.push_str(&format!("Title: {t}\n"));
    }
    if !authors.is_empty() {
        out.push_str(&format!("Authors: {}\n", authors.join(", ")));
    }
    for n in notes {
        out.push_str(&format!("[{n}]\n"));
    }
    if !p.sections.is_empty() {
        let names: Vec<String> = p
            .sections
            .iter()
            .take(60)
            .map(|s| match &s.number {
                Some(n) => format!("{n} {}", s.title),
                None => s.title.clone(),
            })
            .collect();
        out.push_str(&format!("Sections: {}\n", names.join(" | ")));
    }
    if !p.references.is_empty() && !refs {
        out.push_str(&format!("References: {} parsed (read them with part=\"references\")\n", p.references.len()));
    }
    let body = if refs {
        if p.references.is_empty() {
            return ToolOutcome::error("no reference list was found in this paper");
        }
        p.references.iter().map(reference_line).collect::<Vec<_>>().join("\n")
    } else {
        let mut b = String::new();
        if let Some(a) = p.abstract_text.as_ref().filter(|a| !a.trim().is_empty() && offset == 0) {
            b.push_str(&format!("Abstract: {}\n\n", a.trim()));
        }
        b.push_str(&p.text);
        b
    };
    let (w, shown, total) = window(&body, offset);
    out.push('\n');
    out.push_str(&w);
    if offset + shown < total {
        out.push_str(&format!(
            "\n\n[showing characters {offset}-{} of {total}. Continue with offset {}.]",
            offset + shown,
            offset + shown
        ));
    }
    let summary = if refs { format!("{} references", p.references.len()) } else { format!("{total} chars") };
    ToolOutcome::ok(out, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_research::Robots;
    use std::sync::{Arc, Mutex};

    /// Answers by URL substring and records what was asked.
    struct Routes(Vec<(&'static str, u16, &'static str, Vec<u8>)>, Mutex<Vec<String>>);
    impl HttpGet for Routes {
        fn get(&self, url: &str, _: &CancelToken) -> Result<HttpResponse, String> {
            self.1.lock().unwrap().push(url.to_string());
            let (_, status, ct, body) = self.0.iter().find(|r| url.contains(r.0)).ok_or("no route")?;
            Ok(HttpResponse {
                status: *status,
                content_type: Some(ct.to_string()),
                bytes: body.clone(),
                final_url: url.into(),
            })
        }
    }

    #[test]
    fn arxiv_robots_refuse_the_eprint_and_the_pdf_is_read_instead() {
        let pdf_text = format!(
            "Abstract\nWe study things.\n1 Introduction\n{}\nReferences\n[1] A. B. A title here. 2020.\n",
            "Words. ".repeat(400)
        );
        let routes = Arc::new(Routes(
            vec![
                ("arxiv.org/html/", 404, "text/html", b"<html><body>No HTML for this paper</body></html>".to_vec()),
                ("arxiv.org/e-print/", 200, "application/x-eprint-tar", b"never served".to_vec()),
                ("arxiv.org/pdf/", 200, "text/plain; charset=utf-8", pdf_text.into_bytes()),
            ],
            Mutex::new(Vec::new()),
        ));
        let cfg = ResearchConfig { http: routes.clone(), ..ResearchConfig::default() };
        let gate = HostGate::new();
        // arxiv.org's robots.txt as of 2026-10, crawl delay dropped to keep the test fast.
        gate.insert(
            "https://arxiv.org",
            Robots::parse("User-agent: *\nAllow: /html\nAllow: /pdf\nDisallow: /e-print\n"),
        );
        let (p, url, notes) = read_arxiv(&cfg, &gate, "2301.01234", &CancelToken::new()).unwrap();
        assert_eq!(url, "https://arxiv.org/pdf/2301.01234");
        assert_eq!(p.references.len(), 1);
        assert_eq!(notes[0], "arXiv HTML: HTTP 404");
        assert!(
            notes[1].starts_with("LaTeX source: not fetched (robots.txt on https://arxiv.org disallows /e-print"),
            "{notes:?}"
        );
        assert!(
            !routes.1.lock().unwrap().iter().any(|u| u.contains("e-print")),
            "a disallowed path is never requested"
        );
    }
}
