//! **`/provider` is a control-strip control that is not on the control strip.**
//!
//! ADR-049 §7. The provider needs to be selectable from the session, and the strip is `[Rect; 5]`
//! with §B13's layout measured against that width at five sizes. Widening it to six is a real
//! interface change with a real argument; adding a command is not. So `ControlId::Provider` exists,
//! is reachable by `/provider`, and is deliberately absent from `ControlId::ALL`.
//!
//! That asymmetry is what this file tests, in both directions: the command must work, **and** the
//! strip must not have grown a sixth cell behind anyone's back.
//!
//! # The view comes from the stub, and that is a boundary not a convenience
//!
//! `marlowe-surface` cannot see `marlowe-daemon` — `c2d_boundary.rs` asserts that a producer is
//! absent from `[dependencies]`, because a surface that can build a view can hold state the daemon
//! does not have. So the projection half — *"the picker selects the provider the daemon
//! reported"* — is asserted in `marlowe-daemon`'s own suite, next to `view_from_status`, and this
//! file asserts only what the surface does with a view it was handed.

use marlowe_surface::commands::{dispatch, Outcome, REGISTRY};
use marlowe_view::notice::{Listing, Notice, Refusal};
use marlowe_view::{ControlId, Intent, SessionView};

fn view() -> SessionView {
    marlowe_stub::Session::new().view().clone()
}

/// Bare `/provider` reports; it does not silently switch to anything.
#[test]
fn provider_with_no_argument_lists_rather_than_changing_anything() {
    match dispatch(&view(), "provider", &[]) {
        Outcome::Say(Notice::Listing(Listing::Control(ControlId::Provider))) => {}
        other => panic!("bare /provider must list the control, got {other:?}"),
    }
}

/// **The switch.** Naming a provider asks the producer to select it — it does not change the view,
/// because a surface that changed it would be showing a provider the daemon may have refused.
#[test]
fn naming_a_provider_asks_the_producer_to_select_it() {
    let v = view();
    let want = v
        .picker(ControlId::Provider)
        .options
        .iter()
        .position(|o| o == "openrouter")
        .expect("openrouter must be offered");

    match dispatch(&v, "provider", &["openrouter"]) {
        Outcome::Ask(Intent::Select { control: ControlId::Provider, option }) => {
            assert_eq!(option, want, "the index must be the one the picker holds");
        }
        other => panic!("/provider openrouter must ask to select it, got {other:?}"),
    }

    // The control: a name nobody offers is refused by name rather than selecting a neighbour.
    // Without this, a dispatch that returned `Select { option: 0 }` for any argument would pass
    // the assertion above whenever `openrouter` happened to be first.
    match dispatch(&v, "provider", &["anthropic"]) {
        Outcome::Rejected(Refusal::NoSuchOption { control: ControlId::Provider, .. }) => {}
        other => panic!("an unknown provider must be refused, got {other:?}"),
    }
}

/// **The strip did not grow.** `ControlId::Provider` being a `ControlId` is exactly the shape that
/// would quietly add a sixth cell — anything iterating `ALL` to lay out or draw would pick it up,
/// and §B13's five-size layout rows would be measuring a strip nobody decided to widen.
#[test]
fn adding_the_provider_control_did_not_widen_the_control_strip() {
    assert_eq!(ControlId::ALL.len(), 5, "the strip is five cells and §B13 measures it as five");
    assert!(
        !ControlId::ALL.contains(&ControlId::Provider),
        "Provider is reachable by command and is not a strip cell"
    );
    // The control: the ones that ARE on the strip are still on it, so the assertion above cannot
    // pass by `ALL` having been emptied.
    for c in [ControlId::Model, ControlId::Autonomy] {
        assert!(ControlId::ALL.contains(&c), "{c:?} must still be a strip cell");
    }
}

/// It is in the registry, so both surfaces get it — §B11's parity is structural and
/// `command_parity.rs` enforces it. This asserts the entry exists for that to bite on, and that
/// its description names the options, since the description is what autocomplete shows.
#[test]
fn provider_is_a_registered_command_whose_description_names_both_options() {
    let c = REGISTRY
        .iter()
        .find(|c| c.name == "provider")
        .expect("/provider must be in the one registry both surfaces read");
    assert!(
        c.description.contains("ollama") && c.description.contains("openrouter"),
        "the description is what autocomplete shows: {}",
        c.description
    );
}
