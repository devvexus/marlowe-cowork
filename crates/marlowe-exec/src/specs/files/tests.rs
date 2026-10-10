//! The files group end to end through the host: `parity_` tests for design 2.1 T1-T9, T19, B1,
//! B6 (writes), and donor cases run as tool calls.
//!
//! Donor fixtures (M4.6 donor policy: their cases, our code):
//!
//! | Source | Repo, sha | Licence |
//! |---|---|---|
//! | `codex-rs/apply-patch/tests/fixtures/scenarios` (25 scenarios) | openai/codex `6c0c6759d0617d7670826b61aca0a931bbb6db1f` | Apache-2.0 |
//! | `crates/tools/src/fs_tools.rs` tests | flashback7766/flashagent `0b08afcd6042615d38d87e80c3e5edda2a49da71` | MIT |
//!
//! The Codex scenarios are V4A patches; each becomes the `write` / `edit` / `multi_edit` calls a
//! model would make for the same intent, against the scenario's input and expected trees.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_agent::{
    CallCtx, CallFlow, Consequence, Conversation, Exposure, SandboxView, SessionScope, StepInfo, ToolCtx, ToolHost,
    ToolMiddleware, ToolOutcome, TurnFlow,
};
use marlowe_model::{CancelToken, ChatRequest, Message, ResultPart, ToolCall, ToolResult};
use marlowe_os::workspace::{ExecOutput, ExecRequest, LocalFs, Meta, WalkEntry, WalkOpts, Workspace};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::{with_edit_lines, BlobDir};
use crate::BuiltinTools;

// ---------------------------------------------------------------- harness

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One session against a host rooted at a temp dir: calls go through the group's middleware
/// (`before_approval`) and then the host with a shared [`SessionScope`], as the loop runs them.
struct Session {
    root: PathBuf,
    tools: BuiltinTools,
    mw: Vec<Arc<dyn ToolMiddleware>>,
    scope: SessionScope,
    n: usize,
}

impl Session {
    fn new(name: &str) -> Self {
        Self::with(name, |t| t)
    }

    fn with(name: &str, f: impl FnOnce(BuiltinTools) -> BuiltinTools) -> Self {
        let root = temp(name);
        let tools = f(BuiltinTools::new(Some(root.clone())).with_state(BlobDir(root.join(".blobs"))));
        let mw = tools.middleware();
        Self { root, tools, mw, scope: SessionScope::default(), n: 0 }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn put(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let p = self.p(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn get(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.p(rel)).unwrap()
    }

    fn text(&self, rel: &str) -> String {
        String::from_utf8(self.get(rel)).unwrap()
    }

    fn cx(&self) -> CallCtx {
        let tool = ToolCtx::new(SessionId::from("s"), 0, Some(self.root.clone()), CancelToken::new(), false);
        CallCtx::new(tool, self.scope.clone())
    }

    fn call(&mut self, name: &str, args: Value) -> ToolOutcome {
        self.n += 1;
        let mut call = ToolCall { id: format!("c{}", self.n), name: name.into(), args };
        let cx = self.cx();
        for m in &self.mw {
            if let CallFlow::Done(o) = m.before_approval(&mut call, &cx) {
                return o;
            }
        }
        self.tools.call_scoped(&call, &cx)
    }

    fn ok(&mut self, name: &str, args: Value) -> ToolOutcome {
        let o = self.call(name, args.clone());
        assert!(!o.is_error, "{name} {args}: {}", o.content);
        o
    }

    fn err(&mut self, name: &str, args: Value) -> ToolOutcome {
        let o = self.call(name, args.clone());
        assert!(o.is_error, "{name} {args} should fail: {}", o.content);
        o
    }

    fn read(&mut self, rel: &str) -> ToolOutcome {
        self.ok("read", json!({"path": rel}))
    }

    /// The loop's step end: extensions back on the conversation, every `after_step`, and the
    /// messages it returns (they go into the next request).
    fn step_end(&mut self) -> Vec<Message> {
        let mut convo = Conversation::new(SessionId::from("s"), Some(self.root.clone()));
        convo.ext = std::mem::take(&mut *self.scope.ext.lock().unwrap());
        let out = self.mw.iter().flat_map(|m| m.after_step(StepInfo { turn: 0, step: 1 }, &mut convo)).collect();
        *self.scope.ext.lock().unwrap() = std::mem::take(&mut convo.ext);
        out
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// [`LocalFs`] with writes that fail for paths ending in `fail_suffix`; `crash` makes them fail
/// the way a crash between the temp file and the rename would (a partial temp left behind).
struct FlakyPort {
    fail_suffix: &'static str,
    crash: bool,
}

impl Workspace for FlakyPort {
    fn kind(&self) -> String {
        "local".into()
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        LocalFs.read(path)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if path.to_string_lossy().ends_with(self.fail_suffix) {
            if self.crash {
                let tmp = path.with_file_name(".crash.tmp");
                std::fs::write(&tmp, &bytes[..bytes.len() / 2])?;
                return Err(std::io::Error::other("simulated crash before the rename"));
            }
            return Err(std::io::Error::other("simulated write failure"));
        }
        LocalFs.write(path, bytes)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        LocalFs.rename(from, to)
    }
    fn walk(&self, dir: &Path, opts: &WalkOpts) -> std::io::Result<Vec<WalkEntry>> {
        LocalFs.walk(dir, opts)
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Meta> {
        LocalFs.metadata(path)
    }
    fn exec(&self, req: &ExecRequest, cancelled: &dyn Fn() -> bool) -> std::io::Result<ExecOutput> {
        LocalFs.exec(req, cancelled)
    }
}

const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// A minimal PDF with one Helvetica text line per page and a correct xref.
fn pdf(pages: &[&str]) -> Vec<u8> {
    let n = pages.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objs = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
    ];
    for (i, t) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        let stream = format!("BT /F1 12 Tf 72 720 Td ({t}) Tj ET");
        objs.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (k, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", k + 1));
    }
    let xref = out.len();
    out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
    for off in offsets {
        out.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
    out.into_bytes()
}

// ---------------------------------------------------------------- parity: read (T1-T4)

/// T1: `cat -n` numbering, a 2,000-line window and the exact continue hint; `offset`/`limit`
/// windows number from the file's start; a huge `limit` reads to the end (FlashAgent
/// `a_window_of_a_file_is_read_without_reading_the_rest`, `a_huge_limit_reads_to_the_end`).
#[test]
fn parity_t1_read_window_and_continue_hint_exact() {
    let mut s = Session::with("t1", |t| t.with_context_tokens(1_000_000));
    let body: String = (1..=2500).map(|n| format!("l{n}\n")).collect();
    s.put("big.txt", &body);
    let o = s.read("big.txt");
    assert!(o.content.starts_with("     1\tl1\n     2\tl2\n"), "{}", &o.content[..40]);
    assert!(
        o.content.ends_with("  2000\tl2000\n\n\n[showing lines 1-2000 of 2500. Continue with offset 2001.]"),
        "{}",
        &o.content[o.content.len() - 120..]
    );
    let o = s.ok("read", json!({"path": "big.txt", "offset": 2001}));
    assert!(o.content.starts_with("  2001\tl2001\n") && o.content.ends_with("  2500\tl2500\n"));
    assert!(!o.content.contains("[showing"), "the end of the file needs no hint");
    let o = s.ok("read", json!({"path": "big.txt", "offset": 11, "limit": 3}));
    assert_eq!(o.content, "    11\tl11\n    12\tl12\n    13\tl13\n");
    let o = s.ok("read", json!({"path": "big.txt", "offset": 2499, "limit": i64::MAX}));
    assert_eq!(o.content, "  2499\tl2499\n  2500\tl2500\n");
    let e = s.err("read", json!({"path": "big.txt", "offset": 9000, "limit": 5}));
    assert!(e.content.contains("past the end") && e.content.contains("2500 lines"), "{}", e.content);
    // On a small context window the hint also points at grep.
    let mut small = Session::with("t1s", |t| t.with_context_tokens(8_000));
    small.put("big.txt", &body);
    assert!(small.read("big.txt").content.contains("`grep` searches inside files"));
}

/// T1 past 4 MB: a 6 MB log reads at any offset, its line count is the whole file's, and a stray
/// invalid byte deep inside does not make it binary.
#[test]
fn parity_t1_a_6mb_file_reads_at_an_offset_past_4mb() {
    let mut s = Session::new("t1big");
    let mut body: Vec<u8> = (1..=100_000).flat_map(|n| format!("line {n:06} {}\n", "x".repeat(48)).into_bytes()).collect();
    assert!(body.len() > 6_000_000);
    let at = body.len() - 1000;
    body[at] = 0xFF; // in line 99_984
    s.put("run.log", &body);
    let o = s.ok("read", json!({"path": "run.log", "offset": 90_000, "limit": 2}));
    let x = "x".repeat(48);
    assert_eq!(o.content, format!(" 90000\tline 090000 {x}\n 90001\tline 090001 {x}\n"));
    let o = s.read("run.log");
    assert!(o.content.contains("of 100000. Continue with offset"), "{}", &o.content[o.content.len() - 300..]);
    assert!(!o.content.contains("invalid bytes"), "only a window that shows one says so");
    let o = s.ok("read", json!({"path": "run.log", "offset": 99_980, "limit": 10}));
    assert!(o.content.contains('\u{fffd}') && o.content.contains("invalid bytes show as"), "{}", o.content);
    let o = s.ok("read", json!({"path": "run.log", "range": "99999-100000"}));
    assert!(o.content.starts_with(" 99999\tline 099999") && o.content.contains("100000\tline 100000"), "{}", o.content);
    let e = s.err("read", json!({"path": "run.log", "offset": 100_001}));
    assert!(e.content.contains("has 100000 lines"), "{}", e.content);
}

/// Files above the hash ceiling are streamed: the window at any offset and the whole file's line
/// count, without holding the file (driven directly; a 64 MB fixture is not worth the disk).
#[test]
fn a_huge_file_is_streamed_by_window() {
    let s = Session::new("huge");
    let long = "y".repeat(super::MAX_LINE_CHARS * 5);
    s.put("h.txt", format!("\u{feff}a\nb\n{long}\nd\ne"));
    let (raw, total) = super::stream_lines(&s.p("h.txt"), 3, 2).unwrap();
    assert_eq!(total, 5, "an unterminated last line counts");
    assert!(raw.ends_with(b"\nd\n") && raw.len() < long.len(), "{}", raw.len());
    let o = super::read_huge(&s.tools, &json!({"offset": 3, "limit": 2}), &s.p("h.txt"), "h.txt", 1 << 27);
    assert!(o.content.starts_with("     3\tyyy") && o.content.contains("     4\td"), "{}", &o.content[..60]);
    assert!(o.content.contains("line(s) longer than"), "a long line is still marked as cut");
    let o = super::read_huge(&s.tools, &json!({}), &s.p("h.txt"), "h.txt", 1 << 27);
    assert!(o.content.starts_with("     1\ta\n"), "the BOM is not shown: {:?}", &o.content[..12]);
    let e = super::read_huge(&s.tools, &json!({"offset": 9}), &s.p("h.txt"), "h.txt", 1 << 27);
    assert!(e.is_error && e.content.contains("has 5 lines"), "{}", e.content);
}

/// T1, FlashAgent `line_endings_and_a_last_line_without_one_read_the_same`: CRLF shows as LF,
/// and a last line without an ending reads like the others.
#[test]
fn read_shows_crlf_as_lf_and_an_unterminated_last_line() {
    let mut s = Session::new("crlf-read");
    s.put("crlf.txt", "one\r\ntwo\r\nthree");
    assert_eq!(s.read("crlf.txt").content, "     1\tone\n     2\ttwo\n     3\tthree");
    // An LF snippet copied from that read lands on the CRLF file, which stays CRLF.
    let o = s.ok("edit", json!({"path": "crlf.txt", "old_string": "one\ntwo", "new_string": "ONE\ntwo"}));
    assert!(o.content.contains("normalising line endings"), "{}", o.content);
    assert_eq!(s.text("crlf.txt"), "ONE\r\ntwo\r\nthree");
}

/// T2: a PNG comes back as an image part, stored by blake3 in the blob store, and serialised in
/// the request as `{"type":"image",..}` inside the tool result.
#[test]
fn parity_t2_png_read_is_an_image_part_in_the_request_json() {
    let mut s = Session::new("t2");
    s.put("pixel.png", PNG_1X1);
    let o = s.read("pixel.png");
    assert!(o.content.contains("image/png, 1x1"), "{}", o.content);
    let [ResultPart::Image(img)] = o.parts.as_slice() else { panic!("{:?}", o.parts) };
    assert_eq!((img.mime.as_str(), img.w, img.h), ("image/png", 1, 1));
    assert_eq!(img.blob, marlowe_code::filestate::hex(&marlowe_code::filestate::digest(PNG_1X1)));
    assert_eq!(std::fs::read(s.p(".blobs").join(&img.blob)).unwrap(), PNG_1X1);
    // As the loop records it (run_tools.rs) and a provider receives it.
    let result = ToolResult { call_id: "c1".into(), content: o.content.clone(), is_error: o.is_error, parts: o.parts.clone() };
    let req = ChatRequest::new("m", vec![Message::user("look"), Message::tool_results([result])]);
    let v = serde_json::to_value(&req).unwrap();
    let parts = &v["messages"][1]["content"][0]["parts"];
    assert_eq!(parts[0], json!({"type": "image", "mime": "image/png", "blob": img.blob, "w": 1, "h": 1}), "{v}");
    // A non-image binary says what it is instead.
    s.put("blob.bin", [0u8, 159, 146, 150, 0xff]);
    assert!(s.read("blob.bin").content.contains("not UTF-8 text"));
}

/// T3: `pages: "2-3"` returns those pages' text only; above 10 pages `pages` is required.
#[test]
fn parity_t3_pdf_pages_2_3_text_only() {
    let mut s = Session::new("t3");
    s.put("doc.pdf", pdf(&["AlphaPageOne", "BravoPageTwo", "CharliePageThree", "DeltaPageFour"]));
    let o = s.ok("read", json!({"path": "doc.pdf", "pages": "2-3"}));
    assert!(o.content.contains("--- page 2 of 4 ---") && o.content.contains("BravoPageTwo"), "{}", o.content);
    assert!(o.content.contains("--- page 3 of 4 ---") && o.content.contains("CharliePageThree"), "{}", o.content);
    assert!(!o.content.contains("AlphaPageOne") && !o.content.contains("DeltaPageFour"), "{}", o.content);
    assert!(o.parts.is_empty(), "text only");
    let all = s.read("doc.pdf");
    assert!(all.content.contains("AlphaPageOne") && all.content.contains("DeltaPageFour"));
    let e = s.err("read", json!({"path": "doc.pdf", "pages": "7"}));
    assert!(e.content.contains("page 7 does not exist"), "{}", e.content);
    let many: Vec<String> = (1..=12).map(|i| format!("Page{i}")).collect();
    s.put("long.pdf", pdf(&many.iter().map(String::as_str).collect::<Vec<_>>()));
    let e = s.err("read", json!({"path": "long.pdf"}));
    assert!(e.content.contains("12 pages") && e.content.contains("pages"), "{}", e.content);
    assert!(s.ok("read", json!({"path": "long.pdf", "pages": "11-12"})).content.contains("Page12"));
    s.put("t.txt", "x\n");
    let e = s.err("read", json!({"path": "t.txt", "pages": "1"}));
    assert!(e.content.contains("`pages` is for PDFs"), "{}", e.content);
}

/// T4: the fixture notebook through `read`: cells, outputs, and its image output attached.
#[test]
fn parity_t4_notebook_read_through_the_tool() {
    let mut s = Session::new("t4");
    s.put("demo.ipynb", include_str!("../../../../marlowe-code/src/notebook/fixtures/demo.ipynb"));
    let o = s.read("demo.ipynb");
    assert!(o.content.starts_with("demo.ipynb: Jupyter notebook, 4 cells.\n<cell index=\"0\" id=\"intro\""), "{}", o.content);
    assert!(o.content.contains("<outputs cell=\"1\">\nloaded 3 rows"), "{}", o.content);
    assert!(matches!(o.parts.as_slice(), [ResultPart::Image(i)] if i.mime == "image/png"), "{:?}", o.parts);
    // `offset`/`limit` pick cells (0-based, as `<cell index>` and `notebook_edit` count).
    let o = s.ok("read", json!({"path": "demo.ipynb", "offset": 2, "limit": 1}));
    assert!(o.content.contains("Cells 2-2:\n<cell index=\"2\"") && !o.content.contains("index=\"3\""), "{}", o.content);
    assert_eq!(o.parts.len(), 1, "cell 2's image comes with it");
    let e = s.err("read", json!({"path": "demo.ipynb", "offset": 4}));
    assert!(e.content.contains("has 4 cells, numbered 0-3"), "{}", e.content);
}

/// T4 at research size: a 150-cell notebook larger than one read comes back in whole cells, says
/// exactly where to continue, and the rest is reachable by that offset.
#[test]
fn parity_t4_a_large_notebook_reads_in_cell_windows() {
    let mut s = Session::with("t4big", |t| t.with_context_tokens(8_000));
    let cells: Vec<Value> = (0..150)
        .map(|i| {
            json!({"cell_type": "code", "id": format!("c{i}"), "execution_count": i, "metadata": {},
                   "source": [format!("step_{i} = train({i})\n"), "x".repeat(900)],
                   "outputs": [{"output_type": "stream", "name": "stdout", "text": [format!("done {i}\n")]}]})
        })
        .collect();
    let nb = json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
    s.put("big.ipynb", serde_json::to_string(&nb).unwrap());
    let o = s.read("big.ipynb");
    assert!(o.content.len() <= 32 * 1024 + 300, "{}", o.content.len());
    let tail = &o.content[o.content.len() - 120..];
    let next: usize = tail.split("Continue with offset ").nth(1).and_then(|t| t.split(' ').next()).unwrap().parse().unwrap();
    assert!(tail.contains(&format!("[showing cells 0-{} of 150", next - 1)), "{tail}");
    assert!(o.content.contains(&format!("<outputs cell=\"{}\">\ndone {}", next - 1, next - 1)), "whole cells only");
    assert!(!o.content.contains("read fewer pages or cells"));
    let o = s.ok("read", json!({"path": "big.ipynb", "offset": next}));
    assert!(o.content.contains(&format!("Cells {next}-")) && o.content.contains(&format!("step_{next} = train")), "{}", &o.content[..200]);
    let o = s.ok("read", json!({"path": "big.ipynb", "offset": 149}));
    assert!(o.content.contains("done 149") && !o.content.contains("Continue with"), "{}", o.content);
}

// ---------------------------------------------------------------- parity: write (T5)

/// T5: a write that dies between the temp file and the rename leaves the original intact (the
/// tools write only through the port's temp+rename); BOM and CRLF are kept; an unread file is
/// not overwritten.
#[test]
fn parity_t5_write_crash_sim_leaves_original() {
    let mut s = Session::with("t5", |t| t.with_workspace_port(Arc::new(FlakyPort { fail_suffix: "keep.txt", crash: true })));
    s.put("keep.txt", "original\n");
    s.read("keep.txt");
    let e = s.err("write", json!({"path": "keep.txt", "content": "replacement that never lands\n"}));
    assert!(e.content.contains("simulated crash"), "{}", e.content);
    assert_eq!(s.text("keep.txt"), "original\n");
    let e = s.err("edit", json!({"path": "keep.txt", "old_string": "original", "new_string": "edited"}));
    assert!(e.content.contains("cannot write"), "{}", e.content);
    assert_eq!(s.text("keep.txt"), "original\n");

    // On this machine: no temp file is left behind, and BOM + CRLF survive an LF write.
    let mut s = Session::new("t5b");
    s.put("w.cs", b"\xEF\xBB\xBFusing A;\r\nclass C {}\r\n");
    let e = s.err("write", json!({"path": "w.cs", "content": "x\n"}));
    assert!(e.content.contains("have not read it"), "{}", e.content);
    s.read("w.cs");
    let o = s.ok("write", json!({"path": "w.cs", "content": "using B;\nclass D {}\n"}));
    assert!(o.content.contains("kept its byte-order mark and CRLF line endings"), "{}", o.content);
    assert_eq!(s.get("w.cs"), b"\xEF\xBB\xBFusing B;\r\nclass D {}\r\n");
    let names: Vec<String> =
        std::fs::read_dir(&s.root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    // Writing a new file needs no read; writing over one just written needs no re-read.
    s.ok("write", json!({"path": "sub/new.txt", "content": "a\n"}));
    s.ok("write", json!({"path": "sub/new.txt", "content": "b\n"}));
    assert_eq!(s.text("sub/new.txt"), "b\n");
}

// ---------------------------------------------------------------- parity: edit (T6)

/// T6 through the tool: the tiers name themselves, ambiguity lists sites, `replace_all` takes
/// them, a miss shows the closest region, and a pasted `read` prefix is refused with its place.
#[test]
fn parity_t6_edit_tool_tiers_and_refusals() {
    let mut s = Session::new("t6");
    s.put("a.py", "def f():\n    if x:\n        go()\n    return 1\n");
    s.read("a.py");
    let o = s.ok("edit", json!({"path": "a.py", "old_string": "if x:\n    go()", "new_string": "if y:\n    stop()"}));
    assert!(o.content.contains("re-indented"), "{}", o.content);
    assert_eq!(s.text("a.py"), "def f():\n    if y:\n        stop()\n    return 1\n");
    s.put("b.txt", "a\nb\na\n");
    s.read("b.txt");
    let e = s.err("edit", json!({"path": "b.txt", "old_string": "a", "new_string": "c"}));
    assert!(e.content.contains("2 times") && e.content.contains("lines 1, 3"), "{}", e.content);
    assert_eq!(s.text("b.txt"), "a\nb\na\n");
    let o = s.ok("edit", json!({"path": "b.txt", "old_string": "a", "new_string": "c", "replace_all": true}));
    assert!(o.content.contains("replacing 2 occurrences at lines 1, 3"), "{}", o.content);
    let e = s.err("edit", json!({"path": "a.py", "old_string": "    return 2", "new_string": "x"}));
    assert!(e.content.contains("Did you mean lines 4-4"), "{}", e.content);
    let e = s.err("edit", json!({"path": "a.py", "old_string": "     4\t    return 1", "new_string": "    return 0"}));
    assert!(e.content.contains("line-number prefix") && e.content.contains("starting at line 4"), "{}", e.content);
    // Legacy argument names still work; a call with neither names the current ones.
    s.ok("edit", json!({"path": "a.py", "replacing": "return 1", "content": "return 0"}));
    let e = s.err("edit", json!({"path": "a.py", "frobnicate": "x"}));
    assert!(e.content.contains("`new_string` is required"), "{}", e.content);
}

// ---------------------------------------------------------------- parity: FileState (T7, B1)

/// T7: a file changed by something else after the agent read it produces a reminder with a
/// short diff at the step's end, which goes into the next request; once reported it counts as
/// seen.
#[test]
fn parity_t7_external_modify_reminder_in_next_request() {
    let mut s = Session::new("t7");
    s.put("cfg.toml", "a = 1\nb = 2\n");
    s.read("cfg.toml");
    assert!(s.step_end().is_empty(), "nothing changed yet");
    // A formatter, the user or a shell command changes it.
    s.put("cfg.toml", "a = 1\nb = 3\nc = 4\n");
    let msgs = s.step_end();
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    let text = msgs[0].joined_text();
    assert!(text.contains("<system-reminder>") && text.contains("cfg.toml: modified."), "{text}");
    assert!(text.contains("-b = 2\n+b = 3\n+c = 4"), "{text}");
    // It reaches the next request as a user message.
    let req = ChatRequest::new("m", vec![Message::user("go"), msgs[0].clone()]);
    assert!(serde_json::to_string(&req).unwrap().contains("cfg.toml: modified."));
    assert!(s.step_end().is_empty(), "reported once");
    // The reminder showed it, so an edit now proceeds without another read.
    s.ok("edit", json!({"path": "cfg.toml", "old_string": "c = 4", "new_string": "c = 5"}));
    assert!(s.step_end().is_empty(), "the agent's own edit is not an external change");
    std::fs::remove_file(s.p("cfg.toml")).unwrap();
    assert!(s.step_end()[0].joined_text().contains("cfg.toml: deleted."));
}

/// T7 between turns: Matthew edits a file the agent read while no turn runs. The next turn
/// starts with the reminder as its own message ahead of his, and his words stay untouched (they
/// are what memory ingests at user trust).
#[test]
fn parity_t7_a_change_between_turns_is_reported_when_the_next_turn_starts() {
    let mut s = Session::new("t7turn");
    s.put("train.py", "lr = 1e-3\n");
    s.read("train.py");
    s.step_end(); // the turn ends
    s.put("train.py", "lr = 0.0003\n");
    let mut convo = Conversation::new(SessionId::from("s"), Some(s.root.clone()));
    convo.ext = std::mem::take(&mut *s.scope.ext.lock().unwrap());
    let mut text = "why is the loss flat?".to_string();
    for m in &s.mw {
        assert!(matches!(m.before_turn(&mut convo, &mut text), TurnFlow::Continue));
    }
    assert_eq!(text, "why is the loss flat?");
    let [reminder] = convo.messages.as_slice() else { panic!("{:?}", convo.messages) };
    let r = reminder.joined_text();
    assert!(r.contains("train.py: modified.") && r.contains("-lr = 1e-3\n+lr = 0.0003"), "{r}");
    // Seen now: the next turn starts clean.
    let mut text = "go on".to_string();
    for m in &s.mw {
        m.before_turn(&mut convo, &mut text);
    }
    assert_eq!(convo.messages.len(), 1);
}

/// T7 with a change the reminder cannot show whole (a data file over the 256 KB snapshot, a diff
/// over 40 lines): the reminder says so, and a `write` from the agent's old view is refused until
/// it reads the file again, so changes it never saw are not overwritten.
#[test]
fn parity_t7_a_change_not_shown_whole_needs_a_reread_before_a_write() {
    let mut s = Session::new("t7big");
    let rows = |tag: &str, n: usize| -> String { (0..n).map(|i| format!("{{\"{tag}\": {i}}}\n")).collect() };
    s.put("data.jsonl", rows("a", 30_000));
    s.put("cfg.yaml", rows("k", 100));
    s.ok("read", json!({"path": "data.jsonl", "limit": 10}));
    s.read("cfg.yaml");
    s.put("data.jsonl", rows("b", 30_000)); // Matthew regenerates it
    s.put("cfg.yaml", rows("v", 100));
    let text = s.step_end()[0].joined_text();
    assert!(text.contains("data.jsonl: modified (") && text.contains("too large to diff), read it again before editing it."), "{text}");
    assert!(text.contains("cfg.yaml: modified; only part of the change is shown"), "{text}");
    assert!(s.step_end().is_empty(), "reported once");
    for rel in ["data.jsonl", "cfg.yaml"] {
        let e = s.err("write", json!({"path": rel, "content": "{}\n"}));
        assert!(e.content.contains("changed since you last read it"), "{rel}: {}", e.content);
    }
    assert_eq!(s.text("cfg.yaml"), rows("v", 100), "Matthew's change survives");
    s.read("cfg.yaml");
    s.ok("write", json!({"path": "cfg.yaml", "content": "k: 1\n"}));
}

/// T7 for a log something keeps appending to (`runs/x/train.log`): one line per turn saying it
/// grew, never its new lines every step.
#[test]
fn parity_t7_an_appended_log_is_noted_once_per_turn() {
    let mut s = Session::new("t7log");
    s.put("runs/x/train.log", "step 0 loss 2.3\n");
    s.read("runs/x/train.log");
    let append = |s: &Session, n: usize| {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(s.p("runs/x/train.log")).unwrap();
        for i in 0..n {
            writeln!(f, "step {i} loss 1.{i}").unwrap();
        }
    };
    append(&s, 3);
    let msgs = s.step_end();
    let text = msgs[0].joined_text();
    assert!(text.contains("train.log: 3 lines (") && text.contains("appended, earlier content unchanged"), "{text}");
    assert!(!text.contains("loss 1."), "the new lines are not pasted in: {text}");
    for _ in 0..10 {
        append(&s, 50);
        assert!(s.step_end().is_empty(), "at most once per turn");
    }
    // The next turn hears about it once more.
    let mut convo = Conversation::new(SessionId::from("s"), Some(s.root.clone()));
    convo.ext = std::mem::take(&mut *s.scope.ext.lock().unwrap());
    let mut text = "how is it going?".to_string();
    for m in &s.mw {
        m.before_turn(&mut convo, &mut text);
    }
    let [note] = convo.messages.as_slice() else { panic!("{:?}", convo.messages) };
    assert!(note.joined_text().contains("train.log: 503 lines ("), "{}", note.joined_text());
}

/// B1: an existing file must be read before it is edited or overwritten, and an edit of a file
/// that changed since the read is refused with what changed (before the approval gate asks).
#[test]
fn parity_b1_read_before_edit_and_stale_refusal() {
    let mut s = Session::new("b1");
    s.put("m.rs", "fn main() {}\n");
    for (tool, args) in [
        ("edit", json!({"path": "m.rs", "old_string": "main", "new_string": "start"})),
        ("write", json!({"path": "m.rs", "content": "x"})),
        ("multi_edit", json!({"edits": [{"path": "m.rs", "old_string": "main", "new_string": "x"}]})),
    ] {
        let e = s.err(tool, args);
        assert!(e.content.contains("you have not read it"), "{tool}: {}", e.content);
    }
    assert_eq!(s.text("m.rs"), "fn main() {}\n");
    s.read("m.rs");
    s.put("m.rs", "fn main() { run(); }\n");
    let e = s.err("edit", json!({"path": "m.rs", "old_string": "main", "new_string": "start"}));
    assert!(e.content.contains("changed since you last read it") && e.content.contains("+fn main() { run(); }"), "{}", e.content);
    assert_eq!(s.text("m.rs"), "fn main() { run(); }\n");
    s.read("m.rs");
    s.ok("edit", json!({"path": "m.rs", "old_string": "main", "new_string": "start"}));
    // Calls outside a session (no middleware, empty extensions) keep the free-form behaviour.
    let tools = BuiltinTools::new(Some(s.root.clone()));
    let call = ToolCall { id: "x".into(), name: "edit".into(), args: json!({"path": "m.rs", "old_string": "start", "new_string": "go"}) };
    let ctx = ToolCtx::new(SessionId::from("d"), 0, Some(s.root.clone()), CancelToken::new(), false);
    assert!(!tools.call(&call, &ctx).is_error);
}

// ---------------------------------------------------------------- parity: multi_edit (T8)

/// T8: the third edit fails, so nothing is written: not the first two edits' files, and not the
/// file the first edit would have created. A failing write midway puts back what was written
/// (FlashAgent `batch_edit_rolls_back_files_written_before_a_later_write_fails`).
#[test]
fn parity_t8_multi_edit_rollback_on_third_edit_failure() {
    let mut s = Session::new("t8");
    s.put("a.rs", "fn a() {}\n");
    s.put("b.rs", "fn b() {}\n");
    s.read("a.rs");
    s.read("b.rs");
    let e = s.err(
        "multi_edit",
        json!({"edits": [
            {"path": "new.rs", "old_string": "", "new_string": "fn n() {}\n"},
            {"path": "a.rs", "old_string": "fn a()", "new_string": "fn a2()"},
            {"path": "b.rs", "old_string": "fn missing()", "new_string": "fn b2()"},
        ]}),
    );
    assert!(e.content.starts_with("edit 3 of 3 (b.rs) failed:"), "{}", e.content);
    assert!(e.content.contains("Nothing was written"), "{}", e.content);
    assert_eq!((s.text("a.rs"), s.text("b.rs")), ("fn a() {}\n".into(), "fn b() {}\n".into()));
    assert!(!s.p("new.rs").exists());
    // All three land together, each seeing the ones before it in the same file.
    let o = s.ok(
        "multi_edit",
        json!({"path": "a.rs", "edits": [
            {"old_string": "fn a()", "new_string": "fn a2()"},
            {"old_string": "fn a2() {}", "new_string": "fn a2() { b2(); }"},
            {"path": "b.rs", "old_string": "fn b()", "new_string": "fn b2()"},
        ]}),
    );
    assert!(o.content.starts_with("applied 3 edits to 2 file(s), all or nothing"), "{}", o.content);
    assert_eq!((s.text("a.rs"), s.text("b.rs")), ("fn a2() { b2(); }\n".into(), "fn b2() {}\n".into()));

    // A write failing on the second file restores the first.
    let mut s = Session::with("t8b", |t| t.with_workspace_port(Arc::new(FlakyPort { fail_suffix: "blocked.txt", crash: false })));
    s.put("first.txt", "old first\n");
    s.put("blocked.txt", "old blocked\n");
    s.read("first.txt");
    s.read("blocked.txt");
    let e = s.err(
        "multi_edit",
        json!({"edits": [
            {"path": "first.txt", "old_string": "old first", "new_string": "new first"},
            {"path": "blocked.txt", "old_string": "old blocked", "new_string": "new blocked"},
        ]}),
    );
    assert!(e.content.contains("blocked.txt") && e.content.contains("were put back: first.txt"), "{}", e.content);
    assert_eq!(s.text("first.txt"), "old first\n");
}

// ---------------------------------------------------------------- parity: edit_lines (T9)

/// T9 through the tool: `read` shows anchors when the arm is on; a stale hash is refused with
/// fresh anchors and the file unchanged; a fresh one applies.
#[test]
fn parity_t9_edit_lines_tool_rejects_a_stale_hash() {
    let mut off = Session::new("t9off");
    assert!(!off.tools.specs().iter().any(|t| t.name == "edit_lines"), "a bench arm, off by default");
    off.put("x.txt", "x\n");
    assert!(off.read("x.txt").content.starts_with("     1\tx"));

    let mut s = Session::with("t9", with_edit_lines);
    assert!(s.tools.specs().iter().any(|t| t.name == "edit_lines"));
    s.put("f.txt", "one\ntwo\nthree\n");
    let shown = s.read("f.txt").content;
    let h2 = marlowe_code::editlines::line_hash("two");
    assert!(shown.contains(&format!("2:{h2}|two")), "{shown}");
    let wrong = if h2 == "000" { "fff" } else { "000" };
    let e = s.err("edit_lines", json!({"path": "f.txt", "anchor": format!("2:{wrong}"), "text": "TWO"}));
    assert!(e.content.contains("has changed since you read it") && e.content.contains(&format!("2:{h2}|two")), "{}", e.content);
    assert_eq!(s.text("f.txt"), "one\ntwo\nthree\n");
    let o = s.ok("edit_lines", json!({"path": "f.txt", "anchor": format!("2:{h2}"), "text": "TWO"}));
    assert!(o.content.contains("Fresh anchors"), "{}", o.content);
    assert_eq!(s.text("f.txt"), "one\nTWO\nthree\n");
}

// ---------------------------------------------------------------- parity: notebook_edit (T19)

/// T19 through the tool: insert, replace, delete; the notebook stays valid JSON in Jupyter's
/// layout, and needs a read first like any file.
#[test]
fn parity_t19_notebook_edit_tool() {
    let mut s = Session::new("t19");
    let demo = include_str!("../../../../marlowe-code/src/notebook/fixtures/demo.ipynb");
    s.put("n.ipynb", demo);
    let e = s.err("notebook_edit", json!({"path": "n.ipynb", "cell": "load", "new_source": "x"}));
    assert!(e.content.contains("have not read it"), "{}", e.content);
    s.read("n.ipynb");
    s.ok("notebook_edit", json!({"path": "n.ipynb", "cell": "load", "new_source": "losses = []"}));
    let o = s.ok("notebook_edit", json!({"path": "n.ipynb", "cell": 0, "new_source": "## Setup", "cell_type": "markdown", "mode": "insert"}));
    assert!(o.content.contains("inserted cell 1"), "{}", o.content);
    s.ok("notebook_edit", json!({"path": "n.ipynb", "cell": "boom", "mode": "delete"}));
    let nb: Value = serde_json::from_str(&s.text("n.ipynb")).unwrap();
    let cells = nb["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 4);
    assert_eq!(cells[1]["cell_type"], "markdown");
    assert_eq!(cells[2]["source"], json!(["losses = []"]));
    assert!(cells.iter().all(|c| c["id"] != "boom"));
    assert!(s.text("n.ipynb").starts_with("{\n \""), "Jupyter's one-space layout kept");
    let e = s.err("notebook_edit", json!({"path": "n.ipynb", "cell": "nope", "mode": "delete"}));
    assert!(e.content.contains("no cell nope") && e.content.contains("unchanged"), "{}", e.content);
}

// ---------------------------------------------------------------- parity: secrets (B6 writes)

/// A crew sandbox at a directory: writes inside it (relative paths resolve there) are allowed.
struct Sandbox(PathBuf);

impl SandboxView for Sandbox {
    fn root(&self) -> &Path {
        &self.0
    }
    fn allows_write(&self, p: &Path) -> bool {
        self.0.join(p).starts_with(&self.0)
    }
    fn classify_shell(&self, _command: &str, _tainted: bool) -> Consequence {
        Consequence::Ask { summary: "shell".into() }
    }
    fn classify_code(&self, _code: &str) -> Consequence {
        Consequence::Ask { summary: "code".into() }
    }
}

/// B6: writing a secret-shaped file or credential-shaped content asks inside the workspace,
/// however the name is spelled (Win32 drops trailing dots and spaces and reads `:stream` as the
/// file); ordinary writes inside stay free; templates do not count. The crew half below checks
/// this group's sandbox hook only: `CrewTools` does not call it yet (a seam-crew need), so B6 is
/// not live in crew runs until it does.
#[test]
fn parity_b6_secret_writes_need_approval() {
    let root = temp("b6");
    let tools = BuiltinTools::new(Some(root.clone()));
    let call = |name: &str, args: Value| ToolCall { id: "c".into(), name: name.into(), args };
    let asks = |c: &ToolCall| matches!(tools.consequence(c), Consequence::Ask { .. });
    let crew_asks = |c: &ToolCall| {
        matches!(tools.consequence_in_sandbox(c, &Exposure::default(), &Sandbox(root.clone())), Some(Consequence::Ask { .. }))
    };
    let key = format!("ghp_{}", "a".repeat(36));
    let cases = [
        (call("write", json!({"path": ".env", "content": "A=1"})), true),
        (call("write", json!({"path": "deploy/server.pem", "content": "x"})), true),
        (call("edit", json!({"path": "src/cfg.rs", "old_string": "x", "new_string": format!("let t = \"{key}\";")})), true),
        (call("multi_edit", json!({"edits": [{"path": "a.txt", "old_string": "", "new_string": "ok"}, {"path": ".npmrc", "old_string": "", "new_string": "x"}]})), true),
        (call("write", json!({"path": ".env ", "content": "A=1"})), true),
        (call("write", json!({"path": "cfg/.env/.", "content": "A=1"})), true),
        (call("write", json!({"path": "id_rsa.", "content": "x"})), true),
        (call("write", json!({"path": "deploy/server.pem.", "content": "x"})), true),
        (call("write", json!({"path": ".env::$DATA", "content": "A=1"})), true),
        (call("edit", json!({"path": "cfg/../.ENV. ", "old_string": "a", "new_string": "b"})), true),
        (call("write", json!({"path": ".env.example", "content": "A="})), false),
        (call("write", json!({"path": ".env.example.", "content": "A="})), false),
        (call("write", json!({"path": "src/main.rs", "content": "fn main() {}"})), false),
    ];
    for (c, ask) in &cases {
        assert_eq!(asks(c), *ask, "{}", c.args);
        assert_eq!(crew_asks(c), *ask, "crew: {}", c.args);
    }
    match tools.consequence(&cases[0].0) {
        Consequence::Ask { summary } => assert!(summary.contains("secrets need approval"), "{summary}"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A [`LocalFs`] that answers for any path naming `host` without touching anything, and counts
/// each such call: the proof that nothing reached the share.
struct CountingPort {
    host: &'static str,
    hits: std::sync::atomic::AtomicUsize,
}

impl CountingPort {
    fn hit(&self, p: &Path) -> Option<std::io::Error> {
        p.to_string_lossy().to_ascii_lowercase().contains(self.host).then(|| {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the fake share")
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Workspace for CountingPort {
    fn kind(&self) -> String {
        "local".into()
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.hit(path).map_or_else(|| LocalFs.read(path), Err)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.hit(path).map_or_else(|| LocalFs.write(path, bytes), Err)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.hit(from).or_else(|| self.hit(to)).map_or_else(|| LocalFs.rename(from, to), Err)
    }
    fn walk(&self, dir: &Path, opts: &WalkOpts) -> std::io::Result<Vec<WalkEntry>> {
        self.hit(dir).map_or_else(|| LocalFs.walk(dir, opts), Err)
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Meta> {
        self.hit(path).map_or_else(|| LocalFs.metadata(path), Err)
    }
    fn exec(&self, req: &ExecRequest, cancelled: &dyn Fn() -> bool) -> std::io::Result<ExecOutput> {
        LocalFs.exec(req, cancelled)
    }
}

/// Non-negotiable 5 for file paths: a UNC write (or read) is never stat'ed, read or written before
/// approval, in any spelling. The gate asks naming the host as egress; the middleware does not
/// look; the tool refuses unless `before_call` (which runs only after the gate) cleared it; and
/// after approval the call goes through, once. A device path asks too, with no host.
#[test]
fn parity_t5_a_unc_write_is_never_touched_before_approval() {
    const HOST: &str = "marlowe-unc-test.invalid";
    let port = Arc::new(CountingPort { host: HOST, hits: Default::default() });
    let s = Session::with("unc", |t| t.with_workspace_port(port.clone()));
    let cx = s.cx();
    for spelling in [
        format!(r"\\{HOST}\share\notes.txt"),
        format!("//{HOST}/share/notes.txt"),
        format!(r"\\?\UNC\{HOST}\share\notes.txt"),
        format!(r"\\.\UNC\{}\share\notes.txt", HOST.to_uppercase()),
    ] {
        for (name, args) in [
            ("write", json!({"path": spelling, "content": "x"})),
            ("edit", json!({"path": spelling, "old_string": "a", "new_string": "b"})),
            ("multi_edit", json!({"edits": [{"path": "local.txt", "old_string": "", "new_string": "a"}, {"path": spelling, "old_string": "a", "new_string": "b"}]})),
            ("read", json!({"path": spelling})),
        ] {
            let mut call = ToolCall { id: "c".into(), name: name.into(), args };
            for m in &s.mw {
                assert!(matches!(m.before_approval(&mut call, &cx), CallFlow::Continue), "{name} {spelling}");
            }
            match s.tools.consequence_exposed(&call, &Exposure::default()) {
                Consequence::Ask { summary } => assert!(summary.contains(&format!("reaches network host {HOST}")), "{summary}"),
                other => panic!("{name} {spelling}: {other:?}"),
            }
            assert_eq!(s.tools.egress_host(&call).as_deref(), Some(HOST), "{name} {spelling}");
            if name == "read" {
                assert!(s.tools.taints(&call));
                let approved = Exposure { approved_hosts: [HOST.to_string()].into(), ..Exposure::default() };
                assert_eq!(s.tools.consequence_exposed(&call, &approved), Consequence::Free, "an allowed host");
            }
            // Called without the gate's clearance (a host without the middleware, a crew run).
            let o = s.tools.call_scoped(&call, &cx);
            assert!(o.is_error && o.content.contains("was not opened"), "{name} {spelling}: {}", o.content);
            assert_eq!(port.hits(), 0, "{name} {spelling} touched the share before approval");
        }
    }
    let crew = s.tools.consequence_in_sandbox(
        &ToolCall { id: "c".into(), name: "write".into(), args: json!({"path": format!(r"\\{HOST}\s\x"), "content": "x"}) },
        &Exposure::default(),
        &Sandbox(s.root.clone()),
    );
    assert!(matches!(crew, Some(Consequence::Ask { summary }) if summary.contains(HOST)));
    assert_eq!(port.hits(), 0);

    // Approved: before_call clears it, the tool goes to the share (the fake refuses), once.
    let call = ToolCall { id: "c".into(), name: "write".into(), args: json!({"path": format!(r"\\{HOST}\share\notes.txt"), "content": "x"}) };
    for m in &s.mw {
        assert!(matches!(m.before_call(&call, &cx), CallFlow::Continue));
    }
    let o = s.tools.call_scoped(&call, &cx);
    assert!(o.is_error && o.content.contains("the fake share"), "{}", o.content);
    assert!(port.hits() > 0);
    let o = s.tools.call_scoped(&call, &cx);
    assert!(o.content.contains("was not opened"), "a clearance is spent once: {}", o.content);

    // A device path asks with no host to allow.
    let pipe = ToolCall { id: "c".into(), name: "read".into(), args: json!({"path": r"\\.\pipe\marlowe-test"}) };
    match s.tools.consequence(&pipe) {
        Consequence::Ask { summary } => assert!(summary.contains("device or namespace `pipe`"), "{summary}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.tools.egress_host(&pipe), None);
}

// ---------------------------------------------------------------- donor: codex scenarios

/// One Codex apply-patch scenario as our tool calls.
struct Scenario {
    id: &'static str,
    input: &'static [(&'static str, &'static str)],
    calls: &'static [(&'static str, &'static str)],
    /// The tree afterwards (every file under the root).
    expected: &'static [(&'static str, &'static str)],
    /// Whether the (last) call fails.
    fails: bool,
    /// Why our expected tree differs from Codex's, when it does.
    divergence: &'static str,
}

const SCENARIOS: &[Scenario] = &[
    Scenario { id: "001_add_file", input: &[], calls: &[("write", r#"{"path":"bar.md","content":"This is a new file\n"}"#)], expected: &[("bar.md", "This is a new file\n")], fails: false, divergence: "" },
    Scenario {
        id: "002_multiple_operations",
        input: &[("delete.txt", "obsolete\n"), ("modify.txt", "line1\nline2\n")],
        calls: &[("multi_edit", r#"{"edits":[{"path":"nested/new.txt","old_string":"","new_string":"created\n"},{"path":"modify.txt","old_string":"line2","new_string":"changed"}]}"#)],
        expected: &[("delete.txt", "obsolete\n"), ("modify.txt", "line1\nchanged\n"), ("nested/new.txt", "created\n")],
        fails: false,
        divergence: "no file-delete tool (Claude Code has none either): deletes go through the shell under the permission rules",
    },
    Scenario {
        id: "003_multiple_chunks",
        input: &[("multi.txt", "line1\nline2\nline3\nline4\n")],
        calls: &[("multi_edit", r#"{"path":"multi.txt","edits":[{"old_string":"line2","new_string":"changed2"},{"old_string":"line4","new_string":"changed4"}]}"#)],
        expected: &[("multi.txt", "line1\nchanged2\nline3\nchanged4\n")],
        fails: false,
        divergence: "",
    },
    Scenario { id: "004_move_to_new_directory", input: &[], calls: &[], expected: &[], fails: false, divergence: "not applicable: no move tool (as in Claude Code); moves go through the shell" },
    Scenario { id: "005_rejects_empty_patch", input: &[("foo.txt", "stable\n")], calls: &[("multi_edit", r#"{"edits":[]}"#)], expected: &[("foo.txt", "stable\n")], fails: true, divergence: "" },
    Scenario { id: "006_rejects_missing_context", input: &[("modify.txt", "line1\nline2\n")], calls: &[("edit", r#"{"path":"modify.txt","old_string":"missing","new_string":"changed"}"#)], expected: &[("modify.txt", "line1\nline2\n")], fails: true, divergence: "" },
    Scenario { id: "007_rejects_missing_file_delete", input: &[], calls: &[], expected: &[], fails: false, divergence: "not applicable: no file-delete tool" },
    Scenario { id: "008_rejects_empty_update_hunk", input: &[("foo.txt", "stable\n")], calls: &[("edit", r#"{"path":"foo.txt","old_string":"stable","new_string":"stable"}"#), ("edit", r#"{"path":"foo.txt","old_string":"","new_string":"x"}"#)], expected: &[("foo.txt", "stable\n")], fails: true, divergence: "" },
    Scenario { id: "009_requires_existing_file_for_update", input: &[("foo.txt", "stable\n")], calls: &[("edit", r#"{"path":"missing.txt","old_string":"old","new_string":"new"}"#)], expected: &[("foo.txt", "stable\n")], fails: true, divergence: "" },
    Scenario { id: "010_move_overwrites_existing_destination", input: &[], calls: &[], expected: &[], fails: false, divergence: "not applicable: no move tool" },
    Scenario { id: "011_add_overwrites_existing_file", input: &[("duplicate.txt", "old content\n")], calls: &[("write", r#"{"path":"duplicate.txt","content":"new content\n"}"#)], expected: &[("duplicate.txt", "new content\n")], fails: false, divergence: "" },
    Scenario { id: "012_delete_directory_fails", input: &[("dir/foo.txt", "stable\n")], calls: &[("write", r#"{"path":"dir","content":"x"}"#)], expected: &[("dir/foo.txt", "stable\n")], fails: true, divergence: "mapped: no delete tool, so the directory case is a write aimed at a directory" },
    Scenario { id: "013_rejects_invalid_hunk_header", input: &[("foo.txt", "stable\n")], calls: &[("edit", r#"{"path":"foo.txt","frobnicate":"foo"}"#)], expected: &[("foo.txt", "stable\n")], fails: true, divergence: "mapped: malformed arguments instead of a malformed patch header" },
    Scenario {
        id: "014_update_file_appends_trailing_newline",
        input: &[("no_newline.txt", "no newline at end")],
        calls: &[("edit", r#"{"path":"no_newline.txt","old_string":"no newline at end","new_string":"first line\nsecond line"}"#)],
        expected: &[("no_newline.txt", "first line\nsecond line")],
        fails: false,
        divergence: "Codex appends a final newline; we never add bytes the model did not write (Claude Code behaviour)",
    },
    Scenario {
        id: "015_failure_after_partial_success_leaves_changes",
        input: &[],
        calls: &[("multi_edit", r#"{"edits":[{"path":"created.txt","old_string":"","new_string":"hello\n"},{"path":"missing.txt","old_string":"old","new_string":"new"}]}"#)],
        expected: &[],
        fails: true,
        divergence: "Codex leaves the partial change; multi_edit is all or nothing (design T8), so created.txt is not created",
    },
    Scenario { id: "016_pure_addition_update_chunk", input: &[("input.txt", "line1\nline2\n")], calls: &[("edit", r#"{"path":"input.txt","old_string":"line2","new_string":"line2\nadded line 1\nadded line 2"}"#)], expected: &[("input.txt", "line1\nline2\nadded line 1\nadded line 2\n")], fails: false, divergence: "" },
    Scenario { id: "017_whitespace_padded_hunk_header", input: &[("foo.txt", "old\n")], calls: &[("edit", r#"{"path":"foo.txt","old_string":"old  ","new_string":"new"}"#)], expected: &[("foo.txt", "new\n")], fails: false, divergence: "mapped: padding on the snippet (whitespace tier) instead of on patch markers" },
    Scenario { id: "018_whitespace_padded_patch_markers", input: &[("file.txt", "one\n")], calls: &[("edit", r#"{"path":"file.txt","old_string":" one","new_string":" two"}"#)], expected: &[("file.txt", "two\n")], fails: false, divergence: "mapped: leading padding on the snippet, re-indented away" },
    Scenario { id: "019_unicode_simple", input: &[("foo.txt", "line1\nna\u{ef}ve caf\u{e9}\nline3\n")], calls: &[("edit", "{\"path\":\"foo.txt\",\"old_string\":\"na\u{ef}ve caf\u{e9}\",\"new_string\":\"na\u{ef}ve caf\u{e9} \u{2705}\"}")], expected: &[("foo.txt", "line1\nna\u{ef}ve caf\u{e9} \u{2705}\nline3\n")], fails: false, divergence: "" },
    Scenario { id: "020_delete_file_success", input: &[], calls: &[], expected: &[], fails: false, divergence: "not applicable: no file-delete tool" },
    Scenario { id: "020_whitespace_padded_patch_marker_lines", input: &[("file.txt", "one\n")], calls: &[("edit", r#"{"path":"file.txt","old_string":"one \n","new_string":"two\n"}"#)], expected: &[("file.txt", "two\n")], fails: false, divergence: "mapped: trailing padding on the snippet" },
    Scenario { id: "021_update_file_deletion_only", input: &[("lines.txt", "line1\nline2\nline3\n")], calls: &[("edit", r#"{"path":"lines.txt","old_string":"line1\nline2\nline3","new_string":"line1\nline3"}"#)], expected: &[("lines.txt", "line1\nline3\n")], fails: false, divergence: "" },
    Scenario { id: "022_update_file_end_of_file_marker", input: &[("tail.txt", "first\nsecond\n")], calls: &[("edit", r#"{"path":"tail.txt","old_string":"first\nsecond","new_string":"first\nsecond updated"}"#)], expected: &[("tail.txt", "first\nsecond updated\n")], fails: false, divergence: "" },
    Scenario { id: "023_preserves_crlf_line_endings", input: &[("lines.txt", "one\r\ntwo\r\nthree\r\n")], calls: &[("edit", r#"{"path":"lines.txt","old_string":"one\ntwo\nthree","new_string":"ONE\ntwo\nbetween\nthree"}"#)], expected: &[("lines.txt", "ONE\r\ntwo\r\nbetween\r\nthree\r\n")], fails: false, divergence: "" },
    Scenario {
        id: "024_preserves_mixed_line_endings",
        input: &[("lines.txt", "one\r\ntwo\rthree\nfour\r\n")],
        calls: &[("edit", r#"{"path":"lines.txt","old_string":"three","new_string":"THREE"}"#)],
        expected: &[("lines.txt", "one\r\ntwo\rTHREE\nfour\r\n")],
        fails: false,
        divergence: "Codex rewrites the edited line's own ending to the file's first (CRLF); we leave every byte outside the snippet as it was",
    },
];

fn tree(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if rel.starts_with(".blobs") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((rel, std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// All 25 Codex scenarios: applicable ones reach Codex's expected tree (or ours, where a
/// divergence is stated); refusals leave the input tree exactly as it was.
#[test]
fn codex_apply_patch_scenarios_as_tool_calls() {
    assert_eq!(SCENARIOS.len(), 25);
    let mut applicable = 0;
    for sc in SCENARIOS {
        if sc.calls.is_empty() {
            assert!(sc.divergence.starts_with("not applicable"), "{}", sc.id);
            continue;
        }
        applicable += 1;
        let mut s = Session::new(&format!("codex-{}", &sc.id[..3]));
        for (rel, body) in sc.input {
            s.put(rel, body);
        }
        for (rel, _) in sc.input {
            s.read(rel);
        }
        for (k, (tool, args)) in sc.calls.iter().enumerate() {
            let args: Value = serde_json::from_str(args).unwrap_or_else(|e| panic!("{}: {e}", sc.id));
            let o = s.call(tool, args);
            let last = k + 1 == sc.calls.len();
            let expect_fail = sc.fails && (last || sc.id.starts_with("008"));
            assert_eq!(o.is_error, expect_fail, "{} call {k}: {}", sc.id, o.content);
        }
        let want: Vec<(String, String)> = sc.expected.iter().map(|(p, b)| (p.to_string(), b.to_string())).collect();
        assert_eq!(tree(&s.root), want, "{} ({})", sc.id, sc.divergence);
    }
    assert_eq!(applicable, 21);
}

/// The files group's schemas, pinned. `specs_m46_files.json` is the reviewed M4.6 surface (the
/// M4.5 `read`/`write`/`edit` are frozen in `tests/fixtures/specs_m45.rs`); a schema or
/// description change must show up as a deliberate edit of it, never a regeneration.
#[test]
fn the_files_group_declares_the_reviewed_m46_schemas() {
    let specs: Vec<marlowe_model::ToolSpec> = super::tools().into_iter().map(|d| d.spec).collect();
    let pinned: Vec<marlowe_model::ToolSpec> = serde_json::from_str(include_str!("specs_m46_files.json")).unwrap();
    assert_eq!(specs, pinned);
}
