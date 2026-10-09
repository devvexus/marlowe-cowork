//! **`read` returns `cat -n`, and `edit` refuses the prefix it just printed.**
//!
//! Two halves of one change, and the second exists only because of the first. `read` numbering
//! every line is what lets a `grep` hit at `src/x.rs:512` become a window — and it is also the
//! harness manufacturing a new way for `edit` to miss, because `edit` matches the file byte for
//! byte and the numbers are not in the file. A feature that creates a failure mode owes that
//! failure mode a diagnosis.
//!
//! Every expectation here is derived from [`marlowe_exec::LINE_NUMBER_WIDTH`] or from
//! [`marlowe_exec::number_lines`] itself. Six tests in this repo once encoded `MAX_EXPOSED_TOOLS`
//! as a numeral and one encoded it in its own name; when the cap moved, the same bytes asserted a
//! different property.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::TrustClass;
use marlowe_exec::{
    number_lines, strip_line_numbers, FileSystemTools, LINE_NUMBER_WIDTH, MAX_EDIT_SITES_NAMED,
    NUMBERED_CONTENT_LINES, READ_WINDOW_BYTES, READ_WINDOW_LINES,
};
use marlowe_loop::{ToolBody, ToolHost, ToolOutcome};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{
    Adjudication, Adjudicator, Args, EgressPolicy, Request, TaintSet, Tier,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, ToolRegistry, BUILTIN_TOOLS};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    registry: ToolRegistry,
    store: marlowe_extract::store::DocumentStore,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-linenum-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self {
            root,
            registry: builtin_registry().unwrap(),
            store: marlowe_extract::store::DocumentStore::new(),
        }
    }

    fn seed(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn on_disk(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap()
    }

    /// Adjudicate for real, then hand the result to the executor — the handle comes from the
    /// permission layer, never from the test.
    fn call(&self, tool: &str, args: Args) -> ToolOutcome {
        let exposed =
            ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()).unwrap();
        let mut taint = TaintSet::new();
        for (name, _) in args.iter() {
            taint.insert(name.clone(), TrustClass::UserAsserted);
        }
        let egress = EgressPolicy::DenyAll;
        let mut adj = Adjudicator::new(WorkspaceScope::new().expect("verified platform"));
        let adjudication: Adjudication = adj.adjudicate(Request {
            manifest: self.registry.manifest(&ToolId::new(tool)).unwrap(),
            args: &args,
            taint: &taint,
            exposed: &exposed,
            egress: &egress,
            workspace: &self.root,
            tier: Tier::Silent,
            novelty: None,
        });
        let mut tools =
            FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), &self.root)
                .with_store(self.store.clone());
        tools.execute(&ToolId::new(tool), &args, &adjudication)
    }

    fn text(&self, r: &ToolOutcome) -> String {
        match &r.body {
            ToolBody::Inline(s) => s.clone(),
            ToolBody::Reference { hash, bytes } => format!("<ref {hash} {bytes}>"),
        }
    }

    fn why(&self, r: &ToolOutcome) -> String {
        format!("{} {}", r.summary.detail.clone().unwrap_or_default(), self.text(r))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The prefix `read` prints for `n`, built the way the producer builds it.
fn prefix(n: usize) -> String {
    format!("{n:>width$}\t", width = LINE_NUMBER_WIDTH)
}

// ============================================================================================
// T1 — the producer and the stripper are inverses, over the shapes that break naive versions
// ============================================================================================

/// **Round trip, including the cases a `lines()`-based implementation silently corrupts.**
///
/// `str::lines` strips a trailing `\r`, so a CRLF file routed through it comes back LF — and a
/// `replacing` copied out of that read can never match the file it came from. `read`'s window path
/// did exactly this before numbering existed; numbering routes EVERY read through a line
/// decomposition, so the rare bug would have become universal and been blamed on this change.
#[test]
fn numbering_and_stripping_are_inverses_including_crlf_and_self_similar_text() {
    let cases: Vec<(&str, &str, usize)> = vec![
        ("one line, no terminator", "solo", 1),
        ("one line, terminated", "solo\n", 1),
        ("several", "a\nb\nc\n", 1),
        ("no trailing newline", "a\nb\nc", 1),
        ("CRLF", "a\r\nb\r\nc\r\n", 1),
        ("mixed terminators", "a\r\nb\nc\r\n", 1),
        ("a line that begins with a tab", "\tindented\nplain\n", 1),
        ("blank lines", "a\n\n\nb\n", 1),
        ("non-ASCII", "café\nrésumé\n", 1),
        // The one that makes the grammar's false positives concrete: content that IS `cat -n`.
        ("already numbered content", "     7\tfoo\n     8\tbar\n", 1),
        ("high start", "x\ny\n", 999_998),
        ("past the column width", "x\ny\n", 1_000_000),
    ];
    for (name, text, first) in cases {
        let numbered = number_lines(text, first);
        assert_eq!(
            strip_line_numbers(&numbered).as_deref(),
            Some(text),
            "{name}: the round trip must be exact, terminators included"
        );
        assert_eq!(
            numbered.lines().count(),
            text.lines().count(),
            "{name}: numbering must not add or drop a line"
        );
    }
    // Empty is the one input with no round trip, and it is defined rather than accidental: there
    // is nothing to number and nothing to recognise.
    assert_eq!(number_lines("", 1), "");
    assert_eq!(strip_line_numbers(""), None);
}

/// The grammar rejects what it must, so the detector in `edit` is not a wildcard.
#[test]
fn the_stripper_refuses_anything_that_is_not_this_harnesss_prefix() {
    let w = LINE_NUMBER_WIDTH;
    let refused = [
        ("ordinary source", "fn main() {\n"),
        ("a number then a space", &format!("{:>w$} foo\n", 1)),
        ("a number then a colon", "42: foo\n"),
        ("a markdown table", "| 1 | a |\n"),
        ("left-aligned, unpadded", "1\tfoo\n2\tbar\n"),
        ("one numbered line among plain ones", &format!("{}foo\nbar\n", prefix(1))),
        ("numbered but NOT consecutive", &format!("{}a\n{}b\n", prefix(1), prefix(9))),
        ("numbered but descending", &format!("{}a\n{}b\n", prefix(2), prefix(1))),
        ("repeated number", &format!("{}a\n{}b\n", prefix(3), prefix(3))),
    ];
    for (name, text) in refused {
        assert_eq!(strip_line_numbers(text), None, "{name}: {text:?} must not parse as numbered");
    }
    // The control: make the last one consecutive and it DOES parse, so the refusals above are
    // about consecutiveness rather than about the grammar being unreachable.
    assert!(strip_line_numbers(&format!("{}a\n{}b\n", prefix(3), prefix(4))).is_some());
}

// ============================================================================================
// T2 — the body is numbered, at the enforcement site, and the numbers are the FILE's
// ============================================================================================

#[test]
fn a_read_returns_the_file_line_numbers_and_not_the_windows() {
    let fx = Fixture::new("absolute");
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    fx.seed("hundred.txt", &content);

    let whole = fx.call("read", Args::new().text("path", "hundred.txt"));
    assert_eq!(fx.text(&whole), number_lines(&content, 1));

    // **The negative that catches window-relative numbering.** A range starting at 40 must print
    // 40. Printing 1 would look authoritative and be wrong, which is worse than no numbering at
    // all: the whole point is that a number can be handed straight back as a range.
    let ranged = fx.call("read", Args::new().text("path", "hundred.txt").text("range", "40-45"));
    let body = fx.text(&ranged);
    assert!(body.starts_with(&format!("{}line 40\n", prefix(40))), "{body:?}");
    assert!(!body.starts_with(&prefix(1)), "window-relative numbering: {body:?}");

    // And the number is usable as a range without arithmetic: ask for exactly the line it names.
    let one = fx.call("read", Args::new().text("path", "hundred.txt").text("range", "45-45"));
    assert_eq!(fx.text(&one), format!("{}line 45\n", prefix(45)));
}

// ============================================================================================
// T3 — `grep` and `read` agree. This is the property the feature exists for.
// ============================================================================================

/// **Asserted across two tools, end to end.** `grep` reports `path:line:text`; `read` must return
/// that exact line under that exact number. An off-by-one in either tool turns this red and
/// nothing else in the suite would notice, because each tool is otherwise self-consistent.
#[test]
fn a_grep_hit_can_be_turned_into_a_read_without_counting() {
    let fx = Fixture::new("grep-agrees");
    let mut content = String::new();
    for i in 1..=200 {
        content.push_str(&format!("    filler {i}\n"));
    }
    content.insert_str(
        content.match_indices('\n').nth(136).unwrap().0 + 1,
        "        let needle = 1;\n",
    );
    fx.seed("src/a.rs", &content);

    let hit = fx.call("grep", Args::new().text("pattern", "let needle").text("path", "."));
    let line = fx.text(&hit);
    let line = line.lines().next().expect("one hit");
    let (head, matched) = {
        let rest = line.strip_prefix("src/a.rs:").expect("path:line:text");
        let (n, t) = rest.split_once(':').expect("path:line:text");
        (n.parse::<usize>().expect("a line number"), t.to_string())
    };

    let back = fx.call(
        "read",
        Args::new().text("path", "src/a.rs").text("range", &format!("{head}-{head}")),
    );
    assert_eq!(
        fx.text(&back),
        format!("{}{matched}\n", prefix(head)),
        "the line `grep` reported at {head} must be the line `read` prints at {head}"
    );
}

// ============================================================================================
// T4/T5 — the byte ceiling counts what is RETURNED, and the trailer is not a line of the file
// ============================================================================================

/// **`READ_WINDOW_BYTES` describes what reaches the model.** Capping the raw text and then adding
/// a prefix per line would leave the constant naming a quantity that never arrives — the proxy
/// failure this repo has logged seventeen times.
#[test]
fn the_byte_ceiling_is_measured_on_the_numbered_body() {
    let fx = Fixture::new("bytes");
    // One line under the LINE window, so the line bound cannot be what bites; sized so the raw
    // text fits under the byte ceiling and the same text with prefixes does not.
    let lines = READ_WINDOW_LINES - 1;
    let raw: String = (1..=lines).map(|i| format!("line {i:04} pad\n")).collect();
    assert!(raw.len() < READ_WINDOW_BYTES, "premise: the RAW file fits");
    assert!(
        number_lines(&raw, 1).len() > READ_WINDOW_BYTES,
        "premise: the NUMBERED file does not"
    );
    fx.seed("short-lines.txt", &raw);

    let r = fx.call("read", Args::new().text("path", "short-lines.txt"));
    let body = fx.text(&r);
    assert!(
        body.len() <= READ_WINDOW_BYTES + 400,
        "the body must be bounded by the ceiling (plus the trailer): {} bytes",
        body.len()
    );
    assert!(body.contains("Continue with range"), "and it must say how to get the rest: {body:.200}");
    assert!(
        body.contains(&format!("of {lines}")),
        "and how long the file really is: {}",
        &body[body.len().saturating_sub(200)..]
    );
}

/// The negative control for that. A file whose NUMBERED size still fits is returned whole with no
/// notice — without this, a `read` that always truncated would pass the test above.
#[test]
fn a_file_that_fits_once_numbered_is_not_truncated() {
    let fx = Fixture::new("bytes-fit");
    let raw: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    assert!(number_lines(&raw, 1).len() < READ_WINDOW_BYTES, "premise");
    fx.seed("fits.txt", &raw);
    let r = fx.call("read", Args::new().text("path", "fits.txt"));
    assert_eq!(fx.text(&r), number_lines(&raw, 1), "no notice, no truncation");
}

/// **The trailer is harness speech and carries no number.** A model already reads a `[` at column
/// 0 as the harness; a numbered `[` reads as line 1,993 of the file.
#[test]
fn the_notice_and_the_cap_note_are_never_numbered() {
    let fx = Fixture::new("trailer");
    let raw: String = (1..=READ_WINDOW_LINES + 500).map(|i| format!("line {i}\n")).collect();
    fx.seed("long.txt", &raw);
    let body = fx.text(&fx.call("read", Args::new().text("path", "long.txt")));

    let notice = body.rsplit("\n\n").next().expect("a trailer");
    assert!(notice.starts_with("[showing lines "), "the trailer is at column 0: {notice:?}");
    assert!(
        strip_line_numbers(notice).is_none(),
        "the trailer must not parse as a numbered line: {notice:?}"
    );
    // And the numbered part above it must still be exactly the file's own lines.
    let content = &body[..body.len() - notice.len() - 2];
    let stripped = strip_line_numbers(content).expect("every content line is numbered");
    assert!(raw.starts_with(&stripped), "the content is a true prefix of the file");
}

// ============================================================================================
// T6/T7 — `edit` refuses the prefix BY NAME, and the false-positive bound is structural
// ============================================================================================

/// **The refusal that closes the loop the feature opened.**
///
/// The `replacing` is taken **verbatim from what `read` returned**, never constructed here — the
/// coupling is the point. A test that builds its own prefix asserts the test's idea of the format;
/// this asserts the producer's.
#[test]
fn a_replacing_that_still_carries_the_prefix_is_refused_by_name_and_located() {
    let fx = Fixture::new("prefix-miss");
    fx.seed("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
    let read = fx.call("read", Args::new().text("path", "src/lib.rs"));
    let second = fx.text(&read).lines().nth(1).expect("three lines").to_string();

    let r = fx.call(
        "edit",
        Args::new()
            .text("path", "src/lib.rs")
            .text("replacing", &second)
            .text("content", "fn deux() {}"),
    );
    assert!(r.failed, "a prefixed `replacing` must not match anything");
    let why = fx.why(&r);
    assert!(why.contains("line-number prefix"), "named, not generic: {why}");
    assert!(why.contains("`read`"), "and it must name where the prefix came from: {why}");
    assert!(why.contains("line 2"), "and where the stripped text really is: {why}");
    assert_eq!(
        fx.on_disk("src/lib.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
        "a refusal writes nothing"
    );

    // **The mandatory companion.** A genuinely-absent snippet gets the GENERIC message. Without
    // this, deleting the detector leaves the test above failing on `failed` — which stays true —
    // rather than on the phrase, and a broken implementation passes.
    let plain = fx.call(
        "edit",
        Args::new()
            .text("path", "src/lib.rs")
            .text("replacing", "fn nowhere() {}")
            .text("content", "x"),
    );
    assert!(plain.failed);
    assert!(
        !fx.why(&plain).contains("line-number prefix"),
        "an ordinary miss must not be diagnosed as a prefix: {}",
        fx.why(&plain)
    );
    assert!(fx.why(&plain).contains("not found"), "{}", fx.why(&plain));

    // And the corrected call — the one the message tells it to make — succeeds in ONE step.
    let fixed = strip_line_numbers(&format!("{second}\n")).expect("the message's own advice");
    let ok = fx.call(
        "edit",
        Args::new()
            .text("path", "src/lib.rs")
            .text("replacing", fixed.trim_end())
            .text("content", "fn deux() {}"),
    );
    assert!(!ok.failed, "{}", fx.why(&ok));
    assert_eq!(fx.on_disk("src/lib.rs"), "fn one() {}\nfn deux() {}\nfn three() {}\n");
}

/// **The bound is structural, not statistical, and this is the proof.**
///
/// A file that genuinely contains `      42<TAB>foo` — a transcript, a fixture, this very test —
/// edited with exactly that snippet **matches on the first search** and never reaches the
/// detector. The only way to reach the named refusal is for the numbered form to be ABSENT and the
/// stripped form PRESENT, under which the diagnosis is checked against the file rather than
/// guessed.
#[test]
fn a_file_that_really_contains_numbered_text_is_edited_normally() {
    let fx = Fixture::new("false-positive");
    let numbered = format!("{}foo\n{}bar\n", prefix(42), prefix(43));
    fx.seed("transcript.txt", &format!("header\n{numbered}footer\n"));

    let r = fx.call(
        "edit",
        Args::new()
            .text("path", "transcript.txt")
            .text("replacing", &numbered)
            .text("content", "REPLACED\n"),
    );
    assert!(!r.failed, "the literal is present, so it is replaced: {}", fx.why(&r));
    assert_eq!(fx.on_disk("transcript.txt"), "header\nREPLACED\nfooter\n");

    // The other half: with only the STRIPPED form in the file, the same argument fires the named
    // refusal and states the line it found.
    fx.seed("plain.txt", "header\nfoo\nbar\nfooter\n");
    let miss = fx.call(
        "edit",
        Args::new().text("path", "plain.txt").text("replacing", &numbered).text("content", "X\n"),
    );
    assert!(miss.failed);
    assert!(fx.why(&miss).contains("line-number prefix"), "{}", fx.why(&miss));
    assert!(fx.why(&miss).contains("line 2"), "the located line: {}", fx.why(&miss));
}

/// T8 — non-consecutive numbering falls through to the ordinary message, and consecutive
/// numbering does not. The pair is what shows the consecutiveness rule is doing work.
#[test]
fn consecutiveness_is_what_separates_a_prefix_from_a_coincidence() {
    let fx = Fixture::new("consecutive");
    fx.seed("f.txt", "alpha\nbeta\n");

    let scattered = format!("{}alpha\n{}beta\n", prefix(1), prefix(9));
    let a = fx.call(
        "edit",
        Args::new().text("path", "f.txt").text("replacing", &scattered).text("content", "x"),
    );
    assert!(a.failed);
    assert!(
        !fx.why(&a).contains("line-number prefix"),
        "non-consecutive numbers are not this harness's prefix: {}",
        fx.why(&a)
    );

    let consecutive = format!("{}alpha\n{}beta\n", prefix(1), prefix(2));
    let b = fx.call(
        "edit",
        Args::new().text("path", "f.txt").text("replacing", &consecutive).text("content", "x"),
    );
    assert!(b.failed);
    assert!(fx.why(&b).contains("line-number prefix"), "{}", fx.why(&b));
}

// ============================================================================================
// T9 — the `content` asymmetry: `edit` refuses, `write` warns and proceeds
// ============================================================================================

/// **Numbered `content` is the worse half: it SUCCEEDS and corrupts the file.** A prefixed
/// `replacing` fails loudly; a prefixed `content` writes `   42<TAB>` into the source and reports
/// `+n −m`. So `edit` refuses it and names the tool that would have been right.
#[test]
fn edit_refuses_numbered_content_and_names_write() {
    let fx = Fixture::new("numbered-content");
    fx.seed("f.rs", "fn a() {}\nMARK\nfn b() {}\n");
    let numbered = format!("{}fn c() {{}}\n{}fn d() {{}}", prefix(10), prefix(11));

    let r = fx.call(
        "edit",
        Args::new().text("path", "f.rs").text("replacing", "MARK").text("content", &numbered),
    );
    assert!(r.failed, "numbered content must not be spliced into source");
    assert!(fx.why(&r).contains("line-number prefix"), "{}", fx.why(&r));
    assert!(fx.why(&r).contains("`write`"), "and it must name the escape: {}", fx.why(&r));
    assert_eq!(fx.on_disk("f.rs"), "fn a() {}\nMARK\nfn b() {}\n", "and change nothing");
}

/// The control for the THRESHOLD, which is the part a broken implementation gets wrong in the
/// direction nobody notices: one numbered-looking line is plausibly genuine content and must NOT
/// be refused. Derived from the constant, never typed.
#[test]
fn a_single_numbered_looking_line_is_not_refused_as_content() {
    let fx = Fixture::new("one-line-content");
    fx.seed("f.txt", "before\nMARK\nafter\n");
    let one = format!("{}an ordinary line", prefix(7));
    assert!(NUMBERED_CONTENT_LINES > 1, "premise: the threshold is above one line");

    let r = fx.call(
        "edit",
        Args::new().text("path", "f.txt").text("replacing", "MARK").text("content", &one),
    );
    assert!(!r.failed, "one line is under the threshold: {}", fx.why(&r));
    assert_eq!(fx.on_disk("f.txt"), format!("before\n{one}\nafter\n"));
}

/// **`write` is the escape hatch, and it must really work** — a file that legitimately contains
/// `cat -n` output has to be writable through the tool surface, or the refusal above removes a
/// capability instead of redirecting one.
#[test]
fn write_stores_numbered_text_and_says_that_it_did() {
    let fx = Fixture::new("write-numbered");
    let numbered = format!("{}foo\n{}bar\n", prefix(1), prefix(2));

    let r = fx.call("write", Args::new().text("path", "fixture.txt").text("content", &numbered));
    assert!(!r.failed, "{}", fx.why(&r));
    assert_eq!(fx.on_disk("fixture.txt"), numbered, "the prefixes really are on disk");
    assert!(
        r.summary.render().contains("line-numbered"),
        "and the result says so: {}",
        r.summary.render()
    );
    assert!(
        r.summary.detail.clone().unwrap_or_default().contains("line-number prefix"),
        "in words as well as in a state: {:?}",
        r.summary.detail
    );

    // The control: ordinary content carries no such state, so a `write` that always warned would
    // not pass this pair.
    let plain = fx.call("write", Args::new().text("path", "plain.txt").text("content", "a\nb\n"));
    assert!(!plain.summary.render().contains("line-numbered"), "{}", plain.summary.render());
    assert!(plain.summary.detail.is_none(), "{:?}", plain.summary.detail);
}

// ============================================================================================
// T10 — the quarantine boundary, asserted as a CONJUNCTION
// ============================================================================================

/// **`read(path)` is numbered and does not quarantine; `read(ref)` quarantines and is not
/// numbered.** Both halves in one test, because either alone can drift green.
///
/// `Engine::condense_batch` routes on `marlowe_permission::blocks_composed_targets(outcome.trust)`
/// — engine.rs:1888 — so that predicate, applied to the outcome this executor produced, is the
/// real routing decision rather than a restatement of it.
///
/// Numbering a fetched document would do two kinds of damage. Fidelity: the summariser's input
/// becomes a harness-mangled document and its summary may quote prefixes. Worse, it forges the
/// harness's own voice — `condense_chunk` rests on source labels being *"assigned here, never
/// taken from the content"*, and a prefix on every line of attacker-controlled text is exactly a
/// harness-authored marker inside attacker-controlled text. There is nothing to edit in a fetched
/// page, so the numbers buy nothing and cost the property that section is built on.
#[test]
fn only_the_path_branch_is_numbered_and_only_the_ref_branch_is_quarantined() {
    let fx = Fixture::new("quarantine");
    let body = "first line of the page\nsecond line of the page\nthird line of the page\n";
    fx.seed("workspace.txt", body);

    // Half one: a workspace read.
    let from_path = fx.call("read", Args::new().text("path", "workspace.txt"));
    assert!(
        strip_line_numbers(&fx.text(&from_path)).is_some(),
        "a workspace read is numbered: {:?}",
        fx.text(&from_path)
    );
    assert!(
        !marlowe_permission::blocks_composed_targets(from_path.trust),
        "and it does not go to a quarantined reader"
    );

    // Half two: the same text, fetched, stored, and dereferenced.
    let out = marlowe_exec::corpus::read(
        "https://example.test/p",
        marlowe_net::Fetched {
            status: 200,
            content_type: Some("text/plain; charset=utf-8".into()),
            bytes: body.as_bytes().to_vec(),
            final_url: "https://example.test/p".into(),
            redirect_to: None,
            wire_bytes: body.len(),
            reused_connection: false,
        },
    );
    let reference =
        fx.store.put("https://example.test/p", body.len(), out.document().expect("extracts").clone());

    let from_ref = fx.call("read", Args::new().text("ref", &reference.hash));
    assert!(!from_ref.failed, "{}", fx.why(&from_ref));
    let doc = fx.text(&from_ref);
    assert!(doc.contains("second line of the page"), "premise: the document came back: {doc:?}");
    assert!(
        strip_line_numbers(&doc).is_none(),
        "a fetched document must NOT be numbered: {doc:?}"
    );
    assert!(
        !doc.lines().any(|l| l.starts_with(&prefix(1)) || l.starts_with(&prefix(2))),
        "not one line of it: {doc:?}"
    );
    assert!(
        marlowe_permission::blocks_composed_targets(from_ref.trust),
        "and it IS the class that routes to a quarantined reader"
    );
}

// ============================================================================================
// CRLF — the pre-existing defect numbering would have made universal
// ============================================================================================

/// **A windowed read of a CRLF file used to come back LF**, so nothing copied out of it could ever
/// match. Whole-file reads under both ceilings escaped it, which is why nobody hit it.
#[test]
fn a_crlf_file_survives_a_read_and_the_snippet_it_returns_still_edits() {
    let fx = Fixture::new("crlf");
    let raw = "alpha\r\nbeta\r\ngamma\r\n";
    fx.seed("dos.txt", raw);

    let r = fx.call("read", Args::new().text("path", "dos.txt").text("range", "2-2"));
    let line = fx.text(&r);
    assert_eq!(line, format!("{}beta\r\n", prefix(2)), "terminators are re-emitted verbatim");

    let snippet = strip_line_numbers(&line).expect("numbered");
    let ok = fx.call(
        "edit",
        Args::new().text("path", "dos.txt").text("replacing", &snippet).text("content", "BETA\r\n"),
    );
    assert!(!ok.failed, "a snippet copied from a CRLF read must match: {}", fx.why(&ok));
    assert_eq!(fx.on_disk("dos.txt"), "alpha\r\nBETA\r\ngamma\r\n");
}

/// And a model that COMPOSED the snippet rather than copying it writes LF. The generic message
/// names the wrong cause — "including indentation" sends it to look at spaces — so CRLF gets its
/// own sentence.
#[test]
fn an_lf_snippet_against_a_crlf_file_is_diagnosed_as_line_endings() {
    let fx = Fixture::new("crlf-miss");
    fx.seed("dos.txt", "alpha\r\nbeta\r\ngamma\r\n");
    let r = fx.call(
        "edit",
        Args::new().text("path", "dos.txt").text("replacing", "alpha\nbeta").text("content", "X"),
    );
    assert!(r.failed);
    let why = fx.why(&r);
    assert!(why.contains("CRLF"), "the cause must be named: {why}");
    assert!(why.contains("line 1"), "and located: {why}");
}

// ============================================================================================
// A cap that is derived, not typed
// ============================================================================================

/// `MAX_EDIT_SITES_NAMED` bounds the enumeration and nothing else; the count stays exact. Asserted
/// from the constant so moving it moves the expectation.
#[test]
fn the_named_sites_are_bounded_by_the_constant() {
    let fx = Fixture::new("sites");
    fx.seed("m.txt", &"dup\n".repeat(MAX_EDIT_SITES_NAMED));
    let r = fx.call(
        "edit",
        Args::new().text("path", "m.txt").text("replacing", "dup").text("content", "x"),
    );
    assert!(r.failed);
    let why = fx.why(&r);
    assert!(
        !why.contains(" more"),
        "exactly at the bound, nothing is elided: {why}"
    );
    assert!(why.contains(&format!("{MAX_EDIT_SITES_NAMED}")), "{why}");
}
