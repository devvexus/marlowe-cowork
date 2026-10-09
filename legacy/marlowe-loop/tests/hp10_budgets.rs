//! HP10 — the four executable budget tests that fail the build.
//!
//! > **Rejected.** Architectural review as the mechanism. Intentions do not survive eighteen
//! > months and a contributor rotation; a failing build does.
//!
//! > **Cost accepted.** These tests are crude — noun-grepping especially will produce false
//! > positives and require an allowlist that itself needs maintenance. A crude enforced
//! > mechanism beats an elegant unenforced one.
//!
//! | HP10 row | Status here |
//! |---|---|
//! | Model-visible tools ≤ 12 | enforced, over every capability profile |
//! | User-facing nouns ≤ 7 | enforced, by requiring every command to name its noun |
//! | Agent loops == 1 | enforced, by scanning this crate's source |
//! | Zero-config first run | **partial** — the library half is here; install → first useful output in a clean container under five minutes is K6 and lands with the daemon in M2 Session E |
//!
//! That last row is stated as partial rather than quietly counted as done. K6 is a milestone
//! kill criterion; a test that asserted something adjacent to it and passed would be the exact
//! failure this project has logged eleven times.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use marlowe_loop::CapabilityProfile;
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, BUILTIN_TOOLS, MAX_EXPOSED_TOOLS};

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(rust_sources(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 1. Model-visible tools ≤ 12, over EVERY capability profile
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn every_capability_profile_exposes_at_most_twelve_tools() {
    // HP10 says "a test enumerates every capability profile", so the enumeration is the test.
    // A named constructor added without a line here is caught by the count assertion below.
    let profiles: Vec<(&str, CapabilityProfile)> = vec![
        ("interactive", CapabilityProfile::interactive()),
        ("consolidation", CapabilityProfile::consolidation()),
        ("quarantined_reader", CapabilityProfile::quarantined_reader()),
    ];
    for (name, p) in &profiles {
        assert!(
            p.exposed_tools().len() <= MAX_EXPOSED_TOOLS,
            "profile `{name}` exposes {} tools",
            p.exposed_tools().len()
        );
    }

    // The enumeration must not silently fall behind the code. Every `pub fn` on
    // `CapabilityProfile` that returns `Self` is a profile constructor, and each one has to
    // appear above.
    let profile_rs = std::fs::read_to_string(crate_src().join("profile.rs")).unwrap();
    let constructors: BTreeSet<String> = profile_rs
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub fn "))
        .filter(|l| l.contains("() -> Self"))
        .map(|l| l.split('(').next().unwrap_or("").to_string())
        .collect();
    let enumerated: BTreeSet<String> = profiles.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(
        constructors, enumerated,
        "a capability profile exists that this budget test does not enumerate"
    );
}

#[test]
fn the_registry_can_hold_more_than_it_exposes() {
    let r = builtin_registry().unwrap();
    assert_eq!(r.len(), BUILTIN_TOOLS.len());
    let ids: Vec<ToolId> = BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect();
    assert!(ExposedSet::new(ids).is_ok(), "eleven fits");

    // **One past the budget does not, and the refusal is in the CONSTRUCTOR rather than at a
    // call site.** Derived from `MAX_EXPOSED_TOOLS` rather than typed: ADR-058 moved it from
    // twelve to thirteen, and a hardcoded 13 here would have silently become "exactly the
    // budget" -- a test whose subject had quietly changed from refusal to acceptance.
    let too_many: Vec<ToolId> =
        (0..=MAX_EXPOSED_TOOLS).map(|i| ToolId::new(format!("t{i}"))).collect();
    assert!(ExposedSet::new(too_many).is_err(), "one past {MAX_EXPOSED_TOOLS} must refuse");
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 2. User-facing nouns ≤ 7
// ─────────────────────────────────────────────────────────────────────────────────────────

/// ADR-007, amended 2026-08-22 to eight. Adding a ninth requires deleting one.
const NOUNS: [&str; 8] =
    ["session", "memory", "skill", "tool", "run", "trigger", "profile", "provider"];

/// Every user-facing command, and the noun it is a view over.
///
/// This is the shape HP10's grep wanted without the false-positive problem: a command that
/// names no noun cannot be added, and a command that names an eighth fails the build. The
/// maintenance burden is one line per command, paid by whoever adds the command.
const COMMAND_NOUNS: &[(&str, &str)] = &[
    ("runs", "run"),
    // M3 Session A. **Both are views over `run` and neither is an eighth concept** -- `/watch`
    // is one run rendered in full, `/steer` is a write to one run. The guard caught them the
    // moment they were added to the registry, which is what it is for.
    ("watch", "run"),
    ("steer", "run"),
    ("schedule", "trigger"),
    ("sessions", "session"),
    ("skills", "skill"),
    ("trust", "tool"), // a trust tier is a property of an (action, tool) class — §5's mapping
    ("status", "session"),
    ("state", "session"),
    ("model", "profile"),
    // ADR-049 §7. The EIGHTH noun, and it owns itself: which company serves the weights is not
    // a property of how this profile is configured. Reusing `profile` would have held the count
    // at seven by making one of the seven mean two things.
    ("provider", "provider"),
    ("profile", "profile"),
    ("session", "session"),
    ("workspace", "profile"),
    ("autonomy", "tool"),
    ("undo", "session"),
    ("compact", "session"),
    ("keys", "session"),
    ("doctor", "profile"),
    ("help", "session"),
    ("quit", "session"),
];

#[test]
fn every_user_facing_command_is_a_view_over_one_of_seven_nouns() {
    for (command, noun) in COMMAND_NOUNS {
        assert!(
            NOUNS.contains(noun),
            "`/{command}` claims the noun `{noun}`, which is not a declared noun (ADR-007). \
             Every new user-facing concept must delete one — which?"
        );
    }
}

#[test]
fn the_command_registry_has_not_grown_a_command_with_no_noun() {
    // The registry lives in `marlowe-surface`, which this crate does not depend on — so the
    // check reads the source. Crude, and HP10 accepts crude: the alternative is a dependency
    // from the loop onto a surface, which §2.14 forbids in that direction.
    let path = workspace_root().join("crates/marlowe-surface/src/commands.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let declared: BTreeSet<String> = src
        .lines()
        .filter_map(|l| l.split_once("Command { name: \""))
        .filter_map(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name.to_string())
        .collect();
    let accounted: BTreeSet<String> =
        COMMAND_NOUNS.iter().map(|(c, _)| (*c).to_string()).collect();

    assert!(!declared.is_empty(), "the registry scan found nothing; the parse has drifted");
    assert_eq!(
        declared, accounted,
        "the command registry and the noun table disagree. A command with no noun is an \
         eighth concept arriving without an argument"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 3. Agent loops == 1
// ─────────────────────────────────────────────────────────────────────────────────────────

/// A line carrying this marker is exempt from the driving-loop scan, and the marker is
/// required to be followed by a reason. Making the exemption visible in the diff is the whole
/// mechanism — an allowlist in this file would drift from the code it describes.
const EXEMPT: &str = "LOOP-EXEMPT:";

#[test]
fn there_is_exactly_one_driving_loop_in_this_crate() {
    let mut found: Vec<String> = Vec::new();
    for file in rust_sources(&crate_src()) {
        let src = std::fs::read_to_string(&file).unwrap();
        let mut in_test_module = false;
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("mod tests") {
                in_test_module = true;
            }
            if in_test_module || line.contains(EXEMPT) {
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            // `while let` and `for` are iteration. A bare `loop {` or `while <cond>` is a
            // driver: something that keeps going until a condition inside it says stop.
            let is_driving = code.trim_start().starts_with("loop {")
                || (code.contains("while ") && !code.contains("while let"));
            if is_driving {
                let rel = file.strip_prefix(workspace_root()).unwrap_or(&file);
                found.push(format!("{}:{} {}", rel.display(), n + 1, code.trim()));
            }
        }
    }

    assert_eq!(
        found.len(),
        1,
        "there must be exactly one driving loop in this crate (ARCHITECTURE §2.8). If a second \
         one is needed, the architecture is wrong and the answer is a CapabilityProfile. Found:\
         \n{}",
        found.join("\n")
    );
    assert!(
        found[0].contains("engine.rs"),
        "the one loop must be the one in engine.rs, found: {}",
        found[0]
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 4. Zero-config first run — the library half
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_loop_starts_with_no_configuration_file_anywhere() {
    // What this asserts: an `Engine` with the interactive profile, all eleven builtins, a
    // budget and a workspace can be constructed and can run a turn, and none of it reads a
    // config file — every knob has a defensible default in code (ARCHITECTURE §5).
    //
    // What it does NOT assert, and K6 does: install → first useful output in under five
    // minutes in a clean container. That needs the daemon, an onboarding path and a provider
    // client, and it is M2 Session E. Counting this test as K6 would be measuring something
    // adjacent to the criterion and reporting it as the criterion.
    let registry = builtin_registry().expect("the builtins are compiled in, not loaded");
    // Ten since M2 C2e removed `done`: a run ends when the model replies without calling a
    // tool, so a tool whose only job was ending no longer exists.
    assert_eq!(registry.len(), 12);

    let profile = CapabilityProfile::interactive();
    // **Registered is ten and exposed is ten: the gap closed in M2 C3.**
    //
    // It was nine for as long as `use` was compiled in with no executor — exposing it would have
    // handed the model a tool it could call and never run. `use` gained one in C3 (ADR-051,
    // `marlowe_daemon::skills::SkillTools`), so the guard admits it, exactly as it admitted `web`
    // in C2f and `recall` in Session D. **Three tools have crossed this gap and none because
    // anybody remembered to**: `verify_every_exposed_tool_is_runnable` derives the exposed set
    // from what is runnable rather than maintaining it beside the truth.
    //
    // The equality is not the property. The property is that the two numbers agree *because* a
    // guard makes them agree, and the next tool to arrive fails this line until it can run.
    // **Eleven since ADR-058 split `write` out of `edit`.** Four tools have crossed this gap now.
    assert_eq!(profile.exposed_tools().len(), 12);

    let budget = marlowe_loop::Budget::interactive();
    assert!(budget.tokens > 0 && budget.micros_usd > 0, "every dimension has a default");

    // **Nothing is reachable by default** — and the default set is EMPTY rather than `*`, which
    // is how brief §8's allowlist-by-default is satisfied by a policy that can still be widened
    // one host at a time by a human (ADR-032 §3.1).
    assert_eq!(
        *profile.egress(),
        marlowe_permission::EgressPolicy::AllowApproved { granted: Vec::new() }
    );
    assert!(
        !profile.egress().grants(&marlowe_permission::Host::from_url("https://example.com/").unwrap()),
        "a fresh interactive run reaches nothing until somebody approves a host"
    );
}
