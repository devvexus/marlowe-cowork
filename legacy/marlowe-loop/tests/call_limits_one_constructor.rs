//! **A `CallLimits` is built in exactly one production place, and that place reads the run's own
//! profile.**
//!
//! # Why a grep, and not privacy
//!
//! `CallLimits` has public fields, because 32 test sites read `max_output_tokens` and a getter
//! per field would be ceremony. Making `route` private would have moved the same problem into a
//! `pub fn new(max_output_tokens, route)` — a second way to name a model the run's profile did
//! not declare, wearing a different hat. That is the objection `grant_egress_host`'s doc comment
//! makes about holding the granted egress set on the `Run` rather than inside the type that owns
//! the invariant, and it applies one field over.
//!
//! So the invariant is stated where it can be checked: `Budget::call_limits(&spent, route)` is
//! the only production constructor, its `route` argument is `run.profile.model_route()` at the
//! one call site, and therefore **no code path can send a call to a model the run's profile did
//! not declare**. A driver, a daemon or a future provider building its own `CallLimits` would
//! break that silently, and nothing about the types would object.
//!
//! # The negative control is in the test
//!
//! A grep that matched nothing would pass. This asserts a **count** and asserts *where* the one
//! hit is: a guard reading "zero matches" on a workspace where the type was renamed is the
//! "a guarded path that moved is unguarded" family, fourteenth instance.

use std::fs;
use std::path::{Path, PathBuf};

/// The one file permitted to construct one in production.
const DOOR: &str = "crates/marlowe-loop/src/budget.rs";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf()
}

fn src_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            // Production only. `tests/`, `examples/` and `benches/` legitimately build them.
            if name == "target" || name == "tests" || name == "examples" || name == "benches" {
                continue;
            }
            src_rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn call_limits_has_one_production_constructor_and_it_is_the_door() {
    let root = repo_root();
    let mut files = Vec::new();
    src_rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 20, "the walk found {} files; it is not reaching the tree", files.len());

    let mut hits: Vec<String> = Vec::new();
    for f in &files {
        let Ok(text) = fs::read_to_string(f) else { continue };
        for (i, line) in text.lines().enumerate() {
            // **A RETURN TYPE IS NOT A CONSTRUCTION, and this guard could not tell them apart.**
            //
            // `pub fn call_limits(&self, spent: &Budget, route: ModelRoute) -> CallLimits {` ends
            // in the same three tokens a construction starts with. It went red on 2026-08-31 when
            // `route` was added — a parameter that changed nothing about how many places build a
            // `CallLimits` — and reported `budget.rs:181` and `:182` as two production sites when
            // 181 is the signature of the one function 182 lives in.
            //
            // Excluded by the arrow rather than by `starts_with("pub fn")`, because the property
            // is *"this line returns the type"* and not *"this line is a function"*: a helper that
            // is not `pub`, or one whose signature wraps, would slip past the second and not the
            // first. **The guard is unchanged in what it forbids** — a second real construction
            // still reds it, and the `count == 1` assertion below is what proves the exclusion did
            // not empty the scan.
            let is_signature = line.contains("-> CallLimits {");
            if line.contains("CallLimits {")
                && !line.contains("pub struct CallLimits")
                && !is_signature
            {
                let rel = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().replace('\\', "/");
                hits.push(format!("{rel}:{}", i + 1));
            }
        }
    }

    println!("production CallLimits construction sites: {hits:?}");
    assert_eq!(
        hits.len(),
        1,
        "exactly one production site is permitted, and it is `Budget::call_limits`. Found: {hits:?}"
    );
    assert!(
        hits[0].starts_with(DOOR),
        "the one production site moved out of `{DOOR}`: {hits:?}"
    );
}

/// **The route on a call is the run's own, not a caller's.**
///
/// `call_limits` takes the route rather than reading it from anywhere, so this asserts the two
/// halves that make the guard above mean something: the value passed through unchanged, and the
/// one production call site handing it `run.profile.model_route()`.
///
/// *Mutation:* have `call_limits` ignore its argument and return `ModelRoute::Orchestrator` —
/// the first half reds. *Mutation:* revert `engine.rs`'s call site to a constant — the second
/// half reds.
#[test]
fn the_route_on_a_call_is_the_one_the_run_declared() {
    use marlowe_loop::{Budget, ModelRoute};

    let b = Budget::interactive();
    let spent = Budget { tokens: 0, ..Budget::interactive() };
    for route in [ModelRoute::Orchestrator, ModelRoute::Worker, ModelRoute::Summarizer] {
        assert_eq!(b.call_limits(&spent, route).route, route);
    }

    let engine = fs::read_to_string(repo_root().join("crates/marlowe-loop/src/engine.rs"))
        .expect("engine.rs is readable");
    assert!(
        engine.contains("call_limits(&run.spent, run.profile.model_route())"),
        "the one production call site must hand `call_limits` the RUN'S declared route. A \
         constant there is what made `CapabilityProfile::model_route()` a field with zero \
         readers for two milestones"
    );
}
