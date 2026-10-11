//! **ADR-031 §2.3 was a comment. This is the line of code that reads it.**
//!
//! `marlowe-net/Cargo.toml` has said since ADR-031 that it is *"the whole of the TLS supply-chain
//! surface"* and that *"`cargo tree -p marlowe-provider` must continue to show no TLS so ADR-028's
//! 'the default path reaches no network' stays a property rather than a comment."*
//!
//! Nothing checked it. That is precisely CLAUDE.md's sixteenth instance — **a declared control that
//! nothing reads** — and `web`'s `inline_threshold_bytes: 0` is the recorded example: a field, a
//! confident comment naming the section it enforces, a green test asserting the field's *value*,
//! and no code anywhere that acted on it.
//!
//! ADR-046 is the first change that could have broken this property, and it is the change that
//! discovered nothing was watching. The obvious implementation of a hosted provider — an
//! `openrouter.rs` beside `ollama.rs` — would have added `rustls` to this crate's dependency graph
//! with no error, no warning, and no failing test. So the hosted driver lives in
//! `marlowe-openrouter`, which depends on **this** crate rather than the other way round, and this
//! test is what keeps that true.
//!
//! # Why this walks manifests instead of running `cargo tree`
//!
//! `cargo tree` would be the direct reading, and shelling out to cargo from inside a cargo test is
//! a recursive build under a lock this test already holds. The manifest walk answers the same
//! question — *is any TLS crate reachable from `marlowe-provider`'s dependencies* — deterministically
//! and in milliseconds, and it fails loudly if it cannot find a manifest rather than concluding
//! "nothing found, therefore clean". That last part is the difference between a guard and a
//! comment: **a walk that silently found nothing would be green on a broken build.**

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Crates whose presence means TLS is in the graph. Names, because a version bump must not need
/// this list edited.
const TLS_CRATES: &[&str] = &["rustls", "webpki-roots", "native-tls", "openssl", "openssl-sys"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> sits two levels under the workspace root")
        .to_path_buf()
}

/// `name -> (path-dependencies, external-dependency-names)` for every workspace member.
fn manifests() -> BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut out = BTreeMap::new();
    let entries = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("crates/ must be readable at {}: {e}", crates_dir.display()));
    let mut dirs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", manifest.display()));
        let name = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("name = "))
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_else(|| panic!("{} has no package name", manifest.display()));

        // Only the `[dependencies]` section. `[dev-dependencies]` do not reach a shipped binary,
        // and folding them in would make this test fail for a reason it is not about.
        let mut internal = BTreeSet::new();
        let mut external = BTreeSet::new();
        let mut in_deps = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                in_deps = t == "[dependencies]";
                continue;
            }
            if !in_deps || t.is_empty() || t.starts_with('#') {
                continue;
            }
            let Some((dep, _)) = t.split_once('=') else { continue };
            let dep = dep.trim().trim_matches('"').split('.').next().unwrap_or("").trim();
            if dep.is_empty() {
                continue;
            }
            if dep.starts_with("marlowe-") {
                internal.insert(dep.to_string());
            } else {
                external.insert(dep.to_string());
            }
        }
        out.insert(name, (internal, external));
    }
    assert!(
        out.contains_key("marlowe-provider") && out.contains_key("marlowe-net"),
        "the manifest walk found neither the crate under test nor the TLS crate — it is reading \
         the wrong directory, and a walk that silently finds nothing is green on a broken build. \
         Found: {:?}",
        out.keys().collect::<Vec<_>>()
    );
    out
}

/// Every crate reachable from `start` through `[dependencies]`, plus the external crate names.
fn reachable(start: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let all = manifests();
    let mut seen = BTreeSet::new();
    let mut externals = BTreeSet::new();
    let mut queue = vec![start.to_string()];
    // LOOP-EXEMPT: a graph walk over a fixed manifest set, not a driving loop.
    while let Some(next) = queue.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        let Some((internal, external)) = all.get(&next) else { continue };
        externals.extend(external.iter().cloned());
        queue.extend(internal.iter().cloned());
    }
    (seen, externals)
}

#[test]
fn marlowe_provider_reaches_no_tls_crate() {
    // ADR-028's claim is that the DEFAULT path reaches no network. TLS in this crate's graph
    // would not by itself break that — but it is the reading that would stop being checkable,
    // and it is the first thing that changes when someone puts a hosted driver in the wrong file.
    let (crates, externals) = reachable("marlowe-provider");
    let offenders: Vec<&&str> =
        TLS_CRATES.iter().filter(|t| externals.contains(**t)).collect();
    assert!(
        offenders.is_empty(),
        "`marlowe-provider` now reaches {offenders:?}. ADR-031 §2.3 makes `marlowe-net` the whole \
         of the TLS supply-chain surface so that this crate's dependency tree stays evidence for \
         ADR-028. If a hosted provider needs TLS, it goes in a crate that DEPENDS on this one — \
         see `marlowe-openrouter`. Reachable crates were: {crates:?}"
    );
    assert!(
        !crates.contains("marlowe-net"),
        "`marlowe-provider` now depends on `marlowe-net`, which is where TLS lives"
    );
}

/// The negative control. **Without this, the test above would pass on a build where the walk was
/// broken and found nothing** — the exact shape this file's header is about.
#[test]
fn the_walk_actually_finds_tls_where_tls_is() {
    let (_, externals) = reachable("marlowe-net");
    assert!(
        externals.contains("rustls"),
        "the manifest walk did not find `rustls` in `marlowe-net`, so it would not have found it \
         in `marlowe-provider` either. The guard above is measuring nothing."
    );

    // ...and it follows a transitive edge, which is how TLS would actually arrive: through a
    // dependency, not by being written into this crate's own manifest.
    let (crates, externals) = reachable("marlowe-openrouter");
    assert!(
        crates.contains("marlowe-provider") && crates.contains("marlowe-net"),
        "the hosted crate should reach both; it reached {crates:?}"
    );
    assert!(
        externals.contains("rustls"),
        "a transitive TLS edge was not followed, so the guard cannot see the case that matters"
    );
}
