//! The context meter and the "Context & hardware" planner: what they show for the dusk-super
//! plan (built from the real GGUF headers and runner log), what the keys send, and that nothing
//! is ever a background fill.

mod common;

use common::*;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use marlowe_hw::arch::{ArchProfile, Role};
use marlowe_hw::{build, calibrate, log, CacheType, GpuBudget, ModelHeader, PlanInputs};
use marlowe_model::{Effort, ModelInfo, ModelRef, ProviderKind};
use marlowe_proto::{
    ApprovalId, ArchSummary, ContextPlan, ContextPolicy, Engine, EngineSettings, Event, KvType, LibraryModel, ModelContext,
    PolicySource, Request, RunnerSlot, Slot, SlotAssignment, SERVER_RESTART_TOOL,
};
use marlowe_tui::state::{Overlay, Target};
use marlowe_tui::State;
use ratatui::layout::Rect;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures").join(name)).unwrap()
}

fn header(name: &str) -> ModelHeader {
    ModelHeader::from_show(&serde_json::from_str(&fixture(name)).unwrap()).unwrap()
}

fn dusk_ref() -> ModelRef {
    "ollama/marlowe-dusk:27b-super".parse().unwrap()
}

fn dusk_info() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super");
    m.context_window = Some(32_768);
    m.supports_tools = true;
    m.supports_reasoning = true;
    m.effort_levels = vec![Effort::Off, Effort::Medium];
    m
}

fn running() -> EngineSettings {
    EngineSettings {
        engine: Engine::Ollama,
        loaded: true,
        num_ctx: Some(32_768),
        launch_ctx: Some(32_768),
        parallel: Some(1),
        kv_type: Some(KvType::Q8_0),
        flash_attn: Some(true),
        spec_n: Some(3),
        gpu_layers: Some((66, 66)),
        saved_kv_type: Some(KvType::Q8_0),
        saved_parallel: Some(1),
        saved_flash_attn: Some(true),
        slots: vec![RunnerSlot { id: 0, n_ctx: 32_768, n_prompt_tokens: 1187, is_processing: false }],
        decode_tok_s: Some(81.99),
        load_secs: Some(6.84),
    }
}

/// The plan the daemon computes for dusk-super with the runner loaded (PDH: 13,485.7 MiB).
fn dusk_plan() -> ContextPlan {
    let main = ArchProfile::analyze(&header("qwen35_dusk_27b_super.json"), Role::Main).unwrap();
    let draft = ArchProfile::analyze(&header("qwen35_dusk_draft_mtp.json"), Role::Draft).unwrap();
    let load = log::parse(&fixture("server_dusk_super.log")).pop().unwrap();
    let plan = build(&PlanInputs {
        main: &main,
        draft: Some(&draft),
        spec_n: 3,
        draft_kv: CacheType::F16,
        ubatch: 512,
        mmproj_b: 0,
        gpu: Some(GpuBudget {
            name: "NVIDIA GeForce RTX 4080 SUPER".into(),
            total_mib: 16_376.0,
            used_mib: 14_799.7,
            free_mib: 1_576.3,
            runners_mib: 13_485.7,
            runners_shared_mib: 1_122.0,
            headroom_mib: 1_024.0,
            bandwidth_gbs: Some(736.0),
        }),
        calib: calibrate(&load, Some(13_485.7)),
    });
    let policy = ContextPolicy { max_ctx: 32_768, kv_type: KvType::Q8_0, parallel: 1, source: PolicySource::Auto, updated_ms: 0 };
    ContextPlan {
        model: dusk_ref(),
        ctx_limit: plan.ctx_limit,
        auto: plan.auto_ctx(KvType::Q8_0, 1).map(|c| ContextPolicy { max_ctx: c, ..policy }),
        plan: Some(plan),
        arch: ArchSummary {
            arch: "qwen35".into(),
            params: main.params,
            quant: Some("IQ3_XXS".into()),
            layers: main.roles.clone(),
            summary: main.summary(),
            state_kind: Some("Gated DeltaNet".into()),
            sliding_window: None,
            mla: false,
            draft: Some("1 layer with a KV cache".into()),
            vision: false,
            source: "GGUF header".into(),
        },
        rows: Vec::new(),
        in_effect: running(),
        policy,
        ram_total_mib: Some(31_914.0),
        ram_free_mib: Some(12_300.0),
        warnings: Vec::new(),
    }
}

/// Online, dusk-super is the conductor, the meter knows its engine, a session is open.
fn with_dusk() -> State {
    let mut s = state();
    welcome(&mut s, 1);
    let r = dusk_ref();
    feed(
        &mut s,
        2,
        Event::LibraryState {
            models: vec![LibraryModel { model: r.clone(), info: dusk_info(), added_ms: 1 }, LibraryModel { model: sonnet_ref(), info: sonnet(), added_ms: 2 }],
            slots: vec![SlotAssignment { slot: Slot::Conductor, model: Some(r.clone()), effort: Effort::Medium, inherited: false }],
            providers: providers(true),
        },
    );
    feed(
        &mut s,
        0,
        Event::ContextPolicies {
            models: vec![
                ModelContext { model: r, max_ctx: 32_768, policy: None, in_effect: running() },
                ModelContext { model: sonnet_ref(), max_ctx: 200_000, policy: None, in_effect: EngineSettings { engine: Engine::Cloud, ..Default::default() } },
            ],
        },
    );
    s.session = Some(sid());
    s.title = Some("kv planning".into());
    feed(&mut s, 3, Event::ContextUsage { session: sid(), turn: 1, tokens: 12_400, window: 32_768 });
    s
}

#[test]
fn the_meter_shows_used_of_allowed_with_the_engine() {
    let s = with_dusk();
    // The default 160-column window: the board is wide enough for the bar.
    let buf = render(&s, 160, 45);
    snapshot("context_meter", &buf);
    assert_palette_only(&buf);
    let text = dump(&buf);
    assert!(text.contains("12.4k / 32k"), "{text}");
    assert!(text.contains("kv q8"));
    assert!(text.contains("1 slot"));
    assert!(text.contains("┊"), "the compaction point is marked");
}

#[test]
fn the_meter_drops_the_bar_first_on_a_narrow_board_and_warns_in_amber() {
    let mut s = with_dusk();
    let mut e = running();
    e.gpu_layers = Some((57, 65));
    e.saved_kv_type = Some(KvType::Q8_0);
    let pol = ContextPolicy { max_ctx: 32_768, kv_type: KvType::Q4_0, parallel: 1, source: PolicySource::User, updated_ms: 1 };
    feed(&mut s, 0, Event::ContextPolicies { models: vec![ModelContext { model: dusk_ref(), max_ctx: 32_768, policy: Some(pol), in_effect: e }] });
    let wide = dump(&render(&s, 160, 45));
    assert!(wide.contains("57/65 on GPU") && wide.contains("q4 pending"), "{wide}");
    let narrow = render(&s, 120, 30);
    assert_palette_only(&narrow);
    let t = dump(&narrow);
    let row = t.lines().find(|l| l.contains("12.4k / 32k")).unwrap();
    assert!(row.contains("57/65") && row.contains("q4 next"), "warnings stay, in short form: {row}");
    assert!(!row.contains('━'), "the bar went first: {row}");
}

#[test]
fn a_hosted_conductor_says_cloud() {
    let mut s = with_dusk();
    feed(
        &mut s,
        4,
        Event::LibraryState {
            models: vec![LibraryModel { model: sonnet_ref(), info: sonnet(), added_ms: 1 }],
            slots: vec![SlotAssignment { slot: Slot::Conductor, model: Some(sonnet_ref()), effort: Effort::Medium, inherited: false }],
            providers: providers(true),
        },
    );
    let t = dump(&render(&s, W, H));
    assert!(t.contains("/ 200k · cloud"), "{t}");
}

#[test]
fn the_palette_opens_the_planner_and_asks_for_the_plan() {
    let mut s = with_dusk();
    s.on_key(ctrl('k'));
    type_str(&mut s, "context");
    let out = s.on_key(key(KeyCode::Enter));
    assert_eq!(out, vec![Request::ContextPlan { model: dusk_ref() }]);
    let loading = render(&s, W, H);
    assert!(contains(&loading, "reading the model file"));
    feed(&mut s, 0, Event::ContextPlan(Box::new(dusk_plan())));
    let buf = render(&s, W, H);
    snapshot("context_planner", &buf);
    assert_palette_only(&buf);
    let t = dump(&buf);
    for want in ["Gated DeltaNet", "agents that fit", "max one agent", "without the draft", "free for Marlowe", "×4 with the draft", "Loads fully on the GPU"] {
        assert!(t.contains(want), "{want}\n{t}");
    }
}

#[test]
fn adjusting_reviewing_and_saving_with_a_restart() {
    let mut s = with_dusk();
    open_planner(&mut s);
    feed(&mut s, 0, Event::ContextPlan(Box::new(dusk_plan())));
    s.on_key(key(KeyCode::Right)); // 32k → 36k
    s.on_key(key(KeyCode::Tab)); // → kv
    s.on_key(key(KeyCode::Right)); // q8_0 → q4_0
    let Some(Overlay::ContextPlan { draft: Some(d), .. }) = &s.overlay else { panic!("{:?}", s.overlay) };
    assert_eq!((d.max_ctx, d.kv_type, d.source), (36_864, KvType::Q4_0, PolicySource::User));
    s.on_key(key(KeyCode::Enter));
    let buf = render(&s, W, H);
    snapshot("context_review", &buf);
    assert_palette_only(&buf);
    let t = dump(&buf);
    assert!(t.contains("num_ctx 36,864 (was 32,768)"), "{t}");
    assert!(t.contains("OLLAMA_KV_CACHE_TYPE q8_0 → q4_0"), "{t}");
    let out = s.on_key(key(KeyCode::Char('r')));
    let want = ContextPolicy { max_ctx: 36_864, kv_type: KvType::Q4_0, parallel: 1, source: PolicySource::User, updated_ms: 0 };
    assert_eq!(
        out,
        vec![
            Request::SetContextPolicy { model: dusk_ref(), policy: want },
            Request::ApplyOllamaServer { kv_type: Some(KvType::Q4_0), parallel: None, flash_attn: None },
        ]
    );
    assert!(s.overlay.is_none());
}

#[test]
fn saving_without_a_restart_sends_only_the_policy_and_esc_backs_out() {
    let mut s = with_dusk();
    open_planner(&mut s);
    feed(&mut s, 0, Event::ContextPlan(Box::new(dusk_plan())));
    s.on_key(key(KeyCode::Down)); // the next row: 64k
    s.on_key(key(KeyCode::Enter));
    s.on_key(key(KeyCode::Esc));
    assert!(matches!(s.overlay, Some(Overlay::ContextPlan { review: false, .. })), "esc goes back from the review");
    s.on_key(key(KeyCode::Char('a')));
    let Some(Overlay::ContextPlan { draft: Some(d), .. }) = &s.overlay else { panic!() };
    assert_eq!(d.source, PolicySource::Auto);
    let auto = d.max_ctx;
    s.on_key(key(KeyCode::Enter));
    let out = s.on_key(key(KeyCode::Char('s')));
    assert!(matches!(&out[..], [Request::SetContextPolicy { policy, .. }] if policy.max_ctx == auto), "{out:?}");
    s.on_key(key(KeyCode::Esc));
}

#[test]
fn the_meter_click_the_picker_and_the_library_open_the_planner() {
    let mut s = with_dusk();
    let area = Rect::new(0, 0, W, H);
    let buf = render(&s, W, H);
    let (x, y) = find(&buf, "12.4k / 32k");
    s.on_mouse(MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE }, area);
    assert_eq!(s.hover, Some(Target::ContextMeter));
    let out = s.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE }, area);
    assert_eq!(out, vec![Request::ContextPlan { model: dusk_ref() }]);
    s.on_key(key(KeyCode::Esc));
    assert!(s.overlay.is_none());

    // The model picker: `c` on a row, and each row shows its window.
    s.focus = marlowe_tui::state::Focus::Model;
    s.on_key(key(KeyCode::Enter));
    let picker = dump(&render(&s, W, H));
    assert!(picker.contains("32k") && picker.contains("200k"), "{picker}");
    s.on_key(key(KeyCode::Down));
    let out = s.on_key(key(KeyCode::Char('c')));
    assert_eq!(out, vec![Request::ContextPlan { model: sonnet_ref() }]);
    s.on_key(key(KeyCode::Esc));

    // The Library: the window column and `c`.
    s.on_key(ctrl('l'));
    let lib = dump(&render(&s, W, H));
    assert!(lib.contains("window"), "{lib}");
    s.on_key(key(KeyCode::Tab));
    let out = s.on_key(key(KeyCode::Char('c')));
    assert_eq!(out, vec![Request::ContextPlan { model: dusk_ref() }]);
}

#[test]
fn a_server_restart_is_allow_or_deny_never_for_the_session() {
    let mut s = with_dusk();
    feed(
        &mut s,
        5,
        Event::ApprovalRequested {
            approval: ApprovalId(9),
            session: marlowe_proto::SessionId("context-planner".into()),
            tool: SERVER_RESTART_TOOL.into(),
            summary: "Restart Ollama with OLLAMA_KV_CACHE_TYPE=q4_0".into(),
            detail: "These are Ollama server settings: every Ollama model uses them.".into(),
            run: None,
            proposed_rule: None,
        },
    );
    let buf = render(&s, W, H);
    snapshot("context_server_approval", &buf);
    assert_palette_only(&buf);
    let t = dump(&buf);
    assert!(!t.contains("allow for session"), "{t}");
    assert!(t.contains("every Ollama model is affected"));
    s.now_ms += 1_000;
    assert!(s.on_key(key(KeyCode::Char('a'))).is_empty(), "no session-wide allow");
    s.on_key(key(KeyCode::Left));
    assert_eq!(s.approval_sel, 0);
    s.on_key(key(KeyCode::Right));
    assert_eq!(s.approval_sel, 2, "skips the session choice");
}

#[test]
fn saved_and_applied_events_flash() {
    let mut s = with_dusk();
    let pol = ContextPolicy { max_ctx: 47_104, kv_type: KvType::Q4_0, parallel: 1, source: PolicySource::User, updated_ms: 1 };
    feed(&mut s, 6, Event::ContextPolicySaved { model: dusk_ref(), policy: pol, changes: vec![], pending_restart: true });
    assert!(s.flash.as_ref().unwrap().text.contains("waits for an Ollama restart"));
    assert_eq!(s.context.model(&dusk_ref()).unwrap().max_ctx, 47_104);
    feed(&mut s, 7, Event::ServerApplied { ok: false, message: "not restarted; the change waits".into() });
    assert!(s.flash.as_ref().unwrap().text.starts_with("not restarted"));
    assert_eq!(s.meter().unwrap().window, 47_104);
}

/// ^k, "context", ↵: the palette's way in.
fn open_planner(s: &mut State) {
    s.on_key(ctrl('k'));
    type_str(s, "context");
    s.on_key(key(KeyCode::Enter));
}

/// The first cell of `needle` in the buffer.
fn find(buf: &ratatui::buffer::Buffer, needle: &str) -> (u16, u16) {
    let text = dump(buf);
    for (y, line) in text.lines().enumerate() {
        if let Some(i) = line.find(needle) {
            let x = line[..i].chars().count();
            return (x as u16, y as u16);
        }
    }
    panic!("{needle} not on screen");
}
