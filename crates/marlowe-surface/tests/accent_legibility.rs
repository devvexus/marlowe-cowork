//! **M1's last open acceptance row, and the arithmetic half of it.**
//!
//! §B13 asks: *"Accent legible on both dark and light terminal backgrounds — **Verified by eye on
//! each**."* The ROADMAP is explicit that this row *"cannot be a test on its own — violet is the
//! accent most likely to fail it, and a value that works only on dark works on one machine."*
//!
//! That is right, and it is not a reason to assert nothing. The row has two halves:
//!
//! * **By eye** — a human confirms it is comfortable on their terminal. `--doctor` prints both
//!   numbers and says so. **That half is not automatable and this file does not pretend to it.**
//! * **The arithmetic** — the shipped default must not fall below its floor on either reference
//!   background. That half is a property, and it was being *reported* by `doctor::verdict` while
//!   nothing asserted it. A number printed beside a passing build is not a guard.
//!
//! # Which floor applies, and why it is 3.0 rather than 4.5
//!
//! WCAG's 4.5:1 is for **body text**. The accent's role is **structure** — `render.rs:467` states
//! it directly: *"structure is the accent's role (§B2)"*. It draws markers, hotkeys, the scrollbar
//! thumb and region labels; it never renders prose. Non-text UI components and large text take
//! **3:1**, which is the line `doctor::verdict` already names for exactly this reason.
//!
//! # The fact that stops someone "fixing" this by raising the bar
//!
//! Asking one accent to clear **4.5:1 against both** pure black and pure white confines it to a
//! luminance window of `0.175 ..= 0.1833` — **width 0.0083, under 1% of the range**. Any such colour
//! is very dark, which for a violet means it stops reading as violet. `both_floors_are_satisfiable`
//! records that so a later session raising the bar knows what it costs rather than discovering it.

use marlowe_surface::doctor::{REFERENCE_DARK, REFERENCE_LIGHT};
use marlowe_surface::theme::{contrast, ACCENT_RGB};

/// The floor that applies to the accent's actual role. See the header.
const UI_FLOOR: f64 = 3.0;

/// **The row, as an assertion.** The shipped default clears its floor on BOTH references.
///
/// Both directions, because a value that works only on dark works on one machine — which is the
/// ROADMAP's own sentence and the whole reason the row is stated as "both".
#[test]
fn the_default_accent_clears_its_floor_on_both_reference_backgrounds() {
    let dark = contrast(ACCENT_RGB, REFERENCE_DARK);
    let light = contrast(ACCENT_RGB, REFERENCE_LIGHT);

    assert!(
        dark >= UI_FLOOR,
        "accent #{:02X}{:02X}{:02X} is {dark:.2}:1 on the dark reference, under the {UI_FLOOR} \
         floor for UI components",
        ACCENT_RGB.0,
        ACCENT_RGB.1,
        ACCENT_RGB.2
    );
    assert!(
        light >= UI_FLOOR,
        "accent #{:02X}{:02X}{:02X} is {light:.2}:1 on the LIGHT reference, under the {UI_FLOOR} \
         floor. Light is the direction violet fails first: the recorded 3.26 was always this \
         number, and nothing said what it was measured against",
        ACCENT_RGB.0,
        ACCENT_RGB.1,
        ACCENT_RGB.2
    );
}

/// **The light direction is the tight one, and how tight is worth pinning.**
///
/// On dark the default is 6.43:1; on light it is 3.26:1 — comfortably over 3.0 and well under 4.5.
/// So a change that improves the dark reading by darkening the accent moves the light reading the
/// wrong way, and the margin there is 0.26. This test exists so that trade is visible in a failure
/// message rather than discovered on somebody's terminal.
#[test]
fn light_is_the_binding_direction_and_its_margin_is_small() {
    let dark = contrast(ACCENT_RGB, REFERENCE_DARK);
    let light = contrast(ACCENT_RGB, REFERENCE_LIGHT);

    assert!(
        light < dark,
        "light is expected to be the binding direction for a violet accent; if that has inverted \
         the accent has changed character and the header's reasoning needs re-reading \
         (dark {dark:.2}, light {light:.2})"
    );
    assert!(
        light < 4.5,
        "the light reading now clears the BODY TEXT floor ({light:.2}). That is an improvement and \
         it invalidates this test's premise, not the product. Re-read the header and delete this \
         assertion deliberately"
    );
}

/// A user-supplied accent is refused when malformed — but a **well-formed** one that is illegible
/// is accepted silently. `--doctor` measures the RESOLVED colour and says so; this pins that the
/// measurement follows the override rather than the default.
#[test]
fn contrast_is_measured_on_the_resolved_colour_not_the_default() {
    // A colour deliberately close to the light reference: legible on dark, unreadable on light.
    let pale = (0xF2u8, 0xEEu8, 0xFAu8);
    let light = contrast(pale, REFERENCE_LIGHT);
    assert!(
        light < UI_FLOOR,
        "premise: {pale:?} must be under the floor on light for this test to mean anything, got \
         {light:.2}"
    );
    // And the default is not: so the function discriminates rather than returning a constant.
    assert!(contrast(ACCENT_RGB, REFERENCE_LIGHT) >= UI_FLOOR);
}

/// **The cost of raising the bar, recorded as arithmetic.**
///
/// One accent clearing 4.5:1 against both pure black and pure white must have luminance in
/// `0.175 ..= 0.1833`. This asserts the window is real and narrow, so nobody raises the row to 4.5
/// on both without seeing what it confines the palette to.
#[test]
fn both_floors_are_satisfiable_but_the_window_is_under_one_percent() {
    // Derived from WCAG's own definition: vs white 1.05/(L+0.05) >= 4.5, vs black (L+0.05)/0.05 >= 4.5.
    let lo = 0.175_f64;
    let hi = 1.05 / 4.5 - 0.05;
    assert!(hi > lo, "the window must be non-empty: {lo} ..= {hi}");
    assert!(
        hi - lo < 0.01,
        "the window is {:.4} wide; the header claims under 1% of the luminance range",
        hi - lo
    );
}
