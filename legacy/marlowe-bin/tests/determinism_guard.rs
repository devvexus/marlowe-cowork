//! Two crude, enforced guards against the failures that pass locally and fail `repro`.
//!
//! HP10's rule: *"a crude enforced mechanism beats an elegant unenforced one."* These are
//! source greps. They will produce the occasional false positive and need an allowlist that
//! itself needs maintenance, and that is the accepted cost.
//!
//! Both guards exist because the failure they catch is **invisible until the acceptance
//! test runs**, and by then it presents as an unexplained hash mismatch rather than as the
//! line of code that caused it.

use std::fs;
use std::path::{Path, PathBuf};

/// Lines a grep-based guard must not judge, with the reason each is exempt.
///
/// Only two exemptions, and neither one hides a line of real code:
///
/// * **Comments.** A guard that fires on the sentence explaining the rule makes the rule
///   undocumentable.
/// * **The `fn` line of a `#[test]`.** Tests asserting these properties are named after
///   them, so `fn memory_ids_do_not_contain_a_timestamp()` trips the very guard it supports.
///
/// The exemption is deliberately the *declaration line of an annotated test* and not the
/// file, the module, or anything named "test". Allowlisting `entry.rs` would have been one
/// character of diff and would have blinded the guard to the single file where the real
/// mistake would live — which is how a guard survives as a passing test while protecting
/// nothing.
fn judgeable_lines(src: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut prev_was_test_attr = false;
    for (n, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let is_comment = trimmed.starts_with("//") || trimmed.starts_with('*');
        let is_test_signature = prev_was_test_attr && trimmed.starts_with("fn ");
        if !is_comment && !is_test_signature {
            out.push((n + 1, line));
        }
        if !trimmed.is_empty() {
            prev_was_test_attr = trimmed == "#[test]";
        }
    }
    out
}

/// Does `path` end with this exemption's path suffix?
///
/// # Exemptions match a PATH, never a bare file name, and that changed on 2026-08-17
///
/// Every list in this file used to match `path.file_name()`. That is fine while every fenced name
/// is unique in the workspace — `clock.rs`, `elapsed.rs` and `frame_clock.rs` each appear once, so
/// it happened to be exact. It stops being fine the moment an exemption is needed for a file whose
/// name is common: **there are fourteen `lib.rs` files in `crates/`**, so a single `"lib.rs"` entry
/// would have silently exempted the entire workspace's library roots, and the guard would still
/// have been green.
///
/// This is a **narrowing**, not a widening. The existing entries mean exactly what they meant; they
/// are now written as paths so that what they mean is what they say.
fn matches(path: &Path, entries: &[&str]) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    entries.iter().any(|e| normalized.ends_with(e))
}

/// **An exemption naming a file that no longer exists is a comment.**
///
/// Every list in this file is checked with this, including the `HashMap` guard's — which had no
/// self-check at all until 2026-08-17, so an entry there could have outlived its file silently and
/// then excused the next file to take that path. That is the fourteenth-instance shape: a guard is
/// a claim about a path, and a claim about a path needs something checking the path is still there.
fn assert_every_entry_still_exists(entries: &[&str], list: &str) {
    let all: Vec<String> = crate_sources()
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for e in entries {
        assert!(
            all.iter().any(|p| p.ends_with(e)),
            "`{list}` exempts `{e}`, and no such file exists under crates/. Either it was renamed \
             — in which case point the entry at the new path — or it was deleted, in which case \
             delete the entry. An exemption that outlives what it exempts silently excuses the \
             next file to take that path."
        );
    }
}

fn crate_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Sorted, because even a test's own traversal order should not depend on the filesystem.
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `HashMap` iteration order is randomised per process in Rust. A map whose order can reach
/// the wire therefore produces a different `run.jsonl` on every run, and `marlowe-eval repro
/// --runs 2` fails with two different sha256 values and no indication of where they came
/// from.
///
/// The rule is blunt on purpose: use `BTreeMap`/`BTreeSet`. If a hash map is ever genuinely
/// needed for a hot path whose order provably cannot reach output, add it to the allowlist
/// **with the reason**, and expect to defend it.
///
/// **Nothing is allowlisted here except this file, and that is a result rather than a policy.**
/// On 2026-08-17 four crates were carrying hash-ordered collections — `marlowe-net`'s connection
/// pool and DNS cache, `marlowe-loop`'s condensation cache, `marlowe-extract`'s document store, and
/// a test's dedup set. Every one of them was `get`/`insert`/`len`, never iterated, so every one of
/// them qualified for an allowlist entry under the rule above. **They were converted to `BTreeMap`
/// and `BTreeSet` instead**, because the conversion costs nothing measurable on maps this size and
/// an exemption costs a line of judgement that has to be re-made by every future reader — and
/// because an entry naming `marlowe-net/src/lib.rs` blinds this guard to every future map in the
/// file that actually issues requests.
#[test]
fn no_hash_map_in_crate_sources() {
    const ALLOWED: &[&str] = &[
        // This file names the types in order to ban them.
        "marlowe/tests/determinism_guard.rs",
    ];
    assert_every_entry_still_exists(ALLOWED, "ALLOWED");

    let mut offenders = Vec::new();
    for path in crate_sources() {
        if matches(&path, ALLOWED) {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        for (n, line) in judgeable_lines(&src) {
            if line.contains("HashMap") || line.contains("HashSet") {
                offenders.push(format!("{}:{}: {}", path.display(), n, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "hash-ordered collections found. Their iteration order is randomised per process, so \
         anything reaching the wire from one makes `marlowe-eval repro` fail with two \
         different hashes and no clue why. Use BTreeMap/BTreeSet:\n  {}",
        offenders.join("\n  ")
    );
}

/// Section 4.5, and the one legitimate exception to it.
///
/// *"On any path reachable from sections 4.1, 4.6, or 4.7, the implementation MUST NOT read
/// a system clock. Every timestamp is derived from the `clock` supplied by the caller."*
///
/// Exactly one reading of a real clock is legitimate **on a contract path**: `cost.latency_ms`,
/// which the contract requires the implementation to self-report and which the harness excludes
/// from its reproduction hash for that reason. That read is fenced into one module so this guard
/// can name it, and so a second one cannot appear without appearing here.
///
/// M1 adds a second fence, off every contract path. A terminal that animates needs a monotonic
/// time base, and K4 is stated in milliseconds to first frame, so `--timing-probe` has to read a
/// real clock to report one. It is fenced for the same reason the first one is: so that the
/// *third* read cannot appear silently.
///
/// **Both entries are exact file names, never directories.** The `HashMap` guard's argument
/// applies here too — allowlisting a directory would blind this guard to the files where the real
/// mistake would live. `marlowe-surface` gets no exemption at all: every render there is a pure
/// function of `(state, now_ms)`, which is what makes §B13's flicker rows diffable in the first
/// place, and a stray `Instant::now()` inside a widget would take that away.
#[test]
fn the_only_real_clock_read_is_the_latency_fence() {
    const FENCES: &[&str] = &[
        // Section 4.2's self-reported `cost.latency_ms`. The original fence.
        "marlowe/src/elapsed.rs",
        // M1: the stub owns the time base because ARCHITECTURE.md §2.14 says a surface holds no
        // state the daemon lacks, and time is state. Not on any contract path.
        "marlowe-stub/src/frame_clock.rs",
        // M2 C2c: the PRODUCTION harness clock. §4.5 forbids a system clock on the contract
        // paths and names the legitimate case in the same paragraph — "in production the harness
        // supplies the real clock". The daemon is that harness, so real time enters the system
        // here, once. It is a whole file holding one struct so this fence stays narrow:
        // exempting `daemon.rs` would blind the guard to every future clock read in it, which is
        // the failure this test's own header warns about for directories. The eval adapter never
        // constructs a `Daemon` and still supplies M0a's synthetic clock.
        "marlowe-daemon/src/clock.rs",
        // M2 Session E: `marlowe-net`'s connection hygiene — DNS cache age and pooled-connection
        // idle time. Monotonic durations only; `Mark` deliberately exposes `elapsed()` and no way
        // to obtain a time value, so a stamp made here cannot reach a payload even by accident.
        //
        // It is a twenty-line module for the same reason `clock.rs` is: fencing
        // `marlowe-net/src/lib.rs` would exempt every future clock read in the four-hundred-line
        // file that actually issues requests, including one that DID reach a result, and it would
        // do it silently because the fence would already be green.
        "marlowe-net/src/age.rs",
        // ADR-052. An MCP server is a child process, and a child that never answers would block
        // the turn forever with no output and no indication why -- the failure Addendum B §B5
        // exists to prevent, and one that counting iterations cannot solve, because a blocking
        // `read_line` on a silent pipe does not iterate.
        //
        // A FILE, not the crate, and for `clock.rs`'s stated reason: `marlowe-mcp/src/lib.rs` is
        // four hundred lines of protocol handling and is exactly the file that should keep failing
        // this guard if a second clock read appears in it. `deadline.rs` holds one type with one
        // method returning `bool` -- no `Duration`, no accessor, nothing a decay path could key
        // off -- and asserts that property about itself.
        "marlowe-mcp/src/deadline.rs",
        // ADR-060. `hybrid::start` spawns a `llama-server` and waits for it to answer `/health`.
        // A child that never answers would hang the daemon forever with no output and no
        // indication why -- the same failure `marlowe-mcp/src/deadline.rs` is fenced for, one
        // subsystem over -- and counting poll iterations cannot substitute, because the loop's own
        // HTTP probes carry multi-second timeouts and would make the count understate the elapsed
        // time by whatever they cost.
        //
        // A FILE, not the crate, and for `clock.rs`'s stated reason: `llamacpp.rs` is eleven
        // hundred lines and `hybrid.rs` spawns processes, so fencing either would exempt every
        // future clock read in them, silently, because the fence would already be green.
        // `deadline.rs` holds one type exposing `elapsed()` and NO way to obtain a time value.
        //
        // **`marlowe_net::age::Mark` was tried first and reverted.** It is the same type, already
        // fenced and already argued, and reusing it would have added zero entries here -- but
        // `marlowe-net` carries `rustls` and a vendored root store, and ADR-031 §2.3 keeps that
        // out of `marlowe-provider` so the crate's dependency tree stays evidence for ADR-028.
        // `no_tls_in_the_default_path.rs` failed, correctly, and one fence entry is the cheaper
        // of the two costs.
        "marlowe-provider/src/deadline.rs",
    ];

    /// The engine spike — a **temporary** measurement crate, exempt with an expiry.
    ///
    /// It times forward passes to decide ADR-004's runtime, so a real clock is the whole
    /// point. It implements no section 4 interface, is not a dependency of `marlowe`, and is
    /// deleted once ADR-004 names an engine. **Delete this entry with the crate** — an
    /// exemption that outlives what it exempts is how a guard quietly stops guarding.
    ///
    /// Note this is a directory exemption, which the `HashMap` guard deliberately refuses to
    /// grant. The difference: that guard's whole value is covering the files where the real
    /// mistake would live, whereas nothing on a contract path is inside this crate.
    const TEMPORARY_SPIKE: &str = "marlowe-embed-spike";

    // **A fence naming a file that no longer exists is a comment.**
    //
    // This list matches on file NAME, so a fenced file moving between crates keeps working and a
    // fenced file being *deleted* leaves a silent exemption behind — and the exemption would then
    // sit there ready to excuse a future file that happened to take the same name. That is the
    // fourteenth-instance shape (a guard whose subject moved) with the subject gone entirely.
    //
    // `protect-boundaries.py` grew `--self-check` for exactly this after M2 Session B; this guard
    // never did. M2 C2d moved `turn.rs` and `model.rs` between crates, which is what made the gap
    // worth closing rather than noting.
    assert_every_entry_still_exists(FENCES, "FENCES");

    /// Files that **name** the forbidden spellings in order to ban them, and read no clock.
    ///
    /// Deliberately a separate list from [`FENCES`], not extra entries in it. A fence says *this
    /// file legitimately reads real time*; these say *this file mentions the words*. Collapsing
    /// them would let a genuine clock read appear in a test file under an exemption that was
    /// granted for a string literal — the two claims are different and the list that records them
    /// should be too.
    /// Files that compare **filesystem timestamps** and never read the current time.
    ///
    /// A third list, and the separation is the point. `FENCES` means *this file legitimately reads
    /// real time*; `NAMES_BUT_DOES_NOT_READ` means *this file spells the words in order to ban
    /// them*. Neither describes a file that compares two mtimes — and folding it into `FENCES`
    /// would grant it the right to call `SystemTime::now()`, which is exactly what it must not do.
    const COMPARES_MTIMES: &[&str] = &[
        // M2 C2e: the daemon staleness guard. Compares the executable's mtime against the newest
        // source file. Nothing here reaches a journal timestamp, a memory id or a repro hash.
        "marlowe-daemon/src/staleness.rs",
    ];
    assert_every_entry_still_exists(COMPARES_MTIMES, "COMPARES_MTIMES");

    const NAMES_BUT_DOES_NOT_READ: &[&str] = &[
        "marlowe/tests/determinism_guard.rs",
        // M2 C2d. Asserts locally that `marlowe-surface` reads no clock, which means spelling out
        // what it is looking for. See `marlowe-surface/tests/c2d_boundary.rs`.
        "marlowe-surface/tests/c2d_boundary.rs",
    ];
    assert_every_entry_still_exists(NAMES_BUT_DOES_NOT_READ, "NAMES_BUT_DOES_NOT_READ");

    /// Benchmarks and timing tests. **A fifth list, and every entry is a FILE — never `examples/`.**
    ///
    /// A benchmark that cannot read a clock cannot benchmark, so these reads are the point of the
    /// files rather than a mistake in them. None is reachable from §4.1/4.6/4.7: an example is not
    /// linked into the product, and a test measuring its own elapsed time reports nothing to a
    /// journal, a memory id or a repro hash.
    ///
    /// **The directory exemption was considered and refused.** `examples/*.rs` would have been one
    /// line instead of six, and it would have meant that an example added later — or a contract
    /// path demonstrated inside one — reads a clock with nothing noticing. This guard's own header
    /// makes that argument about `entry.rs` and the `HashMap` list; the same argument applies here,
    /// and the cost of refusing is that adding an example means adding a line. That cost is the
    /// feature: it makes the author say why.
    ///
    /// `marlowe-embed-spike` remains the single directory exemption in this file, and it carries
    /// an expiry.
    const BENCHMARKS_AND_TIMING_TESTS: &[&str] = &[
        // ADR-040's parallelism measurements. `batch_parallelism` times the same batch at several
        // widths; `corpus_bench` and `deep_research*` time a live corpus fetch. All four exist to
        // produce a number in milliseconds.
        "marlowe-exec/examples/batch_parallelism.rs",
        "marlowe-exec/examples/corpus_bench.rs",
        "marlowe-exec/examples/deep_research.rs",
        "marlowe-exec/examples/deep_research_attack.rs",
        // Extraction throughput, asserted as MB/s against a floor.
        "marlowe-extract/tests/injector.rs",
        // ADR-015's two baselines for the embedder: CPU and CUDA are different scorers, so this
        // times the same fixed text set on each at three sequence lengths. It exists to produce a
        // number in milliseconds, with the embedding cache off so the second provider cannot be
        // measuring the first one's disk.
        "marlowe-memory/examples/embed_provider_bench.rs",
        // M2 Session E's socket-auth test: asserts the daemon ANSWERED before a deliberately held
        // connection let go, which is a statement about elapsed time and cannot be made without
        // reading one. Restructuring it to use an injected clock would mean injecting a clock into
        // the test's own peer, which measures the harness rather than the daemon.
        "marlowe-daemon/tests/socket_auth.rs",
        // ADR-047: what markdown rendering costs per frame, against K4's 150 ms first-frame
        // budget. The whole file is a wall-clock number and cannot be written without one; it
        // renders into a headless `TestBackend` and reports to nobody -- no journal timestamp, no
        // memory id, no repro hash.
        //
        // `marlowe-surface` still gets NO exemption in `src/`, which is the claim this guard's own
        // header makes and `marlowe-surface/tests/c2d_boundary.rs` asserts locally. This is a
        // FILE entry for the same reason every other line here is: exempting the directory would
        // excuse the next timing read in a crate whose whole property is that it has none.
        "marlowe-surface/tests/markdown_cost.rs",
    ];
    assert_every_entry_still_exists(BENCHMARKS_AND_TIMING_TESTS, "BENCHMARKS_AND_TIMING_TESTS");

    let mut offenders = Vec::new();
    for path in crate_sources() {
        if matches(&path, FENCES)
            || matches(&path, NAMES_BUT_DOES_NOT_READ)
            || matches(&path, COMPARES_MTIMES)
            || matches(&path, BENCHMARKS_AND_TIMING_TESTS)
        {
            continue;
        }
        if path.components().any(|c| c.as_os_str() == TEMPORARY_SPIKE) {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        for (n, line) in judgeable_lines(&src) {
            if line.contains("SystemTime")
                || line.contains("Instant::now")
                || line.contains("UNIX_EPOCH")
            {
                offenders.push(format!("{}:{}: {}", path.display(), n, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a real clock is read outside the fences {FENCES:?}. Section 4.5 is binding on every path \
         reachable from sections 4.1/4.6/4.7, and a stray read makes staleness half-life \
         unmeasurable and every decay-dependent result irreproducible. If this is a latency \
         measurement, route it through the fence:\n  {}",
        offenders.join("\n  ")
    );
}

/// The clock probe shifts every supplied timestamp by ten years and compares the injected
/// `memory_id`s for equality, so an id built from a timestamp fails translation invariance.
/// This guard is weaker than the two above — it only catches the obvious spelling — so the
/// real defence is the probe itself. It is here to fail early and name the reason.
#[test]
fn memory_ids_are_not_built_from_timestamps() {
    let mut offenders = Vec::new();
    for path in crate_sources() {
        // Path, not bare name — see `matches`. One list style in one file.
        if matches(&path, &["marlowe/tests/determinism_guard.rs"]) {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        for (n, line) in judgeable_lines(&src) {
            let looks_like_id = line.contains("memory_id") || line.contains("MemoryId");
            let looks_like_time = line.contains("now_ms")
                || line.contains("occurred_at")
                || line.contains("timestamp");
            if looks_like_id && looks_like_time {
                offenders.push(format!("{}:{}: {}", path.display(), n, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "an identifier appears to be derived from a time value. Clock probe test A shifts \
         every supplied timestamp by ten years and asserts the injected ids are unchanged, \
         so this fails translation invariance:\n  {}",
        offenders.join("\n  ")
    );
}
