//! The four executors, driven through a **real adjudication** rather than a hand-built handle.
//!
//! That matters: the thing under test is not "can this code read a file" but "does the executor
//! use the handle the permission layer opened, and only that handle". A test that constructed a
//! `ScopedPath` itself would be testing the reading and not the wall — and it could not, because
//! `ScopedPath` has no constructor from a string.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{ToolBody, ToolHost};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{
    Adjudication, Adjudicator, Args, EgressPolicy, Outcome, Request, TaintSet, Tier,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, ToolRegistry, BUILTIN_TOOLS};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    registry: ToolRegistry,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("marlowe-exec-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        fs::write(root.join("notes.md"), "alpha\nbeta needle\ngamma\n").unwrap();
        Self { root, registry: builtin_registry().unwrap() }
    }

    /// Adjudicate for real, then hand the result to the executor. Everything a tool touches
    /// therefore came through the wall.
    fn call(&self, tool: &str, args: Args) -> (Outcome, marlowe_loop::ToolOutcome) {
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

    fn text(&self, r: &marlowe_loop::ToolOutcome) -> String {
        match &r.body {
            ToolBody::Inline(s) => s.clone(),
            ToolBody::Reference { hash, bytes } => format!("<ref {hash} {bytes}>"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn read_returns_the_file_through_the_adjudicated_handle() {
    let fx = Fixture::new("read");
    let (_, r) = fx.call("read", Args::new().text("path", "src/main.rs"));
    assert!(!r.failed, "{:?}", r.summary);
    assert!(fx.text(&r).contains("println!"));
    // The body is `cat -n`, so the byte count is the file plus one prefix per line. Derived from
    // the producer rather than typed: a `LINE_NUMBER_WIDTH` that moves must move this number too.
    let numbered = marlowe_exec::number_lines(&fs::read_to_string(fx.root.join("src/main.rs")).unwrap(), 1);
    assert_eq!(fx.text(&r), numbered);
    assert_eq!(r.summary.render(), format!("3 lines · {} B", numbered.len()));
}

/// **Two tools, one mode each.** `edit` patches an existing file; `write` creates or replaces one.
/// They were a single tool separated by an OPTIONAL parameter, so a model holding a request and a
/// schema had to infer which mode it was in — and inferred wrong.
#[test]
fn edit_patches_through_the_handle_and_write_creates() {
    let fx = Fixture::new("edit");

    // `edit`: one snippet, the rest untouched.
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "beta needle").text("content", "beta found"),
    );
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fs::read_to_string(fx.root.join("notes.md")).unwrap(), "alpha\nbeta found\ngamma\n");

    // `edit` without `replacing` no longer means "overwrite". It means nothing, and the refusal
    // names the tool that would have worked.
    let (_, r) = fx.call("edit", Args::new().text("path", "notes.md").text("content", "whatever"));
    assert!(r.failed, "`edit` has no whole-file mode any more: {:?}", r.summary);
    let d = r.summary.detail.clone().unwrap_or_default();
    assert!(
        d.contains("`write`"),
        "a refusal must name the tool that would have worked, or the model guesses again: {d:?}"
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("notes.md")).unwrap(),
        "alpha\nbeta found\ngamma\n",
        "the refused call must not have touched the file"
    );

    // `write`: creates. The parent must exist -- `write` creates the file, not the tree.
    fs::create_dir_all(fx.root.join("out")).unwrap();
    let (_, r) = fx.call("write", Args::new().text("path", "out/new.txt").text("content", "made"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fs::read_to_string(fx.root.join("out").join("new.txt")).unwrap(), "made");

    // `write`: and replaces what was there.
    let (_, r) = fx.call("write", Args::new().text("path", "out/new.txt").text("content", "remade"));
    assert!(!r.failed, "{:?}", r.summary);
    assert_eq!(fs::read_to_string(fx.root.join("out").join("new.txt")).unwrap(), "remade");
}

/// **A write must never cost a model call and leave nothing behind.**
///
/// Watched live 2026-08-26: asked for a new file, the model called `edit` with `replacing` set.
/// Path scoping had already opened the path `CreateOrOpen`, so the file existed at zero bytes,
/// `"".find(..)` missed, the call failed, and `Session Handoff - 2087.md` was left on disk at 0 B.
/// Six calls and three minutes followed.
///
/// The zero-byte file is unavoidable *inside `edit`* — scoping opens the handle before any
/// executor runs, and this crate cannot tell a file it just created from one already empty. What
/// is avoidable is spending a model call to produce one, and `write` is how: one call, both
/// arguments required, no mode to choose.
#[test]
fn write_leaves_content_not_an_empty_file() {
    let fx = Fixture::new("write-new");
    let (_, r) = fx.call(
        "write",
        Args::new().text("path", "Session Handoff - 99.md").text("content", "# Handoff\n"),
    );
    assert!(!r.failed, "one call must be enough to write a new file: {:?}", r.summary);
    let on_disk = fs::read_to_string(fx.root.join("Session Handoff - 99.md")).unwrap();
    assert_eq!(on_disk, "# Handoff\n");
    assert!(!on_disk.is_empty(), "a written file with nothing in it is the whole bug");
}

#[test]
fn grep_reports_matches_and_how_many_files_it_actually_read() {
    let fx = Fixture::new("grep");
    let (_, r) = fx.call("grep", Args::new().text("pattern", "needle").text("path", "."));
    assert!(!r.failed, "{:?}", r.summary);
    assert!(fx.text(&r).contains("notes.md:2"), "{}", fx.text(&r));
    // The second metric is the honest one: how many files were opened THROUGH THE SCOPE. A
    // search that reported matches without reporting its denominator would hide a wall that
    // silently refused everything.
    assert!(r.summary.render().contains("files"), "{}", r.summary.render());
}

#[test]
fn bash_runs_in_the_verified_directory() {
    let fx = Fixture::new("bash");
    // `cd` printed the directory in `cmd`; in bash it goes to $HOME and prints nothing.
    let cmd = "pwd";
    let (outcome, r) = fx.call("bash", Args::new().text("command", cmd).text("cwd", "src"));

    // ADR-026: `bash` escalates unconditionally, at EVERY tier including Silent. Asserted here
    // rather than assumed — this test drives the executor as though the approval had been
    // granted, and without this line it would look like `bash` runs unattended.
    assert_eq!(
        outcome,
        Outcome::NeedsApproval { tier: marlowe_permission::RiskTier::Irreversible },
        "bash must ask at every tier"
    );
    assert!(!r.failed, "{:?} {}", r.summary, fx.text(&r));
    assert!(
        fx.text(&r).to_lowercase().contains("src"),
        "the child did not start in the verified directory: {}",
        fx.text(&r)
    );
}

#[test]
fn an_escape_never_reaches_an_executor_at_all() {
    // The integration assertion: a refused call produces no handle, so the executor has nothing
    // to work with and says so. The refusal happens in the permission layer, not in the tool.
    let fx = Fixture::new("escape");
    let (outcome, r) = fx.call("read", Args::new().text("path", "../../../etc/passwd"));
    assert!(outcome.is_blocked(), "{outcome:?}");
    assert!(r.failed);
    assert!(
        r.summary.detail.as_deref().unwrap_or("").contains("no adjudicated handle"),
        "{:?}",
        r.summary
    );
}

/// **The loop a model could not escape: `edit` creates the file, then refuses to write it.**
///
/// `path` is a `WritePath`, so path scoping opens it `CreateOrOpen` before the executor runs. A
/// model writing a NEW file and supplying `replacing` therefore searched an empty string and was
/// told *"`replacing` was not found in the file"* — a true sentence about a situation that did not
/// exist, since the file it named had been created by that same call.
///
/// Watched live 2026-08-26, journal seq 4813-4859: `edit`, refusal, `read` (`0 lines · 0 B`, which
/// an empty file and a missing one both produce), `edit` again, `read`, `read`, then `bash` to run
/// `dir`. **Six calls, three minutes, and a zero-byte file left on disk.**
///
/// Each branch is asserted separately, because one message covering all three is what the old one
/// was.
#[test]
fn a_failed_replace_says_which_of_the_three_things_went_wrong() {
    let fx = Fixture::new("edit-miss");

    // 1. A NEW file. This is the live case, and the answer is now a different TOOL.
    let (_, r) = fx.call(
        "edit",
        Args::new()
            .text("path", "brand-new.md")
            .text("replacing", "anything at all")
            .text("content", "hello"),
    );
    assert!(r.failed);
    let d = r.summary.detail.clone().unwrap_or_default();
    assert!(
        d.contains("`write`"),
        "the remedy is a TOOL, and naming it is what stops the retry loop: {d:?}"
    );
    // The side effect this crate cannot undo -- scoping opened the handle `CreateOrOpen` before
    // any executor ran -- and which a later `read` would otherwise present as a new mystery.
    assert!(
        d.contains("zero bytes"),
        "the call left an empty file on disk and did not say so: {d:?}"
    );

    // 2. A whitespace-only mismatch — the commonest miss in a real file, and the one where a
    //    generic message sends the model round the same loop.
    let (_, r) = fx.call(
        "edit",
        Args::new()
            .text("path", "src/main.rs")
            .text("replacing", "println!(\"hi\");")
            .text("content", "println!(\"bye\");"),
    );
    assert!(!r.failed, "that snippet IS in the file verbatim, so this must succeed: {:?}", r.summary);

    let (_, r) = fx.call(
        "edit",
        Args::new()
            .text("path", "notes.md")
            // `notes.md` is "alpha\nbeta needle\ngamma\n" — same words, wrong spacing.
            .text("replacing", "beta    needle")
            .text("content", "x"),
    );
    assert!(r.failed);
    let d = r.summary.detail.clone().unwrap_or_default();
    assert!(
        d.contains("apart from whitespace"),
        "the file contains that text apart from spacing, and saying so turns an unbounded retry \
         into one corrected call: {d:?}"
    );

    // 3. Genuinely absent, in a file with content. The message must carry the file's size, or
    //    the model cannot tell "wrong file" from "wrong snippet".
    let (_, r) = fx.call(
        "edit",
        Args::new().text("path", "notes.md").text("replacing", "no such text").text("content", "x"),
    );
    assert!(r.failed);
    let d = r.summary.detail.clone().unwrap_or_default();
    assert!(
        d.contains("bytes") && d.contains("lines"),
        "a miss in a non-empty file must state what the file actually is: {d:?}"
    );
    assert!(
        !d.contains("apart from whitespace"),
        "**THE CONTROL.** If the whitespace branch fired here it fires on everything, and \
         branch 2 proves nothing: {d:?}"
    );
}

/// **A quoted command reaches the shell as it was written.**
///
/// `Command::arg` applies **Rust's** escaping rules — an argument containing a double quote is
/// wrapped and its quotes turned into `\"` — and `cmd.exe` does not use those rules. It reads `\"`
/// literally, so on Windows every command the model quoted arrived corrupted.
///
/// Measured 2026-08-27, the same code against this repo:
///
/// ```text
///   arg:      dir "docs\*" /b  ->  exit 1, "The system cannot find the path specified."
///   arg:      echo "hello"     ->  prints  \"hello\"      <- the escaping, visible in stdout
///   raw_arg:  dir "docs\*" /b  ->  exit 0
/// ```
///
/// Live, the model made **four** `bash` calls trying to list one directory, each stopping to ask
/// the user for approval before failing (journal seq 4884-4908). Three of the four were correct
/// `cmd` that would have worked if typed at a prompt. `SHELL_DESCRIPTION` had told it to *"quote
/// with double quotes"* — the harness instructing the model to do the one thing that could not
/// work, which is why this read as a model that could not use a shell.
///
/// The assertion is on the shell's OUTPUT rather than on the argument vector, because the argument
/// vector is what looked right the whole time.
#[test]
fn quotes_in_a_command_survive_to_the_shell() {
    let fx = Fixture::new("quoting");
    std::fs::create_dir_all(fx.root.join("a dir")).unwrap();
    std::fs::write(fx.root.join("a dir").join("inside.txt"), "found me").unwrap();

    // A path with a space CANNOT be expressed without quotes, so this fails on any build where
    // quoting is mangled — there is no unquoted spelling that would pass by accident.
    // **One spelling for both platforms, because there is one shell now.** This was `cfg`-split
    // for `type` versus `cat` while Windows ran `cmd /C`; it runs bash on both.
    let cmd = r#"cat "a dir/inside.txt""#;
    let (_, r) = fx.call("bash", Args::new().text("command", cmd));
    let text = fx.text(&r);
    assert!(!r.failed, "a quoted path did not survive to the shell: {:?} {text}", r.summary);
    assert!(text.contains("found me"), "the file was not read: {text}");

    // **The control.** The escaping was VISIBLE in stdout — `echo "x"` printed `\"x\"` — so this
    // fails loudly on a build that reverts to `Command::arg`, rather than only failing on paths
    // that happen to contain spaces.
    let echo = r#"echo "quoted""#;
    let (_, r) = fx.call("bash", Args::new().text("command", echo));
    let out = fx.text(&r);
    assert!(
        !out.contains(r#"\""#),
        "the shell received backslash-escaped quotes, which is Rust's escaping reaching a shell \
         that does not use it: {out}"
    );
}
