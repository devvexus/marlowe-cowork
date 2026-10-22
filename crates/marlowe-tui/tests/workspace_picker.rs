//! The workspace picker's path field: folder suggestions under what is typed, recents, `~` and the
//! drives; Tab, the arrows, clicks and Enter; and a path that does not exist being refused with
//! the reason on screen. Folder listings are read by the same worker code the run loop uses.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_tui::coding::LocalEvent;
use marlowe_tui::render::geometry;
use marlowe_tui::state::{ClientEvent, Overlay};
use marlowe_tui::{local, State};
use ratatui::layout::Rect;
use std::path::PathBuf;

/// A scratch tree removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("marlowe-wsp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["alpha", "alps", "beta", ".hidden"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        Self(dir)
    }

    /// The folder as typed in the picker: forward slashes.
    fn typed(&self) -> String {
        self.0.to_string_lossy().replace('\\', "/")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn area() -> Rect {
    Rect::new(0, 0, 160, 45)
}

fn picker() -> State {
    let mut s = state();
    s.date = "Friday 9 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Char('w')));
    assert!(matches!(s.overlay, Some(Overlay::WorkspacePicker { .. })));
    s
}

/// Runs the folder reads the picker asked for, as the run loop's workers would, and feeds the
/// answers back.
fn settle(s: &mut State) {
    for _ in 0..8 {
        let effects = s.take_effects();
        if effects.is_empty() {
            return;
        }
        for e in effects {
            if let Some(ev @ LocalEvent::DirListing { .. }) = local::run(e) {
                s.on_client(ClientEvent::Local(ev));
            }
        }
    }
}

fn query(s: &State) -> String {
    match &s.overlay {
        Some(Overlay::WorkspacePicker { query, .. }) => query.clone(),
        other => panic!("not the workspace picker: {other:?}"),
    }
}

fn sel(s: &State) -> usize {
    match &s.overlay {
        Some(Overlay::WorkspacePicker { sel, .. }) => *sel,
        other => panic!("not the workspace picker: {other:?}"),
    }
}

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

#[test]
fn typing_a_folder_lists_its_children_filtered_by_the_typed_prefix() {
    let t = Tree::new("list");
    let mut s = picker();
    type_str(&mut s, &format!("{}/", t.typed()));
    settle(&mut s);
    let buf = render(&s, 160, 45);
    assert_palette_only(&buf);
    for shown in ["/alpha", "/alps", "/beta"] {
        assert!(contains(&buf, shown), "{shown} is offered:\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "/.hidden") && !contains(&buf, "file.txt"), "hidden folders and files are not offered");

    type_str(&mut s, "al");
    let buf = render(&s, 160, 45);
    assert!(contains(&buf, "/alpha") && contains(&buf, "/alps") && !contains(&buf, "/beta"), "{}", dump(&buf));
    // The typed text stays the first row.
    assert_eq!(s.workspace_choices(&query(&s))[0], format!("{}/al", t.typed()));
}

#[test]
fn tab_completes_the_common_start_then_the_only_match_and_descends() {
    let t = Tree::new("tab");
    let mut s = picker();
    type_str(&mut s, &format!("{}/a", t.typed()));
    settle(&mut s);
    s.on_key(key(KeyCode::Tab));
    assert_eq!(query(&s), format!("{}/alp", t.typed()), "alpha and alps share `alp`");
    type_str(&mut s, "h");
    s.on_key(key(KeyCode::Tab));
    assert_eq!(query(&s), format!("{}/alpha/", t.typed()), "the only match is taken and entered");
    settle(&mut s);
    assert!(s.workspace_choices(&query(&s)).len() >= 1);
}

#[test]
fn tab_with_nothing_to_share_takes_the_selected_row() {
    let t = Tree::new("sel");
    let mut s = picker();
    type_str(&mut s, &format!("{}/alp", t.typed()));
    settle(&mut s);
    // Rows: the typed text, alpha, alps.
    s.on_key(key(KeyCode::Down));
    s.on_key(key(KeyCode::Down));
    assert_eq!(sel(&s), 2);
    s.on_key(key(KeyCode::Tab));
    assert_eq!(query(&s), format!("{}/alps/", t.typed()));
    s.on_key(key(KeyCode::Up));
    assert_eq!(sel(&s), 0, "a completed query starts at the top");
}

#[test]
fn enter_sets_the_chosen_folder_in_its_canonical_form() {
    let t = Tree::new("enter");
    let mut s = picker();
    type_str(&mut s, &format!("{}/be", t.typed()));
    settle(&mut s);
    s.on_key(key(KeyCode::Down));
    s.on_key(key(KeyCode::Enter));
    assert!(s.overlay.is_none());
    let want = marlowe_proto::workspace::normalise(&format!("{}/beta", t.typed())).unwrap();
    assert_eq!(s.default_workspace.as_deref(), want.to_str());
    assert!(!s.default_workspace.as_deref().unwrap().starts_with(r"\\?\"));
}

#[test]
fn clicking_a_suggestion_picks_it() {
    let t = Tree::new("click");
    let mut s = picker();
    type_str(&mut s, &format!("{}/", t.typed()));
    settle(&mut s);
    let r = geometry(area(), &s).picker.expect("the picker is open");
    // Rows start under the field and a blank: typed text, alpha, alps, beta.
    s.on_mouse(click(r.x + 6, r.y + 3 + 3), area());
    assert!(s.overlay.is_none());
    assert!(s.default_workspace.as_deref().unwrap().ends_with("beta"), "{:?}", s.default_workspace);
}

#[test]
fn a_hovered_suggestion_lights_like_the_other_rows() {
    let t = Tree::new("hover");
    let mut s = picker();
    type_str(&mut s, &format!("{}/", t.typed()));
    settle(&mut s);
    let r = geometry(area(), &s).picker.unwrap();
    let before = render(&s, 160, 45);
    s.on_mouse(MouseEvent { kind: MouseEventKind::Moved, column: r.x + 8, row: r.y + 3 + 2, modifiers: KeyModifiers::NONE }, area());
    let after = render(&s, 160, 45);
    assert_palette_only(&after);
    assert_ne!(before, after, "the row under the pointer changes");
}

#[test]
fn a_missing_folder_is_refused_with_the_reason_in_the_picker() {
    let t = Tree::new("missing");
    let mut s = picker();
    type_str(&mut s, &format!("{}/nowhere", t.typed()));
    settle(&mut s);
    s.on_key(key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::WorkspacePicker { .. })), "the picker stays open");
    assert_ne!(s.default_workspace.as_deref(), Some(format!("{}/nowhere", t.typed()).as_str()));
    let buf = render(&s, 160, 45);
    assert_palette_only(&buf);
    assert!(contains(&buf, "does not exist"), "{}", dump(&buf));
    // Editing clears the reason.
    type_str(&mut s, "x");
    assert!(!contains(&render(&s, 160, 45), "does not exist"));

    // A file is not a workspace either.
    for _ in 0..8 {
        s.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut s, "/file.txt");
    s.on_key(key(KeyCode::Enter));
    assert!(contains(&render(&s, 160, 45), "is a file"));
}

#[test]
fn home_and_the_drives_are_offered_before_anything_is_typed() {
    let mut s = picker();
    // What the worker answers for the roots.
    let roots = marlowe_tui::wspath::list_dirs("");
    assert!(roots.contains(&"~".to_string()));
    s.on_client(ClientEvent::Local(LocalEvent::DirListing { dir: String::new(), names: roots.clone() }));
    let choices = s.workspace_choices("");
    assert!(choices.contains(&"~".to_string()), "{choices:?}");
    if cfg!(windows) {
        assert!(choices.iter().any(|c| c.len() == 3 && c.ends_with(":\\")), "a drive root is offered: {choices:?}");
    }
    // Typing a letter narrows to the roots that start with it.
    let c = roots.iter().find(|r| r.ends_with(":\\")).cloned();
    if let Some(drive) = c {
        let letter = drive[..1].to_lowercase();
        let got = s.workspace_choices(&letter);
        assert!(got.iter().any(|g| g.eq_ignore_ascii_case(&drive)), "{got:?}");
    }
}

#[test]
fn the_picker_asks_for_each_folder_once_and_never_reads_while_drawing() {
    let t = Tree::new("once");
    let mut s = picker();
    // Opening asks for the roots.
    let first = s.take_effects();
    assert!(first.iter().any(|e| matches!(e, marlowe_tui::coding::Effect::ListDir { dir } if dir.is_empty())), "{first:?}");
    type_str(&mut s, &format!("{}/", t.typed()));
    // One read for each folder the path goes through, the typed one last, none twice.
    let asked = s.take_effects();
    let dirs: Vec<String> = asked.iter().map(|e| format!("{e:?}")).collect();
    assert!(dirs.last().unwrap().contains(&format!("{}/", t.typed())), "{dirs:?}");
    let mut seen = dirs.clone();
    seen.dedup();
    assert_eq!(seen, dirs, "no folder is read twice");
    type_str(&mut s, "alp");
    assert!(s.take_effects().is_empty(), "more letters in the same folder read nothing");
    // Drawing before the answer arrives shows the typed row only, and does not touch the disk.
    let buf = render(&s, 160, 45);
    assert!(!contains(&buf, "/alpha"));
}

#[test]
fn suggestions_are_capped_to_the_rows_the_picker_has() {
    let mut s = picker();
    let names: Vec<String> = (0..50).map(|i| format!("dir{i:02}")).collect();
    s.on_client(ClientEvent::Local(LocalEvent::DirListing { dir: "Z:/many/".into(), names }));
    assert_eq!(s.workspace_choices("Z:/many/").len(), marlowe_tui::state::WORKSPACE_ROWS);
}
