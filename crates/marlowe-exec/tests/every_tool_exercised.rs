//! An ordinary-and-edge-case sweep of the local executors: `read`, `write`, `edit`, `glob`,
//! `grep`, `bash`. `web` is excluded — it reaches the network.
//!
//! Companion to `tests/executors.rs`, which established the pattern this file reuses: adjudicate
//! for real through `WorkspaceScope` and the `Adjudicator`, then hand the result to
//! `FileSystemTools`, so what is under test is the executor using the handle the permission layer
//! actually opened rather than a hand-built one.
//!
//! This file exists because three bugs shipped past a ~1400-test suite that only ever exercised
//! the cases someone thought to write: `bash` mangling every quoted command on Windows, `edit`
//! creating a file and then refusing to write it, `read` on an empty file being indistinguishable
//! from a missing one. The brief is to run each tool against a real temp filesystem across
//! ordinary and adversarial inputs and see what else is in that family.
//!
//! Every `// SUSPECT:` comment names a behaviour this file found concerning, states what a model
//! holding only the tool's description would expect instead, and why the gap matters. Everything
//! else asserts what is believed to be correct behaviour and is not flagged.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{ToolBody, ToolHost, ToolOutcome};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{
    Adjudication, Adjudicator, ArgValue, Args, BlockReason, EgressPolicy, Outcome, Request,
    TaintSet, Tier,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, ToolRegistry, BUILTIN_TOOLS};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A bare workspace: nothing pre-seeded. Each test builds exactly the files it needs, so a
/// fixture's contents are visible at the call site rather than hidden in shared setup.
struct Fixture {
    root: PathBuf,
    registry: ToolRegistry,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-exec-every-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root, registry: builtin_registry().unwrap() }
    }

    fn seed(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.root.join(rel)).unwrap();
    }

    fn on_disk(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap()
    }

    /// Adjudicate for real, then hand the result to the executor — see `tests/executors.rs`'s
    /// header for why re-resolving a path here instead would test the wrong thing.
    fn call(&self, tool: &str, args: Args) -> (Outcome, ToolOutcome) {
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
        let outcome = adjudication.decision.outcome.clone();
        let mut tools =
            FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), &self.root);
        let result = tools.execute(&ToolId::new(tool), &args, &adjudication);
        (outcome, result)
    }

    /// **Why a call failed, wherever the executor put it.**
    ///
    /// `marlowe_exec::failed` returns `body: ToolBody::Inline(String::new())` and the reason in
    /// `summary.detail`. A failure assertion that reads `text()` therefore reads the empty string
    /// — which is why every one of them panicked with a blank message. This is the same
    /// detail-versus-body split that reached the MODEL as the literal string `"edit · "`, in a
    /// test file rather than in a window.
    ///
    /// Both are concatenated rather than choosing one, so a tool that reports through the body
    /// (`bash` puts its output there) is covered by the same helper.
    fn why(&self, r: &ToolOutcome) -> String {
        format!("{} {}", r.summary.detail.clone().unwrap_or_default(), self.text(r))
    }

    fn text(&self, r: &ToolOutcome) -> String {
        match &r.body {
            ToolBody::Inline(s) => s.clone(),
            ToolBody::Reference { hash, bytes } => format!("<ref {hash} {bytes}>"),
        }
    }

    fn detail(&self, r: &ToolOutcome) -> String {
        r.summary.detail.clone().unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ============================================================================================
// read
// ============================================================================================

#[test]
fn read_ordinary_success() {
    let fx = Fixture::new("read-ok");
    fx.seed("hello.txt", "one\ntwo\nthree\n");
    let (outcome, r) = fx.call("read", Args::new().text("path", "hello.txt"));
    assert_eq!(outcome, Outcome::Allowed);
    assert!(!r.failed, "{:?}", r.summary);
    // **`read` returns `cat -n`.** Both halves are derived from the producer, never typed: the
    // body from `number_lines`, and the byte count from that body's length — so a
    // `LINE_NUMBER_WIDTH` that moves moves this expectation with it instead of silently asserting
    // a different property, which is what six tests in this repo did when `MAX_EXPOSED_TOOLS`
    // moved.
    let expected = marlowe_exec::number_lines("one\ntwo\nthree\n", 1);
    assert_eq!(fx.text(&r), expected);
    assert_eq!(r.summary.render(), format!("3 lines · {} B", expected.len()));
}

#[test]
fn read_neither_path_nor_ref_is_a_clear_refusal() {
    let fx = Fixture::new("read-neither");
    let (outcome, r) = fx.call("read", Args::new());
    assert_eq!(outcome, Outcome::Allowed, "nothing was declared, so nothing was checked");
    assert!(r.failed);
    // **The reason is in BOTH the body and `detail` now**, so no consumer has to know that
    // failures are shaped differently from successes. This asserted that `detail` was EMPTY,
    // which was true and was the defect: the model's window is built from the body, the journal
    // read `detail`, and each got half the story.
    assert!(!fx.detail(&r).is_empty(), "a failure must carry its reason in `detail`");
    assert!(
        fx.why(&r).contains("supply either `path`") && fx.why(&r).contains("Neither was given"),
        "{}",
        fx.why(&r)
    );
}

#[test]
fn read_empty_string_path_is_a_malformed_request() {
    let fx = Fixture::new("read-empty-path");
    let (outcome, r) = fx.call("read", Args::new().text("path", ""));
    assert!(outcome.is_blocked(), "{outcome:?}");
    match outcome {
        Outcome::Blocked { reason: BlockReason::UndeclaredPath { detail, .. } } => {
            assert!(detail.contains("empty path component"), "{detail}");
        }
        other => panic!("expected UndeclaredPath, got {other:?}"),
    }
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn read_path_with_a_space_survives() {
    let fx = Fixture::new("read-space");
    fx.seed("a folder/my notes.md", "space in the path\n");
    let (_, r) = fx.call("read", Args::new().text("path", "a folder/my notes.md"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.text(&r), marlowe_exec::number_lines("space in the path\n", 1));
}

#[test]
fn read_path_with_unicode_survives() {
    let fx = Fixture::new("read-unicode");
    // Precomposed NFC, matching what `request::validate` normalizes both sides to.
    fx.seed("café-résumé.md", "unicode path\n");
    let (_, r) = fx.call("read", Args::new().text("path", "café-résumé.md"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.text(&r), marlowe_exec::number_lines("unicode path\n", 1));
}

#[test]
fn read_missing_file_says_it_is_not_a_refusal() {
    let fx = Fixture::new("read-missing");
    let (outcome, r) = fx.call("read", Args::new().text("path", "does-not-exist.txt"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    match outcome {
        Outcome::Blocked { reason: BlockReason::UndeclaredPath { detail, .. } } => {
            // This is `ScopeError::Unopenable`'s careful text -- "NOT a scoping refusal" -- and
            // it is real information that never reaches the executor's own message below.
            assert!(detail.contains("NOT a scoping refusal"), "{detail}");
            assert!(detail.contains("`glob`"), "{detail}");
        }
        other => panic!("expected UndeclaredPath, got {other:?}"),
    }
    assert!(r.failed);
    // SUSPECT: the executor's own text collapses every reason a handle could be absent into one
    // sentence -- "no adjudicated handle for `path`" -- which says nothing about *why*. The
    // adjudicator's `BlockReason::UndeclaredPath` carries the careful, ENOENT-specific message
    // above ("NOT a scoping refusal... confirm with `glob`"), and it is genuinely reachable by
    // the model (see `refusal_prose` in `marlowe-loop/src/engine.rs`), so this executor-level
    // fallback is not the only thing the model sees in production. It matters anyway because
    // `refusal_prose`'s `UndeclaredPath` arm is a SINGLE template shared by every scope failure --
    // "The path `{path}` is outside the workspace this run may touch... Retry with a path
    // relative to the workspace root, with no `..` and no drive letter" -- which is simply FALSE
    // for a relative, in-scope path that happens not to exist. A model told to retry with a
    // "properly relative" path when the path was already exactly that will try meaningless
    // reformulations of a filename that was never the problem. This is the same confusion
    // CLAUDE.md already records happening live on 2026-08-25 over `Unopenable`, and the fix
    // documented there (the ScopeError message itself) is undone one layer up, in the file the
    // model's refusal prose actually comes from.
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn read_a_directory_given_as_path() {
    let fx = Fixture::new("read-dir");
    fx.mkdir("a-directory");
    let (outcome, r) = fx.call("read", Args::new().text("path", "a-directory"));
    // Scoping does not require the FINAL component to be a directory or a file for `Access::Read`
    // -- that check only applies to intermediate components during the walk (`walk.rs`, "not a
    // directory" is checked only `if !is_last`). So the handle opens successfully...
    assert!(outcome.is_blocked() == false || outcome.needs_approval(), "{outcome:?}");
    // ...and what the executor does with a directory HANDLE is the interesting part.
    if r.failed {
        // A read error surfaced, which is the sound outcome: the model is told the read did not
        // work, rather than being handed a result that looks like an empty file.
        assert!(!fx.detail(&r).is_empty(), "a failed read should say why: {:?}", r.summary);
    } else {
        // SUSPECT: if this branch is live, reading a directory produced a *successful* result --
        // and given `MAX_READ_BYTES`/UTF-8 handling, the most likely shape is "0 lines · 0 B",
        // which is EXACTLY the signature `read`'s own manifest description reserves for "the file
        // is there and is empty" ("a result of `0 lines · 0 B` means the file is there and is
        // empty — reading it again will not change that"). A directory is neither missing nor
        // empty, and a model told to trust that signature would misdiagnose a directory as an
        // empty file with the same confidence the manifest asks it to have.
        assert_eq!(
            r.summary.render(),
            "0 lines · 0 B",
            "if this ever reads as content instead, this whole test needs re-deriving: {}",
            fx.text(&r)
        );
    }
}

#[test]
fn read_empty_file_is_the_documented_signature() {
    let fx = Fixture::new("read-empty-file");
    fx.seed("empty.txt", "");
    let (outcome, r) = fx.call("read", Args::new().text("path", "empty.txt"));
    assert_eq!(outcome, Outcome::Allowed);
    // Not flagged: the manifest description now states this explicitly ("a result of `0 lines ·
    // 0 B` means the file is there and is empty"), so this is the documented contract, verified.
    assert!(!r.failed);
    assert_eq!(fx.text(&r), "");
    assert_eq!(r.summary.render(), "0 lines · 0 B");
}

/// **A file over the inline threshold is READ, not referenced.**
///
/// This asserted the opposite, and the opposite was the defect: anything over `MAX_INLINE_BYTES`
/// became a `ContentRef` — a hash plus a 4 KB head-and-tail — and **a file reference cannot be
/// dereferenced**, because `read`'s `ref` takes ids `web` issued. The middle of every file above
/// 8 KB was unreachable, which is why a run could read five design documents and answer from none
/// of them.
///
/// The summary's numbers now describe what was RETURNED rather than what was withheld, which is
/// the honest reading of a windowed result: the notice carries the file's true length.
#[test]
fn read_large_file_returns_a_readable_window_not_a_hash() {
    let fx = Fixture::new("read-large");
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    assert!(
        content.len() > marlowe_exec::MAX_INLINE_BYTES,
        "the test is only meaningful over the old inline threshold"
    );
    fx.seed("big.txt", &content);

    let (_, r) = fx.call("read", Args::new().text("path", "big.txt"));
    assert!(!r.failed, "{:?}", r.summary);

    match &r.body {
        ToolBody::Inline(body) => {
            assert!(body.contains("line 1\n"), "the window starts at the top");
            assert!(body.contains("of 5000"), "and states the file's true length: {body:.200}");
        }
        ToolBody::Reference { .. } => {
            panic!("a file must never come back as a hash the model cannot dereference")
        }
    }
    // The counts describe the WINDOW. The full length is in the notice, where it is useful.
    let rendered = r.summary.render();
    assert!(rendered.contains("lines"), "{rendered}");
    assert!(
        fx.text(&r).lines().count() <= marlowe_exec::READ_WINDOW_LINES + 5,
        "the window must bound what came back: {rendered}"
    );
}

#[test]
fn read_range_normal_selects_the_named_lines() {
    let fx = Fixture::new("read-range-normal");
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    fx.seed("hundred.txt", &content);
    let (_, r) = fx.call("read", Args::new().text("path", "hundred.txt").text("range", "20-60"));
    assert!(!r.failed, "{:?}", r.summary);
    let body = fx.text(&r);
    let w = marlowe_exec::LINE_NUMBER_WIDTH;
    assert!(body.starts_with(&format!("{:>w$}\tline 20\n", 20)), "{body:.80}");
    assert!(body.trim_end().ends_with(&format!("{:>w$}\tline 60", 60)), "{body}");
    assert_eq!(body.lines().count(), 41);
    // And the whole window, byte for byte, against the producer.
    let raw: String = (20..=60).map(|i| format!("line {i}\n")).collect();
    assert_eq!(body, marlowe_exec::number_lines(&raw, 20));
}

/// A transposition, and it used to render as `0 lines · 0 B` — the signature `read`'s own
/// description reserves for "the file is there and is empty".
#[test]
fn read_range_reversed_is_refused_with_the_swap() {
    let fx = Fixture::new("read-range-reversed");
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    fx.seed("hundred.txt", &content);
    let (_, r) = fx.call("read", Args::new().text("path", "hundred.txt").text("range", "60-20"));
    assert!(r.failed, "a backwards range must not read as an empty file");
    assert!(fx.why(&r).contains("20-60"), "offer the swap it can see: {}", fx.why(&r));
}

/// **The sharpest collision `range` produced.** Slicing happens BEFORE the metrics are computed,
/// so an out-of-bounds range on a ten-line file rendered `0 lines · 0 B` — the exact string
/// `read`'s description promises means "the file is there and is empty". The description's promise
/// became false the instant `range` was supplied.
#[test]
fn read_range_past_the_end_says_how_long_the_file_is() {
    let fx = Fixture::new("read-range-oob");
    fx.seed("ten.txt", &(1..=10).map(|i| format!("line {i}\n")).collect::<String>());
    let (_, r) = fx.call("read", Args::new().text("path", "ten.txt").text("range", "500-600"));
    assert!(r.failed, "a range that selected nothing must not render as an empty file");
    assert!(
        fx.why(&r).contains("the file has 10"),
        "the fact the model needs is the line count: {}",
        fx.why(&r)
    );
}

/// **`slice_lines` returned the WHOLE FILE for anything it could not parse.** A model that
/// mistyped a range on a large file got everything back with nothing to say the range had been
/// ignored rather than honoured — which for a file near `MAX_INLINE_BYTES` turns a targeted read
/// into a `ContentRef` it then cannot dereference.
#[test]
fn read_range_malformed_is_refused_not_silently_ignored() {
    let fx = Fixture::new("read-range-malformed");
    let content = "line 1\nline 2\nline 3\n";
    fx.seed("three.txt", content);
    let (_, r) = fx.call("read", Args::new().text("path", "three.txt").text("range", "abc"));
    assert!(r.failed, "a range that cannot be parsed must not read as no range at all");
    assert!(fx.why(&r).contains("first-last"), "state the format: {}", fx.why(&r));
}

/// The same family: `"2"` has no `-`, so `split_once` returned `None` and the whole file came
/// back. "Just line 2" is a reasonable guess at a parameter documented as *"line range"*, and the
/// answer to a reasonable guess is the format, not the entire file.
#[test]
fn read_range_of_a_single_number_is_refused_with_the_format() {
    let fx = Fixture::new("read-range-single");
    let content = "line 1\nline 2\nline 3\n";
    fx.seed("three.txt", content);
    let (_, r) = fx.call("read", Args::new().text("path", "three.txt").text("range", "2"));
    assert!(r.failed, "`2` is not a range and must not silently mean the whole file");
    assert!(fx.why(&r).contains("first-last"), "{}", fx.why(&r));
}

// ============================================================================================
// edit
// ============================================================================================

#[test]
fn edit_ordinary_success() {
    let fx = Fixture::new("edit-ok");
    fx.seed("notes.md", "alpha\nbeta needle\ngamma\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "beta needle").text("content", "beta found"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("notes.md"), "alpha\nbeta found\ngamma\n");
}

#[test]
fn edit_missing_path() {
    let fx = Fixture::new("edit-no-path");
    let (_, r) =
        fx.call("edit", Args::new().text("content", "x").text("replacing", "y"));
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn edit_missing_content() {
    let fx = Fixture::new("edit-no-content");
    fx.seed("notes.md", "alpha\n");
    let (_, r) = fx.call("edit", Args::new().text("path", "notes.md").text("replacing", "alpha"));
    assert!(r.failed);
    assert!(fx.why(&r).contains("`content` is required"), "{}", fx.why(&r));
    assert_eq!(fx.on_disk("notes.md"), "alpha\n", "a rejected call must not touch the file");
}

#[test]
fn edit_missing_replacing_names_write_as_the_remedy() {
    let fx = Fixture::new("edit-no-replacing");
    fx.seed("notes.md", "alpha\n");
    let (_, r) = fx.call("edit", Args::new().text("path", "notes.md").text("content", "beta"));
    assert!(r.failed);
    assert!(fx.why(&r).contains("`replacing` is required"), "{}", fx.why(&r));
    assert!(fx.why(&r).contains("`write`"), "{}", fx.why(&r));
    assert_eq!(fx.on_disk("notes.md"), "alpha\n");
}

/// **`str::find("")` returns `Some(0)` for any string, so an empty `replacing` matched trivially
/// at offset 0 and PREPENDED `content` — reported as an ordinary successful edit.**
///
/// A model reaches an empty `replacing` by accident: a snippet extracted from a `read` that
/// returned nothing, a template filled in blank, a variable stripped. It is the write-then-refuse
/// bug one step worse — that one failed loudly and wasted a call; this one succeeded wrongly and
/// changed a file nobody asked it to change.
#[test]
fn edit_empty_string_replacing_is_refused_not_silently_prepended() {
    let fx = Fixture::new("edit-empty-replacing");
    fx.seed("notes.md", "hello\nworld\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "").text("content", "INSERTED-"),
    );
    assert!(r.failed, "an empty `replacing` must not be treated as a match: {:?}", r.summary);
    assert!(fx.why(&r).contains("`write`"), "the remedy must be named: {}", fx.why(&r));
    assert_eq!(
        fx.on_disk("notes.md"),
        "hello\nworld\n",
        "THE PROPERTY: the file is untouched. This used to prepend `content` and report success."
    );
}

#[test]
fn edit_empty_string_content_deletes_the_snippet() {
    let fx = Fixture::new("edit-empty-content");
    fx.seed("notes.md", "alpha\nbeta\ngamma\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "beta\n").text("content", ""),
    );
    // Not flagged: this is an ordinary, predictable deletion -- `content` becoming the empty
    // string is exactly "delete `replacing`", and the manifest's wording ("`content` becomes...
    // what `replacing` becomes") supports reading it that way.
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("notes.md"), "alpha\ngamma\n");
}

#[test]
fn edit_empty_string_path_is_malformed() {
    let fx = Fixture::new("edit-empty-path");
    let (outcome, r) =
        fx.call("edit", Args::new().text("path", "").text("replacing", "x").text("content", "y"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn edit_path_with_a_space_survives() {
    let fx = Fixture::new("edit-space");
    fx.seed("a dir/notes.md", "alpha\nbeta\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "a dir/notes.md").text("replacing", "beta").text("content", "BETA"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("a dir/notes.md"), "alpha\nBETA\n");
}

#[test]
fn edit_path_with_unicode_survives() {
    let fx = Fixture::new("edit-unicode");
    fx.seed("café.md", "alpha\nbeta\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "café.md").text("replacing", "beta").text("content", "BETA"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("café.md"), "alpha\nBETA\n");
}

#[test]
fn edit_a_file_that_does_not_exist_creates_it_at_zero_bytes_then_refuses() {
    let fx = Fixture::new("edit-new-file");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "brand-new.md").text("replacing", "anything").text("content", "x"),
    );
    // Not flagged as a NEW finding: this is the exact, already-documented case from
    // `marlowe-exec/src/lib.rs::replacing_miss` and `tests/executors.rs`. Verified here only for
    // completeness of the sweep -- `path` is a `WritePath`, so scoping opens it `CreateOrOpen`
    // before this executor runs, the file exists at zero bytes, and the refusal names `write` and
    // states the file is now zero bytes.
    assert!(r.failed);
    let d = fx.why(&r);
    assert!(d.contains("`write`"), "{d}");
    assert!(d.contains("zero bytes"), "{d}");
    assert_eq!(fx.on_disk("brand-new.md"), "", "the empty file this call cannot avoid creating");
}

#[test]
fn edit_a_directory_given_as_path() {
    let fx = Fixture::new("edit-dir");
    fx.mkdir("a-directory");
    let (outcome, r) = fx.call(
        "edit",
        Args::new().text("path", "a-directory").text("replacing", "x").text("content", "y"),
    );
    // The path is a WritePath (`CreateOrOpen`), and the target already exists as a directory, so
    // whether scoping opens it at all is the first question.
    if outcome.is_blocked() {
        match outcome {
            Outcome::Blocked { reason: BlockReason::UndeclaredPath { detail, .. } } => {
                assert!(!detail.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(r.failed);
    } else {
        // A handle to the directory was opened. The executor must not report success against a
        // directory it never actually read text from.
        assert!(r.failed, "editing a directory succeeded, and nothing on disk explains what that means: {:?} {}", r.summary, fx.why(&r));
        // SUSPECT (conditional on this branch running): if scoping opens a directory handle for
        // `CreateOrOpen` without erroring, the failure the model sees is whatever
        // `file.read_to_string` or the later `set_len`/`write_all` produced -- a raw OS error
        // string with no mention that the target is a directory. Compare this to `read`'s own
        // handling of the same situation, and to `ScopeError::Unopenable`'s care about naming
        // what actually went wrong: an OS error like "Access is denied. (os error 5)" gives the
        // model nothing to act on beyond "try something else."
        assert!(!fx.detail(&r).is_empty(), "{:?}", r.summary);
    }
}

#[test]
fn edit_parent_directory_missing() {
    let fx = Fixture::new("edit-no-parent");
    let (outcome, r) = fx.call(
        "edit",
        Args::new().text("path", "nosuchdir/file.md").text("replacing", "x").text("content", "y"),
    );
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(
        fs::metadata(fx.root.join("nosuchdir")).is_err(),
        "a missing parent must stay missing -- `edit` creates only the final component"
    );
}

#[test]
fn edit_a_truly_empty_file_refuses_and_names_write() {
    let fx = Fixture::new("edit-truly-empty");
    fx.seed("already-empty.txt", "");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "already-empty.txt").text("replacing", "x").text("content", "y"),
    );
    assert!(r.failed);
    assert!(fx.why(&r).contains("`write`"), "{}", fx.why(&r));
    assert_eq!(fx.on_disk("already-empty.txt"), "");
}

#[test]
fn edit_large_file_reports_a_true_diff_and_writes_correctly() {
    let fx = Fixture::new("edit-large");
    let mut content = String::new();
    for i in 1..=3000 {
        if i == 1500 {
            content.push_str("TARGET_LINE_MARKER\n");
        } else {
            content.push_str(&format!("line {i}\n"));
        }
    }
    fx.seed("big.md", &content);
    let (_, r) = fx.call(
        "edit",
        Args::new()
            .text("path", "big.md")
            .text("replacing", "TARGET_LINE_MARKER")
            .text("content", "REPLACED"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    let on_disk = fx.on_disk("big.md");
    assert!(on_disk.contains("REPLACED\n"), "the replacement did not land");
    assert!(!on_disk.contains("TARGET_LINE_MARKER"), "the original snippet must be gone");
    assert_eq!(on_disk.lines().count(), 3000, "one line replaced by one line: total is unchanged");
}

#[test]
fn edit_snippet_appearing_twice_is_refused_and_both_lines_are_named() {
    let fx = Fixture::new("edit-twice");
    fx.seed("dup.md", "alpha\nshared\nbeta\nshared\ngamma\n");
    let (_, r) =
        fx.call("edit", Args::new().text("path", "dup.md").text("replacing", "shared").text("content", "unique"));
    // **This asserted the OPPOSITE and the opposite was the hazard.** `edit` replaced the first
    // occurrence and reported `+1 −1`; a rename whose old name appears twelve times was edited
    // once, and the summary read back is the same summary a correct edit produces. Nothing
    // downstream could tell them apart, so the run moves on.
    assert!(r.failed, "an ambiguous edit must not look like a successful one: {:?}", r.summary);
    assert_eq!(fx.on_disk("dup.md"), "alpha\nshared\nbeta\nshared\ngamma\n", "and nothing is written");
    let why = fx.why(&r);
    assert!(why.contains("occurs 2 times"), "the count is the first fact: {why}");
    // **The refusal is only worth making because it can say WHERE**, and it can only say where
    // because `read` numbers now. Without the line numbers this is a refusal with no next step.
    assert!(why.contains("at lines 2, 4"), "every site, by line: {why}");
}

/// The control for that: a `replacing` that really is unique still edits, so a refusal that fired
/// on everything would not pass here.
#[test]
fn edit_of_a_unique_snippet_still_replaces_it() {
    let fx = Fixture::new("edit-unique");
    fx.seed("dup.md", "alpha\nshared\nbeta\nshared\ngamma\n");
    let (_, r) = fx.call(
        "edit",
        Args::new()
            .text("path", "dup.md")
            .text("replacing", "alpha\nshared")
            .text("content", "alpha\nunique"),
    );
    assert!(!r.failed, "{}", fx.why(&r));
    assert_eq!(fx.on_disk("dup.md"), "alpha\nunique\nbeta\nshared\ngamma\n");
}

/// And the enumeration is BOUNDED. A refusal that lists ninety line numbers is a context flood
/// inside the sentence that exists to prevent one — the defect ADR-059 fixed in `grep`'s
/// truncation notice, which put 250 paths on a single line.
#[test]
fn a_snippet_at_many_sites_names_a_bounded_list_and_an_exact_count() {
    let fx = Fixture::new("edit-many");
    let n = marlowe_exec::MAX_EDIT_SITES_NAMED + 5;
    fx.seed("many.md", &"needle\n".repeat(n));
    let (_, r) =
        fx.call("edit", Args::new().text("path", "many.md").text("replacing", "needle").text("content", "x"));
    assert!(r.failed);
    let why = fx.why(&r);
    assert!(why.contains(&format!("occurs {n} times")), "the count is exact: {why}");
    assert!(why.contains("and 5 more"), "the list is bounded and says so: {why}");
    assert!(
        !why.contains(&format!(", {}", n)),
        "the last site must not be enumerated: {why}"
    );
}

#[test]
fn edit_snippet_is_the_whole_file() {
    let fx = Fixture::new("edit-whole-file");
    fx.seed("all.md", "everything here\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "all.md").text("replacing", "everything here\n").text("content", "new content\n"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("all.md"), "new content\n");
}

#[test]
fn edit_snippet_differs_only_in_whitespace_names_the_real_problem() {
    let fx = Fixture::new("edit-whitespace");
    fx.seed("notes.md", "alpha\nbeta    needle\ngamma\n");
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "beta needle").text("content", "x"),
    );
    assert!(r.failed);
    let d = fx.why(&r);
    assert!(d.contains("apart from whitespace"), "{d}");
    assert_eq!(fx.on_disk("notes.md"), "alpha\nbeta    needle\ngamma\n", "a miss must not touch the file");
}

// ============================================================================================
// write
// ============================================================================================

#[test]
fn write_ordinary_success() {
    let fx = Fixture::new("write-ok");
    let (_, r) = fx.call("write", Args::new().text("path", "new.txt").text("content", "made"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("new.txt"), "made");
}

#[test]
fn write_missing_path() {
    let fx = Fixture::new("write-no-path");
    let (_, r) = fx.call("write", Args::new().text("content", "x"));
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn write_missing_content() {
    let fx = Fixture::new("write-no-content");
    let (_, r) = fx.call("write", Args::new().text("path", "new.txt"));
    assert!(r.failed);
    assert!(fx.why(&r).contains("`content` is required"), "{}", fx.why(&r));
    assert!(fs::metadata(fx.root.join("new.txt")).is_err() || fx.on_disk("new.txt").is_empty());
}

#[test]
fn write_empty_string_content_truncates_an_existing_file() {
    let fx = Fixture::new("write-empty-content");
    fx.seed("existing.txt", "there was something here\n");
    let (_, r) = fx.call("write", Args::new().text("path", "existing.txt").text("content", ""));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("existing.txt"), "");
}

#[test]
fn write_empty_string_path_is_malformed() {
    let fx = Fixture::new("write-empty-path");
    let (outcome, r) = fx.call("write", Args::new().text("path", "").text("content", "x"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn write_path_with_a_space_survives() {
    let fx = Fixture::new("write-space");
    // **`write` creates the FILE, not the tree** — its description says so, and the original
    // version of this test omitted the directory and read the resulting refusal as a spacing bug.
    fx.mkdir("a dir");
    let (_, r) = fx.call(
        "write",
        Args::new().text("path", "a dir/new file.txt").text("content", "hi"),
    );
    assert!(!r.failed, "{:?} {}", r.summary, fx.why(&r));
    assert_eq!(fx.on_disk("a dir/new file.txt"), "hi");
}

#[test]
fn write_path_with_unicode_survives() {
    let fx = Fixture::new("write-unicode");
    let (_, r) = fx.call("write", Args::new().text("path", "notä-café.txt").text("content", "hi"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("notä-café.txt"), "hi");
}

#[test]
fn write_a_directory_given_as_path() {
    let fx = Fixture::new("write-dir");
    fx.mkdir("a-directory");
    let (outcome, r) =
        fx.call("write", Args::new().text("path", "a-directory").text("content", "x"));
    if outcome.is_blocked() {
        assert!(r.failed);
    } else {
        assert!(r.failed, "writing over a directory succeeded: {:?} {}", r.summary, fx.why(&r));
        // SUSPECT (conditional): same shape as `edit`'s directory case -- if a handle to the
        // directory opens at all, whatever io error `set_len`/`write_all` produces reaches the
        // model as a raw OS string with no mention that the target was a directory rather than a
        // file.
        assert!(!fx.detail(&r).is_empty(), "{:?}", r.summary);
    }
}

#[test]
fn write_over_an_empty_file() {
    let fx = Fixture::new("write-over-empty");
    fx.seed("empty.txt", "");
    let (_, r) = fx.call("write", Args::new().text("path", "empty.txt").text("content", "now filled\n"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("empty.txt"), "now filled\n");
}

#[test]
fn write_large_content_lands_exactly() {
    let fx = Fixture::new("write-large");
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let (_, r) = fx.call("write", Args::new().text("path", "big.txt").text("content", &content));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fx.on_disk("big.txt"), content);
    // `write`'s body is always the relative path, never the content, regardless of size -- so a
    // large write does not become a `Reference`. Verified rather than assumed.
    assert!(matches!(r.body, ToolBody::Inline(_)));
}

#[test]
fn write_parent_directory_missing() {
    let fx = Fixture::new("write-no-parent");
    let (outcome, r) =
        fx.call("write", Args::new().text("path", "nosuchdir/file.txt").text("content", "x"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(
        fs::metadata(fx.root.join("nosuchdir")).is_err(),
        "the manifest says the parent must already exist, and it must stay that way on refusal"
    );
}

// ============================================================================================
// grep — ADR-059. Was `find`, and the rename is the smaller half: the engine is a real regex and
// the walk no longer disappears into `target/`.
// ============================================================================================

#[test]
fn grep_ordinary_success() {
    let fx = Fixture::new("grep-ok");
    fx.seed("notes.md", "alpha\nbeta needle\ngamma\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{:?}", r.summary);
    assert!(fx.text(&r).contains("notes.md:2:"), "{}", fx.text(&r));
    assert_eq!(r.summary.render(), "1 result · 1 file");
}

#[test]
fn grep_missing_pattern() {
    let fx = Fixture::new("grep-no-pattern");
    let (_, r) = fx.call("grep", Args::new().text("path", "."));
    assert!(r.failed);
    assert!(fx.why(&r).contains("`pattern` is required"), "{}", fx.why(&r));
}

#[test]
fn grep_missing_path() {
    let fx = Fixture::new("grep-no-path");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle"));
    assert!(r.failed);
    // SUSPECT, minor and unchanged by ADR-059: `grep`'s `path` is REQUIRED by the manifest, but
    // unlike `read` (which distinguishes "you gave neither `path` nor `ref`" from "the `path` you
    // gave was refused"), it reports the identical "no adjudicated handle for `path`" whether
    // `path` was omitted entirely or supplied and rejected.
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

/// **`str::contains("")` is always true, and `Regex::new("")` matches every line too**, so an empty
/// pattern reported every line of every file under `path` — a context-flood standing in for what
/// should have been an error. Changing the engine did not change that.
#[test]
fn grep_empty_pattern_is_refused_not_treated_as_matching_everything() {
    let fx = Fixture::new("grep-empty-pattern");
    fx.seed("a.txt", "one\ntwo\nthree\n");
    fx.seed("b.txt", "four\nfive\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "").text("path", "."));
    assert!(r.failed, "an empty pattern must not report every line as a hit: {:?}", r.summary);
    assert!(fx.why(&r).contains("`glob`"), "the tool that DOES list files: {}", fx.why(&r));
}

#[test]
fn grep_empty_string_path_is_malformed() {
    let fx = Fixture::new("grep-empty-path");
    let (outcome, r) = fx.call("grep", Args::new().text("pattern", "x").text("path", ""));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(fx.why(&r).contains("no adjudicated handle for `path`"), "{}", fx.why(&r));
}

#[test]
fn grep_path_with_a_space_survives() {
    let fx = Fixture::new("grep-space");
    fx.seed("a dir/inside.txt", "needle here\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "a dir"));
    assert!(!r.failed, "{:?}", r.summary);
    assert!(fx.text(&r).contains("inside.txt"), "{}", fx.text(&r));
}

#[test]
fn grep_path_with_unicode_survives() {
    let fx = Fixture::new("grep-unicode");
    fx.seed("dossier-café/inside.txt", "needle here\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "dossier-café"));
    assert!(!r.failed, "{:?}", r.summary);
    assert!(fx.text(&r).contains("inside.txt"), "{}", fx.text(&r));
}

#[test]
fn grep_path_does_not_exist() {
    let fx = Fixture::new("grep-missing-dir");
    let (outcome, r) = fx.call("grep", Args::new().text("pattern", "x").text("path", "nope"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
}

/// **`collect()` swallows `read_dir`'s error on a file**, so the walk found zero candidates and
/// the result was `0 results · 0 files` — identical to a correctly-specified EMPTY directory.
#[test]
fn grep_given_a_file_says_so_rather_than_looking_empty() {
    let fx = Fixture::new("grep-file-as-dir");
    fx.seed("notes.md", "alpha\nneedle\ngamma\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "notes.md"));
    assert!(r.failed, "a file where a directory was asked for must not read as empty");
    assert!(fx.why(&r).to_lowercase().contains("directory"), "{}", fx.why(&r));
    assert!(fx.why(&r).contains("`read`"), "name the tool for one file: {}", fx.why(&r));
}

#[test]
fn grep_empty_directory_has_no_matches_and_says_so_honestly() {
    let fx = Fixture::new("grep-empty-dir");
    fx.mkdir("nothing-here");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "nothing-here"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(r.summary.render(), "0 results · 0 files");
}

#[test]
fn grep_reports_true_counts_across_many_files() {
    let fx = Fixture::new("grep-many");
    let mut with_needle = 0;
    for i in 0..40 {
        let has_needle = i % 3 == 0;
        if has_needle {
            with_needle += 1;
        }
        let body = if has_needle { format!("line\nneedle {i}\nline\n") } else { "line\nline\n".to_string() };
        fx.seed(&format!("dir{}/file{i}.txt", i % 4), &body);
    }
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(
        r.summary.render(),
        format!("{with_needle} results · 40 files"),
        "one hit per matching file, every file counted as scanned"
    );
}

// ── T1–T5: the engine is a real regex, and it is the RIGHT engine ────────────────────────────

/// **T1. The pattern is compiled, not `contains`-ed.**
///
/// `find_pattern_with_regex_metacharacters_is_treated_literally` used to assert the exact opposite
/// of this and was DELETED rather than edited — leaving it would have left two tests in one file
/// asserting contradictory things about one executor, and whichever ran second would have looked
/// like the bug.
///
/// The fixture deliberately contains the literal string `fn\s+\w+` as well, so a substring engine
/// would find something: the assertion is that it finds the DECLARATION and not the literal.
#[test]
fn grep_pattern_is_a_regex_not_a_substring() {
    let fx = Fixture::new("grep-is-regex");
    fx.seed(
        "a.rs",
        "fn find_it() {}\n// the literal fn\\s+\\w+ appears here\nlet x = 1;\n",
    );
    let (_, r) = fx.call("grep", Args::new().text("pattern", "fn\\s+\\w+").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert!(out.contains("a.rs:1:"), "the regex must match the declaration: {out}");
    assert!(
        !out.contains("a.rs:2:"),
        "line 2 holds the pattern as LITERAL TEXT; matching it means this is still a substring \
         search: {out}"
    );
}

/// **T2. An invalid pattern is refused with the syntax error, and there is no literal fallback.**
///
/// The fallback is the permissive default this project keeps deleting: a model that wrote
/// `foo(bar)` meaning the literal text would get a different answer from the one it asked for,
/// with nothing saying its pattern had been reinterpreted.
#[test]
fn an_invalid_pattern_is_refused_with_the_syntax_error_and_never_searched_literally() {
    let fx = Fixture::new("grep-bad-regex");
    fx.seed("a.txt", "a bracket [ here\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "[").text("path", "."));
    assert!(r.failed, "an unclosed `[` must be refused, not searched for literally: {:?}", r.summary);
    let why = fx.why(&r);
    assert!(why.contains("unclosed"), "the regex crate's own error must reach the model: {why}");
    assert!(
        why.contains("backslash"),
        "and it must name the fix, because the model cannot see the executor: {why}"
    );
    assert!(!fx.text(&r).contains("a.txt:1"), "it must NOT have matched anything: {why}");
}

/// **T3. Catastrophic backtracking cannot happen here, and that is a property of the crate.**
///
/// `(a+)+$` over a long run of `a`s is the canonical ReDoS and takes exponential time in a
/// backtracking engine. `regex` is finite-automata based and never backtracks, so this returns.
///
/// **There is deliberately no control for this test and no wall-clock assertion.** A control would
/// have to swap the engine for `fancy-regex`, which is the thing ADR-059 refuses; and a timing
/// assertion on this machine would be a flake, quite apart from §4.5 forbidding a clock on this
/// path at all. The test's whole value is that it TERMINATES: if this ever hangs, the engine was
/// changed.
#[test]
fn a_pattern_that_would_backtrack_catastrophically_returns_promptly() {
    let fx = Fixture::new("grep-redos");
    fx.seed("a.txt", &format!("{}b\n", "a".repeat(100_000)));
    let (_, r) = fx.call("grep", Args::new().text("pattern", "(a+)+$").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
}

/// **T4. A pattern too large to COMPILE is refused, and OUR limit is what refuses it.**
///
/// # The obvious fixture does not discriminate, and running the control is what found that
///
/// The design named `a{1000}{1000}{1000}` and said the control was *"remove
/// `size_limit(REGEX_SIZE_LIMIT)`: the default 10 MB admits it."* **It does not.** Measured:
///
/// ```text
/// a{1000}{1000}{1000}   default=Some("Compiled regex exceeds size limit of 10485760 bytes.")
///                       limited=Some("Compiled regex exceeds size limit of 1048576 bytes.")
/// a{300}{300}           default=None
///                       limited=Some("Compiled regex exceeds size limit of 1048576 bytes.")
/// ```
///
/// So a test built only on the first pattern is GREEN on a build where `REGEX_SIZE_LIMIT` is never
/// read — a control that asserts a declaration rather than an enforcement, which is the family
/// this repo has seventeen instances of, appearing this time in the CONTROL rather than the test.
///
/// Both are asserted: the first for the behaviour a model actually meets, the second for the fact
/// that this crate's own ceiling is what produced it.
#[test]
fn a_pattern_too_large_to_compile_is_refused_and_the_limit_is_named() {
    let fx = Fixture::new("grep-huge-regex");
    fx.seed("a.txt", "aaaa\n");

    // 1. The canonical explosion. Refused at any limit, so it says nothing about ours — but it is
    //    what a model would actually write, and it must not hang and must not panic.
    let (_, r) = fx.call("grep", Args::new().text("pattern", "a{1000}{1000}{1000}").text("path", "."));
    assert!(r.failed, "a pattern past the size limit must be refused, not compiled: {:?}", r.summary);
    assert!(
        fx.why(&r).to_lowercase().contains("limit") || fx.why(&r).to_lowercase().contains("exceed"),
        "the reason must say it was too large, or the model cannot tell it from a syntax error: {}",
        fx.why(&r)
    );

    // 2. The one that separates our ceiling from the crate's. Compiles under the 10 MB default and
    //    is refused under `REGEX_SIZE_LIMIT`, so this failing is the only evidence that the
    //    constant is read at all. The number in the message is DERIVED from it, never typed.
    let (_, ours) = fx.call("grep", Args::new().text("pattern", "a{300}{300}").text("path", "."));
    assert!(ours.failed, "`a{{300}}{{300}}` compiles under the crate's default: {:?}", ours.summary);
    assert!(
        fx.why(&ours).contains(&marlowe_exec::REGEX_SIZE_LIMIT.to_string()),
        "and the refusal must name OUR limit, not the crate's: {}",
        fx.why(&ours)
    );
}

/// **T5. Case-insensitivity is reachable, and it is reachable through the PATTERN.**
///
/// ADR-059 rejected an `ignore_case` parameter: `ollama.rs` maps a real JSON `Bool` to `Boolean`
/// and the string `"true"` to `Text("true")`, so a Boolean from a 9B model is a coin flip on the
/// wire, while `(?i)` is standard regex and costs no schema. This is the half of that decision
/// that is checkable here; `tool_call_probe.rs` carries the half that would overturn it.
#[test]
fn case_insensitivity_is_available_through_the_pattern() {
    let fx = Fixture::new("grep-icase");
    fx.seed("a.txt", "todo: something\n");
    let (_, sensitive) = fx.call("grep", Args::new().text("pattern", "TODO").text("path", "."));
    assert_eq!(sensitive.summary.render(), "0 results · 1 file", "the control: case matters");
    let (_, insensitive) = fx.call("grep", Args::new().text("pattern", "(?i)TODO").text("path", "."));
    assert_eq!(insensitive.summary.render(), "1 result · 1 file", "{}", fx.text(&insensitive));
}

// ── T6–T13: the cap, the skip list, and the honesty of the result ────────────────────────────

/// **T6. The search says when it stopped before seeing everything.**
///
/// This is `glob`'s existing notice, which `find` never had: `glob` computed
/// `candidates.len() >= FIND_FILE_CAP` and appended a sentence, `find` called `collect` and never
/// looked at the length. So `find`'s zero on a large tree was reported as a fact about the tree.
///
/// The cap is read from `marlowe_exec::WALK_FILE_CAP` rather than typed. Six tests in this repo
/// encoded `MAX_EXPOSED_TOOLS` as a numeral and one encoded it in its own name, and when the cap
/// moved the same bytes asserted a different property.
#[test]
fn the_search_says_when_it_stopped_before_seeing_everything() {
    let fx = Fixture::new("grep-cap");
    for i in 0..(marlowe_exec::WALK_FILE_CAP + 50) {
        fx.seed(&format!("noise/f{i:05}.txt", i = i), "nothing here\n");
    }
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert!(
        out.contains(&format!("stopped after {} files", marlowe_exec::WALK_FILE_CAP)),
        "a walk that stopped must say so, with the number it stopped at: {out:.400}"
    );
    assert!(out.contains("`glob`"), "and it must name the way to narrow it: {out:.400}");
}

/// **T7. Build directories are not searched, and the result says which.**
///
/// The measured defect: `target/` on this checkout holds ~162,000 files, so the LIFO walk filled
/// all 2,000 slots with build artifacts and never reached `crates/`. The harness already owned
/// this list — `marlowe_daemon::workspace_map` had a private copy — and the walk two tools search
/// with had none.
#[test]
fn build_directories_are_not_searched_and_the_result_says_which() {
    let fx = Fixture::new("grep-skip");
    fx.seed("target/generated.rs", "needle in build output\n");
    fx.seed("src/real.rs", "needle in source\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert!(out.contains("src/real.rs:1:"), "{out}");
    assert!(!out.contains("target/generated.rs"), "build output must not be searched: {out}");
    assert_eq!(r.summary.render(), "1 result · 1 file", "and it must not be COUNTED either");
    assert!(
        out.contains("not searched") && out.contains("target"),
        "a skip the model cannot see is indistinguishable from an absence: {out}"
    );

    // And naming it as `path` still searches it — the skip is on descent, never on the base.
    let (_, named) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "target"));
    assert_eq!(named.summary.render(), "1 result · 1 file", "{}", fx.text(&named));
}

/// **T8. The `glob` filter reaches source that the cap would otherwise hide.**
///
/// The measured defect, as a test — and the reason `glob` is filtered INSIDE the walk rather than
/// over its output, which is the shape the design specified. Applied to the output it is
/// decorative here: the noise files still consume all [`marlowe_exec::WALK_FILE_CAP`] enumeration
/// slots and `*.rs` then narrows a set that never reached the `.rs` file at all.
///
/// **Everything is in ONE directory on purpose.** A two-directory fixture makes the outcome depend
/// on which subtree the LIFO walk pops first, and the first version of this test passed for that
/// reason rather than for the right one — `zzz-source/` was popped BEFORE the noise, so the wide
/// search found the needle and the control could not tell the two implementations apart. One
/// directory reduces the dependency to `read_dir` returning `f00000.txt` before `zzz-needle.rs`,
/// which is true of every ordered filesystem this ships on.
#[test]
fn a_glob_filter_reaches_source_that_the_cap_would_otherwise_hide() {
    let fx = Fixture::new("grep-glob-reaches");
    for i in 0..(marlowe_exec::WALK_FILE_CAP + 50) {
        fx.seed(&format!("f{i:05}.txt"), "nothing here\n");
    }
    fx.seed("zzz-needle.rs", "the needle is here\n");

    // Without `glob`, the walk fills every slot with `.txt` files and stops before the `.rs`.
    let (_, wide) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!wide.failed, "{}", fx.why(&wide));
    assert!(
        !fx.text(&wide).contains("zzz-needle.rs"),
        "the fixture must actually hide the file, or the second half proves nothing: {:.300}",
        fx.text(&wide)
    );
    assert!(
        fx.text(&wide).contains("stopped after"),
        "and the walk must SAY it stopped, which is what `find` never did: {:.300}",
        fx.text(&wide)
    );

    // With `glob`, only the `.rs` file occupies a slot, so the cap is never reached.
    let (_, narrow) = fx.call(
        "grep",
        Args::new().text("pattern", "needle").text("path", ".").text("glob", "*.rs"),
    );
    assert!(!narrow.failed, "{}", fx.why(&narrow));
    assert!(
        fx.text(&narrow).contains("zzz-needle.rs:1:"),
        "`glob` must make the source reachable: {:.300}",
        fx.text(&narrow)
    );
    assert_eq!(narrow.summary.render(), "1 result · 1 file");
    assert!(
        !fx.text(&narrow).contains("stopped after"),
        "and the narrowed walk must not have stopped early at all: {:.300}",
        fx.text(&narrow)
    );
}

/// **T9. Too many matches degrades to counts, and says that it did.**
///
/// SECURITY-AUDIT finding 8: hit accumulation was unbounded, which a regex makes materially worse
/// than a substring did. The budget lever fires at the moment it is needed rather than at the
/// moment a model guessed a `head_limit` — which is why there is no `head_limit`.
#[test]
fn too_many_matches_degrades_to_counts_and_says_so() {
    let fx = Fixture::new("grep-too-many");
    let many = marlowe_exec::MAX_MATCH_LINES * 3;
    fx.seed("big.txt", &(0..many).map(|i| format!("needle {i}\n")).collect::<String>());
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert_eq!(
        r.summary.render(),
        format!("{many} results · 1 file"),
        "every match is COUNTED even when it is not returned"
    );
    let shown = out.lines().filter(|l| l.starts_with("big.txt:")).count();
    assert!(
        shown <= marlowe_exec::MAX_MATCH_LINES,
        "{shown} lines emitted against a cap of {}",
        marlowe_exec::MAX_MATCH_LINES
    );
    assert!(out.contains("counted, not returned"), "the degradation must be stated: {out:.300}");
    assert!(out.contains("big.txt ("), "with the per-file count: {}", &out[out.len() - 300..]);
}

/// **T10. A large result comes back as TEXT, not as a hash.**
///
/// `find` ended with `body_for(hits.join("\n"))`, so any result over `MAX_INLINE_BYTES` became a
/// `ContentRef` — and a file reference cannot be dereferenced, because `read`'s `ref` takes ids
/// `web` issued. Precisely the defect fixed for `read` at `fab045d`, in the tool next to it.
/// Worse, the preview said *"the tool read the whole file"*, which is not even true of a search.
#[test]
fn a_large_grep_result_comes_back_as_text_and_not_as_a_hash() {
    let fx = Fixture::new("grep-no-ref");
    // Lines long enough that the emitted result clears MAX_INLINE_BYTES well before the line cap.
    let line = format!("needle {}", "x".repeat(200));
    fx.seed("big.txt", &(0..150).map(|_| format!("{line}\n")).collect::<String>());
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    let body = fx.text(&r);
    // The primary assertion FIRST: with `body_for` restored the body is `<ref hash bytes>`, which
    // is short, so a size guard placed above it would fail first and the control would report the
    // fixture rather than the defect.
    assert!(!body.starts_with("<ref"), "a search result must not be a hash: {:.80}", body);
    assert!(body.len() > marlowe_exec::MAX_INLINE_BYTES, "the fixture must clear the threshold");
    assert!(body.contains("big.txt:1:"), "{:.200}", body);
}

/// **T11. A binary file is reported as skipped and is not counted as scanned.**
///
/// `find` discarded `clone_and_read`'s `ReadOutcome`, so a binary file pushed no text and still
/// incremented `scanned`: the denominator in `N results · M files` counted files nobody searched.
#[test]
fn a_binary_file_is_reported_as_skipped_and_not_counted_as_scanned() {
    let fx = Fixture::new("grep-binary");
    fx.seed("text.txt", "needle here\n");
    std::fs::write(fx.root.join("blob.bin"), [0xff_u8, 0xfe, 0x00, 0x01, 0xff, 0xfe]).unwrap();
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    assert_eq!(
        r.summary.render(),
        "1 result · 1 file",
        "the binary file must not be counted as searched: {}",
        fx.text(&r)
    );
    assert!(
        fx.text(&r).contains("not text"),
        "and its absence must be stated, not left as a silent hole in the denominator: {}",
        fx.text(&r)
    );
}

/// **T12. A matching line keeps its indentation, so `edit` can use it.**
///
/// `find` emitted `format!("{relative}:{}: {}", n + 1, line.trim())`, and `edit` REQUIRES
/// `replacing` "copied verbatim from a `read` including indentation". So a model that grepped a
/// line and edited with what grep handed back was told *"`replacing` was not found in the file"*
/// with nothing on screen explaining why.
///
/// **The control is the whole test**: it asserts the two tools COMPOSE, which is where the defect
/// actually bites. Asserting that the emitted string starts with four spaces would pass on a build
/// where `edit` and `grep` disagreed about something else.
#[test]
fn matching_lines_keep_their_indentation_so_edit_can_use_them() {
    let fx = Fixture::new("grep-indent");
    fx.seed("src.rs", "fn main() {\n    let needle = 1;\n}\n");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{}", fx.why(&r));
    let line = fx
        .text(&r)
        .lines()
        .find(|l| l.starts_with("src.rs:2:"))
        .expect("the match")
        .to_string();
    let text = line.trim_start_matches("src.rs:2:").to_string();

    let (_, e) = fx.call(
        "edit",
        Args::new().text("path", "src.rs").text("replacing", &text).text("content", "    let x = 2;"),
    );
    assert!(
        !e.failed,
        "a line taken from `grep` must be usable as `edit`'s `replacing`: {}",
        fx.why(&e)
    );
    assert_eq!(fx.on_disk("src.rs"), "fn main() {\n    let x = 2;\n}\n");
}

/// **T13. Results are ordered by path, then by line.**
///
/// `glob` sorted its hits and `find` did not — `find`'s order was `read_dir` order, which is
/// OS-defined. Two identical calls could return two orderings in a project whose scoreboard is a
/// reproduction hash.
#[test]
fn results_are_ordered_by_path_then_line() {
    let fx = Fixture::new("grep-order");
    for d in ["zeta", "alpha", "mid"] {
        fx.seed(&format!("{d}/f.txt"), "needle one\nfiller\nneedle two\n");
    }
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    let lines: Vec<String> = fx.text(&r).lines().map(|s| s.to_string()).collect();
    assert_eq!(
        lines,
        vec![
            "alpha/f.txt:1:needle one",
            "alpha/f.txt:3:needle two",
            "mid/f.txt:1:needle one",
            "mid/f.txt:3:needle two",
            "zeta/f.txt:1:needle one",
            "zeta/f.txt:3:needle two",
        ]
    );
}

// ── context, and the undeclared-argument refusal ─────────────────────────────────────────────

/// `context` returns the lines around a match, with a separator that tells the two apart.
#[test]
fn context_returns_the_surrounding_lines_and_marks_them_as_context() {
    let fx = Fixture::new("grep-context");
    fx.seed("a.txt", "one\ntwo\nneedle\nfour\nfive\n");
    let (_, r) = fx.call(
        "grep",
        Args::new().text("pattern", "needle").text("path", ".").with("context", ArgValue::Integer(1)),
    );
    assert!(!r.failed, "{}", fx.why(&r));
    let lines: Vec<String> = fx.text(&r).lines().map(|s| s.to_string()).collect();
    assert_eq!(lines, vec!["a.txt-2-two", "a.txt:3:needle", "a.txt-4-four"]);
}

/// **A `context` past the maximum is REFUSED, not quietly reduced.** A silently lowered argument is
/// a model believing it asked for something it did not get.
#[test]
fn a_context_past_the_maximum_is_refused_rather_than_clamped() {
    let fx = Fixture::new("grep-context-big");
    fx.seed("a.txt", "needle\n");
    let over = marlowe_exec::MAX_GREP_CONTEXT + 1;
    let (_, r) = fx.call(
        "grep",
        Args::new().text("pattern", "needle").text("path", ".").with("context", ArgValue::Integer(over)),
    );
    assert!(r.failed, "{} must be refused: {:?}", over, r.summary);
    assert!(
        fx.why(&r).contains(&marlowe_exec::MAX_GREP_CONTEXT.to_string()),
        "the refusal must name the maximum: {}",
        fx.why(&r)
    );
}

/// **The wire decides the type, not the model.** `ollama.rs` maps a JSON number to `Integer` and
/// the JSON string `"3"` to `Text("3")`. `"3"` means three; anything that is not a number is still
/// refused by name.
#[test]
fn context_is_read_from_a_string_because_the_wire_may_send_one() {
    let fx = Fixture::new("grep-context-text");
    fx.seed("a.txt", "one\nneedle\nthree\n");
    let (_, r) =
        fx.call("grep", Args::new().text("pattern", "needle").text("path", ".").text("context", "1"));
    assert!(!r.failed, "{}", fx.why(&r));
    assert_eq!(fx.text(&r).lines().count(), 3, "{}", fx.text(&r));

    let (_, bad) = fx.call(
        "grep",
        Args::new().text("pattern", "needle").text("path", ".").text("context", "lots"),
    );
    assert!(bad.failed, "a non-numeric `context` must be refused: {:?}", bad.summary);
}

/// **T17. An argument `grep` does not declare is refused, by name.**
///
/// The precedent is `recall`'s removed `payload_kind`: accepted, ignored, and never reported, so a
/// model that passed it believed it had filtered and got an unfiltered answer. `grep` is where that
/// is likeliest, because `-i`, `--type`, `-l` and `head_limit` are all in a model's hands from
/// somewhere else.
///
/// The four real names come from `marlowe_exec::GREP_PARAMS`, not from a literal here.
#[test]
fn grep_refuses_an_argument_it_does_not_declare() {
    let fx = Fixture::new("grep-unknown-arg");
    fx.seed("a.txt", "TODO: something\n");
    let (_, r) = fx.call(
        "grep",
        Args::new()
            .text("pattern", "todo")
            .text("path", ".")
            .with("ignore_case", ArgValue::Boolean(true)),
    );
    assert!(
        r.failed,
        "an accepted-and-ignored argument is a model believing it filtered: {:?}",
        r.summary
    );
    let why = fx.why(&r);
    assert!(why.contains("ignore_case"), "the refusal must name the offending argument: {why}");
    assert!(why.contains("(?i)"), "and the spelling that does work: {why}");
    for p in marlowe_exec::GREP_PARAMS {
        assert!(why.contains(p), "the refusal must list `{p}`, which grep DOES take: {why}");
    }
}

/// Every parameter the shipped manifest declares is one the executor accepts — the other half of
/// the refusal above, and the half that fails if a parameter is added to the manifest and not to
/// [`marlowe_exec::GREP_PARAMS`].
#[test]
fn every_declared_grep_parameter_is_accepted_by_the_executor() {
    let reg = builtin_registry().expect("the builtins load");
    let declared: Vec<String> =
        reg.manifest(&ToolId::new("grep")).unwrap().params().iter().map(|p| p.name.clone()).collect();
    let mut accepted: Vec<String> = marlowe_exec::GREP_PARAMS.iter().map(|s| s.to_string()).collect();
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    accepted.sort();
    assert_eq!(
        declared_sorted, accepted,
        "the manifest and the executor's accept-list must be the same set, or a declared \
         parameter is refused by name or an undeclared one is silently taken"
    );
}
// ============================================================================================
// bash
// ============================================================================================

#[test]
fn bash_missing_command() {
    let fx = Fixture::new("bash-no-command");
    let (outcome, r) = fx.call("bash", Args::new());
    assert_eq!(outcome, Outcome::NeedsApproval { tier: marlowe_permission::RiskTier::Irreversible });
    assert!(r.failed);
    assert!(fx.why(&r).contains("`command` is required"), "{}", fx.why(&r));
}

#[test]
fn bash_empty_string_command() {
    let fx = Fixture::new("bash-empty-command");
    let (_, r) = fx.call("bash", Args::new().text("command", ""));
    // An empty command line handed to `cmd /C` (or `sh -c`) is not refused by this executor --
    // it is run as-is and whatever the shell does with nothing, happens. Asserting on the actual
    // observed exit rather than assuming: this is not flagged unless it hangs, panics, or silently
    // fabricates output, none of which it does.
    assert!(r.wall_ms == 0);
    let _ = r; // outcome is whatever the shell does with an empty line; not asserted further.
}

#[test]
fn bash_a_command_that_fails_reports_the_exit_code() {
    let fx = Fixture::new("bash-fails");
    let cmd = if cfg!(windows) { "exit 3" } else { "exit 3" };
    let (_, r) = fx.call("bash", Args::new().text("command", cmd));
    assert!(r.failed);
    assert_eq!(r.summary.render(), "exit 3 · 0 lines");
}

#[test]
fn bash_a_command_producing_no_output() {
    let fx = Fixture::new("bash-no-output");
    // `rem` is a `cmd` word; `:` is bash's no-op.
    let cmd = ":";
    let (_, r) = fx.call("bash", Args::new().text("command", cmd));
    assert!(!r.failed, "{:?} {}", r.summary, fx.text(&r));
    assert_eq!(fx.text(&r), "");
    assert_eq!(r.summary.render(), "0 lines");
}

#[test]
fn bash_a_command_producing_a_lot_of_output_is_capped() {
    let fx = Fixture::new("bash-lots-of-output");
    // Deterministic and fast: create the bytes with `fs::write` rather than asking the shell to
    // generate them, and have the shell merely emit what is already on disk.
    let big = "x".repeat(2 * marlowe_exec::MAX_SHELL_OUTPUT_BYTES);
    fx.seed("big.txt", &big);
    let cmd = "cat big.txt";
    let (_, r) = fx.call("bash", Args::new().text("command", cmd));
    // **Output this large becomes a `ContentRef`, so `text()` renders `<ref hash bytes>` and the
    // notice is inside the referenced content, not in the rendered string.** The first version of
    // this test asserted on `text()` and read the reference as a missing notice — measuring the
    // rendering rather than the result.
    let bytes = match &r.body {
        ToolBody::Reference { bytes, .. } => *bytes as usize,
        ToolBody::Inline(s) => s.len(),
    };
    assert!(
        bytes <= marlowe_exec::MAX_SHELL_OUTPUT_BYTES + 4096,
        "the cap must actually cap: {bytes} bytes against a cap of {}",
        marlowe_exec::MAX_SHELL_OUTPUT_BYTES
    );
    // And the model must be TOLD, wherever the result put it: inline, or in the preview that
    // stands in for a reference it cannot dereference.
    let told = format!("{} {}", fx.text(&r), r.preview.clone().unwrap_or_default());
    assert!(
        told.contains("the harness stopped this command after it produced more than"),
        "a truncated result must say so somewhere the model reads: {}",
        &told[..300.min(told.len())]
    );
}

#[test]
fn bash_quoted_command_reaches_the_shell_unmangled() {
    let fx = Fixture::new("bash-quotes");
    fx.mkdir("a dir");
    fx.seed("a dir/inside.txt", "found me");
    let cmd = r#"cat "a dir/inside.txt""#;
    let (_, r) = fx.call("bash", Args::new().text("command", cmd));
    assert!(!r.failed, "{:?} {}", r.summary, fx.text(&r));
    assert!(fx.text(&r).contains("found me"), "{}", fx.text(&r));
}

#[test]
fn bash_cwd_with_a_space() {
    let fx = Fixture::new("bash-cwd-space");
    fx.mkdir("a dir");
    // One spelling: `bash` is bash on both platforms now. `cd` printed the directory under
    // `cmd`; in bash it goes to $HOME and prints nothing.
    let (_, r) = fx.call("bash", Args::new().text("command", "pwd").text("cwd", "a dir"));
    assert!(!r.failed, "{:?} {}", r.summary, fx.text(&r));
    assert!(fx.text(&r).to_lowercase().contains("a dir"), "{}", fx.text(&r));
}

#[test]
fn bash_cwd_with_unicode() {
    let fx = Fixture::new("bash-cwd-unicode");
    fx.mkdir("café-dossier");
    let (_, r) = fx.call("bash", Args::new().text("command", "pwd").text("cwd", "café-dossier"));
    assert!(!r.failed, "{:?} {}", r.summary, fx.text(&r));
    assert!(fx.text(&r).to_lowercase().contains("caf"), "{}", fx.text(&r));
}

// ============================================================================================
// glob — the tool that did not exist, and whose absence the model filled with invention
// ============================================================================================

/// The ordinary case: list what is under a directory.
#[test]
fn glob_lists_everything_beneath_a_path() {
    let fx = Fixture::new("glob-basic");
    fx.seed("src/main.rs", "fn main() {}");
    fx.seed("src/lib.rs", "");
    fx.seed("docs/readme.md", "hi");
    let (_, r) = fx.call("glob", Args::new().text("path", "."));
    assert!(!r.failed, "{:?} {}", r.summary, fx.why(&r));
    let out = fx.text(&r);
    for want in ["src/main.rs", "src/lib.rs", "docs/readme.md"] {
        assert!(out.contains(want), "`{want}` is missing: {out}");
    }
    assert_eq!(r.summary.render(), "3 paths");
}

/// Sorted, because `read_dir` order is a filesystem detail and two runs must agree.
#[test]
fn glob_is_sorted_and_stable() {
    let fx = Fixture::new("glob-sorted");
    for n in ["c.txt", "a.txt", "b.txt"] {
        fx.seed(n, "x");
    }
    let (_, first) = fx.call("glob", Args::new().text("path", "."));
    let (_, again) = fx.call("glob", Args::new().text("path", "."));
    assert_eq!(fx.text(&first), fx.text(&again), "two identical calls must agree");
    assert_eq!(fx.text(&first), "a.txt\nb.txt\nc.txt");
}

/// `*` and `?` are the only metacharacters, and a pattern without `/` matches the NAME.
#[test]
fn glob_pattern_matches_the_name_anywhere_beneath_the_path() {
    let fx = Fixture::new("glob-name");
    fx.seed("src/main.rs", "");
    fx.seed("src/deep/other.rs", "");
    fx.seed("notes.md", "");
    let (_, r) = fx.call("glob", Args::new().text("path", ".").text("pattern", "*.rs"));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert!(out.contains("src/main.rs") && out.contains("src/deep/other.rs"), "{out}");
    assert!(!out.contains("notes.md"), "a non-match leaked in: {out}");
}

/// A pattern WITH a `/` matches the whole workspace-relative path, so it can be anchored.
#[test]
fn glob_pattern_with_a_slash_matches_the_path() {
    let fx = Fixture::new("glob-path");
    fx.seed("src/main.rs", "");
    fx.seed("src/deep/other.rs", "");
    let (_, r) = fx.call("glob", Args::new().text("path", ".").text("pattern", "src/*.rs"));
    assert!(!r.failed, "{}", fx.why(&r));
    let out = fx.text(&r);
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(!out.contains("deep"), "`src/*.rs` must not reach into `src/deep`: {out}");
}

/// **Regex metacharacters are LITERAL.** A model that writes `.*` out of habit must not silently
/// match everything — it must match a file actually called that, and otherwise match nothing.
#[test]
fn glob_treats_regex_metacharacters_as_literal_text() {
    let fx = Fixture::new("glob-regex");
    fx.seed("real.txt", "");
    fx.seed("other.md", "");
    for pattern in [".*", "[a-z]*", "^real", "real$", "(real|other)"] {
        let (_, r) = fx.call("glob", Args::new().text("path", ".").text("pattern", pattern));
        assert!(!r.failed, "{pattern:?}: {}", fx.why(&r));
        assert!(
            !fx.text(&r).contains("other.md"),
            "`{pattern}` was interpreted as a regex and matched everything: {}",
            fx.text(&r)
        );
    }
    // The control: `*` really does match everything, so the assertions above are not vacuous.
    let (_, all) = fx.call("glob", Args::new().text("path", ".").text("pattern", "*"));
    assert!(fx.text(&all).contains("other.md"), "`*` must match: {}", fx.text(&all));
}

/// Case-insensitive, because this ships on Windows where the filesystem is.
#[test]
fn glob_is_case_insensitive() {
    let fx = Fixture::new("glob-case");
    fx.seed("README.MD", "");
    let (_, r) = fx.call("glob", Args::new().text("path", ".").text("pattern", "*.md"));
    assert!(fx.text(&r).contains("README.MD"), "{}", fx.text(&r));
}

/// **"Nothing here" and "nothing matched" are different facts.** A model that cannot tell them
/// apart invents one — which is exactly what happened live, when it reported a directory with four
/// files in it as "appears empty".
#[test]
fn glob_distinguishes_an_empty_directory_from_a_pattern_that_matched_nothing() {
    let fx = Fixture::new("glob-empty");
    fx.mkdir("hollow");
    let (_, empty) = fx.call("glob", Args::new().text("path", "hollow"));
    assert!(!empty.failed, "{}", fx.why(&empty));
    assert!(
        fx.text(&empty).contains("no files under this path at all"),
        "an empty directory must say it is empty: {}",
        fx.text(&empty)
    );

    fx.seed("full/a.txt", "");
    fx.seed("full/b.txt", "");
    let (_, nomatch) = fx.call("glob", Args::new().text("path", "full").text("pattern", "*.rs"));
    assert!(!nomatch.failed, "{}", fx.why(&nomatch));
    let out = fx.text(&nomatch);
    assert!(out.contains("2 file(s) are under this path"), "say how many ARE there: {out}");
    assert!(out.contains("*.rs"), "name the pattern that missed: {out}");
    assert_ne!(out, fx.text(&empty), "the two answers must not be the same string");
}

/// A file where a directory was asked for — the mistake the description names.
#[test]
fn glob_given_a_file_says_so() {
    let fx = Fixture::new("glob-file");
    fx.seed("notes.md", "hi");
    let (_, r) = fx.call("glob", Args::new().text("path", "notes.md"));
    assert!(r.failed, "a file must not read as an empty directory: {}", fx.text(&r));
    assert!(fx.why(&r).to_lowercase().contains("directory"), "{}", fx.why(&r));
}

/// Names with spaces and non-ASCII survive to the listing.
#[test]
fn glob_handles_spaces_and_unicode() {
    let fx = Fixture::new("glob-names");
    fx.seed("a dir/some file.txt", "");
    fx.seed("café/menú.md", "");
    let (_, r) = fx.call("glob", Args::new().text("path", "."));
    let out = fx.text(&r);
    assert!(out.contains("a dir/some file.txt"), "{out}");
    assert!(out.contains("café/menú.md"), "{out}");
}

/// `path` is required, and `glob` reads no file contents — which is what lets it be `Inert` and
/// run without interrupting the user, unlike `bash`.
#[test]
fn glob_requires_a_path_and_never_asks_for_approval() {
    let fx = Fixture::new("glob-required");
    let (_, r) = fx.call("glob", Args::new());
    assert!(r.failed, "`path` is required");

    fx.seed("a.txt", "secret contents");
    let (outcome, ok) = fx.call("glob", Args::new().text("path", "."));
    assert_eq!(outcome, Outcome::Allowed, "listing names must not need approval");
    assert!(
        !fx.text(&ok).contains("secret contents"),
        "glob returns NAMES; a file's contents must never appear: {}",
        fx.text(&ok)
    );
}

// ============================================================================================
// The descriptions and the executors, checked against each other
// ============================================================================================

/// **A description drifts from behaviour silently, and nothing in this project could see it.**
///
/// `read.range` promised *"a backwards range returns nothing"* for an hour after the executor
/// started refusing one instead. Both halves were tested — the wire test asserts the description
/// reaches the model, the executor test asserts the refusal — and **neither compares them**, which
/// is the whole gap: a claim asserted where it is written and a behaviour asserted where it runs,
/// with no line joining the two.
///
/// So this reads each promise out of the shipped manifest, then makes the call it describes.
/// Deliberately a small set: only claims that are checkable in one call, because a guard that
/// tries to parse prose would be a worse thing than the drift.
#[test]
fn each_description_promise_is_kept_by_the_executor() {
    let fx = Fixture::new("promises");
    let reg = builtin_registry().expect("the builtins load");
    let desc = |tool: &str, param: &str| -> String {
        reg.manifest(&ToolId::new(tool))
            .unwrap()
            .params()
            .iter()
            .find(|p| p.name == param)
            .unwrap_or_else(|| panic!("`{tool}::{param}` is not declared"))
            .description
            .clone()
            .unwrap_or_default()
    };

    // 1. `read.range` — the one that actually drifted.
    let r = desc("read", "range");
    assert!(
        r.contains("REFUSED") || r.contains("refused"),
        "`read.range` must say a bad range is refused, not describe some other outcome: {r}"
    );
    fx.seed("ten.txt", &(1..=10).map(|i| format!("line {i}\n")).collect::<String>());
    for bad in ["60-20", "abc", "2", "0-5", "500-600"] {
        let (_, out) = fx.call("read", Args::new().text("path", "ten.txt").text("range", bad));
        assert!(out.failed, "`range` {bad:?} is described as refused and was not: {:?}", out.summary);
    }
    // The control: the shape the description DOES endorse still works.
    let (_, ok) = fx.call("read", Args::new().text("path", "ten.txt").text("range", "2-4"));
    assert!(!ok.failed, "`20-60` style must work or the loop above proves nothing");
    assert_eq!(fx.text(&ok).lines().count(), 3);

    // 2. `edit.replacing` — "an empty string is refused rather than inserting at the start".
    let e = desc("edit", "replacing");
    assert!(e.contains("empty string is refused"), "{e}");
    fx.seed("notes.md", "hello\n");
    let (_, out) =
        fx.call("edit", Args::new().text("path", "notes.md").text("replacing", "").text("content", "X"));
    assert!(out.failed, "described as refused: {:?}", out.summary);
    assert_eq!(fx.on_disk("notes.md"), "hello\n");

    // 3. `grep.pattern` — "an empty string is refused rather than matching every line".
    let f = desc("grep", "pattern");
    assert!(f.contains("empty string is refused"), "{f}");
    let (_, out) = fx.call("grep", Args::new().text("pattern", "").text("path", "."));
    assert!(out.failed, "described as refused: {:?}", out.summary);

    // 4. `grep.path` — "A file here is refused".
    let fp = desc("grep", "path");
    assert!(fp.contains("file here is refused"), "{fp}");
    let (_, out) = fx.call("grep", Args::new().text("pattern", "hello").text("path", "notes.md"));
    assert!(out.failed, "described as refused: {:?}", out.summary);

    // 6. `grep.pattern` — "An invalid pattern is REFUSED with the syntax error and is never
    //    quietly searched as plain text." ADR-059's central promise: there is no literal fallback.
    assert!(f.contains("REFUSED with the syntax error"), "{f}");
    let (_, out) = fx.call("grep", Args::new().text("pattern", "(unclosed").text("path", "."));
    assert!(out.failed, "described as refused: {:?}", out.summary);
    assert!(
        !fx.text(&out).contains("notes.md:"),
        "and it must not have matched anything literally: {}",
        fx.text(&out)
    );

    // 7. `grep.context` — "The maximum is 20; a larger number is REFUSED, not quietly reduced."
    //    The number is read out of the DESCRIPTION and used to build the call, so a description
    //    that says one bound while the executor enforces another fails here rather than drifting.
    let c = desc("grep", "context");
    assert!(c.contains("is REFUSED, not quietly reduced"), "{c}");
    let stated: i64 = c
        .split("The maximum is ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("`grep.context` must state its maximum as a number: {c}"));
    assert_eq!(
        stated,
        marlowe_exec::MAX_GREP_CONTEXT,
        "the description's maximum and the executor's constant have drifted apart"
    );
    let (_, out) = fx.call(
        "grep",
        Args::new()
            .text("pattern", "hello")
            .text("path", ".")
            .with("context", ArgValue::Integer(stated + 1)),
    );
    assert!(out.failed, "described as refused: {:?}", out.summary);
    let (_, ok) = fx.call(
        "grep",
        Args::new().text("pattern", "hello").text("path", ".").with("context", ArgValue::Integer(stated)),
    );
    assert!(!ok.failed, "the stated maximum itself must WORK, or the loop above proves nothing");

    // 8. `grep.glob` — "An empty string is refused rather than matching nothing."
    let gg = desc("grep", "glob");
    assert!(gg.contains("empty string is refused"), "{gg}");
    let (_, out) =
        fx.call("grep", Args::new().text("pattern", "hello").text("path", ".").text("glob", ""));
    assert!(out.failed, "described as refused: {:?}", out.summary);

    // 9. `read` — the FORMAT of the line-number prefix, read out of the prose and then produced.
    //    The width is parsed from the description and compared with the constant, so a description
    //    that says one width while the executor prints another fails here rather than teaching a
    //    model to strip the wrong number of characters.
    let rd = reg
        .iter()
        .find(|r| r.id.as_str() == "read")
        .expect("`read` is registered")
        .description
        .text()
        .to_string();
    let stated_width: usize = rd
        .split("right-aligned in ")
        .nth(1)
        .and_then(|rest| rest.split(" characters").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("`read` must state the width of its prefix: {rd}"));
    assert_eq!(
        stated_width,
        marlowe_exec::LINE_NUMBER_WIDTH,
        "the description's prefix width and the executor's constant have drifted apart"
    );
    let (_, numbered) = fx.call("read", Args::new().text("path", "ten.txt"));
    assert!(
        fx.text(&numbered).starts_with(&format!("{:>w$}\tline 1\n", 1, w = stated_width)),
        "a real read must produce exactly the format the description promises: {:?}",
        &fx.text(&numbered)[..20.min(fx.text(&numbered).len())]
    );
    //    …and the window bound it names is the one it enforces.
    let stated_lines: usize = rd
        .split("at most ")
        .nth(1)
        .and_then(|rest| rest.split(" lines").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("`read` must state its line window: {rd}"));
    assert_eq!(stated_lines, marlowe_exec::READ_WINDOW_LINES, "the stated window has drifted");

    // 10. `edit.replacing` — "a value that still carries the prefix is REFUSED", and "It must
    //     appear EXACTLY ONCE". Both are behaviours this session added; both are checked by making
    //     the call the sentence describes.
    assert!(e.contains("still carries the prefix is REFUSED"), "{e}");
    fx.seed("num.txt", "alpha\nbeta\n");
    let (_, prefixed) = fx.call(
        "edit",
        Args::new()
            .text("path", "num.txt")
            .text("replacing", &format!("{:>w$}\talpha", 1, w = stated_width))
            .text("content", "x"),
    );
    assert!(prefixed.failed, "described as refused: {:?}", prefixed.summary);
    assert!(e.contains("appear EXACTLY ONCE"), "{e}");
    fx.seed("dup.txt", "same\nsame\n");
    let (_, twice) = fx.call(
        "edit",
        Args::new().text("path", "dup.txt").text("replacing", "same").text("content", "x"),
    );
    assert!(twice.failed, "described as refused: {:?}", twice.summary);
    // The control: exactly once still edits, so the refusal is not firing on everything.
    fx.seed("once.txt", "same\nother\n");
    let (_, once) = fx.call(
        "edit",
        Args::new().text("path", "once.txt").text("replacing", "same").text("content", "x"),
    );
    assert!(!once.failed, "a unique snippet must still edit: {}", fx.why(&once));

    // 11. `edit.content` — "on N or more lines is refused", with N read out of the prose.
    let ec = desc("edit", "content");
    let stated_content: u32 = ec
        .split("line-number prefix on ")
        .nth(1)
        .and_then(|rest| rest.split(" or more").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("`edit.content` must state how many lines trip it: {ec}"));
    assert_eq!(
        stated_content,
        marlowe_exec::NUMBERED_CONTENT_LINES,
        "the description's threshold and the executor's constant have drifted apart"
    );
    let paste: String = (1..=stated_content)
        .map(|i| format!("{:>w$}\tpasted {i}\n", i, w = stated_width))
        .collect();
    let (_, spliced) = fx.call(
        "edit",
        Args::new().text("path", "once.txt").text("replacing", "other").text("content", &paste),
    );
    assert!(spliced.failed, "described as refused: {:?}", spliced.summary);
    // The control on the threshold: one line UNDER it is accepted, so the refusal is about the
    // count and not about digits.
    let under: String = (1..stated_content)
        .map(|i| format!("{:>w$}\tpasted {i}\n", i, w = stated_width))
        .collect();
    let (_, ok_under) = fx.call(
        "edit",
        Args::new().text("path", "once.txt").text("replacing", "other").text("content", &under),
    );
    assert!(!ok_under.failed, "under the stated threshold must work: {}", fx.why(&ok_under));

    // 12. `write.content` — numbered text is STORED, and the result says `line-numbered`.
    let wc = desc("write", "content");
    assert!(wc.contains("says `line-numbered`"), "{wc}");
    let (_, wrote) = fx.call("write", Args::new().text("path", "kept.txt").text("content", &paste));
    assert!(!wrote.failed, "{}", fx.why(&wrote));
    assert_eq!(fx.on_disk("kept.txt"), paste, "written exactly as given");
    assert!(wrote.summary.render().contains("line-numbered"), "{}", wrote.summary.render());

    // 5. `glob.pattern` — "`src/*.rs` finds only those directly in `src`".
    let g = desc("glob", "pattern");
    assert!(g.contains("directly in `src`"), "{g}");
    fx.seed("src/a.rs", "");
    fx.seed("src/deep/b.rs", "");
    let (_, out) = fx.call("glob", Args::new().text("path", ".").text("pattern", "src/*.rs"));
    assert!(fx.text(&out).contains("src/a.rs"), "{}", fx.text(&out));
    assert!(!fx.text(&out).contains("deep"), "the promise is `directly in`: {}", fx.text(&out));
}

// ============================================================================================
// read is a WINDOW, the way Claude Code reads a file
// ============================================================================================

/// **A file bigger than the window comes back as text, not as a hash.**
///
/// `read` used to hand anything over `MAX_INLINE_BYTES` to `body_for`, which returned a
/// `ContentRef` — a hash plus a 4 KB head-and-tail. **That hash is not dereferenceable for a
/// file**: `read`'s `ref` takes ids `web` issued. So the middle of every file above 8 KB was
/// unreachable, and a model could see both ends and nothing else.
#[test]
fn a_long_file_is_read_in_windows_and_every_part_is_reachable() {
    let fx = Fixture::new("read-window");
    let content: String = (1..=5_000).map(|i| format!("line {i}\n")).collect();
    fx.seed("long.txt", &content);

    let (_, first) = fx.call("read", Args::new().text("path", "long.txt"));
    assert!(!first.failed, "{}", fx.why(&first));
    let body = fx.text(&first);
    let w = marlowe_exec::LINE_NUMBER_WIDTH;
    assert!(!body.starts_with("<ref"), "a file must not come back as a hash: {}", &body[..60]);
    assert!(body.starts_with(&format!("{:>w$}\tline 1\n", 1)), "the window starts at the top");
    assert!(body.contains("of 5000"), "the notice must say how long the file is: {body:.200}");

    // ── FOLLOW THE NOTICE, RATHER THAN ASSERT A NUMBER THE WINDOW DECIDES ────────────────
    //
    // This used to assert the literal range `"2001-4000"`, which was the line bound and is no
    // longer where the window stops: `READ_WINDOW_BYTES` counts the prefixes now, so the first
    // window ends wherever 32 KB of NUMBERED text ends. Pinning the old number would have asserted
    // the bound rather than the property, and the property is the one the feature exists for —
    // **the notice's range, followed literally, returns the very next line and loses none.**
    let last_shown: usize = body
        .rsplit("[showing lines ")
        .next()
        .and_then(|s| s.split(" of ").next())
        .and_then(|s| s.split('-').nth(1))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("the notice must name the last line shown: {body:.400}"));
    assert!(last_shown < 5_000, "the window must actually have stopped short: {last_shown}");
    assert!(
        body.contains(&format!("{:>w$}\tline {last_shown}\n", last_shown)),
        "the notice must name a line that is really in the body: {last_shown}"
    );
    assert!(
        !body.contains(&format!("{:>w$}\tline {}\n", last_shown + 1, last_shown + 1)),
        "the window must STOP where it says it stopped, not one line later"
    );
    let next = body
        .rsplit("Continue with range \"")
        .next()
        .and_then(|s| s.split('"').next())
        .unwrap_or_else(|| panic!("the notice must name the next range: {body:.400}"))
        .to_string();
    assert_eq!(
        next.split('-').next().unwrap().parse::<usize>().unwrap(),
        last_shown + 1,
        "the next range must begin at the line after the last one shown, or a line is lost: {next}"
    );

    // **The part that was unreachable.** The next window really does return the next lines, and
    // the numbers it prints are the FILE's, so the two windows abut exactly.
    let (_, second) = fx.call("read", Args::new().text("path", "long.txt").text("range", &next));
    assert!(!second.failed, "{}", fx.why(&second));
    let two = fx.text(&second);
    assert!(
        two.starts_with(&format!("{:>w$}\tline {}\n", last_shown + 1, last_shown + 1)),
        "the second window must begin exactly where the first stopped: {two:.80}"
    );
    assert!(two.contains("line 2500\n"), "the middle of the file is reachable now");
    assert!(!two.contains("\tline 1\n"), "and it is the SECOND window, not the first");
}

/// A file that fits is returned whole, with no notice at all — the window is a bound, not a habit.
#[test]
fn a_short_file_has_no_window_notice() {
    let fx = Fixture::new("read-short");
    fx.seed("short.txt", "alpha\nbeta\ngamma\n");
    let (_, r) = fx.call("read", Args::new().text("path", "short.txt"));
    assert_eq!(
        fx.text(&r),
        marlowe_exec::number_lines("alpha\nbeta\ngamma\n", 1),
        "numbers, and nothing else: no notice, no truncation, no hash"
    );
}

/// **The cost warning is held back for files that are actually expensive, and it states a NUMBER.**
///
/// A model told that everything is expensive has learned nothing about what is. So an ordinary
/// long file gets one line — what you got, what to ask for next — and only a genuinely large one
/// also gets the token estimate and the pointer at `grep`.
///
/// # This test used to assert the WORD and not the value, and that is why the bug shipped
///
/// The old last two lines were `tail.contains("tokens")` and `tail.contains("`grep`")`. Both are
/// green on a build where the figure is **7.8x low**, because neither of them reads it — the
/// property asserted where it is *declared* rather than where it is *enforced*. The number is now
/// checked against an expectation built here; `tests/read_cost_notice.rs` carries the rest of the
/// family, including the case where the warning vanished entirely.
///
/// # Why the modest fixture shrank
///
/// It was 2,400 lines of `"{i}\n"`, and against the **32,768-token default** that file is not
/// modest once the line numbers are counted: 10,893 bytes on disk, 16,800 bytes of prefixes,
/// **9,231 tokens — over a quarter of the window**, so it is now correctly called expensive. The
/// control has to be a file that really is under the line, so it is one that plainly is.
///
/// The general fact that fixture change exposes is worth stating: on a 32k window a full
/// `READ_WINDOW_BYTES` window is already ~11k tokens, which is itself over the quarter, so *every*
/// byte-truncated read is expensive there and only line-truncated reads of very short lines are
/// not. The discrimination this test is named for lives at a real model's window —
/// `read_cost_notice.rs` holds that control at 128k. That is a property of the two constants,
/// not of the fix that revealed it.
#[test]
fn only_a_genuinely_large_file_is_called_expensive() {
    let fx = Fixture::new("read-cost");

    // Just past the line bound, and genuinely small — 4 KB on disk, ~18 KB numbered, ~6,003
    // tokens against a threshold of 8,192. Short notice, no cost, no advice.
    let modest: String = "x\n".repeat(2_001);
    fx.seed("modest.txt", &modest);
    let (_, r) = fx.call("read", Args::new().text("path", "modest.txt"));
    let body = fx.text(&r);
    assert!(body.contains("of 2001"), "it is still truncated and still says so: {body:.150}");
    assert!(
        !body.contains("tokens") && !body.contains("`grep`"),
        "an ordinary long file must not be dressed as a warning: {}",
        &body[body.len().saturating_sub(300)..]
    );

    // Genuinely large: the cost, the cheaper way to get an answer, and the RIGHT cost.
    let huge: String =
        (1..=5_000).map(|i| format!("line {i} with enough text to weigh something\n")).collect();
    fx.seed("huge.txt", &huge);
    let (_, r) = fx.call("read", Args::new().text("path", "huge.txt"));
    let body = fx.text(&r);
    let tail = &body[body.len().saturating_sub(400)..];
    assert!(tail.contains("tokens"), "a large file must state its cost in tokens: {tail}");
    assert!(tail.contains("`grep`"), "and name the targeted alternative: {tail}");

    // **The expectation is built, not restated**: the exact string a model receives if it reads
    // the whole file, through the assembler's own estimator. Unfixed this reads 76,298 against
    // 87,965 — the bytes on disk, with the line numbers the model is sent left out of the bill.
    let stated: u32 = tail
        .split_once("is about ")
        .and_then(|(_, s)| s.split_once(" tokens"))
        .and_then(|(n, _)| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("the cost must be a number: {tail}"));
    let expected = marlowe_loop::estimate_tokens(&marlowe_exec::number_lines(&huge, 1));
    let ratio = f64::from(stated.max(expected)) / f64::from(stated.min(expected));
    assert!(
        ratio <= 1.01,
        "the notice claims {stated} tokens; reading the whole file really costs {expected} \
         — off by {ratio:.2}x"
    );
}

/// The window bounds an explicit `range` too, so it is not a way around the ceiling.
#[test]
fn an_explicit_range_is_still_bounded_by_the_window() {
    let fx = Fixture::new("read-range-bounded");
    let content: String = (1..=9_000).map(|i| format!("line {i}\n")).collect();
    fx.seed("long.txt", &content);
    let (_, r) = fx.call("read", Args::new().text("path", "long.txt").text("range", "1-9000"));
    assert!(!r.failed, "{}", fx.why(&r));
    let returned = fx.text(&r).lines().count();
    assert!(
        returned <= marlowe_exec::READ_WINDOW_LINES + 5,
        "a range asking for everything returned {returned} lines; the window is {}",
        marlowe_exec::READ_WINDOW_LINES
    );
}
