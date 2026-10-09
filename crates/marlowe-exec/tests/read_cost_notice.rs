//! **`read`'s cost notice must state the cost of the thing it names.**
//!
//! The sentence is *"The whole file is about N tokens; to find something specific, `grep` …"*, and
//! its entire job is to let a model choose between reading this file and searching it. A number
//! that is wrong in the cheap direction is worse than no number, because it is acted on.
//!
//! # Why the old test could not see the bug
//!
//! `only_a_genuinely_large_file_is_called_expensive` asserted `tail.contains("tokens")` — **the
//! presence of the word, never the value.** It reads identically whether the figure is right or
//! 7.8x low, which is what it was. That is CLAUDE.md's instance #16 in miniature: the property
//! asserted where it is *declared* rather than where it is *enforced*. Every assertion here is on
//! a NUMBER, compared against an expectation this file builds for itself.
//!
//! # The expectation, and why it is independent
//!
//! [`whole_numbered_cost`] builds the exact string a model would receive if it read the whole file
//! — `marlowe_exec::number_lines(content, 1)` — and runs the assembler's own
//! `marlowe_loop::estimate_tokens` over it. It shares no arithmetic with the executor: the
//! executor never materialises that string (it only ever holds a window), so it computes the same
//! quantity from a byte count and a line count. If those two routes ever disagree, this file
//! fails.
//!
//! # What each test reads on the unfixed build
//!
//! Recorded before the fix, on the build where `estimate_tokens` ran after the `range` slice and
//! before `number_lines`:
//!
//! | Test | What it reads unfixed |
//! |---|---|
//! | `the_stated_cost_is_the_whole_file_not_the_range` | *"the notice states **45631** tokens, the whole numbered file is **356298** — off by **7.81x**"* |
//! | `the_stated_cost_counts_the_numbers_the_model_receives` | *"states **309632**, the whole numbered file is **356298** — off by **1.15x**"* |
//! | `a_large_file_read_through_a_small_range_still_warns` | *"a **169632**-token file read through a 3,000-line range got **NO cost warning**"* — the body ends `[showing lines 1-2000 of 40000. Continue with range "2001-4000".]` and stops |
//! | `an_ordinary_truncated_file_is_not_dressed_as_a_warning` | passes — it is a negative control, and it passes on both builds |
//! | `a_small_file_gets_no_notice_and_no_cost` | passes — likewise |
//! | `estimate_tokens_of_agrees_with_the_assemblers_estimator` | passes — it pins a helper, not a behaviour |

use std::io::Write;

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{ToolBody, ToolHost};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{Adjudicator, Args, EgressPolicy, Request, TaintSet, Tier};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, BUILTIN_TOOLS};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Workspace {
    dir: std::path::PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("marlowe-readcost-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn write(&self, name: &str, contents: &str) -> String {
        let mut f = std::fs::File::create(self.dir.join(name)).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        name.to_string()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Adjudicate and execute one `read` exactly as the loop does, so the handle comes from the
/// permission layer rather than from the test.
///
/// `context_tokens` is a parameter because "large" is measured against the model's window and
/// several of these tests are about exactly where that line falls. `0` keeps the executor's
/// default — `with_context_tokens` ignores it — which is the 32,768 a host built without one uses.
fn read_with(ws: &Workspace, args: &[(&str, &str)], context_tokens: u32) -> String {
    let registry = builtin_registry().expect("the builtin registry loads");
    let tool = ToolId::new("read");
    let mut a = Args::new();
    for (k, v) in args {
        a = a.text(*k, *v);
    }
    let exposed =
        ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()).unwrap();
    let mut taint = TaintSet::new();
    for (name, _) in a.iter() {
        taint.insert(name.clone(), TrustClass::UserAsserted);
    }
    let mut adjudicator = Adjudicator::new(WorkspaceScope::new().expect("scope"));
    let adjudication = adjudicator.adjudicate(Request {
        manifest: registry.manifest(&tool).expect("read is registered"),
        args: &a,
        taint: &taint,
        exposed: &exposed,
        egress: &EgressPolicy::DenyAll,
        workspace: &ws.dir,
        tier: Tier::Silent,
        novelty: None,
    });
    let mut host = FileSystemTools::new(WorkspaceScope::new().expect("scope"), ws.dir.clone())
        .with_context_tokens(context_tokens);
    let out = host.execute(&tool, &a, &adjudication);
    assert!(!out.failed, "read failed: {:?}", out.summary.detail);
    match &out.body {
        ToolBody::Inline(s) => s.clone(),
        ToolBody::Reference { hash, bytes } => panic!("a file must not come back as {hash} {bytes}"),
    }
}

/// The figure the notice actually printed, or `None` if it printed no cost at all.
///
/// **The `None` case is a real reading, not a parse failure**, and one test exists for it: a large
/// file read through a small range used to produce no cost sentence whatsoever, which is the worse
/// half of the defect — a wrong number can at least be doubted.
fn stated_tokens(body: &str) -> Option<u32> {
    let (_, after) = body.split_once("The whole file is about ")?;
    let (n, _) = after.split_once(" tokens")?;
    n.trim().parse().ok()
}

/// What reading the whole of `content` through `read` really costs the model: the file, numbered.
///
/// Built by materialising the string rather than by arithmetic, so it shares no expression with
/// the executor.
fn whole_numbered_cost(content: &str) -> u32 {
    marlowe_loop::estimate_tokens(&marlowe_exec::number_lines(content, 1))
}

/// Ordinary prose-ish lines: long enough that a 20,000-line file is genuinely large.
fn weighty(lines: usize) -> String {
    (1..=lines).map(|i| format!("line {i} with enough text to weigh something\n")).collect()
}

/// Short lines: a file that is very large in total while any few-thousand-line slice of it is not.
fn slight(lines: usize) -> String {
    (1..=lines).map(|i| format!("{i}\n")).collect()
}

#[track_caller]
fn within_one_percent(stated: u32, expected: u32, what: &str) {
    let (hi, lo) = (stated.max(expected) as f64, stated.min(expected) as f64);
    let ratio = hi / lo.max(1.0);
    assert!(
        ratio <= 1.01,
        "{what}: the notice states {stated} tokens, the whole numbered file is {expected} \
         — off by {ratio:.2}x"
    );
}

/// **DEFECT A: the sentence is about the file, so it must be MEASURED on the file.**
///
/// The figure used to be computed after `range` sliced the text, so a range read produced a
/// sentence about the range wearing the file's name. This is the reproduction from the audit,
/// unchanged: 20,000 weighty lines, read as `range: "1-3000"`.
#[test]
fn the_stated_cost_is_the_whole_file_not_the_range() {
    let ws = Workspace::new("range");
    let content = weighty(20_000);
    let path = ws.write("big.txt", &content);
    let expected = whole_numbered_cost(&content);

    let body = read_with(&ws, &[("path", &path), ("range", "1-3000")], 0);
    let stated = stated_tokens(&body)
        .unwrap_or_else(|| panic!("a 20,000-line file must state a cost: {body:.300}"));

    // Unfixed: 27,631 against 189,632 — the range's cost, printed as the file's.
    within_one_percent(stated, expected, "read through range 1-3000");

    // And the same file read with no range at all states the same number, because the number is
    // a property of the file and not of the request.
    let whole = read_with(&ws, &[("path", &path)], 0);
    let stated_whole = stated_tokens(&whole).expect("the same file, no range");
    assert_eq!(
        stated, stated_whole,
        "the cost of a file cannot depend on which window of it was asked for"
    );
}

/// **DEFECT B: the model receives numbered text, so the figure is measured on numbered text.**
///
/// `READ_WINDOW_BYTES` is applied after `number_lines` precisely so that it *"counts the bytes the
/// model receives rather than the bytes on disk"*. The cost figure four lines above it was never
/// given the same treatment: every line costs `LINE_NUMBER_WIDTH + 1` = 7 bytes, ~+27% here.
///
/// The second assertion is the one that names the old value: the on-disk figure is a specific
/// wrong number, and the test says so rather than only bounding the right one.
#[test]
fn the_stated_cost_counts_the_numbers_the_model_receives() {
    let ws = Workspace::new("numbered");
    let content = weighty(20_000);
    let path = ws.write("big.txt", &content);

    let body = read_with(&ws, &[("path", &path)], 0);
    let stated = stated_tokens(&body).expect("a 20,000-line file must state a cost");

    within_one_percent(stated, whole_numbered_cost(&content), "whole-file read");

    // Unfixed: stated == on_disk == 148,987, against 189,632 numbered.
    let on_disk = marlowe_loop::estimate_tokens(&content);
    let prefixes = 20_000 * (marlowe_exec::LINE_NUMBER_WIDTH + 1);
    assert!(
        stated > on_disk,
        "the figure is the bytes on disk ({on_disk}) and the model is sent {prefixes} bytes of \
         line numbers on top of them"
    );
}

/// **The half that disappears silently, and therefore the one that matters most.**
///
/// `expensive` derives from the same figure, so a large file read through a small range did not
/// merely under-state its cost — it produced **no cost sentence at all**. 40,000 short lines is
/// ~509 KB numbered, and a 3,000-line slice of it is ~14 KB: comfortably under a quarter of a
/// 32,768-token window, so the warning was withheld from a file that is fifteen windows long.
#[test]
fn a_large_file_read_through_a_small_range_still_warns() {
    let ws = Workspace::new("small-range");
    let content = slight(40_000);
    let path = ws.write("many.txt", &content);
    let expected = whole_numbered_cost(&content);

    let body = read_with(&ws, &[("path", &path), ("range", "1-3000")], 0);
    assert!(body.contains("[showing lines "), "the window notice must be there at all: {body:.200}");
    let stated = stated_tokens(&body).unwrap_or_else(|| {
        panic!(
            "a {expected}-token file read through a 3,000-line range got NO cost warning: {}",
            &body[body.len().saturating_sub(300)..]
        )
    });
    within_one_percent(stated, expected, "a large file through a small range");
    assert!(
        body.contains("`grep`"),
        "and it must name the cheaper route: {}",
        &body[body.len().saturating_sub(300)..]
    );
}

/// **NEGATIVE CONTROL: an ordinary truncated file is not dressed as a warning.**
///
/// A model told that everything is expensive has learned nothing about what is. Measured against a
/// real model's window — 128k, what the daemon passes — a 2,400-line file is one truncated read
/// and nothing more: the window notice, and not a word about tokens or `grep`.
///
/// **The context size is explicit here for a reason worth keeping.** Against the 32,768-token
/// default, one full `READ_WINDOW_BYTES` window is already ~11k tokens, which is more than the
/// quarter-of-context the warning triggers on — so on that window nearly every truncated read is
/// genuinely expensive and the notice is not discriminating between files, it is describing the
/// window. That is a property of the two constants, not of this fix, and it is reported with the
/// fix rather than tuned here — changing either constant is a `DECISIONS.md` matter.
#[test]
fn an_ordinary_truncated_file_is_not_dressed_as_a_warning() {
    let ws = Workspace::new("modest");
    let content = slight(2_400);
    let path = ws.write("modest.txt", &content);
    assert!(
        whole_numbered_cost(&content) < 128_000 / marlowe_exec::LARGE_FILE_SHARE_OF_CONTEXT,
        "the control is only a control if the file really is under the line"
    );

    let body = read_with(&ws, &[("path", &path)], 128_000);
    assert!(body.contains("of 2400"), "it is truncated and says so: {body:.200}");
    assert_eq!(stated_tokens(&body), None, "no cost sentence: {}", &body[body.len() - 200..]);
    assert!(
        !body.contains("`grep`"),
        "and no advice: {}",
        &body[body.len().saturating_sub(200)..]
    );
}

/// **NEGATIVE CONTROL: a file that fits gets no notice at all.**
///
/// The cost sentence lives inside the window notice, so a file returned whole must carry neither.
/// Without this, a fix that simply always printed a cost would pass every test above.
#[test]
fn a_small_file_gets_no_notice_and_no_cost() {
    let ws = Workspace::new("short");
    let path = ws.write("short.txt", "alpha\nbeta\ngamma\n");
    let body = read_with(&ws, &[("path", &path)], 0);
    assert_eq!(body, marlowe_exec::number_lines("alpha\nbeta\ngamma\n", 1));
    assert_eq!(stated_tokens(&body), None);
    assert!(!body.contains("[showing lines "), "no window notice on a file that fitted: {body}");
}

/// **The drift pin.** [`marlowe_exec::estimate_tokens_of`] exists because the whole numbered file
/// is never materialised — only a window of it is — so its cost is computed from a length with no
/// string behind it. That is a second copy of the assembler's three-characters-per-token rule, and
/// a second copy held true by a comment is held true by nothing.
///
/// Lengths chosen to cross every rounding boundary, plus the sizes this executor actually deals in.
#[test]
fn estimate_tokens_of_agrees_with_the_assemblers_estimator() {
    for len in [0, 1, 2, 3, 4, 5, 6, 7, 999, 1_000, 32 * 1024, 500_000] {
        let s = "x".repeat(len);
        assert_eq!(
            marlowe_exec::estimate_tokens_of(len),
            marlowe_loop::estimate_tokens(&s),
            "the two estimators have drifted at {len} bytes"
        );
    }
}
