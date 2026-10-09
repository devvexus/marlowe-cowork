//! **The C2d acceptance.** A promotion that left the surface able to reach a producer would be a
//! rename, and a rename would have left every other test in this crate green.
//!
//! So the check is not behavioural. It is that `marlowe-surface`'s *manifest* lists no producer
//! outside `[dev-dependencies]`, which the compiler then enforces for every line in `src/`.
//!
//! # Why a manifest test and not "it compiles"
//!
//! Because "it compiles" is exactly the reading that would not change if somebody added
//! `marlowe-stub` back to `[dependencies]` for one convenient call. The failure would be invisible:
//! the build stays green, the tests stay green, and §2.14 quietly stops being structural. This
//! test asks the question whose answer differs.

use std::fs;
use std::path::Path;

fn manifest() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}). This guard names a path, so it must fail when the path moves \
             rather than scanning nothing and passing.",
            p.display()
        )
    })
}

/// Split a Cargo manifest into `[dependencies]` and everything after `[dev-dependencies]`.
fn sections(src: &str) -> (String, String) {
    let mut deps = String::new();
    let mut dev = String::new();
    let mut current = "";
    for line in src.lines() {
        let t = line.trim();
        // Comments are skipped, and the reason is worth stating: the first version of this test
        // failed on the *comment* in `[dependencies]` explaining why the stub is not there. A
        // guard that fires on the sentence documenting the rule makes the rule undocumentable —
        // `determinism_guard.rs` reaches the same conclusion for the same reason.
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            current = match t {
                "[dependencies]" => "deps",
                "[dev-dependencies]" => "dev",
                _ => "",
            };
            continue;
        }
        match current {
            "deps" => deps.push_str(&format!("{t}\n")),
            "dev" => dev.push_str(&format!("{t}\n")),
            _ => {}
        }
    }
    (deps, dev)
}

/// A producer is anything that can construct session state. The surface may depend on **none** of
/// them, because a surface that can build a view can hold state the daemon does not have.
const PRODUCERS: &[&str] = &["marlowe-stub", "marlowe-daemon", "marlowe-loop", "marlowe-memory"];

#[test]
fn the_surface_depends_on_no_producer() {
    let (deps, dev) = sections(&manifest());

    // The parse is asserted before the absence is. A section reader that silently found nothing
    // would report "no producers" for a manifest full of them -- the vacuous-pass shape.
    assert!(
        deps.contains("marlowe-view"),
        "the dependency section did not parse; it should contain marlowe-view:\n{deps}"
    );

    for p in PRODUCERS {
        assert!(
            !deps.contains(p),
            "`{p}` is a non-dev dependency of marlowe-surface.\n\n\
             ARCHITECTURE §2.14: a surface holds no state the daemon does not have. M1 asserted \
             that in two module headers while `App` owned a `marlowe_stub::Session` mutably and \
             pushed into its transcript. M2 C2d made it structural: the surface renders a \
             `SessionView` it cannot write and returns `Intent`s it cannot apply.\n\n\
             If a producer is needed in `src/`, that is the boundary dissolving. Take the \
             producer as a `&mut impl marlowe_view::Produce` instead -- the surface can then use \
             one without being able to construct one."
        );
    }

    // The stub IS expected in dev-dependencies: a headless interaction suite needs a scripted
    // producer. Asserting its presence keeps the test honest -- if it vanished, the check above
    // would pass for the wrong reason.
    assert!(
        dev.contains("marlowe-stub"),
        "the scripted producer should still be a dev-dependency:\n{dev}"
    );
}

/// The other half: the surface must not read a real clock either.
///
/// `determinism_guard.rs` states it (*"marlowe-surface gets no exemption at all"*) and greps for
/// it across the workspace. This asserts the positive form locally, so the reason travels with
/// the crate it constrains.
#[test]
fn the_surface_reads_no_clock_and_takes_time_as_an_argument() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).expect("src/") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let src = fs::read_to_string(&path).expect("source");
            for (n, line) in src.lines().enumerate() {
                let t = line.trim();
                if t.starts_with("//") {
                    continue;
                }
                for bad in ["Instant::now", "SystemTime", "UNIX_EPOCH"] {
                    assert!(
                        !t.contains(bad),
                        "{}:{} reads a real clock: {t}\n\nEvery render is a pure function of \
                         (state, now_ms), which is what makes §B13's flicker rows diffable. Time \
                         arrives through `marlowe_view::ClockRead`.",
                        path.display(),
                        n + 1
                    );
                }
            }
            checked += 1;
        }
    }
    // Without this, a `read_dir` that returned nothing would pass silently -- the same latent
    // shape this project has logged as its fourteenth instance.
    assert!(checked >= 10, "only {checked} source files scanned; the walk found nothing to check");
    println!("marlowe-surface: {checked} files, 0 real-clock reads");
}
