//! The model picker for local models: every source in one searchable list, each labelled, an MS
//! model that the selected backend cannot run asking whether to switch, and a pasted path adding a
//! model folder.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_model::{ModelInfo, Origin, ProviderKind};
use marlowe_proto::{Event, Request};
use marlowe_tui::library::LibStep;
use marlowe_tui::{State, View};

fn local(id: &str, display: &str, source: &str, detail: Option<&str>) -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, id);
    m.display = display.into();
    m.context_window = Some(32_768);
    m.supports_tools = true;
    m.origin = Some(Origin { source: source.into(), detail: detail.map(str::to_string), size: Some(9 << 30), ms: None, blocked: None, switch_to: None });
    m
}

fn ms_model(blocked: bool) -> ModelInfo {
    let mut m = local("gguf/Qwen3.8-27B-MS2.7", "Qwen3.8-27B-MS2.7", "folder", Some("D:\\models"));
    let o = m.origin.as_mut().unwrap();
    o.ms = Some("MS Gen 1".into());
    if blocked {
        o.blocked = Some("Qwen3.8-27B-MS2.7 is an MS (Mad Shift) model: it runs on ms-llama.cpp.".into());
        o.switch_to = Some("ms-llama.cpp".into());
    }
    m
}

fn catalog(blocked: bool) -> Vec<ModelInfo> {
    vec![
        local("dusk:22b", "dusk:22b", "ollama", None),
        local("gguf/hf/unsloth/Qwen3-8B-GGUF/Qwen3-8B-Q4_K_M", "Qwen3-8B-Q4_K_M", "huggingface", Some("unsloth/Qwen3-8B-GGUF")),
        ms_model(blocked),
    ]
}

/// A state with the local catalog open.
fn open(blocked: bool) -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, empty_library());
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.view, View::Library);
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::ListCatalog { provider: ProviderKind::Ollama }]);
    feed(&mut s, 2, Event::Catalog { provider: ProviderKind::Ollama, models: catalog(blocked) });
    s
}

#[test]
fn every_source_is_one_list_each_labelled_and_the_search_reaches_the_label() {
    let mut s = open(true);
    let buf = render(&s, W, H);
    for want in ["3 of 3 models", "From Ollama", "From Hugging Face", "From folder", "MS Gen 1", "Qwen3.8-27B-MS2.7"] {
        assert!(contains(&buf, want), "the picker is missing {want}");
    }
    assert!(!contains(&buf, "Local GGUF") && !contains(&buf, "coming soon"));
    assert_palette_only(&buf);
    type_str(&mut s, "hugging");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "1 of 3 models") && contains(&buf, "Qwen3-8B-Q4_K_M") && !contains(&buf, "dusk:22b"));
    for _ in 0..7 {
        s.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut s, "mad shift");
    assert!(contains(&render(&s, W, H), "1 of 3 models"), "an MS model is found by `mad shift`");
}

#[test]
fn an_ms_model_the_backend_cannot_run_says_so_in_the_picker_and_offers_the_switch() {
    let mut s = open(true);
    type_str(&mut s, "ms2.7");
    // The cursor's model carries its reason before anything is loaded.
    assert!(contains(&render(&s, W, H), "is an MS (Mad Shift) model: it runs on ms-llama.cpp."));
    // Picking it asks, in the picker, and adds nothing yet.
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Switch backend? [s]"));
    assert_palette_only(&buf);
    snapshot("library_local_switch", &buf);
    // s switches the backend and adds the model.
    let reqs = s.on_key(key(KeyCode::Char('s')));
    assert_eq!(
        reqs,
        vec![
            Request::BackendSelect { backend: "ms-llama.cpp".into() },
            Request::AddModel { provider: ProviderKind::Ollama, model_id: "gguf/Qwen3.8-27B-MS2.7".into() },
            Request::LibraryState,
        ]
    );
    assert_eq!(s.library.step, LibStep::Overview);
}

#[test]
fn enter_adds_it_anyway_and_anything_else_cancels_the_question() {
    let mut s = open(true);
    type_str(&mut s, "ms2.7");
    s.on_key(key(KeyCode::Enter));
    assert_eq!(s.on_key(key(KeyCode::Enter)).len(), 2, "added without switching: AddModel and LibraryState");
    let mut s = open(true);
    type_str(&mut s, "ms2.7");
    s.on_key(key(KeyCode::Enter));
    assert!(s.on_key(key(KeyCode::Esc)).is_empty());
    assert!(matches!(s.library.step, LibStep::Catalog { .. }), "the picker stays open");
    assert!(s.library.ask.is_none());
}

#[test]
fn on_the_right_backend_an_ms_model_is_added_with_no_question() {
    let mut s = open(false);
    type_str(&mut s, "ms2.7");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs[0], Request::AddModel { provider: ProviderKind::Ollama, model_id: "gguf/Qwen3.8-27B-MS2.7".into() });
}

#[test]
fn a_pasted_path_adds_a_model_folder_and_a_name_that_matches_nothing_does_not() {
    let mut s = open(true);
    type_str(&mut s, "nonsense");
    assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "a name is not a path");
    for _ in 0..8 {
        s.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut s, "D:\\more models");
    assert!(contains(&render(&s, W, H), "searches this path for .gguf files"));
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::AddModelDir { path: "D:\\more models".into() }]);
    assert!(s.library.catalog.loading);
    // The daemon answers with the catalog read again.
    feed(&mut s, 3, Event::Catalog { provider: ProviderKind::Ollama, models: catalog(true) });
    assert!(!s.library.catalog.loading);
    // A refusal shows on the catalog and the next key clears it.
    type_str(&mut s, "C:\\nope");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 4, Event::Error { session: None, message: "C:\\nope is not a folder or a .gguf file".into() });
    assert!(contains(&render(&s, W, H), "is not a folder or a .gguf file"));
    s.on_key(key(KeyCode::Char('x')));
    assert!(!contains(&render(&s, W, H), "is not a folder or a .gguf file"));
}

#[test]
fn choosing_a_backend_while_the_catalog_is_open_reads_it_again() {
    let mut s = open(true);
    let reqs = feed(
        &mut s,
        3,
        Event::BackendDone { backend: "ms-llama.cpp".into(), op: marlowe_proto::BackendOp::Select, ok: true, message: "ms-llama.cpp serves models now".into() },
    );
    assert_eq!(reqs, vec![Request::ListCatalog { provider: ProviderKind::Ollama }]);
}
