//! The filesystem and shell executors: `read`, `write`, `edit`, `glob`, `grep`, `bash`, `web`.
//!
//! # The one rule this crate exists to obey
//!
//! **Every executor operates on the handle the adjudicator opened. None of them re-opens a
//! path.** `Adjudication::handles` carries the [`ScopedPath`]s the permission layer produced
//! while checking the call; a tool that took `scoped.resolved()` and called `File::open` on it
//! would reopen the check-then-use race **across the permission boundary** — the one place
//! `tests/traversal.rs` would never look, because that suite tests the checker and the race
//! would be in the caller.
//!
//! `tests/handles.rs` asserts it directly rather than trusting the comment.
//!
//! # Where a tool touches more than its declared targets
//!
//! `grep` reads many files, only one of which the model named. Those extra opens are not
//! unchecked: each one is routed back through the same [`PathScope`], so every byte this crate
//! reads came through the wall. The model's argument is what the `(action, target)` check
//! adjudicated; the enumeration underneath it is a source of *candidate names*, not a source of
//! authority.
//!
//! # `bash` and the cwd string
//!
//! `CreateProcess` takes a working directory as a **string**, not a handle — there is no
//! handle-relative spawn on Windows. So `bash` holds the verified directory handle open across
//! the spawn and relies on the walk's pinning a **second time**: no `FILE_SHARE_DELETE` means
//! the directory cannot be renamed or deleted, so the string still names the verified object.
//!
//! That argument is load-bearing in a second place, so it earns its own assertion rather than
//! inheriting the walk's — see `tests/bash_cwd.rs`. On POSIX there is no such gap: the child
//! `fchdir`s to the descriptor before `exec`.

// `deny`, not `forbid`, and the exception is named. `Command::pre_exec` is unsafe by contract —
// the closure runs after `fork` and before `exec`, where only async-signal-safe calls are legal.
// `fchdir` is one. This is the only `unsafe` in the crate and it is what lets POSIX pass a
// DESCRIPTOR to the child instead of the path string Windows is forced to use.
#![deny(unsafe_code)]

pub mod corpus;

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use marlowe_contract::TrustClass;
use marlowe_loop::{ToolBody, ToolHost, ToolOutcome};
use marlowe_permission::scope::{Access, PathScope, ScopedPath};
use marlowe_permission::{Adjudication, ArgValue, Args};
use marlowe_tools::{Metric, PathGlob, ResultSummary, ToolId};

/// How much of a tool's output is kept. Above this the result is a reference and the loop sees
/// only the §8 summary — §6's *"the raw data should never touch attention"*.
pub const MAX_INLINE_BYTES: usize = 8_192;

/// How many lines one `read` returns when the caller does not say.
///
/// # A file is read through a WINDOW, the way Claude Code reads one
///
/// `read` used to return the whole file. Anything over [`MAX_INLINE_BYTES`] then became a
/// `ContentRef` — a hash plus a 4 KB head-and-tail — and **that hash is not dereferenceable for a
/// file**: `read`'s `ref` parameter takes ids that `web` issued, not file reads. So the middle of
/// any file above 8 KB was simply unreachable. The model could glimpse both ends and nothing else.
///
/// Watched live 2026-08-27: a run read `DECISIONS.md` (208 KB), `ROADMAP.md` (68 KB),
/// `CONTRACTS.md` (61 KB), `ARCHITECTURE.md` (33 KB) and `M3-DESIGN.md` (32 KB), got head and tail
/// of each, and could not answer from any of them.
///
/// 2,000 lines matches the reference implementation. It is a **default**, not a limit: `range`
/// reads any window of the file, and a truncated result says so and says how.
pub const READ_WINDOW_LINES: usize = 2_000;

/// The byte ceiling on one window, whatever its line count.
///
/// 2,000 lines of ordinary prose is far more than a 32,768-token context can hold, so lines alone
/// do not bound this — one call could still swallow the window. 32 KB is roughly 11k tokens
/// against `DEFAULT_CONTEXT_TOKENS`, which leaves room for the conversation that asked for it.
///
/// Both bounds are reported when either bites, because a result that stops without saying so is
/// indistinguishable from a file that ended.
///
/// **It measures what is RETURNED, so it is applied AFTER the line numbers are added.** A prefix
/// costs [`LINE_NUMBER_WIDTH`] + 1 bytes a line, so capping the raw text and then adding ~14 KB of
/// prefixes would leave this constant describing a quantity that never reaches the model. The
/// visible consequence is that a numbered window is often fewer than [`READ_WINDOW_LINES`] lines;
/// nothing becomes unreachable, because the notice names the next range either way.
pub const READ_WINDOW_BYTES: usize = 32 * 1024;

/// How wide the line-number column is, in characters, before the tab.
///
/// # `read` returns `cat -n`, and every part of that choice is load-bearing
///
/// `grep` reports `path:line:text` and `read` reported neither the line nor a way to compute one,
/// so a model holding a `grep` hit at `src/x.rs:512` could not turn it into a window without
/// counting. Numbering closes that, and the format is chosen for what a model reproduces —
/// **recognising and stripping** a prefix, never emitting one:
///
/// * **`cat -n` is the most-seen form.** It is in every shell transcript, it neighbours `grep -n`,
///   and it is what the reference implementation emits.
/// * **The tab is what makes detection tight.** Source lines legitimately begin with spaces; they
///   essentially never begin with `spaces + digits + TAB`. A `: ` or `| ` separator would make
///   `42: foo` — ordinary log text — indistinguishable from a prefix.
/// * **Right-alignment to a fixed width keeps the content column constant.** Not cosmetic: the
///   most common `edit` failure in this executor is a whitespace mismatch, and a left-aligned
///   number shifts the content column between line 99 and line 100 — the harness itself corrupting
///   the model's view of indentation, inside one window.
///
/// Six digits covers every file under a million lines and degrades by widening, never by dropping
/// the tab, so [`strip_line_numbers`] still parses past it.
pub const LINE_NUMBER_WIDTH: usize = 6;

/// How many of a non-unique `replacing`'s sites are named before the message says "and N more".
///
/// A refusal that lists ninety line numbers is a context flood inside the sentence that exists to
/// prevent one — the same defect ADR-059 fixed in `grep`'s truncation notice, which listed 250
/// paths on one line. The count is always exact; the enumeration is bounded.
pub const MAX_EDIT_SITES_NAMED: usize = 8;

/// How many consecutive numbered lines make `content` a paste rather than a coincidence.
///
/// **Deliberately not 1, and deliberately different from `replacing`'s threshold.** `replacing` is
/// cross-checked against the file it is being matched into, so its named refusal is verified;
/// `content` has nothing to check against, so the grammar is all there is. One line that happens to
/// read `     7\tfoo` is plausible content; two consecutive ones are essentially only produced by
/// pasting a `read` window.
pub const NUMBERED_CONTENT_LINES: u32 = 2;

/// Above this, a truncated read also says what the whole file would cost and names `grep`.
///
/// A quarter of `DEFAULT_CONTEXT_TOKENS`. Below it a file is several windows at worst and the
/// short notice is enough; above it, reading the whole thing is a decision worth making
/// deliberately rather than by repeating `read` until the budget runs out.
///
/// **Held back deliberately.** Warning on every truncated read would make the warning worthless —
/// a model told that everything is expensive has learned nothing about what actually is.
/// What fraction of the context window makes a file worth warning about.
///
/// A quarter. Below it, reading the file costs a share of the window a run can afford to spend on
/// one call; above it, reading the whole thing is a decision worth making deliberately rather than
/// by repeating `read` until the budget runs out.
///
/// **Held back deliberately.** Warning on every truncated read would make the warning worthless —
/// a model told that everything is expensive has learned nothing about what actually is.
pub const LARGE_FILE_SHARE_OF_CONTEXT: u32 = 4;

/// [`marlowe_loop::estimate_tokens`] for text that is never materialised.
///
/// `read` must state what the **whole numbered file** would cost while only ever holding one
/// window of it, so that cost is computed from a length — the bytes on disk plus one
/// [`LINE_NUMBER_WIDTH`] + 1 byte prefix per line — and there is no string to hand the estimator.
///
/// **This is a second copy of the assembler's three-characters-per-token rule**, and a second copy
/// held true by a comment is held true by nothing:
/// `read_cost_notice.rs::estimate_tokens_of_agrees_with_the_assemblers_estimator` compares the two
/// across the rounding boundaries and the sizes this executor deals in, so a change to either one
/// fails a test rather than quietly relabelling every cost `read` prints.
pub fn estimate_tokens_of(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX)
}

/// The window assumed when nobody says. Kept as a literal rather than importing
/// `marlowe_provider::DEFAULT_CONTEXT_TOKENS`, because this crate must not depend on a provider —
/// `with_context_tokens` is how the real number arrives, and the daemon always passes it.
fn marlowe_provider_context_default() -> u32 {
    32_768
}

/// How many files one walk will enumerate. A search is bounded structurally rather than by
/// hoping the pattern is selective.
///
/// **Renamed from `FIND_FILE_CAP`, because the old name was false.** It bounds `glob` exactly as
/// it bounds `grep` — `glob` has always called `collect` with it — so a name saying it belonged to
/// one tool sent anyone reading `glob`'s cost model to the wrong constant.
pub const WALK_FILE_CAP: usize = 2_000;

/// Directories a walk does not descend into, unless the caller names one as `path`.
///
/// # This is the difference between `grep` working on a real project and returning zero
///
/// [`collect`] is a LIFO walk with a hard cap and, until ADR-059, no skip list. On this very
/// checkout `target/` holds ~162,000 files, so `grep(pattern, path=".")` filled all
/// [`WALK_FILE_CAP`] slots with build artifacts, **never reached `crates/` at all**, and reported
/// `0 results`. Worse, `find` — unlike `glob` — never checked whether the cap had bitten, so the
/// zero was reported as a fact about the workspace rather than as a walk that stopped.
///
/// **The harness already owned this list and did not share it.** `marlowe_daemon::workspace_map`
/// carried a private `SKIP` with exactly these six names; the walk two tools actually search with
/// did not. This is the one definition, and the daemon reads it — a second copy is how the two
/// come to disagree about what a project is.
///
/// Naming one of these as `path` still searches it: the check is on directories the walk would
/// *descend into*, never on the base it was handed. And every walk **states** which of these it
/// met, because a model that greps `.` and sees nothing from `target/` must be able to tell a
/// policy from an absence.
pub const WALK_SKIP: [&str; 6] = [".git", "target", "node_modules", ".venv", "__pycache__", "dist"];

/// How many result lines — matches plus context — `grep` will emit before it stops emitting and
/// starts counting.
///
/// SECURITY-AUDIT finding 8: hit accumulation was unbounded, and a regex makes that materially
/// worse than a substring did (`.` matches every line of every file). Past this the result
/// degrades to per-file counts and **says that it did**, which is the budget lever firing at the
/// moment it is needed rather than at the moment a model guessed a `head_limit`.
pub const MAX_MATCH_LINES: usize = 200;

/// How much of one emitted line is kept.
///
/// A minified bundle is a single two-megabyte line. Without this, one such line is the whole
/// result. The truncation is marked in the line itself rather than left to be inferred.
pub const MAX_MATCH_LINE_BYTES: usize = 512;

/// The largest `context` `grep` accepts. A larger value is REFUSED, not quietly reduced — a
/// silently lowered argument is a model believing it asked for something it did not get.
pub const MAX_GREP_CONTEXT: i64 = 20;

/// The ceiling on what one regex may compile to, both as an NFA and as a lazy DFA.
///
/// The crate's own default is 10 MB. This is 1 MiB, which is far above any pattern a model writes
/// and far below anything that matters, and over-limit returns a compile **error** the model can
/// read and fix rather than a hang. Catastrophic *backtracking* needs no bound here at all,
/// because the engine does not backtrack; the residual cost is compilation, and this is it.
///
/// **Public because the test derives its fixture from it, and because the obvious fixture does not
/// discriminate.** `a{1000}{1000}{1000}` — the canonical example, and the one the design named —
/// is refused at the crate's 10 MB default too, so a test built on it is GREEN on a build where
/// this constant is not read at all: the sixteenth-instance family, in the control rather than in
/// the test. The pattern that separates the two is `a{300}{300}`, which compiles at 10 MB and is
/// refused at 1 MiB, and `a_pattern_too_large_to_compile_is_refused_and_the_limit_is_named`
/// asserts on both — one for the behaviour, one for the fact that THIS line is what produced it.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// How many paths a result's notice will name before it says "and N more".
///
/// **A notice is not exempt from the caps the result is under.** Run against this checkout with
/// `WALK_SKIP` disabled, the partial-read notice listed 250 `.rlib` and `.pdb` paths on one line —
/// a context flood inside the sentence that exists to prevent one. Ten names is enough to
/// recognise a pattern; the count is what carries the magnitude.
const NAMES_IN_A_NOTICE: usize = 10;

/// Every parameter `grep` declares — and therefore every one it will accept.
///
/// **The list is here, in the executor, and the refusal is built from it with `join`.** A refusal
/// that typed the four names would be a second declaration of the tool's surface, one that goes
/// stale the day a fifth is added; `grep_refuses_an_argument_it_does_not_declare` reads it from
/// this constant for the same reason. `every_declared_parameter_is_accepted_by_the_executor`
/// pins it against the shipped manifest, so the two cannot disagree in either direction.
pub const GREP_PARAMS: [&str; 4] = ["pattern", "path", "glob", "context"];

/// One emitted line, bounded, with the truncation MARKED rather than left to look like the line.
///
/// A minified bundle is one two-megabyte line; without this, that line is the entire result.
fn clip_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_string();
    }
    let cut = floor_boundary(line, MAX_MATCH_LINE_BYTES);
    format!("{}…[line continues, {} more bytes]", &line[..cut], line.len() - cut)
}

/// How long `bash` may run before it is killed.
///
/// **This is now read.** Audit finding A1: it was declared here and a grep over the whole repo
/// returned exactly one hit — this definition. Both `spawn_shell` implementations called
/// `Command::output()`, which blocks until the child exits, so any approved non-terminating command
/// (`ping -t`, `tail -f`, a blocking read) hung the batch, the turn and the daemon forever. It is
/// the "declared control nothing reads" family in its purest form, and the test that pins it kills
/// a real sleeping child rather than asserting this number.
pub const BASH_TIMEOUT_MS: u64 = 120_000;

/// How often the wait loop looks at the child.
///
/// **The deadline is counted in sleeps, not measured with a clock**, because §4.5 forbids a real
/// clock on any path reachable from the section 4 interfaces and `marlowe/tests/determinism_guard.rs`
/// enforces it across the workspace — it has already caught an `Instant::now()` in `bash`. So the
/// bound is a *lower* bound on wall time: under load the child gets somewhat longer than
/// [`BASH_TIMEOUT_MS`]. The property that matters is that the wait terminates, and it does.
const POLL_MS: u64 = 10;

/// How much output `bash` will hold, **per stream**.
///
/// Audit finding A2: `Command::output()` buffered stdout and stderr with no ceiling, `combine` made
/// a second copy through `from_utf8_lossy(...).into_owned()`, and `body_for` hashed and previewed it
/// again — peak around 3x the child's output, for `yes`, `cat /dev/urandom` or `dir /s C:\`.
/// `MAX_INLINE_BYTES` bounds only what reaches the model, never what is allocated. CLAUDE.md
/// records a `0x139` bugcheck on this machine under memory exhaustion, so this is not theoretical.
///
/// A breach **kills the child** rather than draining and discarding: draining would leave a command
/// producing output at line rate running until the timeout, burning a core for two minutes to
/// produce nothing.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 1_048_576;

/// How long the reader threads are given to hand back what they read, after the child is gone.
///
/// A grandchild that inherited the pipe keeps it open, so a `join` here could block forever — the
/// hang this whole section exists to remove, moved one level down. After the grace the readers are
/// abandoned with whatever they had; the tool reports rather than waits.
const READER_GRACE_MS: u64 = 250;

/// The production [`ToolHost`].
///
/// It holds a scope because `grep` needs one (see the header). It does **not** hold a workspace
/// path it could use to bypass the scope: the workspace is passed to the scope, which is the only
/// thing that turns a path into a handle.
pub struct FileSystemTools<S: PathScope> {
    scope: S,
    workspace: PathBuf,
    /// ARCHITECTURE §2.2's content store, scoped to fetched documents.
    ///
    /// **Every fetched document lands here, whole**, and the run receives a `DocumentRef` — a
    /// hash and a set of counts with no bytes of the page in it. Today the extracted text ALSO
    /// still flows to the run through layer 1's quarantined reader, so nothing regresses; the
    /// store is what makes the next step possible, where the text stops flowing at all and is
    /// pulled only when something asks a question of it. See `docs/design/adr/ADR-042`.
    store: marlowe_extract::store::DocumentStore,
    /// The model's context window, in tokens — **the thing "large" is large COMPARED TO.**
    ///
    /// A fixed byte or token threshold answers the wrong question. A 10,000-token file is most of
    /// a 32k window and a rounding error in a 200k one, and warning about it in both teaches a
    /// model with room to spare that the warning means nothing. So the executor is told the window
    /// and compares against it.
    ///
    /// Defaults to `DEFAULT_CONTEXT_TOKENS` so a host built without one still behaves sensibly;
    /// the daemon passes the number it actually sends as `num_ctx`, which is the same field the
    /// assembler sizes its view from.
    context_tokens: u32,
    /// ── TWO `edit`s TO ONE FILE IN ONE BATCH RACED, AND THE LOSER WAS SILENT ─────────────
    ///
    /// `execute_batch` runs a turn's calls **concurrently**, and `write`/`edit` each do their own
    /// read-modify-write through their own cloned handle. Two edits to the same file therefore
    /// both read the original, and the second's `set_len(0)` + `write_all` overwrote the first's
    /// result. **Both reported success**, with truthful-looking `+n −m` lines, and nothing
    /// downstream could tell.
    ///
    /// It has been reachable since batching landed. What made it urgent is the non-unique
    /// `replacing` refusal in [`FileSystemTools::edit`]: the remedy that refusal names is *"edit
    /// each site in a separate call"*, and separate calls in one turn are exactly one batch.
    /// Closing one hazard by routing the model into another is not a fix.
    ///
    /// **One lock for all mutations, not one per path.** A path-keyed map is the tempting shape and
    /// it buys nothing here: a batch is a turn's worth of calls, a file rewrite is microseconds,
    /// and the batch's real cost is `web` fetches, which do not take this lock at all. A single
    /// mutex has no key to get wrong and no map to grow.
    writes: Mutex<()>,
}

impl<S: PathScope> FileSystemTools<S> {
    pub fn new(scope: S, workspace: impl Into<PathBuf>) -> Self {
        Self {
            scope,
            workspace: workspace.into(),
            store: marlowe_extract::store::DocumentStore::new(),
            context_tokens: marlowe_provider_context_default(),
            writes: Mutex::new(()),
        }
    }

    /// The model's context window, so "this file is large" is measured against something real.
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        if tokens > 0 {
            self.context_tokens = tokens;
        }
        self
    }

    /// Share one store across hosts. The daemon builds a single host today, so this exists for the
    /// case where a corpus must outlive one of them.
    pub fn with_store(mut self, store: marlowe_extract::store::DocumentStore) -> Self {
        self.store = store;
        self
    }

    /// The documents fetched so far. The dereference path reads from here.
    pub fn store(&self) -> &marlowe_extract::store::DocumentStore {
        &self.store
    }
}

/// Why `replacing` did not match, told so the model can act on it.
///
/// # The loop this ends
///
/// `path` is a `WritePath`, so the scoping layer opens it `CreateOrOpen` — **the file exists,
/// empty, by the time this executor runs.** A model writing a NEW file and supplying `replacing`
/// therefore searched an empty string, and got back *"`replacing` was not found in the file"*: a
/// true sentence describing a situation that does not exist, since the file it names had been
/// created by that same call one line earlier.
///
/// Watched live 2026-08-26, journal seq 4813-4839. The model tried `edit`, was told that, `read`
/// the file, got **"0 lines · 0 B"** — which is what an empty file and a file that was never
/// there both look like — tried `edit` again, read again, read again, and finally fell back to
/// `bash` to run `dir`. **Six calls, two minutes, one zero-byte file, and no way to learn why.**
/// The user's summary was "he can't even write a file, and he doesn't even know why".
///
/// Nothing was broken. `existing.find` was correct, the refusal was honest, and the message was
/// about the wrong thing. So the three cases are separated and each says what to do next.
fn replacing_miss(existing: &str, replacing: &str) -> String {
    // **The empty-file case does not reach here any more** — `edit` writes it, because a
    // `replacing` that cannot match anything in a file with no content is vacuous rather than
    // wrong. See the note at that branch. Debug-asserted rather than handled, so a future change
    // that routes an empty file back here is caught in the suite instead of shipping a message
    // about a case that has a better answer.
    debug_assert!(!existing.is_empty(), "an empty file is written, not refused");

    // ── THE PREFIX `read` ITSELF PUT THERE, AND THE ONLY MISS THE HARNESS CAUSED ───────────
    //
    // `read` returns `cat -n`, `edit` matches byte for byte, and the description tells the model to
    // copy `replacing` out of a `read`. So the harness now manufactures a miss, and it is the one
    // miss it can diagnose with certainty rather than with a heuristic.
    //
    // **Three conjuncts, and only the first is a guess.** (i) the value parses as numbered;
    // (ii) `existing.find(replacing)` has ALREADY missed — this function runs only on that path,
    // so a file that genuinely contains `      42\tfoo` edited with exactly that text matched and
    // never arrived here; (iii) the stripped form IS in the file. (iii) turns the guess into a
    // checked diagnosis: the message does not say the text "looks numbered", it says where the
    // stripped text is, verified against the file this executor is holding.
    if let Some(stripped) = strip_line_numbers(replacing) {
        if let Some(at) = existing.find(&stripped) {
            return format!(
                "`replacing` carries the line-number prefix `read` printed — {} characters, then a \
                 tab, in front of every line. `edit` matches the file byte for byte and the file \
                 does not contain those prefixes, so strip them. Stripped, your text IS in the \
                 file, starting at line {}.",
                LINE_NUMBER_WIDTH,
                line_at(existing, at),
            );
        }
        // Grammar only, no cross-check — so this says both facts and guesses neither.
        return format!(
            "`replacing` carries the line-number prefix `read` printed ({} characters then a tab \
             in front of every line), which `edit` never accepts — strip it. Even stripped the \
             text was not found, so `read` the file again and copy the snippet from what comes \
             back, without the prefixes. The file is {} bytes, {} lines.",
            LINE_NUMBER_WIDTH,
            existing.len(),
            existing.lines().count(),
        );
    }

    // **CRLF, which this executor could not previously say anything about.** `str::lines` strips a
    // trailing `\r`, so a windowed read of a CRLF file used to hand back LF and nothing copied out
    // of it could ever match. `read` preserves terminators now, but a model that *composed* the
    // snippet rather than copying it still writes LF, and the generic message names the wrong
    // cause — "including indentation" sends it to look at spaces.
    if existing.contains("\r\n") && !replacing.contains('\r') && replacing.contains('\n') {
        let unix = existing.replace("\r\n", "\n");
        if let Some(at) = unix.find(replacing) {
            return format!(
                "`replacing` was not found because the file uses CRLF (`\\r\\n`) line endings and \
                 your text uses LF (`\\n`). Apart from that it is there, at line {}. `read` the \
                 file and copy the snippet from what comes back rather than retyping it.",
                line_at(&unix, at),
            );
        }
    }

    // **The overwhelmingly common miss is whitespace**, and saying so turns an unbounded retry
    // into one corrected call. Checked by collapsing runs of whitespace on both sides: if the
    // snippet is there apart from spacing, the model copied the text and not the indentation.
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    if !replacing.trim().is_empty() && squash(existing).contains(&squash(replacing)) {
        return format!(
            "`replacing` was not found. The file DOES contain that text apart from whitespace, so \
             the indentation or line breaks differ — `replacing` must match byte for byte. `read` \
             the file and copy the snippet exactly as it comes back. The file is {} bytes, {} \
             lines.",
            existing.len(),
            existing.lines().count(),
        );
    }

    format!(
        "`replacing` was not found in the file, which is {} bytes and {} lines. It must match byte \
         for byte, including indentation. `read` the file first and copy the snippet from what it \
         returns — or omit `replacing` entirely to overwrite the whole file with `content`.",
        existing.len(),
        existing.lines().count(),
    )
}

fn failed(verb: &'static str, detail: impl Into<String>) -> ToolOutcome {
    // ── THE REASON GOES IN THE BODY TOO, AND THAT IS THE WHOLE POINT ────────────────────
    //
    // This returned an EMPTY body with the reason in `summary.detail`, and `detail` is the §B6
    // expansion — a field every consumer had to know to look in. The ones that did not:
    //
    // * the model's own window, which is built from `summary.render()` and the BODY, so a failed
    //   `edit` arrived as the literal string `"edit · "`;
    // * the journal, which recorded `{"tool":"read","summary":"read"}` — the name, twice;
    // * the surface, which shows `render()` only;
    // * and every failure assertion in `every_tool_exercised.rs`, which read the body and got the
    //   empty string, panicking with a blank message.
    //
    // Four consumers, four separate patches, one cause. **A failure's reason is not an optional
    // detail — it is the result.** Putting it in the body means nothing downstream has to know
    // that failures are shaped differently from successes, and the places already patched keep
    // working (`Engine::finish` appends `detail` only when the text does not already contain it).
    let detail = detail.into();
    ToolOutcome {
        summary: ResultSummary::with_detail(vec![Metric::State(verb)], detail.clone()),
        body: ToolBody::Inline(detail),
        // The harness computed this refusal, so it is agent-observed. Inheriting the call's own
        // taint would make a blocked-call notice unreadable by the very next step.
        trust: TrustClass::AgentObserved,
        failed: true,
        wall_ms: 0,
        preview: None,
    }
}

/// Inline if small, reference if not. §2.8's first axis — **size**, independent of trust.
///
/// **`read` does not come through here any more** — see [`body_for_window`]. A file bounded by
/// [`READ_WINDOW_BYTES`] is meant to be READ, and turning it into a hash the model cannot
/// dereference was the whole defect. This still governs `bash` output, where a
/// reference is the honest answer to "more than you asked for".
fn body_for(text: String) -> (ToolBody, u64, Option<String>) {
    let bytes = text.len() as u64;
    if text.len() <= MAX_INLINE_BYTES {
        (ToolBody::Inline(text), bytes, None)
    } else {
        // Content-addressed by the store at M2 D; until then the hash names the bytes so the
        // summary is honest about what it is standing in for.
        let hash = format!("{:016x}", fnv1a(text.as_bytes()));
        let preview = Some(head_and_tail(&text));
        (ToolBody::Reference { hash, bytes }, bytes, preview)
    }
}

/// A window that has already been bounded reaches the model as TEXT.
///
/// `body_for` would hand back a `ContentRef` for anything over 8 KB, and a file reference cannot
/// be dereferenced — `read`'s `ref` takes ids `web` issued. That is what made every file above
/// 8 KB unreadable past its first and last 4 KB. The window is bounded by
/// [`READ_WINDOW_BYTES`] before it gets here, so there is nothing left to protect against.
fn body_for_window(text: String) -> (ToolBody, u64, Option<String>) {
    let bytes = text.len() as u64;
    (ToolBody::Inline(text), bytes, None)
}

/// How much of an over-large body still reaches the model.
///
/// A hash it cannot dereference is not a result — measured live, `read` on a 69 KB file returned
/// only `ref 225bfe8df7bbc044` and the model called `read` five times chasing the same hash. Head
/// and tail with the omission **stated in words** is something it can act on.
const PREVIEW_BYTES: usize = 4_000;

fn head_and_tail(text: &str) -> String {
    if text.len() <= PREVIEW_BYTES {
        return text.to_string();
    }
    let head_len = floor_boundary(text, PREVIEW_BYTES / 2);
    let tail_len = floor_boundary(text, PREVIEW_BYTES / 4);
    let tail_start = ceil_boundary(text, text.len() - tail_len);
    let omitted = tail_start - head_len;
    format!(
        "{}
…[{omitted} characters omitted from the middle; the tool read the whole file]…
{}",
        &text[..head_len],
        &text[tail_start..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        // LOOP-EXEMPT: walking back at most three bytes to a UTF-8 boundary.
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        // LOOP-EXEMPT: walking forward at most three bytes to a UTF-8 boundary.
        i += 1;
    }
    i
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn text_arg<'a>(args: &'a Args, name: &str) -> Option<&'a str> {
    args.get(name).and_then(ArgValue::as_text)
}

fn handle_for<'a>(a: &'a Adjudication, param: &str) -> Option<&'a ScopedPath> {
    a.handles.get(param)
}

impl<S: PathScope> FileSystemTools<S> {
    fn read(&self, args: &Args, a: &Adjudication) -> ToolOutcome {
        // **The dereference path.** `engine.rs` has anticipated this since M2: *"The content store
        // lands at M2 D and the `read`-a-reference path with it."* Until it existed, a reference
        // was a hash the model could not turn back into anything, so `web` had to ship the page.
        if let Some(id) = text_arg(args, "ref") {
            return self.read_ref(id, args);
        }
        let Some(scoped) = handle_for(a, "path") else {
            // **Two different failures, two different messages.** A `path` that was supplied and
            // refused has no handle; so does a call that named no subject at all. Collapsing them
            // told a model whose traversal had just been blocked that it had "given neither",
            // which is false and points it at the wrong correction.
            return if args.get("path").is_some() {
                failed("read", "no adjudicated handle for `path`")
            } else {
                failed(
                    "read",
                    "supply either `path` (a workspace-relative file) or `ref` (the id a `web` result reported). Neither was given.",
                )
            };
        };
        let mut text = String::new();
        let capped = match clone_and_read(scoped, &mut text) {
            Ok(c) => c,
            Err(e) => return failed("read", e.to_string()),
        };
        let capped = match capped {
            ReadOutcome::Whole => false,
            ReadOutcome::Capped => true,
            // A binary is REPORTED, not decoded. `read_to_string` used to fail outright, so `read`
            // could say nothing at all about a non-UTF-8 file; decoding it lossily would be worse,
            // filling the window with replacement characters that look like content.
            ReadOutcome::NotText { bytes } => {
                return ToolOutcome {
                    summary: ResultSummary::with_detail(
                        vec![Metric::State("binary"), Metric::Bytes { n: bytes }],
                        "this file is not UTF-8 text, so its bytes were not returned",
                    ),
                    body: ToolBody::Inline(String::new()),
                    trust: TrustClass::AgentObserved,
                    failed: false,
                    wall_ms: 0,
                    preview: None,
                };
            }
        };
        // ── THE `MAX_READ_BYTES` NOTE IS HARNESS SPEECH AND LIVES IN THE TRAILER ───────────
        //
        // It used to be pushed into `text` here, BEFORE `range` and before the window. So it was a
        // range-selectable line, it counted toward `total_lines`, and — once numbering exists — it
        // would acquire a line number, which stops the numbering being a faithful map of the file.
        // It is emitted with the window notice instead: column 0, no prefix, after everything.
        //
        // `capped` is carried, not the string, so the note is written once at the bottom.
        //
        // `range` is a Payload: untrusted prose may shape it freely, because it selects nothing
        // outside a file the target check already approved.
        //
        // **The file's own length is measured before slicing**, so the notice can speak in
        // absolute line numbers whether or not a range was given.
        let total_lines = text.lines().count();
        // ── AND SO IS ITS COST, FOR THE SAME REASON AND WITH THE SAME URGENCY ──────────────
        //
        // The notice says *"The whole file is about N tokens"*. It used to be computed **after**
        // the `range` slice below and **before** the numbering above, so it was neither of the two
        // things that sentence claims:
        //
        // * **It measured the RANGE and called it the file.** 20,000 weighty lines read as
        //   `range: "1-3000"`: 45,631 claimed against 356,298 real, **7.8x low**, in the one
        //   sentence whose entire job is to let a model choose between reading this and `grep`ping
        //   it. `expensive` derives from the same value, so the failure is not only a wrong
        //   number — a 509 KB file read through a 3,000-line range fell under the threshold and
        //   got **no warning at all**, which is the worse half.
        // * **It measured the bytes on disk while the model receives numbered text.** Exactly the
        //   reasoning `READ_WINDOW_BYTES` is given below — *"the bytes the model receives rather
        //   than the bytes on disk"* — never applied four lines up. Each line costs
        //   `LINE_NUMBER_WIDTH + 1`; +15% on that file, and more the shorter the lines are.
        //
        // **It costs nothing to measure here.** `estimate_tokens` is a length divided by three,
        // `text` already holds the whole file (`clone_and_read` read it before `range` existed),
        // and `total_lines` is already counted for the notice. Moving it earlier removes a pass
        // over the text rather than adding one — see [`estimate_tokens_of`], which takes the
        // length because the numbered whole file is never built.
        //
        // A file cut at `MAX_READ_BYTES` reports the cost of the 4 MB prefix rather than of the
        // file. That is an under-report, it is stated in words by the trailer below, and it cannot
        // change the decision: 4 MB is ~1.4M tokens and is expensive against every window there is.
        let whole_file_tokens =
            estimate_tokens_of(text.len() + total_lines * (LINE_NUMBER_WIDTH + 1));
        let mut first_line = 1usize;
        if let Some(range) = text_arg(args, "range") {
            // **Both failure modes here were SILENT and both produced a result that means
            // something else.** See `slice_lines`.
            let of = total_lines;
            match slice_lines(&text, range) {
                Ok((sliced, _)) if sliced.is_empty() && of > 0 => {
                    // `0 lines · 0 B` is the signature `read`'s own description reserves for "the
                    // file is there and is empty". A range that selected nothing rendered
                    // IDENTICALLY, so a model asking for lines 500-600 of a ten-line file was
                    // handed the exact string it had been told to read as "this file is empty".
                    return failed(
                        "read",
                        format!(
                            "`range` \"{range}\" selected no lines: the file has {of}. Ask for a \
                             range inside 1-{of}, or omit `range` for the whole file."
                        ),
                    );
                }
                Ok((sliced, start)) => {
                    text = sliced;
                    first_line = start;
                }
                // `slice_lines` used to swallow this and return the WHOLE FILE. A model that
                // mistyped a range on a large file got everything back, with nothing to say the
                // range had been ignored rather than honoured.
                Err(why) => return failed("read", why),
            }
        }
        // ── THE WINDOW, AND WHAT IT COSTS ──────────────────────────────────────────────
        //
        // Applied AFTER `range`, so an explicit range is bounded by the same ceiling rather than
        // being a way around it, and BEFORE the metrics, so the numbers describe what was
        // actually returned.
        //
        // **Two notices, and the size decides which.** An ordinary long file needs one line: what
        // you got, what to ask for next. A 208 KB file is a different decision, and the unit that
        // decision is made in is TOKENS -- bytes do not tell a model what it is committing to.
        // Watched live 2026-08-27: a run read five design documents totalling ~400 KB, spent its
        // whole 200k budget and answered from none of them.
        //
        // The long form is held back for genuinely large files -- more than
        // [`LARGE_FILE_SHARE_OF_CONTEXT`] of the window -- so that routine reads are not dressed
        // as warnings. A model told everything is expensive learns nothing about what actually is.
        //
        // **Two things this paragraph used to say are no longer true and are corrected rather
        // than deleted, because both were load-bearing to someone reading it.** It cited a
        // `LARGE_FILE_TOKENS` constant, which has not existed since the threshold became a
        // fraction of the window; and it offered *"a 2,400-line file is ordinary"* as the example,
        // which stopped holding the moment the figure counted the line numbers the model is
        // actually sent — 2,400 lines of `"{i}\n"` is 9,231 tokens, which is over a quarter of a
        // 32,768-token window, so that file is now correctly called expensive.
        //
        // **Large COMPARED TO THIS MODEL'S WINDOW**, not against a constant. The same file is
        // most of a 32k context and a rounding error in a 200k one.
        //
        // **The trigger reads the same figure the sentence prints**, measured above on the whole
        // numbered file. Triggering on one quantity and printing another is how this went wrong in
        // the first place.
        let expensive = whole_file_tokens > self.context_tokens / LARGE_FILE_SHARE_OF_CONTEXT;

        let selected = text.lines().count();
        let mut kept = selected;
        if selected > READ_WINDOW_LINES {
            text = take_lines(&text, READ_WINDOW_LINES);
            kept = READ_WINDOW_LINES;
        }

        // ── NUMBERED HERE: AFTER THE WINDOW, BEFORE THE BYTE CEILING, BEFORE THE TRAILER ──
        //
        // After the window so the numbers describe the lines that actually came back; before the
        // ceiling so `READ_WINDOW_BYTES` counts the bytes the model receives rather than the bytes
        // on disk; before the trailer so the trailer stays at column 0 as harness speech.
        //
        // **This is the `path` branch only.** `read(ref)` returned long before here, and it must:
        // its result is `UntrustedContent`, so layer 1 routes it to a quarantined reader, and
        // `condense_chunk` is built on source labels being *"assigned here, never taken from the
        // content"*. Numbering an attacker-controlled document would hand it a harness-authored
        // prefix on every line, after which the reader cannot tell a harness prefix from document
        // text. There is nothing to edit in a fetched page, so the numbers buy nothing and cost
        // the one property that section rests on.
        let mut text = number_lines(&text, first_line);
        if text.len() > READ_WINDOW_BYTES {
            let (cut, lines) = cut_to_whole_lines(&text, READ_WINDOW_BYTES);
            text = cut;
            kept = lines;
        }

        // ── THE TRAILER: HARNESS SPEECH, UNNUMBERED, AT COLUMN 0 ──────────────────────────
        //
        // Appended after truncation so it is never itself cut off, and inside the body so it
        // survives whatever the body becomes. A model already reads a leading `[` as the harness
        // speaking; a numbered `[` would read as line 1,993 of the file.
        if capped {
            text.push_str(&format!(
                "\n\n[the harness stopped reading at {MAX_READ_BYTES} bytes. The file is longer \
                 than this and the text above is a prefix.]"
            ));
        }
        if kept < selected {
            let last = first_line + kept - 1;
            let next_end = (last + READ_WINDOW_LINES).min(total_lines);
            let cost = if expensive {
                format!(
                    " The whole file is about {whole_file_tokens} tokens; to find something \
                     specific, `grep` searches inside files and returns `path:line:text`."
                )
            } else {
                String::new()
            };
            text.push_str(&format!(
                "\n\n[showing lines {first_line}-{last} of {total_lines}. Continue with range \
                 \"{}-{next_end}\".{cost}]",
                last + 1,
            ));
        }

        // **The line count is the CONTENT's**, not the body's: the trailer is not a line of the
        // file, and counting it made `read`'s own numbers disagree with the numbers it printed.
        let lines = kept as u64;
        let (body, bytes, preview) = body_for_window(text);
        let mut metrics =
            vec![Metric::Count { n: lines, unit: "lines" }, Metric::Bytes { n: bytes }];
        if capped {
            metrics.insert(0, Metric::State("truncated"));
        }
        ToolOutcome {
            summary: ResultSummary::new(metrics),
            body,
            // Workspace content is what the harness read from disk, not what a model asserted.
            // A file whose *origin* is untrusted (a fetched page written to disk) is a §2.8
            // problem the content store solves at M2 D; a workspace read is agent-observed.
            trust: TrustClass::AgentObserved,
            failed: false,
            wall_ms: 0,
            preview,
        }
    }

    /// **`write` exists because a tool's NAME is the first thing a model matches on.**
    ///
    /// Told to write a file, the model looked for a write verb, found none, and reached for the
    /// shell: `cat > session-handoff.md << 'EOF'` — journal seq 4806, 2026-08-26, which failed
    /// under `cmd /C` with a bare exit code. `edit` did not occur to it until the shell had
    /// failed, and then it picked `edit`'s wrong mode.
    ///
    /// That second part was the deeper fault. `edit` used to be **two tools wearing one name**,
    /// told apart by an OPTIONAL parameter: supply `replacing` and it patches, omit it and it
    /// overwrites. A model holding a request and a schema had to infer a mode, and the mode it
    /// inferred was wrong. No description fixes that — the ambiguity is in the shape.
    ///
    /// So the two modes are two tools and every parameter of each is required. `write` creates or
    /// overwrites; `edit` replaces a snippet and fails if it is absent. Neither has a mode.
    fn write(&self, args: &Args, a: &Adjudication) -> ToolOutcome {
        let Some(scoped) = handle_for(a, "path") else {
            return failed("write", "no adjudicated handle for `path`");
        };
        let Some(content) = text_arg(args, "content") else {
            return failed("write", "`content` is required");
        };
        // Held across the whole read-modify-write — see [`FileSystemTools::writes`]. Taken after
        // the argument checks so a malformed call does not queue behind a real one.
        let _writing = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = match scoped.handle().try_clone() {
            Ok(f) => f,
            Err(e) => return failed("write", e.to_string()),
        };
        let mut existing = String::new();
        let _ = file.read_to_string(&mut existing);
        // Truncate through the handle, not by reopening with `create(true)` — the handle is the
        // one the permission layer checked, and reopening by path is the TOCTOU this avoids.
        if let Err(e) = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| file.write_all(content.as_bytes()))
            .and_then(|()| file.flush())
        {
            return failed("write", e.to_string());
        }
        // ── `write` WARNS WHERE `edit` REFUSES, AND THAT ASYMMETRY IS THE ESCAPE HATCH ────────
        //
        // Numbered text in `content` is almost always a `read` window pasted back — but a whole
        // file that legitimately contains `cat -n` output exists (a transcript, a fixture, the
        // tests for this very feature), and it has to be writable through the tool surface. So
        // `write` proceeds and SAYS SO: the file is on disk with the prefixes in it, and the model
        // is told, in the same result, that it just wrote them.
        let mut metrics = vec![Metric::Diff {
            added: count_lines(content),
            removed: count_lines(&existing),
        }];
        let mut numbered_warning = None;
        if count_lines(content) >= NUMBERED_CONTENT_LINES && strip_line_numbers(content).is_some() {
            metrics.push(Metric::State("line-numbered"));
            numbered_warning = Some(format!(
                "the file was written, and its lines carry `read`'s line-number prefix ({} \
                 characters then a tab). If that came from pasting a `read` result, the numbers \
                 are now IN the file — write it again without them.",
                LINE_NUMBER_WIDTH,
            ));
        }
        ToolOutcome {
            summary: match numbered_warning {
                Some(w) => ResultSummary::with_detail(metrics, w),
                None => ResultSummary::new(metrics),
            },
            body: ToolBody::Inline(scoped.relative().to_string()),
            trust: TrustClass::AgentObserved,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }

    /// Replace one snippet. **`replacing` is required** — see [`FileSystemTools::write`] for why
    /// the two modes are two tools.
    fn edit(&self, args: &Args, a: &Adjudication) -> ToolOutcome {
        let Some(scoped) = handle_for(a, "path") else {
            return failed("edit", "no adjudicated handle for `path`");
        };
        let Some(content) = text_arg(args, "content") else {
            return failed("edit", "`content` is required");
        };
        // **AN EMPTY `replacing` IS A SILENT, UNREQUESTED WRITE, and this is the one place it
        // could be caught.** `str::find("")` returns `Some(0)` for ANY string, so an empty
        // `replacing` matches trivially at offset 0 and the splice PREPENDS `content` to the file
        // — reported back as an ordinary successful `edit`, with a `+n −0` line that looks right.
        //
        // A model reaches an empty `replacing` by accident, not on purpose: a snippet extracted
        // from a `read` that returned nothing, a template that filled in blank, a variable that
        // was stripped. Every other failure in this executor at least SAYS something; this one
        // mutated a file and said "done".
        //
        // It is the write-then-refuse bug one step worse — that one failed loudly and wasted a
        // call; this one succeeds wrongly and damages a file.
        if text_arg(args, "replacing").is_some_and(|r| r.is_empty()) {
            return failed(
                "edit",
                "`replacing` is empty, which would match at the very start of the file and insert \
                 `content` there rather than replace anything. Give the exact snippet to replace, \
                 or use `write` to replace the whole file.",
            );
        }
        let Some(replacing) = text_arg(args, "replacing") else {
            return failed(
                "edit",
                "`replacing` is required: `edit` replaces one exact snippet. To create a file or \
                 replace all of it, use `write` instead.",
            );
        };

        // Held across the whole read-modify-write — see [`FileSystemTools::writes`]. Two `edit`s
        // to one file in one batch both read the original and the second overwrote the first.
        let _writing = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = match scoped.handle().try_clone() {
            Ok(f) => f,
            Err(e) => return failed("edit", e.to_string()),
        };
        let mut existing = String::new();
        if let Err(e) = file.read_to_string(&mut existing) {
            return failed("edit", e.to_string());
        }
        // **An empty file has nothing to patch, and `edit` no longer creates one to find out.**
        // `path` is a `WritePath`, so scoping has already opened it `CreateOrOpen` — the zero-byte
        // file exists by the time this runs and cannot be un-created here. What CAN be done is
        // refuse in a sentence that names the tool that would have worked.
        if existing.is_empty() {
            return failed(
                "edit",
                "the file is empty, so there is no snippet to replace. Use `write` with `path` \
                 and `content` to create it. (Naming a path here creates it at zero bytes before \
                 this tool runs, so it now exists and is empty.)",
            );
        }
        let Some(at) = existing.find(replacing) else {
            return failed("edit", replacing_miss(&existing, replacing));
        };
        // ── A `replacing` THAT APPEARS MORE THAN ONCE IS A GUESS, AND IT LOOKED LIKE SUCCESS ──
        //
        // `edit` replaced the FIRST occurrence and reported `+1 −1`. A rename whose old name
        // appears twelve times was therefore edited once, and the summary a model reads back is
        // indistinguishable from the summary of the edit it meant to make — so it moves on.
        //
        // The refusal is only worth making because it can say WHERE. That sentence exists because
        // `read` numbers now: `grep` already reported `path:line:text`, and the model can turn any
        // of these numbers into a window. Without the line numbers this would be a refusal with no
        // next step, which is worse than the wrong edit.
        let sites: Vec<usize> =
            existing.match_indices(replacing).map(|(i, _)| line_at(&existing, i)).collect();
        if sites.len() > 1 {
            let shown = sites.iter().take(MAX_EDIT_SITES_NAMED).map(usize::to_string)
                .collect::<Vec<_>>().join(", ");
            let rest = if sites.len() > MAX_EDIT_SITES_NAMED {
                format!(" and {} more", sites.len() - MAX_EDIT_SITES_NAMED)
            } else {
                String::new()
            };
            return failed(
                "edit",
                format!(
                    "`replacing` occurs {} times, at lines {shown}{rest}. `edit` changes ONE \
                     snippet, so it must appear exactly once — add a neighbouring line to make it \
                     unique, or edit each site in a separate call. `read` a range around the line \
                     you want. The file is unchanged.",
                    sites.len(),
                ),
            );
        }
        // ── NUMBERED `content` IS THE WORSE HALF: IT SUCCEEDS AND CORRUPTS THE FILE ───────────
        //
        // A prefixed `replacing` fails loudly. A prefixed `content` writes `   42\t` into the
        // source, reports `+n −m`, and nothing downstream can tell. That is the empty-`replacing`
        // prepend one step worse, at the same call site.
        //
        // **The threshold differs from `replacing`'s on purpose.** `replacing` fires at one line
        // because it is cross-checked against the file, so the named refusal cannot be wrong;
        // `content` has no file to check against, so the grammar carries the whole burden and two
        // consecutive numbered lines is the bar — a single one is plausibly genuine.
        //
        // **And `edit` refuses where `write` warns.** Splicing prefixes into existing code is never
        // intended; writing a file that legitimately contains `cat -n` output — a transcript, a
        // fixture, this crate's own tests — must stay possible, and `write` is where it happens.
        if count_lines(content) >= NUMBERED_CONTENT_LINES && strip_line_numbers(content).is_some() {
            return failed(
                "edit",
                format!(
                    "`content` carries `read`'s line-number prefix ({} characters then a tab in \
                     front of every line). Writing that into the file would put the numbers in the \
                     source, so it is refused — strip the prefixes and call `edit` again. If you \
                     really do mean to write numbered text, `write` does that and says so.",
                    LINE_NUMBER_WIDTH,
                ),
            );
        }
        let mut next = String::with_capacity(existing.len());
        next.push_str(&existing[..at]);
        next.push_str(content);
        next.push_str(&existing[at + replacing.len()..]);

        if let Err(e) = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| file.write_all(next.as_bytes()))
            .and_then(|()| file.flush())
        {
            return failed("edit", e.to_string());
        }
        ToolOutcome {
            summary: ResultSummary::new(vec![Metric::Diff {
                added: count_lines(content),
                removed: count_lines(replacing),
            }]),
            body: ToolBody::Inline(scoped.relative().to_string()),
            trust: TrustClass::AgentObserved,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }

    /// **List what is in a directory. There was no way to do this at all.**
    ///
    /// `grep` searches file CONTENTS and needs a pattern. `read` needs a path you already know.
    /// `bash` is `Irreversible`, so every attempt stops and asks the user. Asked what was inside
    /// `docs/requirements`, the model had exactly one option and it cost a prompt each time:
    /// journal seq 4884-4908 shows four `bash` calls — `dir "docs/requirements" /s`,
    /// `dir "docs\*" /b`, `list "docs"`, a GNU-`find` invocation — and 2026-08-27 shows three more
    /// (`ls -la docs/requirements/`, `ls docs/requirements`, `find ... | head -20`) declined for
    /// want of an approval surface. It then reported the directory **"appears empty"**. It has four
    /// files in it.
    ///
    /// That last part is the cost of the gap: with no way to look and no way to say "I could not
    /// look", a model fills the silence. The answer is not a better refusal, it is a tool.
    ///
    /// # Names only, and that is what makes it `Inert`
    ///
    /// This returns paths. It opens nothing and reads no bytes, so no file content — trusted or
    /// otherwise — passes through it, which is why it can be `Inert` and run without asking while
    /// `bash` cannot. Enumeration is bounded by [`WALK_FILE_CAP`] exactly as `grep`'s is, and the
    /// cap being hit is **stated**, because a listing that silently stops makes absence
    /// indistinguishable from truncation.
    fn glob(&self, args: &Args, a: &Adjudication) -> ToolOutcome {
        let Some(root) = handle_for(a, "path") else {
            return failed("glob", "no adjudicated handle for `path`");
        };
        // Absent means "everything here", which is the common case: `glob` with a path and no
        // pattern is "list this directory".
        let pattern = text_arg(args, "pattern").unwrap_or("*");

        let base = root.resolved().to_path_buf();
        // Same as `grep`: a file here is the mistake the description names, and it must not
        // look like an empty directory.
        if base.is_file() {
            return failed(
                "glob",
                "`path` is a file, and `glob` lists a DIRECTORY. Pass the directory that contains \
                 it — the file you named is already the answer.",
            );
        }
        // **The pattern is applied INSIDE the walk, so the cap counts files that could match.**
        // Filtering afterwards let 2,000 build artifacts fill every slot before the first `.rs`
        // file was reached, which made `pattern` narrow a set that had already stopped short of
        // the code. See [`collect`].
        //
        // The pattern matches the NAME when it has no slash, and the workspace-relative PATH when
        // it does — so `*.rs` means "any .rs anywhere under here" and `src/*.rs` means what it
        // looks like. Stated in the tool's description in full, so nothing is left to infer.
        let keep = |p: &Path| -> bool {
            if pattern == "*" {
                return true;
            }
            let subject = if pattern.contains('/') {
                match p.strip_prefix(&self.workspace) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => return false,
                }
            } else {
                p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            };
            glob_match(pattern, &subject)
        };
        let walk = collect(&base, WALK_FILE_CAP, &keep);
        let truncated = walk.truncated;

        let mut hits: Vec<String> = Vec::new();
        for candidate in &walk.files {
            let Ok(relative) = candidate.strip_prefix(&self.workspace) else { continue };
            hits.push(relative.to_string_lossy().replace('\\', "/"));
        }
        hits.sort();

        let found = hits.len() as u64;
        // **Bounded before it is joined, and returned as TEXT rather than as a hash.** 2,000 paths
        // is comfortably past `MAX_INLINE_BYTES`, so `glob(".", "*.rs")` on a real project used to
        // come back through `body_for` as a `ContentRef` — and a file reference cannot be
        // dereferenced, because `read`'s `ref` takes ids `web` issued. That is the identical defect
        // fixed for `read` at `fab045d`, in the tool right next to it. See [`body_for_window`].
        let mut listing = String::new();
        if found == 0 {
            // **An empty result says which of the two things happened.** "No matches" and "that
            // directory has nothing in it" are different facts, and a model that cannot tell them
            // apart invents one — which is exactly what happened live.
            //
            // **Built first and then given the same notices as any other result**, because there
            // is now a THIRD thing an empty listing can mean — the walk declined to enter the
            // directory the files were in — and that one is invisible unless it is stated. This
            // used to overwrite `listing`, which would have thrown the skip notice away in exactly
            // the case it exists for.
            listing = if walk.seen == 0 {
                format!("no files under this path at all (`{pattern}` was not the reason)")
            } else {
                // `walk.seen` rather than the kept set, which is empty by construction here. The
                // two facts an empty listing can carry — "nothing is here" and "things are here
                // and none matched" — need a count that survives the filter.
                format!("{} file(s) are under this path and none matched `{pattern}`", walk.seen)
            };
        }
        let mut listed = 0usize;
        for hit in &hits {
            if listing.len() + hit.len() + 1 > READ_WINDOW_BYTES {
                break;
            }
            if !listing.is_empty() {
                listing.push('\n');
            }
            listing.push_str(hit);
            listed += 1;
        }
        if listed < hits.len() {
            listing.push_str(&format!(
                "\n[{} of {found} matching paths are listed; the rest did not fit. Narrow \
                 `pattern`, or list a subdirectory.]",
                listed
            ));
        }
        if truncated {
            listing.push_str(&format!(
                "\n[enumeration stopped at {WALK_FILE_CAP} files; there may be more under this \
                 path than are listed]"
            ));
        }
        listing.push_str(&skipped_notice(&walk.skipped));
        let (body, _, preview) = body_for_window(listing);
        ToolOutcome {
            summary: ResultSummary::new(vec![Metric::Count { n: found, unit: "paths" }]),
            body,
            trust: TrustClass::AgentObserved,
            failed: false,
            wall_ms: 0,
            preview,
        }
    }

    /// **`grep` — search file CONTENTS with a real regular expression.** ADR-059.
    ///
    /// # The rename is a bug fix, not a preference
    ///
    /// This was `find`, and `SHELL_DESCRIPTION` told the model both of these in one paragraph:
    /// *"`ls`, `grep`, `find`, `head`, `sed`, `awk` … all work as you expect"* — asserting unix
    /// semantics, where `find` matches NAMES — and *"`glob` lists files, `find` searches inside
    /// them"*, asserting the inverse. Two contradictory definitions of one word, in one request
    /// body. The repo's own probe corroborates it: `tool_call_probe.rs` elicited a *contents*
    /// search with the prompt *"Find every occurrence of TODO"*, which is the English word for
    /// filenames. Renaming the tool without also deleting `grep` and `find` from that unix list
    /// would have left `grep` appearing twice in one description meaning two different things.
    ///
    /// # What it returns, and why each notice exists
    ///
    /// `path:line:text` for a match and `path-line-text` for a context line — ripgrep's
    /// convention. **The line keeps its indentation**, which is not cosmetic: `edit` requires
    /// `replacing` "copied verbatim from a `read` including indentation", and `find` trimmed every
    /// line it emitted, so a model that grepped a line and edited with what it got back was told
    /// *"`replacing` was not found in the file"* with nothing on screen explaining why.
    ///
    /// Four things were previously inferable and are now stated: the walk stopping at
    /// [`WALK_FILE_CAP`], the directories [`WALK_SKIP`] declined to enter, matches truncated at
    /// [`MAX_MATCH_LINES`], and files that were binary or longer than [`MAX_READ_BYTES`] — the last
    /// of which `find` discarded outright, so a binary file incremented nothing and the `files`
    /// denominator was simply wrong.
    fn grep(&self, args: &Args, a: &Adjudication, declared: &[PathGlob]) -> ToolOutcome {
        // ── AN ARGUMENT IT DOES NOT DECLARE IS REFUSED, BY NAME ──────────────────────────────
        //
        // The precedent is `recall`'s removed `payload_kind`: accepted, ignored, and never
        // reported, so a model that passed it believed it had filtered and got an unfiltered
        // answer. `grep` is the tool where that is likeliest, because every model has `-i`,
        // `--type`, `-l` and `head_limit` in its hands from somewhere else. A refusal naming the
        // four real parameters and `(?i)` costs one call; a silently case-sensitive answer to a
        // model that believes it asked for case-insensitive costs the turn.
        for (name, _) in args.iter() {
            if !GREP_PARAMS.contains(&name.as_str()) {
                return failed(
                    "grep",
                    format!(
                        "`{name}` is not a parameter of `grep`, and the search was NOT run with it \
                         ignored. `grep` takes exactly these: {}. To ignore case, start `pattern` \
                         with `(?i)` — `(?i)todo`. To restrict which files are opened, use `glob`, \
                         e.g. `glob: \"*.rs\"`. There is no way to ask for filenames only and no \
                         way to page through results; narrow the search instead.",
                        GREP_PARAMS.join(", ")
                    ),
                );
            }
        }

        let Some(pattern) = text_arg(args, "pattern") else {
            return failed("grep", "`pattern` is required");
        };
        // **An empty pattern matched every line of every file** under the old substring engine
        // (`str::contains("")` is always true), and it does the same as a regex. A model arrives at
        // an empty pattern the same way it arrives at an empty `replacing`: a stripped variable, a
        // bad split, never on purpose.
        if pattern.is_empty() {
            return failed(
                "grep",
                "`pattern` is empty, which matches every line of every file. Give the regular \
                 expression to search for, or use `glob` to list files without searching inside \
                 them.",
            );
        }

        // ── THE PATTERN COMPILES, OR THE CALL IS REFUSED WITH THE SYNTAX ERROR ───────────────
        //
        // **There is deliberately no fallback to a literal search.** That fallback is the
        // permissive default this project keeps deleting: a model that wrote `foo(bar)` meaning
        // the literal text would get a *different* answer from the one it asked for, with no
        // signal at all that its pattern had been reinterpreted. The error names the fix, because
        // the model cannot see this code.
        //
        // `size_limit`/`dfa_size_limit` are the ONLY resource guard needed. Catastrophic
        // backtracking — `(a+)+$` — is impossible in this engine, which is finite-automata based
        // and never backtracks; the residual cost is a pattern that is expensive to COMPILE, and
        // that is what these two bound. Over-limit is an error the model can read and fix, not a
        // hang and not a kill.
        let re = match regex::RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                return failed(
                    "grep",
                    format!(
                        "`pattern` is not a valid regular expression, and it was NOT searched for \
                         as plain text instead: {e}\nTo search for text that CONTAINS one of \
                         `. * + ? ( ) [ ] {{ }} | ^ $ \\`, put a backslash before each one — \
                         `Cargo\\.toml`, `foo\\(bar\\)`. There is no `\\Q...\\E`. Backreferences \
                         and lookaround do not exist in this engine and always fail here."
                    ),
                );
            }
        };

        // ── `context`, VALIDATED RATHER THAN CLAMPED ────────────────────────────────────────
        //
        // A `Text` arm as well as `Integer`, because the WIRE decides the type and the model does
        // not: `ollama.rs` maps a JSON number to `Integer` and the JSON string `"3"` to
        // `Text("3")`, and which of the two a 9B model emits for one intent is a coin flip. `"3"`
        // means three and nothing else, so reading it is honesty about the transport rather than
        // inference about intent — anything that is not a number is still refused, by name.
        let context: usize = match args.get("context") {
            None => 0,
            Some(v) => {
                let n = match v {
                    ArgValue::Integer(n) => Some(*n),
                    ArgValue::Text(s) if !s.trim().is_empty() => s.trim().parse::<i64>().ok(),
                    _ => None,
                };
                match n {
                    // Refused rather than clamped: a silently lowered argument is a model
                    // believing it asked for something it did not get.
                    Some(n) if (0..=MAX_GREP_CONTEXT).contains(&n) => n as usize,
                    Some(n) => {
                        return failed(
                            "grep",
                            format!(
                                "`context` is {n}, and it must be a whole number from 0 to \
                                 {MAX_GREP_CONTEXT}. It was NOT reduced to fit — say what you want \
                                 and call again."
                            ),
                        )
                    }
                    None => {
                        return failed(
                            "grep",
                            format!(
                                "`context` must be a whole number from 0 to {MAX_GREP_CONTEXT} — \
                                 how many lines above and below each match to return as well. Omit \
                                 it for matching lines only."
                            ),
                        )
                    }
                }
            }
        };

        // An empty `glob` matches no file at all, so it would return a silent zero — the same
        // shape as the empty `pattern` above, and refused for the same reason.
        let file_glob = match args.get("glob") {
            None => None,
            Some(v) => match v.as_text() {
                Some(g) if !g.is_empty() => Some(g),
                _ => {
                    return failed(
                        "grep",
                        "`glob` is empty, which matches no file at all. Give a pattern like \
                         `*.rs`, or omit `glob` to search every file.",
                    )
                }
            },
        };

        let Some(root) = handle_for(a, "path") else {
            return failed("grep", "no adjudicated handle for `path`");
        };

        // Enumeration produces candidate NAMES. Every one is then opened through the scope, so
        // nothing this loop reads bypassed the wall.
        let base = root.resolved().to_path_buf();
        // **A file where a directory was asked for produced `0 results · 0 files` — the same
        // answer an empty directory gives.** `collect` swallows `read_dir`'s error on a file
        // (`let Ok(entries) = read_dir(..) else { continue }`), so the walk simply found nothing.
        // The manifest says "the DIRECTORY to search"; nothing enforced it, and the model that made
        // exactly the mistake the manifest names got no signal at all.
        if base.is_file() {
            return failed(
                "grep",
                "`path` is a file, and `grep` searches a DIRECTORY. Pass the directory that \
                 contains it, or use `read` to look at this one file.",
            );
        }
        // **The `glob` filter runs INSIDE the walk, and that is what makes it worth having.**
        // As a filter over the walk's OUTPUT it would be decorative: the cap bounds enumeration,
        // so 2,000 build artifacts still fill every slot before the first `.rs` file is reached,
        // and `glob: "*.rs"` would then narrow a set that had already stopped short of the code.
        //
        // The same matcher `glob` uses, not a second one — `glob_match` already implements the
        // exact name-versus-path rule this parameter's description promises, and it is already
        // tested; a private copy here would be a second definition of a pattern language that
        // exists to stop exactly that.
        let keep = |p: &Path| -> bool {
            let Some(g) = file_glob else { return true };
            let subject = if g.contains('/') {
                match p.strip_prefix(&self.workspace) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => return false,
                }
            } else {
                p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            };
            glob_match(g, &subject)
        };
        let walk = collect(&base, WALK_FILE_CAP, &keep);

        let mut emitted: Vec<String> = Vec::new();
        let mut emitted_bytes = 0usize;
        let mut per_file: Vec<(String, usize)> = Vec::new();
        let mut total_matches = 0usize;
        let mut scanned = 0u64;
        let mut not_text = 0usize;
        let mut part_read: Vec<String> = Vec::new();
        let mut budget_spent = false;

        for candidate in &walk.files {
            let Ok(relative) = candidate.strip_prefix(&self.workspace) else { continue };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let Ok(scoped) = self.scope.open(declared, &self.workspace, &relative, Access::Read)
            else {
                // Refused by the wall — a link, an undeclared subtree, an unreadable name. It is
                // skipped rather than reported as a match, and the count says how many were read.
                continue;
            };
            let mut text = String::new();
            // **`ReadOutcome` is read rather than discarded, and that is a defect fix.** `find`
            // dropped it, so a file capped at `MAX_READ_BYTES` was searched in its first 4 MiB
            // silently, and a BINARY file pushed no text yet still incremented `scanned` — the
            // denominator in `N results · M files` counted files nobody had searched.
            let outcome = match clone_and_read(&scoped, &mut text) {
                Ok(o) => o,
                Err(_) => continue,
            };
            match outcome {
                ReadOutcome::NotText { .. } => {
                    not_text += 1;
                    continue;
                }
                ReadOutcome::Capped => part_read.push(relative.clone()),
                ReadOutcome::Whole => {}
            }
            scanned += 1;

            let lines: Vec<&str> = text.lines().collect();
            let matched: Vec<usize> =
                lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i).collect();
            if matched.is_empty() {
                continue;
            }
            total_matches += matched.len();
            per_file.push((relative.clone(), matched.len()));
            if budget_spent {
                // Still COUNTED, no longer emitted. That is what makes the degraded result a count
                // of everything rather than a count of whatever happened to fit.
                continue;
            }

            // `context` expands each match into a window; overlapping windows are merged so a line
            // is emitted once, in order, with matches still distinguishable from their
            // surroundings by the separator.
            let mut wanted: Vec<(usize, bool)> = Vec::new();
            let mut next = 0usize;
            for &m in &matched {
                let from = m.saturating_sub(context).max(next);
                let to = (m + context).min(lines.len().saturating_sub(1));
                for i in from..=to {
                    wanted.push((i, matched.binary_search(&i).is_ok()));
                }
                next = to + 1;
            }
            for (i, is_match) in wanted {
                if emitted.len() >= MAX_MATCH_LINES || emitted_bytes >= READ_WINDOW_BYTES {
                    budget_spent = true;
                    break;
                }
                let sep = if is_match { ':' } else { '-' };
                let line = clip_line(lines[i]);
                let rendered = format!("{relative}{sep}{}{sep}{line}", i + 1);
                emitted_bytes += rendered.len() + 1;
                emitted.push(rendered);
            }
        }

        let found = total_matches as u64;
        let mut body = emitted.join("\n");
        if found == 0 {
            // Three different facts, and a model that cannot tell them apart invents one.
            body = if walk.seen == 0 {
                format!("no files under this path at all (`{pattern}` was not the reason)")
            } else if scanned == 0 {
                // Nothing was READ, and the two reasons are different actions for the model:
                // `glob` matched no file, or every file that matched was not text.
                match file_glob {
                    Some(g) => format!(
                        "no file was searched: {} file(s) are under this path, and none of them \
                         matched `glob: \"{g}\"` or could be read as text",
                        walk.seen
                    ),
                    None => format!(
                        "no file was searched: {} file(s) are under this path and none of them \
                         could be read as text",
                        walk.seen
                    ),
                }
            } else {
                format!("no line matched `{pattern}` in the {scanned} file(s) searched")
            };
        }
        if budget_spent {
            let mut worst = per_file.clone();
            worst.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            worst.truncate(NAMES_IN_A_NOTICE);
            body.push_str(&format!(
                "\n[{found} matches in {} file(s); the first {} lines are shown and the rest were \
                 counted, not returned. Most matches: {}. Tighten `pattern`, set `glob`, or search \
                 a subdirectory to see the rest.]",
                per_file.len(),
                emitted.len(),
                worst.iter().map(|(p, n)| format!("{p} ({n})")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !part_read.is_empty() {
            part_read.sort();
            // **The list is bounded, and finding that out took running it on a real tree.** With
            // `WALK_SKIP` disabled as a control, this notice listed 250 `.rlib` and `.pdb` paths in
            // one line — a context flood in the notice that exists to prevent one. A notice is not
            // exempt from the caps the result is under.
            let total = part_read.len();
            part_read.truncate(NAMES_IN_A_NOTICE);
            let more = if total > NAMES_IN_A_NOTICE {
                format!(" and {} more", total - NAMES_IN_A_NOTICE)
            } else {
                String::new()
            };
            body.push_str(&format!(
                "\n[{total} file(s) were searched only as far as {MAX_READ_BYTES} bytes, so \
                 anything later in them was not looked at: {}{more}]",
                part_read.join(", ")
            ));
        }
        if not_text > 0 {
            body.push_str(&format!(
                "\n[{not_text} file(s) were not text and were not searched; they are excluded from \
                 the file count.]"
            ));
        }
        if walk.truncated {
            // **`find` never had this**, and `glob` did — so `find`'s zero on a real repository was
            // reported as a fact about the workspace. The number is `format!`ed from the constant
            // so the sentence cannot drift from the cap that produced it.
            body.push_str(&format!(
                "\n[the search stopped after {WALK_FILE_CAP} files and there is more under this \
                 path. It did not reach everything — narrow it with `glob`, e.g. `glob: \"*.rs\"`, \
                 or search a subdirectory instead of \".\".]"
            ));
        }
        body.push_str(&skipped_notice(&walk.skipped));

        // `body_for_window`, not `body_for`: this is already bounded above, and `body_for` would
        // hand back a `ContentRef` whose hash the model cannot dereference — with a preview saying
        // "the tool read the whole file", which is not even true of a search.
        let (body, _, preview) = body_for_window(body);
        ToolOutcome {
            summary: ResultSummary::new(vec![
                Metric::Count { n: found, unit: "results" },
                Metric::Count { n: scanned, unit: "files" },
            ]),
            body,
            trust: TrustClass::AgentObserved,
            failed: false,
            wall_ms: 0,
            preview,
        }
    }

    fn bash(&self, args: &Args, a: &Adjudication) -> ToolOutcome {
        let Some(command) = text_arg(args, "command") else {
            return failed("bash", "`command` is required");
        };
        // `cwd` is a declared Path parameter, so a handle exists whenever it was supplied. It is
        // held open for the whole spawn — see the crate header for why that is the mechanism on
        // Windows rather than a convenience.
        let cwd = handle_for(a, "cwd");
        let dir = cwd.map(|c| c.resolved().to_path_buf()).unwrap_or_else(|| self.workspace.clone());

        // No clock is read here, and that is not an oversight. Section 4.5 is binding on every
        // path reachable from the section 4 interfaces, and `marlowe/tests/determinism_guard.rs`
        // enforces it across the workspace — it caught an `Instant::now()` in this very function.
        // The LOOP measures the call with its injected `ClockSource`, which is the clock a test
        // can hold still; an executor reading the wall clock would make a decay-dependent result
        // irreproducible and would do it from a component nobody would think to look in.
        let out = spawn_shell(command, &dir, cwd);

        // The handle is dropped only now, after the child has been spawned and waited on.
        drop(cwd);

        match out {
            Err(e) => failed("bash", e.to_string()),
            Ok(ShellRun { code, text, stopped, flooded }) => {
                // **Told in the body, not only in a metric.** A metric is a summary line; the model
                // reads the body. A killed command whose output merely ends is indistinguishable
                // from one that finished, and acting on a truncated prefix as though it were the
                // whole answer is the failure worth preventing here.
                let text = match (stopped, flooded) {
                    (true, _) => format!(
                        "{text}\n[the harness stopped this command after {BASH_TIMEOUT_MS} ms. \
                         The output above is what it had produced by then, and the command did \
                         not finish.]"
                    ),
                    (_, true) => format!(
                        "{text}\n[the harness stopped this command after it produced more than \
                         {MAX_SHELL_OUTPUT_BYTES} bytes on one stream. The output above is a \
                         prefix, and the command did not finish.]"
                    ),
                    _ => text,
                };
                let (body, _, preview) = body_for(text);
                let lines = text_lines(&body);
                let mut metrics = vec![Metric::Count { n: lines, unit: "lines" }];
                if stopped {
                    metrics.insert(0, Metric::State("timed-out"));
                }
                if flooded {
                    metrics.insert(0, Metric::State("truncated"));
                }
                if code != 0 {
                    metrics.insert(0, Metric::Exit { code });
                }
                ToolOutcome {
                    summary: ResultSummary::new(metrics),
                    body,
                    // A shell's stdout is bytes from whatever it ran. The harness observed the
                    // exit code; it did not author the output.
                    trust: TrustClass::AgentObserved,
                    failed: code != 0,
                    // Filled in by the loop from its `ClockSource`. See above.
                    wall_ms: 0,
                    preview,
                }
            }
        }
    }
}

impl<S: PathScope> FileSystemTools<S> {
    /// Read a fetched document back out of the store. **This is where untrusted text re-enters.**
    ///
    /// `web` no longer returns page content, so this is the ONLY way a fetched document reaches a
    /// run — which is what makes the boundary auditable: one function, one trust class, one place
    /// layer 1 has to fire. The result is `UntrustedContent`, so the loop condenses it through a
    /// quarantined reader exactly as before.
    ///
    /// The consequence worth stating: a research pass that fetches thirty pages and reads three of
    /// them pays for **three**, not thirty.
    fn read_ref(&self, id: &str, args: &Args) -> ToolOutcome {
        let Some(document) = self.store.get(id) else {
            return failed(
                "read",
                format!(
                    "no fetched document has ref {id:?}. Refs are reported by `web` and last for this session; fetch the URL again if you need it."
                ),
            );
        };
        let mut text = crate::corpus::render(&document);
        // The `ref` path takes the same treatment as the `path` path: a malformed range is an
        // error rather than a silent whole-document read, and a range that selects nothing is
        // reported instead of rendering as an empty document.
        if let Some(range) = text_arg(args, "range") {
            let of = text.lines().count();
            match slice_lines(&text, range) {
                Ok((sliced, _)) if sliced.is_empty() && of > 0 => {
                    return failed(
                        "read",
                        format!(
                            "`range` \"{range}\" selected no lines: the document has {of}. Ask                              for a range inside 1-{of}, or omit `range` for all of it."
                        ),
                    );
                }
                // **Deliberately dropped: the start line.** `number_lines` is not called on this
                // path and must not be — see the note in `read`. Binding it here would make the
                // ingredient available at the one call site that must not have it.
                Ok((sliced, _)) => text = sliced,
                Err(why) => return failed("read", why),
            }
        }
        let chars = text.len() as u64;
        let (body, _, preview) = body_for(text);
        ToolOutcome {
            summary: ResultSummary::with_detail(
                vec![
                    Metric::State("doc"),
                    Metric::Count { n: chars, unit: "chars" },
                ],
                format!("{} · {}", document.format.as_str(), id),
            ),
            body,
            // **Unchanged by the round trip through the store.** A document does not become
            // trustworthy by being written down and read back; storing it is not laundering.
            trust: TrustClass::UntrustedContent,
            failed: false,
            wall_ms: 0,
            preview,
        }
    }

    /// `web` — fetch. ADR-031.
    ///
    /// **Egress was already decided before this runs.** The adjudicator checked the URL's host
    /// against the run's `EgressPolicy` intersected with the manifest's declared hosts, and a
    /// refusal never reaches an executor. This function does not re-check and must not: a second
    /// egress implementation inside a networking call site is how the two sides come to disagree.
    ///
    /// **Redirects are NOT followed here, and that is the security design rather than a
    /// shortcut.** A redirect is a second egress destination chosen by the host being fetched —
    /// untrusted content selecting a target. Following it inside this function would mean
    /// re-implementing the allowlist check where the taint layer cannot see it. Instead the
    /// `Location` is returned as a *result*, and following it requires a fresh `web` call that
    /// goes through the real adjudicator, with the real taint, like any other target.
    ///
    /// The consequence is deliberate and worth naming: once this run has read a page, ADR-023's
    /// latch blocks model-composed targets, so **the model generally cannot follow the redirect**.
    /// That is the trifecta break working, not a defect. The path is a quarantined reader.
    ///
    /// **Nothing here parses the body.** Bytes and a content type go back; extraction is a
    /// separate module and a separate session.
    fn web(&self, a: &Args) -> ToolOutcome {
        let Some(url) = a.get("url").and_then(ArgValue::as_text) else {
            // `query` exists in the manifest for a search this build does not have. Saying so
            // plainly beats a refusal the model reads as "the URL was malformed".
            return failed(
                "web",
                "`url` is required. This build fetches a single URL and does not search, so \
                 `query` alone cannot be answered — supply a full https:// URL.",
            );
        };

        let target = match marlowe_net::Target::parse(url) {
            Ok(t) => t,
            Err(e) => return failed("web", e.to_string()),
        };

        let fetched = match marlowe_net::fetch(&target) {
            Ok(res) => res,
            Err(e) => return failed("web", e.to_string()),
        };
        let status = fetched.status;
        let content_type = fetched.content_type.clone();
        let wire = fetched.wire_bytes;
        let raw_bytes = fetched.bytes.len();
        let outcome = crate::corpus::read(url, fetched);
        self.web_outcome(url, status, content_type.as_deref(), raw_bytes, wire, outcome)
    }

    /// Turn one [`corpus::Outcome`](crate::corpus::Outcome) into the result the model sees.
    ///
    /// **Split out of `web` so it has a test.** Audit finding A3 asked for a hostile `Location`
    /// case *"asserting on the `ToolOutcome`, not on `DocumentRef`"* — and there was no way to
    /// reach a `ToolOutcome` without a live fetch, so every existing boundary test asserted one
    /// level below the thing that shipped. That is the shape this project keeps logging: the weaker
    /// claim is true, cheap and adjacent. `web` now does the network and this does the rest, so the
    /// bytes the model receives are directly assertable.
    pub fn web_outcome(
        &self,
        url: &str,
        status: u16,
        content_type: Option<&str>,
        raw_bytes: usize,
        wire: usize,
        outcome: crate::corpus::Outcome,
    ) -> ToolOutcome {
        match outcome {
            // **The `Location` header does not reach here verbatim.** Audit finding A3: this arm
            // used to interpolate the raw header at `AgentObserved`, straight into the parent's
            // window, with the trust floor unmoved — the one genuine layer-1 bypass in the round.
            // `RedirectTo` is the validated form and `render()` is the only way it becomes text.
            crate::corpus::Outcome::Redirect { status, location, .. } => ToolOutcome {
                summary: ResultSummary::with_detail(
                    vec![Metric::State("redirect")],
                    // The host only. A summary line is the most quotable thing a tool produces.
                    format!("{status} -> {}", location.host().unwrap_or("an unusable location")),
                ),
                body: ToolBody::Inline(format!(
                    "{url} redirected {}. It was NOT followed: a redirect target \
                     is chosen by the site, so it is checked like any other target. Call \
                     `web` again with that URL if it is what you want.",
                    location.render()
                )),
                // The harness observed the status and authored every character of this body
                // except a validated host and a path that cannot carry prose.
                trust: TrustClass::AgentObserved,
                failed: false,
                wall_ms: 0,
                preview: None,
            },

            // **Extraction failed, so nothing readable exists — and the raw bytes are NOT a
            // fallback.** Handing over undecodable input would put the exact material this
            // change exists to remove back into the window, on the one path nobody tests.
            //
            // **Nor is the parser's error message a fallback.** Audit finding A4: `detail` is built
            // from `pdf_extract`'s `Display` and from downcast panic payloads, both of which carry
            // document-derived text. `kind` is a harness constant; `detail` stays in the summary,
            // which is journalled.
            crate::corpus::Outcome::Unreadable { kind, .. } => ToolOutcome {
                summary: ResultSummary::with_detail(
                    vec![Metric::State("unreadable"), Metric::Bytes { n: raw_bytes as u64 }],
                    format!("{status} {}", normalize_content_type(content_type)),
                ),
                body: ToolBody::Inline(format!(
                    "{url} returned {raw_bytes} bytes that could not be turned into text \
                     ({kind}). Nothing was read."
                )),
                trust: TrustClass::AgentObserved,
                failed: true,
                wall_ms: 0,
                preview: None,
            },

            crate::corpus::Outcome::Unreachable { detail, .. } => failed("web", detail),

            crate::corpus::Outcome::Read { document, wire_bytes, .. } => {
                // **Stored whole, before anything is summarised.** The reference this produces
                // carries a hash and counts and no bytes of the page — see
                // `marlowe_extract::store`, where that property is asserted directly.
                let reference = self.store.put(url, wire_bytes, document);
                // ── ADR-042. THE PAGE DOES NOT COME BACK. ────────────────────────────────────
                //
                // What crosses is a `DocumentRef`: a hash and a set of counts, every one of them
                // measured by the harness. There is no substring of the page anywhere in it — no
                // title, no headings, no description, no snippet — which is why this result is
                // `AgentObserved` rather than `UntrustedContent`.
                //
                // **That reclassification is the whole win, and it is not a relaxation.** Layer 1
                // condenses untrusted results because attacker *prose* is crossing; a page can
                // influence these numbers but cannot author them, and a number cannot carry an
                // instruction. So a fetch now costs **no model call at all**, and the agent can
                // decide what to read while having read nothing.
                //
                // The content is still reachable, through exactly one door: `read(ref=…)`, which
                // returns it at `UntrustedContent` and goes through the quarantined reader.
                // ── THE STATUS HAS TO REACH THE MODEL, AND IT DID NOT ────────────────────
                //
                // ADR-049 §5. The exact code was formatted into `ResultSummary::detail` by every
                // arm of this function -- and **`detail` has no reader in the shipped product**.
                // It is §8's expansion payload, nothing expands it, and `render()` walks the
                // metrics only. So `web` computed the status, journalled it, and showed the model
                // the bare word `http`: 400, 403, 404 and 503 were one indistinguishable state,
                // and a malformed query looked like an outage.
                //
                // Same family as `inline_threshold_bytes: 0` -- a control that is declared, is
                // correct, and that no line of code reads. It was found by an assertion on what
                // the model receives failing, which is the only place it could have been found.
                //
                // Fixed in the two places the model actually looks: a harness-authored sentence
                // at the head of the body, and a class in the state metric. **Both are harness
                // constants and a number the harness measured** -- ADR-042's rule is unchanged,
                // and this stays `AgentObserved` because a status line is not the page.
                let text = reference.render();
                let text = match status {
                    s if s >= 500 => format!(
                        "The server returned HTTP {s}. What follows is what came back with that \
                         error, not the document you asked for.\n{text}"
                    ),
                    s if s >= 400 => format!(
                        "The server returned HTTP {s}: it refused this request. What follows is \
                         the error page, not the document you asked for.\n{text}"
                    ),
                    _ => text,
                };
                let chars = reference.chars as u64;
                let (body, _, preview) = body_for(text);
                ToolOutcome {
                    summary: ResultSummary::with_detail(
                        vec![
                            // `State` is `&'static str`, so the CLASS is what a metric can carry
                            // and the exact code goes in the body above. Three constants beat one
                            // formatted string here: no new `Metric` variant, and CONTRACTS.md
                            // §8's pinned enum is untouched.
                            Metric::State(match status {
                                s if s >= 500 => "http 5xx",
                                s if s >= 400 => "http 4xx",
                                _ => "ok",
                            }),
                            Metric::Count { n: chars, unit: "chars" },
                        ],
                        format!(
                            "{status} {} · {} · {} B wire · read it with ref {}",
                            reference.format.as_str(),
                            content_type.as_deref().unwrap_or("no content-type"),
                            wire,
                            reference.hash
                        ),
                    ),
                    body,
                    // **`AgentObserved`, and the justification is the absence of content, not a
                    // judgement about the site.** The harness fetched some bytes and measured
                    // them; what it is reporting is its own measurements. The moment any
                    // attacker-authored substring is added to this result, this must go back to
                    // `UntrustedContent` — `marlowe-extract`'s `store` tests assert the absence
                    // that licenses it, and `web_returns_no_page_content` asserts it here.
                    trust: TrustClass::AgentObserved,
                    failed: status >= 400,
                    wall_ms: 0,
                    preview,
                }
            }
        }
    }
}

impl<S: PathScope> ToolHost for FileSystemTools<S> {
    /// **The five this host actually has arms for.** Kept beside the match below so the two
    /// cannot drift; `every_declared_tool_has_a_match_arm` asserts they agree.
    fn executes(&self) -> Vec<marlowe_tools::ToolId> {
        ["read", "write", "edit", "glob", "grep", "bash", "web"]
            .iter()
            .map(|t| marlowe_tools::ToolId::new(*t))
            .collect()
    }

    fn execute(&mut self, tool: &ToolId, args: &Args, adjudication: &Adjudication) -> ToolOutcome {
        self.dispatch(tool, args, adjudication)
    }

    /// **Run the batch concurrently.** This is what makes *"fetch these 30 sites"* one round of
    /// work instead of thirty.
    ///
    /// # What licenses the concurrency, stated precisely
    ///
    /// Not the fact that the model emitted the calls together — that argument is about data flow
    /// and says nothing about side effects. The loop only ever hands this method a group of calls
    /// whose manifests declare `ConsequenceLevel::Inert` — *"pure reads, no side effects"* — so
    /// `edit` and `bash` are never in a group of more than one and never overlap with anything.
    /// See `Engine::run_group`.
    ///
    /// Every item was adjudicated individually before arriving here, including the per-host egress
    /// check: thirty URLs is thirty separate allowlist decisions, and a refused one never reaches
    /// this method. **Nothing about permission is batched.**
    ///
    /// # Threads, not an executor
    ///
    /// These calls are network-bound, so the useful width is well above the core count and the
    /// threads spend their lives parked on sockets. `marlowe-net`'s `Client` is `Send + Sync` and
    /// holds the shared connection pool, DNS cache and TLS session cache, so concurrent fetches to
    /// one host reuse connections rather than racing to open thirty.
    fn execute_batch(&mut self, items: &[marlowe_loop::BatchItem<'_>]) -> Vec<ToolOutcome> {
        if items.is_empty() {
            return Vec::new();
        }

        // **STANDING RULE: extraction never runs on the caller's thread, not even for one call.**
        //
        // The obvious optimisation here is to run a single-item batch inline and skip the thread.
        // That is exactly what must not happen: a batch of one is the common case, and a single
        // `web` call on a 2 MB PDF is hundreds of milliseconds of parsing. Run inline, that lands
        // on the daemon's thread and the interface stops responding for the duration — the user
        // sees a frozen surface and no indication why.
        //
        // The caller still *blocks* here, because `ToolHost` is synchronous by design. What it no
        // longer does is *perform the parse*. On a machine with this many cores, spending one
        // thread-spawn (tens of microseconds) to keep heavy CPU work off the thread that draws the
        // screen is not a trade worth thinking about twice.
        let workers = items.len().min(batch_concurrency());
        let slots: Vec<Mutex<Option<ToolOutcome>>> =
            (0..items.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        // `&*self`, so every worker shares one host. `PathScope` is already `Send + Sync` and
        // every executor takes `&self`, which is what makes this a borrow rather than a redesign.
        let me: &Self = self;

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() {
                        break;
                    }
                    let item = &items[i];
                    let outcome = me.dispatch(item.tool, item.args, item.adjudication);
                    *slots[i].lock().expect("batch slot poisoned") = Some(outcome);
                });
            }
        });

        // **In input order.** The loop attributes results to calls positionally, so a reordering
        // here would hand the model one call's result under another call's id.
        slots
            .into_iter()
            .map(|s| {
                s.into_inner()
                    .expect("batch slot poisoned")
                    .expect("every slot is filled before the scope ends")
            })
            .collect()
    }
}

/// How wide to run a batch on **this** machine. **Derived, never hardcoded.**
///
/// Delegates to [`marlowe_net::io_concurrency`] rather than keeping a second constant: a batch
/// here is `Inert` calls, overwhelmingly `web` fetches, so it is the same I/O-bound question and
/// two answers to it would drift.
///
/// The CPU half needs no arithmetic at all: extraction runs on `rayon`'s global pool, already
/// sized to the core count, and each worker extracts its own document — so network waits and
/// parsing overlap without a second pool to tune.
pub fn batch_concurrency() -> usize {
    marlowe_net::io_concurrency()
}

impl<S: PathScope> FileSystemTools<S> {
    /// The one dispatch table. `execute` and `execute_batch` both route through it so the serial
    /// and concurrent paths cannot come to disagree about what a tool name means.
    fn dispatch(&self, tool: &ToolId, args: &Args, adjudication: &Adjudication) -> ToolOutcome {
        // The declared globs are the manifest's; the adjudicator already matched the model's
        // argument against them. `grep` needs them again for its own opens.
        let declared = [PathGlob::new("./**")];
        match tool.as_str() {
            "read" => self.read(args, adjudication),
            "write" => self.write(args, adjudication),
            "edit" => self.edit(args, adjudication),
            "glob" => self.glob(args, adjudication),
            "grep" => self.grep(args, adjudication, &declared),
            "bash" => self.bash(args, adjudication),
            "web" => self.web(args),
            other => failed("tool", format!("`{other}` has no executor in this build")),
        }
    }
}

/// How much of a file `read` will hold in memory.
///
/// Audit finding A8: `read_to_string` had no cap, `grep` does the same for up to
/// [`WALK_FILE_CAP`] files, and `MAX_INLINE_BYTES` gates only what reaches the model — the whole
/// file was already resident by then. Sized well above any source file and well below anything that
/// threatens the process.
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// What a bounded read found. Not a `bool`, because "binary" is a third answer and collapsing it
/// into `Err` is what made `read` unable to say anything at all about a non-UTF-8 file.
enum ReadOutcome {
    Whole,
    Capped,
    NotText { bytes: u64 },
}

fn clone_and_read(scoped: &ScopedPath, into: &mut String) -> std::io::Result<ReadOutcome> {
    let mut f = scoped.handle().try_clone()?;
    f.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    // One byte past the cap, so "exactly at the cap" and "longer than the cap" are distinguishable
    // — otherwise a file of exactly MAX_READ_BYTES would be reported as truncated when it is not.
    let taken = (&mut f).take(MAX_READ_BYTES + 1).read_to_end(&mut bytes)?;
    let capped = taken as u64 > MAX_READ_BYTES;
    if capped {
        bytes.truncate(MAX_READ_BYTES as usize);
    }
    match String::from_utf8(bytes) {
        Ok(s) => {
            into.push_str(&s);
            Ok(if capped { ReadOutcome::Capped } else { ReadOutcome::Whole })
        }
        Err(e) => {
            // A cap can land mid-character, and that is not a binary file. Keep the valid prefix.
            let valid = e.utf8_error().valid_up_to();
            if capped && valid > 0 {
                let raw = e.into_bytes();
                into.push_str(std::str::from_utf8(&raw[..valid]).unwrap_or_default());
                Ok(ReadOutcome::Capped)
            } else {
                Ok(ReadOutcome::NotText { bytes: taken as u64 })
            }
        }
    }
}

fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        s.lines().count() as u32
    }
}

/// Select `range` (`"a-b"`, 1-based, inclusive) out of `text`.
///
/// **Audit finding A7, and the arithmetic is the whole finding.** `take(b.saturating_sub(a) + 1)`
/// had an unguarded `+ 1`: `range = "1-18446744073709551615"` makes the subtraction `usize::MAX` and
/// the addition overflows — a **panic** in debug and test, a silent wrap to `take(0)` in release,
/// where `[profile.release]` sets no `overflow-checks`.
///
/// The panic is the serious half. `dispatch` runs inside a `scope.spawn` closure and
/// `std::thread::scope` re-raises on join, so one bad `range` aborted the **entire batch** and
/// unwound out through `run_group`, with no `catch_unwind` anywhere between here and the daemon.
///
/// And it is reachable by design, not by accident: `range` is a declared **Payload**, so §9
/// explicitly permits untrusted content to choose this value with no permission check in the way.
/// Injected prose could pick the number directly.
/// `*` and `?` only, and deliberately no more.
///
/// `*` matches any run of characters including none; `?` matches exactly one. **No `**`, no `[a-z]`,
/// no `{a,b}`** — every one of those is a spelling some shells accept and others do not, and a
/// pattern language a model has to guess at is the thing this tool exists to stop. What it does is
/// stated in the tool's description in full, so there is nothing left to infer.
///
/// Case-insensitive, because this ships on Windows where the filesystem is, and a `*.MD` that
/// silently found nothing would be indistinguishable from an empty directory.
fn glob_match(pattern: &str, subject: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => {
                // Match zero characters, or one more and try again. Bounded by the subject's
                // length because each recursion consumes one.
                //
                // **`*` does not cross a `/`.** Without this it did, and `src/*.rs` matched
                // `src/deep/other.rs` — contradicting the tool's own description, which promises
                // that a pattern with a slash finds only what is directly in that directory. Found
                // by `glob_pattern_with_a_slash_matches_the_path`, which is why that test names
                // the directory it must NOT reach into rather than only what it must find.
                //
                // A name-only pattern is unaffected: a file name contains no separator, so the
                // guard never fires on `*.rs`, which is the spelling for "anywhere beneath".
                go(&p[1..], s) || (!s.is_empty() && s[0] != '/' && go(p, &s[1..]))
            }
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let s: Vec<char> = subject.to_lowercase().chars().collect();
    go(&p, &s)
}

/// Put `{:>6}\t` in front of every line, counting from `first_line` — the ABSOLUTE file line.
///
/// **Absolute is the whole feature.** `read(range: "500-600")` returns `   500\t…`; window-relative
/// numbering would look authoritative and be wrong, which is worse than no numbering at all,
/// because a `grep` hit could then not be turned into a location.
///
/// **Line terminators are re-emitted verbatim**, which is why this walks `split_inclusive('\n')`
/// rather than `lines()`. `str::lines` strips a trailing `\r`, so a CRLF file routed through it
/// comes back LF — and a `replacing` copied out of that read can never match the file it came
/// from. That defect pre-dates numbering (`read`'s window path did exactly this) and numbering
/// would have made it universal, since every read now decomposes into lines.
pub fn number_lines(text: &str, first_line: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (i, line) in text.split_inclusive('\n').enumerate() {
        out.push_str(&format!("{:>width$}\t", first_line + i, width = LINE_NUMBER_WIDTH));
        out.push_str(line);
    }
    out
}

/// The inverse: `Some(text)` iff **every** line carries a well-formed prefix and the numbers run
/// consecutively; `None` otherwise.
///
/// # Both conjuncts are what keep this from being a guess
///
/// The grammar alone has false positives — a terminal transcript, a fixed-width report, a fixture
/// in this very crate. Requiring *every* line to match, and the numbers to be strictly
/// consecutive, is what bounds them. The rest of the bound is structural and lives at the call
/// site: [`replacing_miss`] runs this only **after** `existing.find(replacing)` has already missed,
/// and then re-searches for the stripped form. So the named refusal is never a guess about what
/// the text looks like — it is a statement that the stripped text *is in the file*, checked
/// against the file the executor is holding.
///
/// A file that genuinely contains `      42\tfoo` and is edited with exactly that snippet matches
/// on the first search and never reaches here at all.
pub fn strip_line_numbers(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut expected: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[spaces..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        let after = &rest[digits..];
        if !after.starts_with('\t') {
            return None;
        }
        // The width is fixed, so a number that has NOT been padded to it is not this harness's
        // prefix — except above a million lines, where the column widens and the padding is gone.
        let padded_to_width = spaces + digits == LINE_NUMBER_WIDTH;
        let overflowed_the_column = spaces == 0 && digits > LINE_NUMBER_WIDTH;
        if !padded_to_width && !overflowed_the_column {
            return None;
        }
        let n: usize = rest[..digits].parse().ok()?;
        if let Some(e) = expected {
            if n != e {
                return None;
            }
        }
        expected = Some(n.checked_add(1)?);
        out.push_str(&after[1..]);
    }
    Some(out)
}

/// The 1-based line `offset` falls on, counting `\n` before it.
fn line_at(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// The first `n` lines, terminators intact. `lines().take(n).join("\n")` was the old shape and it
/// silently rewrote CRLF to LF — see [`number_lines`].
fn take_lines(text: &str, n: usize) -> String {
    text.split_inclusive('\n').take(n).collect()
}

/// Cut to at most `cap` bytes, on a LINE boundary, and report how many whole lines survived.
///
/// The old cut was `floor_boundary`, a *char* boundary — so a window that hit the byte ceiling
/// mid-line kept the fragment, counted it as a line, and then told the model to continue from the
/// line **after** it. The remainder of that line was unreachable by any range. Cutting at the last
/// newline makes the notice's arithmetic true.
///
/// A single line longer than `cap` has no newline to fall back to; the fragment is kept, because
/// returning nothing at all is worse than returning a prefix.
fn cut_to_whole_lines(text: &str, cap: usize) -> (String, usize) {
    let hard = floor_boundary(text, cap);
    let end = match text[..hard].rfind('\n') {
        Some(i) => i + 1,
        None => hard,
    };
    let end = if end == 0 { hard } else { end };
    (text[..end].to_string(), text[..end].lines().count())
}

/// `first-last`, 1-based and inclusive — and **every way of getting it wrong is now an error**.
///
/// # It used to fall back to the whole file, silently, for anything it could not parse
///
/// `split_once('-')` failing, or either half failing `parse::<usize>()`, returned `text` unchanged.
/// So `range: "abc"` and `range: "2"` — the latter a perfectly reasonable guess at "just line 2"
/// from a parameter documented as *"line range"* — both returned the ENTIRE file with no signal
/// that `range` had been ignored. On a large file that turns a targeted read into an unexpectedly
/// huge one, or into a `ContentRef` the model then cannot dereference.
///
/// A caller that wants the whole file omits `range`. There is no reading of a malformed range
/// under which returning everything is what was asked for.
///
/// **It returns the first line's ABSOLUTE number with the text**, because that number is what
/// [`number_lines`] counts from. Recomputing it at the call site is how the two come to disagree,
/// and a disagreement here is a numbered window that lies about where it is in the file.
fn slice_lines(text: &str, range: &str) -> Result<(String, usize), String> {
    let malformed = |detail: &str| {
        Err(format!(
            "`range` must be `first-last`, 1-based and inclusive, e.g. \"20-60\" — {detail}. Omit \
             `range` to read the whole file."
        ))
    };
    let Some((a, b)) = range.split_once('-') else {
        return malformed(&format!("\"{range}\" has no `-`"));
    };
    let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
        return malformed(&format!("\"{range}\" is not two whole numbers"));
    };
    if a == 0 {
        return malformed("lines are numbered from 1, not 0");
    }
    // An inverted range selects nothing. `b.saturating_sub(a)` would read 0 and `take(1)` would
    // return line `a` — an answer to a question nobody asked. It is refused rather than silently
    // emptied, because a caller that wrote `60-20` meant `20-60`.
    if b < a {
        return malformed(&format!("\"{range}\" runs backwards; did you mean \"{b}-{a}\"?"));
    }
    Ok((
        text.split_inclusive('\n')
            .skip(a.saturating_sub(1))
            .take(b.saturating_sub(a).saturating_add(1))
            .collect::<String>(),
        a,
    ))
}

/// What one enumeration found, and what it did not.
///
/// **A `Vec<PathBuf>` was the wrong return type and that is the whole of ADR-059's measured
/// defect.** Four facts come out of a walk — the files kept, how many were seen at all, the fact
/// that it stopped early, and the directories it declined to enter — and only the first had
/// anywhere to go. `glob` recomputed the third at its call site and `find` did not compute it at
/// all, so `find`'s zero on this repository was indistinguishable from an empty workspace. They
/// are returned together now, because a caller cannot forget to look at a field it destructures.
pub(crate) struct Walk {
    /// The files that passed `keep`, sorted.
    files: Vec<PathBuf>,
    /// Every regular file the walk reached, whether or not `keep` took it. This is what lets an
    /// empty result say *"N files are here and none matched"* rather than *"there is nothing
    /// here"* — two different facts, and a model that cannot tell them apart invents one.
    seen: usize,
    /// Which [`WALK_SKIP`] names were actually met, deduplicated and sorted. Empty when none were,
    /// so a result never claims to have skipped a directory that was not there.
    skipped: Vec<String>,
    /// The cap stopped the walk before it had seen everything.
    truncated: bool,
}

/// Enumerate regular files under `base` that satisfy `keep`, to a hard cap, skipping
/// [`WALK_SKIP`] directories.
///
/// Symlinked directories are **not** descended — `read_dir` reports them, and following one here
/// would walk outside the workspace before the scope ever saw the path. The scope refuses them
/// anyway on the way back in; not descending is the cheaper half of the same refusal.
///
/// **The skip is on directories this walk would DESCEND into, never on `base`.** So `path:
/// "target"` searches `target`, and `path: "."` does not — which is the behaviour both tools'
/// descriptions promise, and it is enforced here rather than restated at two call sites.
///
/// # `keep` is applied HERE, and applying it later would have been useless
///
/// The obvious shape for `grep`'s new `glob` argument is a filter over the walk's output. It does
/// not work, and the reason is the whole point of the argument: the cap bounds how many files the
/// walk ENUMERATES, so a filter applied afterwards still lets 2,000 irrelevant files consume every
/// slot before the first `.rs` file is reached. `glob: "*.rs"` would then narrow a set that had
/// already stopped short of the code. Filtering inside the walk is what makes the cap count files
/// that could actually match, and `a_glob_filter_reaches_source_that_the_cap_would_otherwise_hide`
/// is the test that fails if it moves back out.
pub(crate) fn collect(base: &Path, cap: usize, keep: &dyn Fn(&Path) -> bool) -> Walk {
    let mut files = Vec::new();
    let mut seen = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut queue = vec![base.to_path_buf()];
    while let Some(dir) = queue.pop() {
        // LOOP-EXEMPT: a breadth-first enumeration, not a driving loop. The crate has no agent
        // loop in it; HP10's check is scoped to `marlowe-loop`.
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if files.len() >= cap {
                truncated = true;
                // Not `return`: the remaining queue is not walked, but the skip names already
                // found stay in the result. A truncated walk that also declined to enter `target/`
                // has to be able to say both things at once.
                queue.clear();
                break;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(link_meta) = entry.path().symlink_metadata() else { continue };
            if link_meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if WALK_SKIP.contains(&name.as_str()) {
                    if !skipped.contains(&name) {
                        skipped.push(name);
                    }
                    continue;
                }
                queue.push(entry.path());
            } else if meta.is_file() {
                seen += 1;
                let path = entry.path();
                if keep(&path) {
                    files.push(path);
                }
            }
        }
    }
    skipped.sort();
    // **Sorted here rather than at each call site.** `read_dir` order is OS-defined, `glob` sorted
    // its hits and `find` did not, and two identical searches returning two orderings in a project
    // whose scoreboard is a reproduction hash is not a cosmetic difference.
    files.sort();
    Walk { files, seen, skipped, truncated }
}

/// The sentence a result uses to say a walk declined to enter a directory.
///
/// One definition, called by `glob` and by `grep`, so the two cannot describe the same policy
/// differently — and built with `format!` from [`WALK_SKIP`] so the names cannot drift from the
/// constant that produced them.
fn skipped_notice(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!(
        "\n[not searched, because they are build output or version control: {}. Name one as \
         `path` to look inside it.]",
        skipped.join(", ")
    )
}

/// The two bounds a shell run is held to.
///
/// **A parameter rather than two constants read inside the function, and that is the whole reason
/// this type exists.** A1's proposed test says the check must *"run a sleeping command with a
/// lowered constant and assert the call returns"* — and note the sentence that follows it:
/// *"asserting on the constant's value would be the sixteenth-instance family again."* With the
/// production numbers baked in, the only affordable test is `assert_eq!(BASH_TIMEOUT_MS, 120_000)`,
/// which is green on a build where nothing reads it — exactly the defect A1 reports. Injecting the
/// bounds is what buys a test that kills a real child in under a second.
#[derive(Debug, Clone, Copy)]
pub struct ShellLimits {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl ShellLimits {
    /// What `bash` actually runs with.
    pub fn production() -> Self {
        Self { timeout_ms: BASH_TIMEOUT_MS, max_output_bytes: MAX_SHELL_OUTPUT_BYTES }
    }
}

/// What a shell run produced, and how it ended.
///
/// `stopped` and `flooded` are carried out to the caller rather than folded into the text, because
/// the model has to be **told** it got a prefix. A silent truncation is a result that looks
/// complete and is not, which is the whole family this project keeps logging.
pub struct ShellRun {
    pub code: i32,
    pub text: String,
    /// The child hit [`BASH_TIMEOUT_MS`] and was killed.
    pub stopped: bool,
    /// The child breached [`MAX_SHELL_OUTPUT_BYTES`] on a stream and was killed.
    pub flooded: bool,
}

/// The shell every `bash` call runs, as a `Command` with its program and `-c` already set.
///
/// **Public because `shell_bounds.rs` had its own copy and the copy drifted.** That file built
/// `Command::new("cmd").arg("/C")` under a doc comment reading *"the way `spawn_shell` does on
/// this platform"* — true when it was written, false the moment the interpreter changed, and
/// nothing could have reported it. A test that constructs its own idea of the subject is testing
/// its own idea.
///
/// Fails rather than falling back: a shell that is silently a different shell is the defect being
/// removed here, not a graceful degradation.
pub fn shell_command() -> std::io::Result<std::process::Command> {
    #[cfg(windows)]
    {
        let Some(bash) = git_bash() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the `bash` tool needs Git Bash and it is not installed. Install Git for Windows \
                 (https://git-scm.com/download/win), or set MARLOWE_BASH to a bash.exe. The \
                 `read`, `write`, `edit`, `glob` and `grep` tools do not need it.",
            ));
        };
        let mut c = std::process::Command::new(bash);
        c.arg("-c");
        // The surface renders this child's output; a console window would show the same bytes
        // twice, in a frame the user cannot close without killing the tool.
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        Ok(c)
    }
    #[cfg(unix)]
    {
        let mut c = std::process::Command::new("bash");
        c.arg("-c");
        Ok(c)
    }
}

/// Where Git Bash is, resolved once and never taken from `PATH`.
///
/// # `PATH` holds a bash that is a different machine
///
/// `C:\Windows\System32\bash.exe` comes first on a default `PATH` and it is **WSL's launcher**,
/// not a shell — it runs inside a Linux VM with its own filesystem. Measured on this machine, the
/// same `pwd` through each:
///
/// ```text
///   Git Bash   ->  /c/Users/matth/Projects/Marlowe_Harness
///   PATH bash  ->  /mnt/c/Users/matth/Projects/Marlowe_Harness
/// ```
///
/// Both "work", which is what makes it dangerous: a path scoping decision made about a Windows
/// directory would be enforced against a handle in one filesystem and a command run in another,
/// and on a machine with no WSL distribution installed the same call fails outright. So the
/// candidates are explicit and `PATH` is not consulted.
///
/// `MARLOWE_BASH` overrides, for a machine that keeps Git somewhere else. It is read once.
#[cfg(windows)]
fn git_bash() -> Option<std::path::PathBuf> {
    use std::sync::OnceLock;
    static FOUND: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(explicit) = std::env::var_os("MARLOWE_BASH") {
                let p = std::path::PathBuf::from(explicit);
                return p.is_file().then_some(p);
            }
            let mut roots: Vec<std::path::PathBuf> = Vec::new();
            for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(v) = std::env::var_os(var) {
                    roots.push(std::path::PathBuf::from(&v).join("Git"));
                    roots.push(std::path::PathBuf::from(&v).join("Programs").join("Git"));
                }
            }
            roots.push(std::path::PathBuf::from(r"C:\Program Files\Git"));
            for root in roots {
                // `bin\bash.exe` is the launcher Git for Windows puts on a user's PATH;
                // `usr\bin\bash.exe` is the same shell one level down. Either is fine.
                for rel in [r"bin\bash.exe", r"usr\bin\bash.exe"] {
                    let candidate = root.join(rel);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            None
        })
        .clone()
}

/// **The `bash` tool runs bash.** On Windows that is Git Bash, which is what this project's own
/// tooling uses and what the model's shell vocabulary assumes.
///
/// # It ran `cmd /C` until 2026-08-27, and that is why every shell call failed
///
/// The tool has always been NAMED `bash`. It ran `cmd.exe`, so a model writing the shell it was
/// told it had -- `ls`, `find -type f`, `head`, `grep`, `2>/dev/null` -- got failures that looked
/// like a model unable to use a shell. Across two live sessions it made **seven** attempts to list
/// one directory and never succeeded once; each was `Irreversible`, so each stopped and asked the
/// user first. It then reported a directory with four files in it as *"appears empty"*.
///
/// Two separate faults, and fixing only the first left it broken:
///
/// 1. `Command::arg` applies **Rust's** escaping, turning `"` into `\"`, and `cmd.exe` reads that
///    literally -- so every QUOTED command arrived corrupted. `raw_arg` fixed that for `cmd`.
/// 2. The interpreter was still wrong. Teaching the model `cmd` was the other option and it is the
///    worse one: the tool's name, the model's priors and this project's own scripts are all bash.
///
/// **Rust's escaping is correct for this shell**, so `arg` is right here where `raw_arg` was right
/// for `cmd` -- MSYS2 parses the MSVC-style command line the way `Command` writes it. Measured:
/// `echo "hello world"` through `arg` prints `hello world`, where the same call to `cmd` printed
/// `\"hello world\"`.
///
/// **No fallback to `cmd`.** A shell that is silently a different shell is precisely the defect
/// being removed; if Git Bash is absent the call fails and says what to install.
#[cfg(windows)]
fn spawn_shell(
    command: &str,
    dir: &Path,
    _cwd_handle: Option<&ScopedPath>,
) -> std::io::Result<ShellRun> {
    // The cwd crosses as a STRING because Win32 has no handle-relative spawn. What makes it safe
    // is that `_cwd_handle` is still alive: the walk opened it without FILE_SHARE_DELETE, so the
    // directory cannot be renamed or deleted, and the string still names the verified object.
    let mut cmd = shell_command()?;
    cmd.arg(command).current_dir(dir);
    run_bounded(cmd, ShellLimits::production())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn spawn_shell(
    command: &str,
    _dir: &Path,
    cwd_handle: Option<&ScopedPath>,
) -> std::io::Result<ShellRun> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;

    // **`bash`, not `sh`.** The tool is named `bash` and the model writes bash; `sh` is a
    // different shell on several distributions and the difference shows up exactly where a model
    // reaches for a bashism.
    let mut cmd = shell_command()?;
    cmd.arg(command);
    match cwd_handle {
        Some(scoped) => {
            // No string crosses at all: the child changes directory to the descriptor the walk
            // verified, before `exec`. This is the gap Windows cannot close.
            let fd = scoped.handle().as_raw_fd();
            unsafe {
                cmd.pre_exec(move || {
                    rustix::process::fchdir(std::os::fd::BorrowedFd::borrow_raw(fd))
                        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
                });
            }
        }
        None => {
            cmd.current_dir(_dir);
        }
    }
    run_bounded(cmd, ShellLimits::production())
}

/// Spawn, read both pipes under a cap, wait under a deadline, kill on breach of either.
///
/// # What this does NOT close, stated rather than implied
///
/// **The grandchild.** `child.kill()` kills the shell, not what the shell started. On Unix the
/// child is not put in its own process group and no group signal is sent; on Windows there is no
/// Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which would need a `windows-sys`
/// dependency this crate does not have. So `bash("sh -c 'sleep 999 &'")` leaves the grandchild
/// running after this returns.
///
/// What *is* closed is the one the audit named: the **harness** no longer waits on it. The turn,
/// the batch and the daemon come back. A surviving grandchild is a resource leak; a hung daemon was
/// a denial of service, and they are not the same severity. The Job Object is worth doing and is
/// not being claimed here.
pub fn run_bounded(
    mut cmd: std::process::Command,
    limits: ShellLimits,
) -> std::io::Result<ShellRun> {
    use std::process::Stdio;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    // **stdin is null, not inherited.** An inherited stdin lets an approved command read the
    // terminal the user is typing into, and a command that blocks on it would have been another
    // way to hang before the deadline existed.
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let over_cap = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();

    // `true` marks stdout so the two streams can be told apart on the way back; the channel is what
    // makes abandoning a blocked reader possible, which `join` would not be.
    for (is_stdout, pipe) in [
        (true, child.stdout.take().map(DynRead::Out),),
        (false, child.stderr.take().map(DynRead::Err)),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        let flag = Arc::clone(&over_cap);
        std::thread::spawn(move || {
            let buf = drain_capped(pipe, limits.max_output_bytes, &flag);
            let _ = tx.send((is_stdout, buf));
        });
    }
    drop(tx);

    let mut stopped = false;
    let mut flooded = false;
    let mut waited = 0u64;
    let status = loop {
        // LOOP-EXEMPT: waiting on a child process, not a driving loop.
        if over_cap.load(Ordering::Relaxed) {
            flooded = true;
            let _ = child.kill();
            break child.wait()?;
        }
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if waited >= limits.timeout_ms {
                    stopped = true;
                    let _ = child.kill();
                    break child.wait()?;
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                waited += POLL_MS;
            }
        }
    };

    // Whatever the readers managed to hand back within the grace. Two streams, so two receives.
    let grace = std::time::Duration::from_millis(READER_GRACE_MS);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(grace) {
            Ok((true, b)) => out = b,
            Ok((false, b)) => err = b,
            Err(_) => break,
        }
    }

    Ok(ShellRun {
        code: status.code().unwrap_or(-1),
        text: combine(&out, &err),
        stopped,
        flooded: flooded || over_cap.load(Ordering::Relaxed),
    })
}

/// The two pipe types, so one reader function serves both without a trait object per read.
enum DynRead {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Read for DynRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            DynRead::Out(r) => r.read(buf),
            DynRead::Err(r) => r.read(buf),
        }
    }
}

/// Read until EOF or the cap, setting `flag` the moment the cap is passed.
///
/// It keeps reading after the cap and discards, rather than returning immediately: returning would
/// close the pipe and hand the child a broken pipe mid-write, and the caller is about to kill it
/// anyway. The flag is what the wait loop watches.
fn drain_capped(mut r: impl Read, cap: usize, flag: &std::sync::atomic::AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        // LOOP-EXEMPT: draining a pipe, not a driving loop.
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() >= cap {
                    flag.store(true, Ordering::Relaxed);
                    continue;
                }
                let room = cap - buf.len();
                buf.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    buf
}

/// A `Content-Type` header, reduced to a media type or `"unknown"`.
///
/// Audit finding A4's second half. The raw header went into the summary detail, and a header is a
/// server-chosen string: parameters, quoted values and anything else the origin cares to write.
/// This keeps the part before the first `;` and only if it is a media-type **token**; everything
/// else is reported as unknown rather than quoted.
fn normalize_content_type(ct: Option<&str>) -> &'static str {
    // Returning `&'static str` is the enforcement, not a style choice: a `String` return could be
    // built from the header by a later edit and nothing would notice. This signature cannot.
    const KNOWN: &[&str] = &[
        "text/html",
        "text/plain",
        "text/markdown",
        "text/csv",
        "text/xml",
        "application/xml",
        "application/json",
        "application/pdf",
        "application/zip",
        "application/epub+zip",
        "application/octet-stream",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ];
    let Some(ct) = ct else { return "no content-type" };
    let media = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    KNOWN.iter().copied().find(|k| *k == media).unwrap_or("an unrecognised content-type")
}

fn text_lines(body: &ToolBody) -> u64 {
    match body {
        ToolBody::Inline(s) => s.lines().count() as u64,
        ToolBody::Reference { .. } => 0,
    }
}

fn combine(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&String::from_utf8_lossy(stderr));
    }
    s
}
