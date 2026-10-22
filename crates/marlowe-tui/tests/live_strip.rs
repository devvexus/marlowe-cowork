//! **The live strip.** Matthew: "there should be no time, even for a millisecond, the user is
//! thinking 'what am I waiting on'". These tests drive the phase events a turn really produces
//! through the reducer and read the screen: every stage visible for at least a frame, nothing that
//! moves when a phase changes, estimates marked `~` until the real numbers replace them, and the
//! palette untouched.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{
    Envelope, Event, PhaseEvent, PhaseKind, PhaseSpan, ReadStat, Request, StageMs, StoredTurn, TurnNumbers, TurnStop,
};
use marlowe_tui::State;

const BASE: i64 = 1_800_000_000_000;
const SPINNERS: &str = "⣾⣽⣻⢿⡿⣟⣯⣷";

/// A session with the harness driving it: a clock and a sequence number.
struct Rig {
    s: State,
    seq: u64,
}

impl Rig {
    /// A session where "hi" was answered and the user has just pressed Enter on the next message.
    fn new() -> Rig {
        let mut s = state();
        welcome(&mut s, 0);
        feed(&mut s, 1, library_with_sonnet());
        type_str(&mut s, "hi");
        s.on_key(key(KeyCode::Enter));
        feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
        feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
        let mut r = Rig { s, seq: 3 };
        r.at(0, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "hi".into() });
        r.at(10, Event::TextDelta { session: sid(), turn: 1, text: "Hello.".into() });
        r.at(20, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
        r.s.now_ms = BASE + 1_000;
        r
    }

    /// Enter on the next message, the way the user does it.
    fn enter(&mut self, ms: i64, text: &str) -> Vec<Request> {
        self.s.now_ms = BASE + ms;
        type_str(&mut self.s, text);
        self.s.on_key(key(KeyCode::Enter))
    }

    fn at(&mut self, ms: i64, ev: Event) {
        self.s.now_ms = BASE + ms;
        let seq = match &ev {
            Event::Phase(p) if p.state == marlowe_proto::PhaseState::Progress => 0,
            _ => {
                self.seq += 1;
                self.seq
            }
        };
        self.s.on_client(marlowe_tui::ClientEvent::Envelope(Envelope { seq, at_ms: BASE + ms, event: ev }));
    }

    fn phase(&mut self, ms: i64, turn: u32, p: PhaseEvent) {
        self.at(ms, Event::Phase(p.on(&sid(), turn)));
    }

    fn started(&mut self, ms: i64, turn: u32, user: &str) {
        self.at(ms, Event::TurnStarted { session: sid(), turn, model: sonnet_ref(), user: user.into() });
    }

    /// One frame at `ms`, palette-checked, as text.
    fn frame(&mut self, ms: i64) -> String {
        self.s.now_ms = BASE + ms;
        let buf = render(&self.s, W, H);
        assert_palette_only(&buf);
        dump(&buf)
    }

    /// The pane's two strip rows in a frame at `ms`, after every queued phase has had its frame.
    fn strip(&mut self, ms: i64) -> (String, String) {
        for _ in 0..24 {
            self.frame(ms);
        }
        self.strip_now(ms)
    }

    /// The strip's row in the next frame, which sits right under the pane's last line, and the row
    /// under it, which must be empty: the strip is the bottom of the conversation.
    fn strip_now(&mut self, ms: i64) -> (String, String) {
        let text = self.frame(ms);
        let rows: Vec<&str> = text.lines().collect();
        let g = marlowe_tui::render::geometry(ratatui::layout::Rect::new(0, 0, W, H), &self.s);
        let v = self.s.viewport.get();
        let y = g.conversation.y as usize + v.total.min(v.size.1 as usize);
        let pane_bottom = marlowe_tui::render::transcript_pane(g.conversation, &self.s).bottom() as usize;
        let (row, below) = (pane_text(rows[y]), if y + 1 < pane_bottom { rows[y + 1].chars().skip(1).take(90).collect::<String>().trim().to_string() } else { String::new() });
        assert!(below.is_empty(), "something sits below the current activity {row:?}: {below:?}\n{text}");
        (row, below)
    }

    /// Opens the `n`th reasoning block on screen by clicking its fold marker.
    fn open_fold(&mut self, ms: i64, n: usize) {
        let text = self.frame(ms);
        let (y, line) = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("▸ thought") || l.contains("▸ reasoning"))
            .nth(n)
            .unwrap_or_else(|| panic!("no fold header #{n}:\n{text}"));
        let x = line.chars().position(|c| c == '▸').unwrap();
        let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x as u16, row: y as u16, modifiers: KeyModifiers::NONE };
        self.s.on_mouse(click, ratatui::layout::Rect::new(0, 0, W, H));
    }
}

/// How often `needle` is on screen: a state shown twice is a bug.
fn times(text: &str, needle: &str) -> usize {
    // The pane's title bar carries the app-wide activity pulse; that is chrome, not the strip.
    text.lines().filter(|l| !l.contains("╭ Conversation")).map(|l| l.matches(needle).count()).sum()
}

/// A line that joins the read to what follows it: `read 8.2k · 1.9s  ›  thinking`.
fn combined(text: &str) -> bool {
    text.lines().any(|l| l.contains("read ") && l.contains(" › "))
}

fn has_spinner(row: &str) -> bool {
    let pane = pane_text(row);
    pane.chars().next().is_some_and(|c| SPINNERS.contains(c) || c == '✓' || c == '✗') && pane.chars().nth(1) == Some(' ')
}

/// What the conversation pane shows on a screen row: between its border and the board's.
fn pane_text(row: &str) -> String {
    let inner: String = row.chars().skip(1).take(100).collect();
    inner.trim().to_string()
}

fn ph(kind: PhaseKind) -> PhaseEvent {
    PhaseEvent::start(kind)
}

// ---- Enter ---------------------------------------------------------------------------------------

#[test]
fn enter_shows_the_message_and_the_strip_in_the_same_frame_and_stamps_the_ask() {
    let mut r = Rig::new();
    let reqs = r.enter(2_000, "and the eval?");
    assert!(matches!(&reqs[..], [Request::Ask { client_ms: Some(t), .. }] if *t == BASE + 2_000), "{reqs:?}");
    // No daemon event yet.
    let text = r.frame(2_000);
    assert!(text.contains("and the eval?"), "the message is on screen at once:\n{text}");
    let (first, _) = r.strip(2_000);
    assert!(first.contains("sending"), "{first}");
    assert!(r.s.strip_up());
}

#[test]
fn a_failed_ask_takes_the_strip_down_again() {
    let mut r = Rig::new();
    r.enter(2_000, "again");
    assert!(r.s.strip_up());
    r.at(2_050, Event::Error { session: Some(sid()), message: "no model".into() });
    assert!(!r.s.strip_up());
    assert!(r.s.pending_user.is_none());
}

#[test]
fn a_wait_with_no_stage_of_its_own_still_ticks() {
    // A remote model reports nothing between the request leaving and its first frame: the
    // strip names the wait and counts it, so no moment is an unlabelled stall.
    let mut r = Rig::new();
    r.enter(2_000, "and the eval?");
    r.started(2_010, 2, "and the eval?");
    r.phase(2_020, 2, ph(PhaseKind::Wire));
    let (a, _) = r.strip(2_100);
    let (b, _) = r.strip(4_600);
    assert!(a.contains("calling the model"), "{a}");
    assert!(a.contains("100ms") && b.contains("2.6s"), "the wait counts up: {a} / {b}");
    assert_eq!(a.chars().count().abs_diff(b.chars().count()), 0, "and does not shift: {a} / {b}");
}

// ---- recall ---------------------------------------------------------------------------------------

fn stage(r: &mut Rig, ms: i64, name: &str, n: u64, took: u64) {
    r.phase(ms, 2, PhaseEvent::start(PhaseKind::Recall).stage(name));
    r.phase(ms, 2, PhaseEvent::end(PhaseKind::Recall, took).stage(name).with_total(n).unit("items"));
}

#[test]
fn every_recall_stage_is_on_screen_for_at_least_a_frame_even_when_they_all_arrive_at_once() {
    let mut r = Rig::new();
    r.enter(2_000, "what did we decide?");
    r.started(2_005, 2, "what did we decide?");
    r.phase(2_006, 2, ph(PhaseKind::Recall));
    // The whole pipeline inside one millisecond, as a warm recall does it: no frame in between.
    for (name, n, took) in [("pool", 4198, 1), ("embed", 1, 12), ("bm25", 4198, 2), ("dense", 4198, 4), ("gate", 30, 1), ("rerank", 10, 61), ("assemble", 3, 0)] {
        stage(&mut r, 2_007, name, n, took);
    }
    r.at(2_008, Event::Recalled { session: sid(), turn: 2, count: 3, summary: "decision · 0.42".into(), total_ms: 84, stages: Vec::new(), abstained: false });
    let mut seen = String::new();
    for k in 0..14 {
        let (first, _) = r.strip_now(2_010 + k);
        seen.push_str(&first);
        seen.push('\n');
    }
    for name in ["pool", "embed", "bm25", "dense", "gate", "rerank", "assemble"] {
        assert!(seen.contains(&format!("recall · {name}")), "{name} was never drawn:\n{seen}");
    }
    // Drained: the row is free for what comes next.
    let (first, _) = r.strip_now(2_100);
    assert!(!first.contains("recall ·"), "{first}");
}

#[test]
fn a_recall_stage_that_loops_shows_its_progress_and_rate() {
    let mut r = Rig::new();
    r.enter(2_000, "q");
    r.started(2_005, 2, "q");
    r.phase(2_006, 2, PhaseEvent::start(PhaseKind::Recall).stage("dense"));
    let mut p = PhaseEvent::progress(PhaseKind::Recall, 2_940, 4_198).stage("dense").unit("items");
    p.rate = Some(1_900_000.0);
    r.phase(2_010, 2, p);
    let (first, _) = r.strip(2_011);
    assert!(first.contains("recall · dense 2,940/4,198 · 1.9M/s"), "{first}");
    assert_eq!(times(&r.frame(2_012), "recall ·"), 1, "one recall line, and not the old second row besides");
    r.phase(2_020, 2, PhaseEvent::start(PhaseKind::Recall).stage("rerank").detail("cpu"));
    let mut p = PhaseEvent::progress(PhaseKind::Recall, 6, 10).stage("rerank").unit("pairs");
    p.rate = Some(210.0);
    r.phase(2_030, 2, p);
    let mut seen = String::new();
    for k in 0..4 {
        seen.push_str(&r.strip(2_040 + k).0);
    }
    assert!(seen.contains("recall · rerank 6/10 · 210 pairs/s · cpu"), "{seen}");
}

fn recalled(stages: &[(&str, u64)], count: u32, abstained: bool) -> Event {
    Event::Recalled {
        session: sid(),
        turn: 2,
        count,
        summary: "decision · gate threshold 0.42; fact · tokenizer pinned".into(),
        total_ms: 84,
        stages: stages.iter().map(|(n, ms)| StageMs { name: n.to_string(), ms: *ms, n: None }).collect(),
        abstained,
    }
}

#[test]
fn the_recall_line_keeps_the_stage_timings_and_abbreviates_by_width() {
    let mut r = Rig::new();
    r.enter(2_000, "q");
    r.started(2_005, 2, "q");
    r.at(2_010, recalled(&[("embed", 12), ("bm25", 2), ("dense", 4), ("gate", 1), ("rerank", 61)], 3, false));
    let wide = r.frame(2_020);
    assert!(wide.contains("recall · 3 memories · 84ms  embed 12 · bm25 2 · dense 4 · gate 1 · rerank 61"), "{wide}");
    // Narrower: the stages shorten to a letter each rather than wrap or vanish.
    let buf = render(&r.s, 120, 30);
    assert_palette_only(&buf);
    let text = dump(&buf);
    let row = text.lines().find(|l| l.contains("recall · 3 memories")).unwrap_or_else(|| panic!("recall line:\n{text}"));
    assert!(row.contains("84ms"), "{row}");
    assert!(!text.lines().any(|l| l.contains("recall") && l.trim_end().ends_with("bm25")), "no wrapped fragments:\n{text}");
}

#[test]
fn a_recall_that_found_nothing_says_so_and_where_the_time_went() {
    let mut r = Rig::new();
    r.enter(2_000, "q");
    r.started(2_005, 2, "q");
    r.at(2_010, recalled(&[("embed", 9), ("gate", 1)], 0, true));
    let text = r.frame(2_020);
    assert!(text.contains("recall · 0 memories · abstained · 84ms  embed 9 · gate 1"), "{text}");
}

#[test]
fn recall_stage_numbers_are_amber_and_labels_dim() {
    let mut r = Rig::new();
    r.enter(2_000, "q");
    r.started(2_005, 2, "q");
    r.at(2_010, recalled(&[("embed", 12)], 3, false));
    let buf = render(&r.s, W, H);
    let th = theme();
    let find = |needle: &str| -> ratatui::buffer::Cell {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect();
            if let Some(at) = row.find(needle) {
                let col = row[..at].chars().count() as u16;
                return buf.cell((col, y)).unwrap().clone();
            }
        }
        panic!("{needle} not on screen:\n{}", dump(&buf));
    };
    assert_eq!(find("84ms").fg, marlowe_surface::chrome::Ink::Amber.color(&th));
    assert_eq!(find("embed").fg, marlowe_surface::chrome::Ink::Dim.color(&th));
}

// ---- the read, then the thinking, on one line -----------------------------------------------------

/// A turn up to the model reading its prompt: the daemon's phases, with a stored prefill rate.
fn reading(r: &mut Rig) {
    r.enter(2_000, "explain the gate");
    r.phase(2_004, 2, ph(PhaseKind::Sending));
    r.phase(2_004, 2, PhaseEvent::end(PhaseKind::Sending, 4));
    r.started(2_006, 2, "explain the gate");
    r.phase(2_010, 2, ph(PhaseKind::Prompt));
    r.phase(2_012, 2, PhaseEvent::end(PhaseKind::Prompt, 2));
    let mut start = ph(PhaseKind::Prefill).with_total(8_200).unit("tok");
    start.est = true;
    r.phase(2_020, 2, start);
}

#[test]
fn the_read_shows_only_what_the_engine_said_and_the_prompt_is_a_tilde_until_it_says_its_size() {
    let mut r = Rig::new();
    reading(&mut r);
    // Nothing from Ollama's output (a server started by hand): tokens and elapsed only.
    let (first, _) = r.strip(2_220);
    assert!(first.contains("reading ~8.2k tok"), "{first}");
    assert!(!first.contains("tok/s") && !first.contains('━') && !first.contains('%') && !first.contains("cached"), "nothing invented: {first}");
    assert!(first.contains("200ms"), "the elapsed ticks: {first}");

    // Ollama's runner log: the whole prompt, where the read started (the reused part), how far it is
    // and its own rate. No tildes: all of it was measured.
    let mut p = PhaseEvent::progress(PhaseKind::Prefill, 9_200, 12_600).unit("tok").with_cached(6_600);
    p.rate = Some(95.55);
    r.phase(2_300, 2, p);
    let (first, _) = r.strip(2_301);
    assert!(first.contains("reading 12.6k tok · 6.6k cached · 2,600/6,000 new · 96 tok/s"), "{first}");
    assert!(first.contains('━') && first.contains("43%"), "the bar and its percentage of the fresh tokens: {first}");
    assert!(!first.contains('~'), "{first}");

    // A cache miss is visible: nothing was reused.
    let mut p = PhaseEvent::progress(PhaseKind::Prefill, 2_560, 7_200).unit("tok").with_cached(0);
    p.rate = Some(95.55);
    r.phase(2_400, 2, p);
    let (first, _) = r.strip(2_401);
    assert!(first.contains("reading 7.2k tok · 0 cached · 2,560/7,200 new"), "{first}");

    // Before it has said where the read starts: the size alone.
    let mut r = Rig::new();
    reading(&mut r);
    r.phase(2_300, 2, PhaseEvent::progress(PhaseKind::Prefill, 0, 12_600).unit("tok"));
    let (first, _) = r.strip(2_301);
    assert!(first.contains("reading 12.6k tok") && !first.contains("cached") && !first.contains('~'), "{first}");
}

#[test]
fn the_first_token_ends_the_read_and_the_engines_end_replaces_the_time() {
    let mut r = Rig::new();
    reading(&mut r);
    let mut p = PhaseEvent::progress(PhaseKind::Prefill, 9_200, 12_600).unit("tok").with_cached(6_600);
    p.rate = Some(95.55);
    r.phase(2_300, 2, p);
    // The first token ends the read: thinking replaces the read line entirely.
    r.at(3_900, Event::ReasoningDelta { session: sid(), turn: 2, text: "Let me look.".into(), tokens: 4 });
    let (first, _) = r.strip(3_901);
    assert!(first.contains("thinking"), "{first}");
    assert!(!first.contains("read") && !first.contains('›'), "no combined read › thinking line: {first}");
    assert!(!combined(&r.frame(3_901)));
    let text = r.frame(3_902);
    assert!(!text.contains("read 6k") && !combined(&text), "the read is not on screen until the block is opened:\n{text}");

    // Opened, the block carries the read: the split is the engine's, only the time is still ours.
    r.open_fold(3_903, 0);
    let text = r.frame(3_904);
    assert!(text.contains("read 6k (6.6k cached) · ~1.9s"), "{text}");

    // The engine's own numbers arrive with the call's usage.
    let mut end = PhaseEvent::end(PhaseKind::Prefill, 61_873).with_done(6_000).with_total(12_600).with_cached(6_600).unit("tok");
    end.rate = Some(97.0);
    r.phase(9_000, 2, end);
    let text = r.frame(9_002);
    assert!(text.contains("read 6k (6.6k cached) · 1m") && !text.contains('~'), "{text}");
}

#[test]
fn a_read_with_no_split_is_the_prompts_size_and_the_time_only() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "a".into(), tokens: 2 });
    let end = PhaseEvent::end(PhaseKind::Prefill, 480).with_done(16_000).with_total(16_000).unit("tok");
    r.phase(2_600, 2, end);
    r.open_fold(2_700, 0);
    let text = r.frame(2_701);
    assert!(text.contains("read 16k · 480ms") && !text.contains("cached") && !text.contains('~'), "{text}");
}

#[test]
fn the_read_stats_are_coloured_under_the_opened_block_and_labels_are_dim() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "Weighing it.".into(), tokens: 3 });
    let mut end = PhaseEvent::end(PhaseKind::Prefill, 1_900).with_done(8_200).with_total(8_200).unit("tok");
    end.est = false;
    r.phase(2_600, 2, end);
    r.open_fold(2_700, 0);
    let th = theme();
    let buf = render(&r.s, W, H);
    let mut found = false;
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect();
        if let Some(at) = row.find("read 8.2k") {
            let col = row[..at].chars().count() as u16;
            use marlowe_surface::chrome::Ink;
            assert_eq!(buf.cell((col, y)).unwrap().fg, Ink::Dim.color(&th), "the label is dim");
            assert_eq!(buf.cell((col + 5, y)).unwrap().fg, Ink::Amber.color(&th), "the number is amber");
            found = true;
        }
    }
    assert!(found, "the read row is under the opened block:\n{}", dump(&buf));
}

#[test]
fn nothing_is_shown_twice_while_the_model_thinks() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "hmm ".into(), tokens: 30 });
    let mut end = PhaseEvent::end(PhaseKind::Prefill, 1_900).with_done(8_200).with_total(8_200).unit("tok");
    end.est = false;
    r.phase(2_600, 2, end);
    for ms in [2_700, 3_500, 9_000] {
        r.strip(ms);
        let text = r.frame(ms + 1);
        assert_eq!(times(&text, "thinking"), 1, "the strip says thinking, the block is only its handle:\n{text}");
        assert_eq!(times(&text, "▸ reasoning"), 1, "{text}");
        assert!(!text.contains("tok") || times(&text, " tok") == 1, "the token count is in one place:\n{text}");
        assert!(!text.contains("read 8.2k") && !combined(&text), "the read is not shown beside it:\n{text}");
    }
}
#[test]
fn a_later_step_reads_only_what_is_new_and_says_what_was_cached() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(3_000, Event::ReasoningDelta { session: sid(), turn: 2, text: "a".into(), tokens: 2 });
    let mut end = PhaseEvent::end(PhaseKind::Prefill, 900).with_done(8_200).with_total(8_200).unit("tok");
    end.est = false;
    r.phase(3_100, 2, end);
    r.at(3_200, Event::ToolStarted { session: sid(), turn: 2, call_id: "c1".into(), tool: "bash".into(), summary: "bash cargo test".into() });
    r.at(5_000, Event::ToolFinished { session: sid(), turn: 2, call_id: "c1".into(), ok: true, summary: "ok".into() });
    let mut start = ph(PhaseKind::Prefill).with_total(9_400).unit("tok");
    start.est = true;
    r.phase(5_010, 2, start);
    r.at(5_300, Event::ReasoningDelta { session: sid(), turn: 2, text: "b".into(), tokens: 2 });
    let end = PhaseEvent::end(PhaseKind::Prefill, 290).with_done(1_200).with_total(9_400).with_cached(8_200).unit("tok");
    r.phase(5_400, 2, end);
    r.open_fold(5_401, 1);
    let text = r.frame(5_402);
    assert!(text.contains("read +1.2k (8.2k cached) · 290ms"), "{text}");
    assert!(!combined(&text), "{text}");
}
#[test]
fn thinking_and_writing_have_their_own_inks_and_a_tool_is_green() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "hmm ".into(), tokens: 30 });
    r.at(3_000, Event::ReasoningDelta { session: sid(), turn: 2, text: "so ".into(), tokens: 38 });
    let th = theme();
    let ink_of = |r: &mut Rig, ms: i64, needle: &str| {
        r.s.now_ms = BASE + ms;
        let buf = render(&r.s, W, H);
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect();
            if has_spinner(&row) {
                if let Some(at) = row.find(needle) {
                    let col = row[..at].chars().count() as u16;
                    return buf.cell((col, y)).unwrap().fg;
                }
            }
        }
        panic!("{needle} not on a strip row:\n{}", dump(&buf));
    };
    use marlowe_surface::chrome::Ink;
    assert_eq!(ink_of(&mut r, 3_001, "thinking"), Ink::Speech.color(&th), "thinking is Marlowe's voice");
    assert!(r.strip(3_002).0.contains("thinking   1.0s · 68 tok · 68 tok/s") || r.strip(3_002).0.contains("68 tok"), "{}", r.strip(3_002).0);
    r.at(3_100, Event::TextDelta { session: sid(), turn: 2, text: "The gate ".into() });
    r.at(3_200, Event::TextDelta { session: sid(), turn: 2, text: "moved.".into() });
    assert_eq!(ink_of(&mut r, 3_201, "writing"), Ink::Accent.color(&th));
    r.at(3_300, Event::ToolStarted { session: sid(), turn: 2, call_id: "c1".into(), tool: "bash".into(), summary: "bash cargo test".into() });
    assert_eq!(ink_of(&mut r, 15_300, "running"), Ink::Green.color(&th));
    let (first, _) = r.strip(15_300);
    assert!(first.contains("running · 12.0s"), "the tool's elapsed ticks: {first}");
    let text = r.frame(15_301);
    assert_eq!(times(&text, "bash cargo test"), 1, "the call is named once, in the transcript:\n{text}");
}

#[test]
fn starting_ollama_is_a_live_line_with_a_clock_that_ends_when_it_answers() {
    let mut r = Rig::new();
    reading(&mut r);
    r.phase(3_000, 2, PhaseEvent::start(PhaseKind::Admit).stage("ollama").detail("starting Ollama"));
    let (first, _) = r.strip(5_200);
    assert!(first.contains("starting Ollama ·  2.2s"), "{first}");
    r.phase(6_000, 2, PhaseEvent::end(PhaseKind::Admit, 3_000).stage("ollama"));
    assert!(!r.strip(6_001).0.contains("starting Ollama"), "the line goes when Ollama answers");
}

#[test]
fn waiting_for_the_user_or_the_gpu_is_amber_and_names_why() {
    let mut r = Rig::new();
    reading(&mut r);
    let mut gpu = PhaseEvent::start(PhaseKind::Admit).stage("gpu").detail("waiting for the GPU · crew run b8ee");
    gpu.est = false;
    r.phase(3_000, 2, gpu);
    let (first, _) = r.strip(3_500);
    assert!(first.contains("waiting for the GPU · crew run b8ee · 500ms"), "{first}");
    let th = theme();
    let buf = render(&r.s, W, H);
    let y = (0..buf.area.height).find(|y| has_spinner(&(0..buf.area.width).map(|x| buf.cell((x, *y)).unwrap().symbol().to_string()).collect::<String>())).unwrap();
    assert_eq!(buf.cell((3, y)).unwrap().fg, marlowe_surface::chrome::Ink::Amber.color(&th));
    r.phase(4_000, 2, PhaseEvent::end(PhaseKind::Admit, 1_000).stage("gpu"));
    assert!(!r.strip(4_001).0.contains("waiting for the GPU"));

    r.at(4_100, Event::ApprovalRequested {
        approval: marlowe_proto::ApprovalId(7),
        session: sid(),
        tool: "bash".into(),
        summary: "bash rm -rf build".into(),
        detail: "{}".into(),
        run: None,
        proposed_rule: None,
    });
    assert!(r.strip(4_200).0.contains("waiting for your approval"));
}

// ---- nothing moves --------------------------------------------------------------------------------

#[test]
fn no_layout_shift_across_the_phases_from_enter_to_the_first_token() {
    let mut r = Rig::new();
    let mut frames: Vec<(i64, String)> = Vec::new();
    let note = |r: &mut Rig, ms: i64, frames: &mut Vec<(i64, String)>| frames.push((ms, r.frame(ms)));
    r.enter(2_000, "explain the gate");
    note(&mut r, 2_000, &mut frames);
    r.phase(2_004, 2, ph(PhaseKind::Sending));
    r.phase(2_004, 2, PhaseEvent::end(PhaseKind::Sending, 4));
    note(&mut r, 2_005, &mut frames);
    r.started(2_006, 2, "explain the gate");
    note(&mut r, 2_007, &mut frames);
    r.phase(2_010, 2, ph(PhaseKind::Recall));
    note(&mut r, 2_011, &mut frames);
    stage(&mut r, 2_012, "pool", 4198, 1);
    note(&mut r, 2_013, &mut frames);
    r.at(2_020, recalled(&[("embed", 12), ("gate", 1)], 3, false));
    note(&mut r, 2_021, &mut frames);
    r.phase(2_030, 2, ph(PhaseKind::Fit));
    note(&mut r, 2_031, &mut frames);
    let mut start = ph(PhaseKind::Prefill).with_total(8_200).unit("tok");
    start.est = true;
    r.phase(2_040, 2, start);
    note(&mut r, 2_041, &mut frames);
    let mut p = PhaseEvent::progress(PhaseKind::Prefill, 4_000, 8_200).unit("tok");
    p.rate = Some(1_900.0);
    p.est = true;
    r.phase(2_500, 2, p);
    note(&mut r, 2_501, &mut frames);
    r.at(4_000, Event::ReasoningDelta { session: sid(), turn: 2, text: "hm".into(), tokens: 1 });
    note(&mut r, 4_001, &mut frames);

    // The strip is one row the whole way, at the same column, and the message above it never
    // moves a cell. (The recall line joins the transcript when it arrives: only that frame
    // changes the rows above the strip, and nothing after it does.)
    let mut strip_x = None;
    let mut heights = std::collections::BTreeSet::new();
    for (ms, text) in &frames {
        let rows: Vec<&str> = text.lines().collect();
        let at = rows.iter().position(|r| has_spinner(r)).unwrap_or_else(|| panic!("no strip at {ms}:\n{text}"));
        let x = rows[at].chars().position(|c| SPINNERS.contains(c)).unwrap();
        assert_eq!(*strip_x.get_or_insert(x), x, "the spinner moved at {ms}");
        heights.insert(*ms);
        assert!(rows.iter().any(|r| r.contains("explain the gate")) || *ms == 2_000, "{ms}:\n{text}");
    }
    let rows_above = |text: &str| -> Vec<String> {
        let rows: Vec<&str> = text.lines().collect();
        let at = rows.iter().position(|r| has_spinner(r)).unwrap();
        rows[..at].iter().map(|r| pane_text(r)).collect()
    };
    let after_recall: Vec<&(i64, String)> = frames.iter().filter(|(ms, _)| (2_021..4_000).contains(ms)).collect();
    for w in after_recall.windows(2) {
        // The title bar's meter animates; only the pane's rows are compared.
        let (a, b) = (rows_above(&w[0].1), rows_above(&w[1].1));
        assert_eq!(a[2..], b[2..], "rows above the strip changed between {} and {}", w[0].0, w[1].0);
    }
    assert_eq!(r.s.viewport.get().strip, 1, "the strip is one row");
}

#[test]
fn ticking_numbers_are_fixed_width_so_nothing_beside_them_moves() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "hmm ".into(), tokens: 30 });
    let mut widths = std::collections::BTreeSet::new();
    for ms in [2_600, 3_900, 11_700, 12_300, 62_000, 119_000] {
        let (first, _) = r.strip(ms);
        // Up to the tokens: `thinking <clock> · N tok`.
        let upto = first.find(" · 30 tok").unwrap_or_else(|| panic!("{first}"));
        widths.insert(first[..upto].chars().count());
    }
    assert_eq!(widths.len(), 1, "the clock changed width: {widths:?}");
}

#[test]
fn a_reader_scrolled_up_keeps_their_rows_when_the_strip_goes_away_under_them() {
    let mut r = Rig::new();
    r.started(100, 9, "long");
    r.at(110, Event::TextDelta { session: sid(), turn: 9, text: (1..=80).map(|i| format!("line {i}

")).collect() });
    r.frame(120);
    r.s.scroll = 15;
    let held = r.frame(140);
    let top = r.s.viewport.get().top;
    assert_eq!(r.s.viewport.get().strip, 1);
    // The reply ends while they read: the strip's rows go, and what they were reading stays.
    r.at(200, Event::TurnFinished { session: sid(), turn: 9, usage: usage(), stop: TurnStop::Done });
    r.frame(210);
    let after = r.frame(5_000);
    assert_eq!(r.s.viewport.get().strip, 0);
    assert_eq!(r.s.viewport.get().top, top, "the line at the top of the pane did not change");
    let first_rows = |t: &str| t.lines().skip(3).take(5).map(pane_text).collect::<Vec<_>>();
    assert_eq!(first_rows(&held), first_rows(&after));
}

#[test]
fn the_text_stays_put_when_the_strip_goes_away_at_the_end_of_a_turn() {
    let mut r = Rig::new();
    r.started(100, 9, "long");
    r.at(110, Event::TextDelta { session: sid(), turn: 9, text: (1..=80).map(|i| format!("line {i}\n\n")).collect() });
    r.frame(115);
    let before = r.s.viewport.get();
    assert_eq!(before.strip, 1);
    r.at(120, Event::TurnFinished { session: sid(), turn: 9, usage: usage(), stop: TurnStop::Done });
    r.frame(125);
    r.frame(2_000);
    let after = r.s.viewport.get();
    assert_eq!(after.strip, 0, "the strip went away");
    assert_eq!(after.top, before.top.max(after.top).min(before.top + 3), "the pane did not slide: {before:?} {after:?}");
}

// ---- the work after and around a turn --------------------------------------------------------------

#[test]
fn the_memory_write_is_a_dim_line_after_the_reply_and_never_the_bottom_of_the_conversation() {
    let mut r = Rig::new();
    r.enter(2_000, "remember this");
    r.started(2_005, 2, "remember this");
    r.at(2_100, Event::TextDelta { session: sid(), turn: 2, text: "Noted.".into() });
    // The ingest lands while the reply is still being written: nothing on the strip, nothing yet.
    r.phase(2_150, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 0).stage("ingest").with_done(1).unit("items"));
    r.phase(2_160, 2, PhaseEvent::start(PhaseKind::MemoryWrite).stage("embed").with_total(1).unit("items"));
    let (first, _) = r.strip(2_170);
    assert!(!first.contains("memor") && !first.contains("embed"), "{first}");
    r.phase(2_180, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 31).stage("embed").with_done(1).unit("items"));
    r.phase(2_190, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 0).stage("index"));
    assert!(!r.frame(2_195).contains("+1 memory"), "held until the reply is down");
    r.at(2_200, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    let text = r.frame(2_250);
    assert_eq!(times(&text, "+1 memory · embedded 31ms · indexed"), 1, "{text}");
    let rows: Vec<&str> = text.lines().collect();
    let reply = rows.iter().position(|l| l.contains("Noted.")).unwrap();
    let note = rows.iter().position(|l| l.contains("+1 memory")).unwrap();
    assert!(note > reply, "after the reply it belongs to:\n{text}");
    assert!(rows.iter().all(|l| !has_spinner(l)), "no strip once the turn is over:\n{text}");
    assert!(!r.s.strip_up());
}

#[test]
fn a_memory_write_after_the_turn_is_the_same_dim_line() {
    let mut r = Rig::new();
    r.enter(2_000, "remember this");
    r.started(2_005, 2, "remember this");
    r.at(2_100, Event::TextDelta { session: sid(), turn: 2, text: "Noted.".into() });
    r.at(2_200, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    r.phase(2_210, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 0).stage("ingest").with_done(1).unit("items"));
    r.phase(2_220, 2, PhaseEvent::start(PhaseKind::MemoryWrite).stage("embed").with_total(2).unit("items"));
    let text = r.frame(2_250);
    assert!(!text.contains("memory") || !text.lines().any(|l| has_spinner(l)), "{text}");
    r.phase(2_300, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 31).stage("embed").with_done(2).unit("items"));
    r.phase(2_310, 2, PhaseEvent::end(PhaseKind::MemoryWrite, 0).stage("index"));
    let text = r.frame(2_320);
    assert_eq!(times(&text, "+1 memory · embedded 31ms · indexed"), 1, "{text}");
    assert!(!r.s.strip_up(), "the strip does not linger for it");
}
#[test]
fn compaction_is_one_line_with_one_clock_then_a_note() {
    let mut r = Rig::new();
    r.enter(2_000, "go on");
    r.started(2_005, 2, "go on");
    r.phase(2_010, 2, ph(PhaseKind::Fit));
    r.phase(2_020, 2, PhaseEvent::end(PhaseKind::Fit, 10).with_done(28_100).unit("tok"));
    r.phase(2_030, 2, ph(PhaseKind::Compact));
    let mut p = PhaseEvent::progress(PhaseKind::Compact, 312, 4_096).unit("tok");
    p.est = true;
    r.phase(2_100, 2, p);
    for ms in [2_200, 12_000, 170_000] {
        let (first, _) = r.strip(ms);
        assert!(first.contains("compacting 28.1k → … · ~312/4,096 tok"), "{first}");
        let text = r.frame(ms + 1);
        assert_eq!(times(&text, "compacting"), 1, "two compaction lines look like two compactions:\n{text}");
        let spinners = text.lines().filter(|l| has_spinner(l)).count();
        assert_eq!(spinners, 1, "one spinning line:\n{text}");
    }
    // The clock is the compaction's own (from its start), not the turn's.
    let (first, _) = r.strip(12_030);
    assert!(first.contains("10.0s"), "{first}");
    r.phase(4_300, 2, PhaseEvent::end(PhaseKind::Compact, 2_270).with_done(28_100).with_total(6_200).unit("tok"));
    let text = r.frame(4_400);
    assert_eq!(times(&text, "compacted 28.1k → 6.2k · 2.3s"), 1, "{text}");
    assert!(!text.lines().any(|l| has_spinner(l) && l.contains("compact")), "the spinner is gone: {text}");
}
#[test]
fn a_failed_compaction_is_red_and_not_silent() {
    let mut r = Rig::new();
    r.enter(2_000, "go on");
    r.started(2_005, 2, "go on");
    r.phase(2_020, 2, PhaseEvent::end(PhaseKind::Fit, 10).with_done(28_100).unit("tok"));
    r.phase(2_030, 2, ph(PhaseKind::Compact));
    r.phase(2_900, 2, PhaseEvent::end(PhaseKind::Compact, 870).with_done(28_100).with_total(28_100).unit("tok").detail("failed"));
    let buf = render(&r.s, W, H);
    let text = dump(&buf);
    assert_eq!(times(&text, "compaction failed · kept 28.1k"), 1, "{text}");
    let th = theme();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect();
        if let Some(at) = row.find("compaction failed") {
            let col = row[..at].chars().count() as u16;
            assert_eq!(buf.cell((col, y)).unwrap().fg, marlowe_surface::chrome::Ink::Red.color(&th));
        }
    }
}
#[test]
fn a_cold_load_draws_a_measured_bar_against_a_marked_prediction_then_the_result() {
    let mut r = Rig::new();
    reading(&mut r);
    let mut start = ph(PhaseKind::Load).stage("model").detail("dusk-27b").with_total(13_100).unit("MiB");
    start.est = true;
    r.phase(2_030, 2, start);
    let (first, _) = r.strip(2_100);
    assert!(first.contains("loading dusk-27b") && !first.contains('━'), "no bar before any growth is measured: {first}");
    let mut p = PhaseEvent::progress(PhaseKind::Load, 6_246, 13_100).stage("model").unit("MiB");
    p.est = true;
    r.phase(3_000, 2, p);
    let (first, _) = r.strip(3_001);
    assert!(first.contains("loading dusk-27b") && first.contains('━') && first.contains("6.1 / ~12.8 GB"), "{first}");
    assert_eq!(times(&r.frame(3_002), "loading"), 1, "one load line");
    r.phase(8_200, 2, PhaseEvent::end(PhaseKind::Load, 6_170).stage("model").detail("dusk-27b").with_done(13_414).unit("MiB"));
    let text = r.frame(8_201);
    assert_eq!(times(&text, "loaded dusk-27b · 13.1 GB · 6.2s"), 1, "{text}");
}
// ---- after the turn ---------------------------------------------------------------------------------

#[test]
fn the_thought_header_keeps_the_rate_on_one_row_and_the_read_lives_under_it() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "Weighing the gate.\n".repeat(3), tokens: 312 });
    let mut end = PhaseEvent::end(PhaseKind::Prefill, 1_900).with_done(8_200).with_total(8_200).unit("tok");
    end.est = false;
    r.phase(7_000, 2, end);
    r.at(6_600, Event::TextDelta { session: sid(), turn: 2, text: "Done.".into() });
    r.at(7_100, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    let text = r.frame(8_000);
    assert!(text.contains("▸ thought 4.1s · 312 tok · 76 tok/s"), "{text}");
    assert!(!text.contains("read 8.2k") && !combined(&text), "closed, the block hides the read:\n{text}");
    r.open_fold(8_001, 0);
    assert!(r.frame(8_002).contains("read 8.2k · 1.9s"), "opened, it shows the read");
    for (w, h) in [(120u16, 30u16), (140, 40), (200, 50)] {
        let buf = render(&r.s, w, h);
        assert_palette_only(&buf);
        let text = dump(&buf);
        let heads: Vec<&str> = text.lines().filter(|l| l.contains("thought")).collect();
        assert_eq!(heads.len(), 1, "one header row at {w}: {text}");
        assert!(text.contains("read 8.2k"), "the read is under the block at {w}: {text}");
    }
}
#[test]
fn a_finished_turns_timings_open_with_reasoning_and_sum_to_the_first_token() {
    let mut r = Rig::new();
    reading(&mut r);
    r.at(2_500, Event::ReasoningDelta { session: sid(), turn: 2, text: "hmm".into(), tokens: 3 });
    let span = |kind, start_ms, ms| PhaseSpan { kind, stage: None, start_ms, ms, n: None, detail: None, parent: None, est: false };
    let mut prefill = span(PhaseKind::Prefill, 300, 1_900);
    prefill.n = Some(8_200);
    r.at(
        7_000,
        Event::TurnTimings {
            session: sid(),
            turn: 2,
            spans: vec![span(PhaseKind::Recall, 10, 180), span(PhaseKind::Prompt, 190, 12), prefill],
            numbers: TurnNumbers {
                ttft_ms: Some(2_100),
                enter_to_wire_ms: Some(5),
                token_to_bus_ms: Some(1),
                read: Some(ReadStat { new_tok: 8_200, cached_tok: 0, ms: 1_900 }),
            },
        },
    );
    r.at(7_001, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    let closed = r.frame(8_000);
    assert!(!closed.contains("first token"), "collapsed by default: no clutter\n{closed}");
    r.s.on_key(ctrl('t'));
    let open = r.frame(8_001);
    assert!(open.contains("recall 180ms · prompt 12ms · prefill 1.9s (8.2k) · first token 2.1s"), "{open}");
    assert!(open.contains("enter→wire 5ms · token→screen ~"), "{open}");
}

#[test]
fn a_reopened_session_shows_the_stored_read_and_timings() {
    let mut r = Rig::new();
    let span = PhaseSpan { kind: PhaseKind::Recall, stage: None, start_ms: 4, ms: 90, n: None, detail: None, parent: None, est: false };
    let turns = vec![
        StoredTurn {
            idx: 0,
            role: marlowe_model::Role::User,
            content: vec![marlowe_model::Part::Text { text: "why".into() }],
            at_ms: BASE,
            phases: vec![span],
            numbers: Some(TurnNumbers {
                ttft_ms: Some(1_400),
                read: Some(ReadStat { new_tok: 4_000, cached_tok: 0, ms: 1_100 }),
                ..TurnNumbers::default()
            }),
        },
        StoredTurn {
            idx: 1,
            role: marlowe_model::Role::Assistant,
            content: vec![
                marlowe_model::Part::Thinking(marlowe_model::Thinking { text: "because".into(), signature: None, details: None }),
                marlowe_model::Part::Text { text: "Because.".into() },
            ],
            at_ms: BASE + 1,
            phases: Vec::new(),
            numbers: None,
        },
    ];
    r.at(9_000, Event::Transcript { session: sid(), turns });
    let text = r.frame(9_001);
    assert!(text.contains("▸ thought") && !text.contains("read 4k"), "{text}");
    r.open_fold(9_001, 0);
    assert!(r.frame(9_002).contains("read 4k · 1.1s"), "the stored read is under the opened block");
    r.s.on_key(ctrl('t'));
    let open = r.frame(9_003);
    assert!(open.contains("recall 90ms · first token 1.4s"), "{open}");
}

#[test]
fn the_strip_keeps_the_screen_on_the_display_rate_while_it_is_up() {
    let mut r = Rig::new();
    assert_eq!(r.s.cadence(), marlowe_tui::pacing::Cadence::Idle);
    r.enter(2_000, "go");
    assert_eq!(r.s.cadence(), marlowe_tui::pacing::Cadence::Live);
}

// ---- the daemon's own background work ------------------------------------------------------------

fn work(stage: &str, p: PhaseEvent) -> Event {
    Event::Phase(p.stage(stage))
}

#[test]
fn background_work_shows_on_the_strip_while_it_runs_and_goes_when_it_ends() {
    let mut r = Rig::new();
    let idle = r.strip_now(1_100).0;
    assert!(!idle.starts_with(|c| SPINNERS.contains(c)) && !idle.contains("embedding"), "nothing runs: {idle:?}");

    r.at(2_000, work("embed", PhaseEvent::start(PhaseKind::Work).detail("embedding 4300 memories")));
    let (row, _) = r.strip(2_000);
    assert!(row.starts_with(|c| SPINNERS.contains(c)) && row.contains("embedding 4300 memories"), "{row:?}");
    assert!(row.contains("0ms"), "its clock starts at its start: {row:?}");

    r.at(5_400, work("embed", PhaseEvent::progress(PhaseKind::Work, 1_200, 4_300).unit("memories")));
    let (row, _) = r.strip(5_400);
    assert!(row.contains("1,200/4,300") && row.contains("3.4s"), "{row:?}");
    assert_eq!(times(&r.frame(5_400), "embedding"), 1, "one line, one clock");

    r.at(9_000, work("embed", PhaseEvent::end(PhaseKind::Work, 7_000)));
    let after = r.frame(9_000);
    assert!(!after.contains("embedding 4300"), "{after}");
    assert!(!r.s.strip_up(), "the strip's row is given back");
}

#[test]
fn a_turn_in_flight_owns_the_strip_and_work_waits_its_turn_not_beside_it() {
    let mut r = Rig::new();
    r.at(1_100, work("voice", PhaseEvent::start(PhaseKind::Work).detail("loading the voice model")));
    r.enter(2_000, "go");
    r.started(2_010, 2, "go");
    r.phase(2_020, 2, ph(PhaseKind::Wire));
    let text = r.frame(2_050);
    assert_eq!(times(&text, "loading the voice model"), 0, "the turn's activity is the one line:\n{text}");
    r.phase(2_060, 2, PhaseEvent::end(PhaseKind::Wire, 5));
    r.at(2_100, Event::TurnFinished { session: sid(), turn: 2, usage: usage(), stop: TurnStop::Done });
    let (row, _) = r.strip(9_000);
    assert!(row.contains("loading the voice model"), "back to the work once the turn is over: {row:?}");
}

#[test]
fn several_background_jobs_are_one_line_with_a_count() {
    let mut r = Rig::new();
    r.at(2_000, work("mcp:git", PhaseEvent::start(PhaseKind::Work).detail("starting MCP server git")));
    r.at(2_100, work("mcp:web", PhaseEvent::start(PhaseKind::Work).detail("starting MCP server web")));
    let (row, _) = r.strip(2_500);
    assert!(row.contains("starting MCP server web") && row.contains("+1"), "{row:?}");
    r.at(3_000, work("mcp:web", PhaseEvent::end(PhaseKind::Work, 900)));
    let (row, _) = r.strip(3_100);
    assert!(row.contains("starting MCP server git") && !row.contains("+1"), "{row:?}");
}

#[test]
fn progress_that_arrives_before_its_start_still_shows_the_job() {
    let mut r = Rig::new();
    r.at(2_000, work("embed", PhaseEvent::progress(PhaseKind::Work, 10, 40).unit("memories")));
    let (row, _) = r.strip(2_100);
    assert!(row.contains("embed") && row.contains("10/40"), "joined mid-way: {row:?}");
}
