//! §B7's inspector: six tabs, one visible at a time, each reachable by its number.
//!
//! **Tabs are pinned; content scrolls.** The tab bar sits outside the pane's scroll area, which is
//! the same rule as the conversation's pager and is the bug this design is most likely to ship
//! with.
//!
//! # The rule that makes the inspector worth having
//!
//! > When the user asks for something the inspector can render, the inspector renders it and the
//! > conversation says only what a colleague would say out loud.
//!
//! The transcript carries judgment; the region carries data. Without that the tabs are just a
//! menu, and a menu is not an argument for a TUI over a chat log.
//!
//! # The four tabs M1 does not fill
//!
//! Sessions, Skills, Trust and Status are **present, reachable, and honest**. Each renders one
//! bordered region with a label, a key, and a line naming what will live there and in which
//! milestone.
//!
//! A tab that silently rendered nothing would be worse than one that says it is not built, because
//! the user cannot tell an empty pane from a broken one. A tab that rendered invented data would
//! be worse still: it would make the tab bar a claim about capability that is false.

use marlowe_view::{Item, SessionView, Tab, Tone};

/// The items for the pane currently displayed.
pub fn items(view: &SessionView, tab: Tab) -> Vec<Item> {
    items_for(view, tab)
}

/// The items for any pane. Used by the key registry, which validates **all six** at startup rather
/// than only the visible one — a collision in a pane the session never opens would otherwise ship.
pub fn items_for(view: &SessionView, tab: Tab) -> Vec<Item> {
    match tab {
        Tab::Runs => view.runs.clone(),
        Tab::Schedule => view.schedule.clone(),
        Tab::Sessions => vec![not_yet(
            "Sessions",
            'k',
            "history, searchable by content",
            "turns, artefacts produced, compaction depth",
        )],
        Tab::Skills => vec![not_yet(
            "Skills",
            'l',
            "grouped by domain, not a flat list",
            "installed count and exposed tools against the ≤12 budget",
        )],
        Tab::Trust => vec![not_yet(
            "Trust",
            'u',
            "action classes, tier, agreement rate, trend",
            "promotion proposals with evidence, and ceilings no evidence lifts",
        )],
        // **The real pane, not the placeholder.** The daemon's announcements are projected into
        // `SessionView::status_pane` by `marlowe_daemon::project`, and this is where they are
        // read. It said *"not built — M2, against real data"* while the data was already arriving
        // on the wire and being folded into the view — the pane existed, the projection existed,
        // and nothing drew it, so the screen said the feature was unbuilt.
        //
        // Empty is still honest: a daemon that has announced nothing has nothing to show, and the
        // placeholder says so rather than leaving a blank region, which §B7 forbids because an
        // empty pane is indistinguishable from a broken one.
        Tab::Status if !view.status_pane.is_empty() => view.status_pane.clone(),
        Tab::Status => vec![Item::new(
            "Status",
            'b',
            Tone::Dim,
            &[("the daemon has announced nothing yet this session", Tone::Dim)],
        )],
    }
}

/// The honest placeholder. Says what the pane is for and when it arrives.
fn not_yet(label: &str, key: char, holds: &str, also: &str) -> Item {
    Item::new(
        label,
        key,
        Tone::Dim,
        &[
            (holds, Tone::Dim),
            (also, Tone::Dim),
            ("not built — M2, against real data", Tone::Amber),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_renders_at_least_one_region() {
        // §B7's tab bar must not lie. An empty pane is indistinguishable from a broken one.
        let s = marlowe_stub::Session::new();
        for tab in Tab::ALL {
            assert!(
                !items_for(s.view(), tab).is_empty(),
                "{} renders nothing; a reachable tab with no region tells the user the interface \
                 is broken",
                tab.title()
            );
        }
    }

    #[test]
    fn the_four_deferred_panes_say_so_rather_than_showing_invented_data() {
        let s = marlowe_stub::Session::new();
        // **Status is no longer one of them.** It went live on 2026-08-27: the daemon's own
        // announcements -- the engine start and its measured tok/s, an Ollama eviction, a reserve
        // that could not be taken -- are projected into `SessionView::status_pane` and drawn here.
        // It said "not built - M2" for a while AFTER the data was already arriving on the wire and
        // being folded into the view, because the pane and the projection existed and nothing drew
        // them. Leaving it in this list would have kept asserting that a shipped pane is unbuilt.
        for tab in Tab::ALL.iter().filter(|t| !t.is_live_in_m1() && **t != Tab::Status) {
            let items = items_for(s.view(), *tab);
            let says_so = items.iter().any(|i| {
                i.lines
                    .iter()
                    .any(|(text, _)| text.contains("not built") && text.contains("M2"))
            });
            assert!(
                says_so,
                "{} neither carries data nor says it is unbuilt. Inventing plausible rows here \
                 would make the tab bar a false claim about capability",
                tab.title()
            );
        }

        // **And the positive half, because excluding a tab from a "says it is unbuilt" check is
        // not the same as showing that it is built.** Dropping Status from the loop above would
        // pass on a build where the pane renders nothing at all -- which is exactly the state it
        // was in this morning.
        let status = items_for(s.view(), Tab::Status);
        assert!(!status.is_empty(), "the Status pane must render a region; an empty pane is                  indistinguishable from a broken one");
        assert!(
            !status.iter().any(|i| i
                .lines
                .iter()
                .any(|(text, _)| text.contains("not built") && text.contains("M2"))),
            "Status still claims to be unbuilt while its projection is live"
        );
    }
}
