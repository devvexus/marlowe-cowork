//! Shared fixtures for the TUI tests.
#![allow(dead_code)]

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_model::{Effort, ModelInfo, ModelRef, Pricing, ProviderKind, Usage};
use marlowe_proto::{
    Envelope, Event, LibraryModel, ProviderStatus, Request, SessionId, Slot, SlotAssignment,
};
use marlowe_surface::Theme;
use marlowe_tui::{ClientEvent, State};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

pub const W: u16 = 140;
pub const H: u16 = 40;

pub fn theme() -> Theme {
    Theme::default_truecolor()
}

pub fn sid() -> SessionId {
    SessionId::from("5e55")
}

/// A state as the run loop would have it, with a fixed clock.
pub fn state() -> State {
    let mut s = State::new(Some("C:/work/valiance".into()));
    s.clock = "21:40".into();
    s.now_ms = 1_800_000_000_000;
    s
}

pub fn feed(s: &mut State, seq: u64, event: Event) -> Vec<Request> {
    s.on_client(ClientEvent::Envelope(Envelope { seq, at_ms: 1_800_000_000_000, event }))
}

pub fn welcome(s: &mut State, seq: u64) -> Vec<Request> {
    feed(
        s,
        0,
        Event::Welcome {
            daemon_version: "0.1.0".into(),
            proto_version: marlowe_proto::PROTO_VERSION,
            profile_home: "C:/Users/m/AppData/Local/MarloweCowork".into(),
            seq,
        },
    )
}

/// What every Welcome asks for: the library, then the Wake's probes.
pub fn welcome_requests() -> Vec<Request> {
    vec![
        Request::LibraryState,
        Request::MemoryStats,
        Request::WatchList,
        Request::ListNotices { limit: marlowe_tui::wake::NOTICE_LIMIT, unseen_only: false },
        Request::VoiceSettings { enabled: None, voice: None },
    ]
}

pub fn providers(openrouter_configured: bool) -> Vec<ProviderStatus> {
    ProviderKind::ALL
        .iter()
        .map(|k| ProviderStatus {
            kind: *k,
            available_now: k.is_available_now(),
            configured: *k == ProviderKind::Ollama
                || (*k == ProviderKind::OpenRouter && openrouter_configured),
            detected: *k == ProviderKind::Ollama,
            note: None,
        })
        .collect()
}

pub fn sonnet() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::OpenRouter, "anthropic/claude-sonnet-4.5");
    m.display = "Claude Sonnet 4.5".into();
    m.context_window = Some(200_000);
    m.max_output = Some(64_000);
    m.supports_tools = true;
    m.supports_reasoning = true;
    m.effort_levels = vec![Effort::Low, Effort::Medium, Effort::High];
    m.pricing = Some(Pricing { input_per_mtok: 3.0, output_per_mtok: 15.0 });
    m
}

pub fn catalog_models() -> Vec<ModelInfo> {
    let mut mini = ModelInfo::bare(ProviderKind::OpenRouter, "openai/gpt-5-mini");
    mini.display = "GPT-5 mini".into();
    mini.context_window = Some(400_000);
    mini.supports_tools = true;
    mini.supports_reasoning = true;
    mini.pricing = Some(Pricing { input_per_mtok: 0.25, output_per_mtok: 2.0 });
    let mut coder = ModelInfo::bare(ProviderKind::OpenRouter, "qwen/qwen3-coder");
    coder.display = "Qwen3 Coder".into();
    coder.context_window = Some(262_144);
    coder.supports_tools = true;
    coder.pricing = Some(Pricing { input_per_mtok: 0.22, output_per_mtok: 0.95 });
    vec![sonnet(), mini, coder]
}

pub fn library_with_sonnet() -> Event {
    let r = sonnet().model_ref();
    Event::LibraryState {
        models: vec![LibraryModel { model: r.clone(), info: sonnet(), added_ms: 1_799_999_000_000 }],
        slots: [Slot::Conductor, Slot::Reflection, Slot::Quick]
            .into_iter()
            .map(|slot| SlotAssignment { slot, model: Some(r.clone()), effort: Effort::Medium, inherited: true })
            .collect(),
        providers: providers(true),
    }
}

pub fn empty_library() -> Event {
    Event::LibraryState { models: vec![], slots: vec![], providers: providers(false) }
}

pub fn sonnet_ref() -> ModelRef {
    sonnet().model_ref()
}

pub fn usage() -> Usage {
    Usage { input: 12_400, output: 830, reasoning: 1_204, cached_input: 0, cost_usd: Some(0.0496) }
}

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

pub fn type_str(s: &mut State, text: &str) -> Vec<Request> {
    let mut out = Vec::new();
    for c in text.chars() {
        out.extend(s.on_key(key(KeyCode::Char(c))));
    }
    out
}

pub fn render(s: &State, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    let th = theme();
    t.draw(|f| marlowe_tui::render::draw(f, s, &th)).unwrap();
    t.backend().buffer().clone()
}

/// The buffer as text, one line per row, trailing spaces trimmed.
pub fn dump(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in a.top()..a.bottom() {
        let mut line = String::new();
        for x in a.left()..a.right() {
            line.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Compares against `tests/snapshots/<name>.txt`. `MARLOWE_BLESS=1` writes the file.
pub fn snapshot(name: &str, buf: &Buffer) {
    let text = dump(buf);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots").join(format!("{name}.txt"));
    if std::env::var("MARLOWE_BLESS").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {}; run with MARLOWE_BLESS=1 and review it\n{text}", path.display()))
        .replace("\r\n", "\n");
    assert!(want == text, "snapshot {name} differs.\n--- want\n{want}\n--- got\n{text}");
}

/// No cell has a background, nothing is reverse video, and every foreground is a palette colour.
pub fn assert_palette_only(buf: &Buffer) {
    let th = theme();
    let allowed = th.declared_colours();
    for y in buf.area.top()..buf.area.bottom() {
        for x in buf.area.left()..buf.area.right() {
            let c = buf.cell((x, y)).unwrap();
            assert_eq!(c.bg, Color::Reset, "background fill at ({x},{y}) {:?}", c.symbol());
            assert!(!c.modifier.contains(Modifier::REVERSED), "reverse video at ({x},{y})");
            assert!(allowed.contains(&c.fg), "colour {:?} at ({x},{y}) is not in the palette", c.fg);
        }
    }
}

pub fn contains(buf: &Buffer, needle: &str) -> bool {
    dump(buf).contains(needle)
}
