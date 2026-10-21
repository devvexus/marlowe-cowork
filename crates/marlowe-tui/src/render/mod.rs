//! Drawing. A pure function of `(State, Theme, area)`: no clock reads, no I/O.
//!
//! Every style here has a foreground only. The terminal's own background is the background.
//!
//! # Layout
//!
//! The old harness's box language over the Conversation + Situation Board layout:
//!
//! ```text
//! * marlowe — what changed overnight?                   2 runs · 1 needs you
//! ╭ Conversation · title ───────── ⣀⣀ idle ╮╭ Model ──────────────╮
//! │                                         ││ Sonnet 4.5 ▾  medium │
//! │                                         │╰─────────────── (m) ─╯
//! │                                         │╭ Workspace ──────────╮
//! │                                         ││ ~/valiance ▾         │
//! │                                         │╰─────────────── (w) ─╯
//! │                                         │╭ 1 Crew 2 Watch … ───╮
//! ╰──────────────────────────────── (c) ───╯│                      │
//! ╭ Message ────────────────────────────────╮│                      │
//! │ › message Marlowe…                      ││                      │
//! ╰──────────────────────────────── (i) ───╯╰────────────── (1-4) ─╯
//!  ^n New  ^o Sessions  ^d Dispatch  …  ^k palette      Friday · 21:40
//! ```
//!
//! Every box is a region: a label on the top border, its hotkey on the bottom border, an accent
//! border while it has the keys and the hover tone while the pointer is over it. The header is
//! one borderless row; the conversation takes every row the header, composer and footer do not; the board column takes about three tenths of the width and shows its four
//! sections stacked when there is height for them, else as numbered tabs.

mod bg;
mod board;
mod boot;
mod context;
mod conversation;
mod crew;
mod library;
mod memory;
mod overlay;
mod plan;
mod panel;
mod popup;
mod strip;
mod transcript;
mod watch;

use marlowe_surface::chrome::Ink;
use marlowe_surface::{FocusLevel, Region, RegionId, Theme, MIN_COLS, MIN_ROWS};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};
use ratatui::Frame;

use crate::composer::MAX_VISIBLE_LINES;
use crate::links::{self, LinkConfig};
use crate::state::{Activity, BoardTab, Command, Conn, Focus, NoteTone, Overlay, State, Target, View};
use crate::text;

/// The board column's width range. Three tenths of the window, within these.
pub const BOARD_MIN: u16 = 38;
pub const BOARD_MAX: u16 = 72;
/// Height of the Model box: the conductor and its context meter.
const PICKER_BOX_H: u16 = 4;
/// Height of the Workspace box.
const WORKSPACE_BOX_H: u16 = 3;
/// The board stacks its sections only when it is at least this tall…
const STACK_MIN_H: u16 = 30;
/// …and Watch, which takes what the others leave, still gets this many rows inside its box.
const STACK_WATCH_MIN: u16 = 6;

/// How the Situation Board shows its four sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardMode {
    /// One box; the numbered tab strip is its title.
    Tabbed,
    /// Four boxes, one per section.
    Stacked,
}

/// Where everything is on a frame of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub too_small: bool,
    /// The header: one row, `* marlowe — <session>` and the live summary.
    pub header: Rect,
    /// Everything between the header and the footer. A full-screen view draws its box here.
    pub main: Rect,
    /// The hotkey footer: the last row.
    pub footer: Rect,
    /// The Conversation box, borders included.
    pub conversation_box: Rect,
    /// The transcript area inside the Conversation box.
    pub conversation: Rect,
    /// The Message box, when the conversation view shows one.
    pub composer: Option<Rect>,
    /// The Model box: the conductor picker.
    pub model: Option<Rect>,
    /// The Workspace box: where new sessions start.
    pub workspace: Option<Rect>,
    /// The Situation Board under the two pickers.
    pub board: Rect,
    pub board_mode: BoardMode,
    /// Each section's box when stacked; the one box four times when tabbed.
    pub sections: [Rect; 4],
    /// Click targets: the tab labels when tabbed, the boxes when stacked.
    pub tabs: [Rect; 4],
    /// The open picker dropdown (model or workspace).
    pub picker: Option<Rect>,
    /// Under the transcript, inside the Conversation box, while a research plan waits.
    pub plan_banner: Option<Rect>,
}

impl Geometry {
    /// The board section under a point, if any.
    pub fn tab_at(&self, p: Position) -> Option<BoardTab> {
        self.tabs.iter().position(|r| r.contains(p)).map(|i| BoardTab::ALL[i])
    }
}

/// The board column's width for a window `cols` wide.
pub fn board_width(cols: u16) -> u16 {
    (cols * 3 / 10).clamp(BOARD_MIN, BOARD_MAX)
}

/// Computes the layout. Shared by drawing and mouse hit-testing.
pub fn geometry(area: Rect, state: &State) -> Geometry {
    let too_small = area.width < MIN_COLS || area.height < MIN_ROWS;
    let mut g = Geometry {
        too_small,
        header: Rect::default(),
        main: area,
        footer: Rect::default(),
        conversation_box: Rect::default(),
        conversation: Rect::default(),
        composer: None,
        model: None,
        workspace: None,
        board: Rect::default(),
        board_mode: BoardMode::Tabbed,
        sections: [Rect::default(); 4],
        tabs: [Rect::default(); 4],
        picker: None,
        plan_banner: None,
    };
    if too_small {
        return g;
    }
    g.header = Rect { height: 1, ..area };
    g.main = Rect { y: area.y + 1, height: area.height - 2, ..area };
    g.footer = Rect { y: area.bottom() - 1, height: 1, ..area };
    if state.view != View::Conversation {
        return g;
    }

    let bw = board_width(area.width);
    let left_w = area.width - bw;
    let (lines, _) = state.composer.layout(composer_text_width(left_w));
    let composer_h = lines.len().clamp(1, MAX_VISIBLE_LINES) as u16 + 2;
    g.conversation_box = Rect { width: left_w, height: g.main.height - composer_h, ..g.main };
    g.conversation = inner(g.conversation_box);
    g.composer = Some(Rect { y: g.conversation_box.bottom(), height: composer_h, ..g.conversation_box });
    // A plan waiting on the user keeps a banner over the composer until it is answered.
    if state.crew.waiting_plan().is_some() && g.conversation.height >= plan::BANNER_ROWS + 6 {
        let rows = plan::BANNER_ROWS + 1;
        g.conversation.height -= rows;
        g.plan_banner = Some(Rect { y: g.conversation.bottom() + 1, height: plan::BANNER_ROWS, ..g.conversation });
    }

    let col = Rect { x: area.x + left_w, width: bw, ..g.main };
    let model = Rect { height: PICKER_BOX_H, ..col };
    let workspace = Rect { y: model.bottom(), height: WORKSPACE_BOX_H, ..model };
    g.model = Some(model);
    g.workspace = Some(workspace);
    g.board = Rect { y: workspace.bottom(), height: col.height - PICKER_BOX_H - WORKSPACE_BOX_H, ..col };
    layout_board(&mut g, state);

    match &state.overlay {
        Some(Overlay::ModelPicker { .. }) => {
            let names: Vec<String> = state
                .library
                .models
                .iter()
                .map(|lm| format!("{}  {}", text::short_model(&lm.model, Some(&lm.info)), lm.model.provider))
                .collect();
            let w = names.iter().map(|n| text::width(n)).max().unwrap_or(0).max(24) as u16 + 10;
            let h = state.library.models.len() as u16 + 1 + 2;
            g.picker = Some(dropdown(area, model, w.max(model.width), h));
        }
        Some(Overlay::WorkspacePicker { query, .. }) => {
            let rows = state.workspace_choices(query).len().clamp(1, 12) as u16;
            g.picker = Some(dropdown(area, workspace, workspace.width.max(72), rows + 4));
        }
        _ => {}
    }
    g
}

/// A dropdown under `anchor`, right-aligned with it, kept on screen.
fn dropdown(area: Rect, anchor: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let x = anchor.right().saturating_sub(w).max(area.x);
    let y = anchor.bottom();
    Rect { x, y, width: w, height: h.min(area.bottom().saturating_sub(y + 1)) }
}

fn inner(r: Rect) -> Rect {
    Rect { x: r.x + 1, y: r.y + 1, width: r.width.saturating_sub(2), height: r.height.saturating_sub(2) }
}

/// Stacked when the column is tall enough for four boxes with Watch still readable; else tabbed.
fn layout_board(g: &mut Geometry, state: &State) {
    let b = g.board;
    let w = b.width.saturating_sub(4) as usize;
    let crew_h = board::crew_rows(state) as u16 + 2;
    let threads_h = 3;
    let memory_h = board::memory_lines(state, w).len() as u16 + 2;
    let fixed = crew_h + threads_h + memory_h;
    let watch_h = b.height.saturating_sub(fixed);
    if b.height >= STACK_MIN_H && watch_h >= STACK_WATCH_MIN + 2 {
        g.board_mode = BoardMode::Stacked;
        let mut y = b.y;
        for (i, h) in [crew_h, watch_h, threads_h, memory_h].into_iter().enumerate() {
            g.sections[i] = Rect { y, height: h, ..b };
            g.tabs[i] = g.sections[i];
            y += h;
        }
    } else {
        g.board_mode = BoardMode::Tabbed;
        g.sections = [b; 4];
        let strip = board::tab_strip(b.width.saturating_sub(4) as usize, state.board_tab);
        for (tab, off, len) in strip.cells {
            g.tabs[tab.index()] = Rect { x: b.x + 2 + off as u16, y: b.y, width: len as u16, height: 1 };
        }
    }
}

fn composer_text_width(cols: u16) -> usize {
    cols.saturating_sub(6) as usize
}

/// The part of the conversation pane the transcript fills: the pane less the live strip's rows
/// while it is up. Tests that read the transcript off the screen use it.
pub fn transcript_pane(pane: Rect, state: &State) -> Rect {
    Rect { height: pane.height.saturating_sub(strip::height(state, pane)), ..pane }
}

/// Draws the whole frame.
pub fn draw(frame: &mut Frame, state: &State, theme: &Theme) {
    crate::images::begin_frame();
    state.strip.begin_frame();
    state.hits.borrow_mut().clear();
    state.code.link_plan.borrow_mut().clear();
    let area = frame.area();
    let g = geometry(area, state);
    if g.too_small {
        draw_too_small(area, frame.buffer_mut(), theme);
        return;
    }
    let buf = frame.buffer_mut();
    match state.view {
        View::Conversation => {
            draw_conversation_box(&g, state, theme, buf);
            if state.booting() {
                boot::draw(g.conversation, state, theme, buf);
            } else {
                conversation::draw(g.conversation, state, theme, buf);
                if let Some(r) = g.plan_banner {
                    plan::banner(r, state, theme, buf);
                }
            }
            board::draw(&g, state, theme, buf);
        }
        view => {
            draw_view_frame(g.main, state, theme, buf);
            match view {
                View::Library => library::draw(g.main, state, theme, buf),
                View::Memory => memory::draw(g.main, state, theme, buf),
                View::Watch => watch::draw(g.main, state, theme, buf),
                View::Crew => crew::draw(g.main, state, theme, buf),
                View::Conversation => {}
            }
        }
    }
    draw_header(g.header, state, theme, buf);
    draw_footer(g.footer, state, theme, buf);
    let cursor = g.composer.and_then(|r| draw_composer(r, state, theme, buf));
    if state.overlay.is_none() && state.view == View::Conversation && state.approvals.is_empty() && !state.booting() {
        popup::draw(&g, state, theme, buf);
    }

    // A floating panel takes the pointer: nothing under it reacts.
    if state.overlay.is_some() {
        state.hits.borrow_mut().clear();
    }
    // The quit modal sits over an approval that is waiting: both are on screen, the modal on top.
    let quit_over_approval = matches!(state.overlay, Some(Overlay::Choice { .. })) && state.current_approval().is_some();
    if let (true, Some(a)) = (quit_over_approval, state.current_approval()) {
        overlay::approval(area, state, a, theme, buf);
    }
    let mut overlay_cursor = None;
    match &state.overlay {
        Some(Overlay::Palette { query, sel }) => overlay::palette(area, state, query, *sel, theme, buf),
        Some(Overlay::ImportOffer { sel }) => overlay::import_offer(area, state, *sel, theme, buf),
        Some(Overlay::Choice { sel }) => overlay::choice(area, state, *sel, theme, buf),
        Some(Overlay::Onboard) => overlay_cursor = overlay::onboard(area, state, theme, buf),
        Some(Overlay::Dispatch) => overlay_cursor = crew::dispatch(area, state, theme, buf),
        Some(Overlay::Panel) => overlay_cursor = panel::draw(area, state, theme, buf),
        Some(Overlay::Sessions { sel }) => overlay::sessions(area, state, *sel, theme, buf),
        Some(Overlay::ContextPlan { .. }) => context::planner(area, state, theme, buf),
        Some(Overlay::ModelPicker { sel }) => {
            if let Some(r) = g.picker {
                overlay::model_picker(r, state, *sel, theme, buf)
            }
        }
        Some(Overlay::WorkspacePicker { query, sel }) => {
            if let Some(r) = g.picker {
                overlay_cursor = overlay::workspace_picker(r, state, query, *sel, theme, buf)
            }
        }
        None => {}
    }
    if state.overlay.is_none() && !state.booting() {
        boot::toasts(&g, state, theme, buf);
    }
    if let (false, Some(a)) = (quit_over_approval, state.current_approval()) {
        state.hits.borrow_mut().clear();
        if let Some(c) = overlay::approval(area, state, a, theme, buf) {
            frame.set_cursor_position(c);
        }
        return;
    }
    plan_links(&g, area, state, buf);
    if let Some(c) = overlay_cursor {
        frame.set_cursor_position(c);
    }
    if state.overlay.is_none() && state.view == View::Conversation && state.focus == Focus::Message {
        if let Some(c) = cursor {
            frame.set_cursor_position(c);
        }
    }
    let view_cursor = match state.view {
        View::Library => library::cursor(g.main, state),
        View::Memory if state.overlay.is_none() => memory::cursor(g.main, state),
        View::Watch if state.overlay.is_none() => watch::cursor(g.main, state),
        View::Crew if state.overlay.is_none() => crew::cursor(g.main, state),
        _ => None,
    };
    if let Some(c) = view_cursor {
        frame.set_cursor_position(c);
    }
}

/// Finds the file references and URLs on screen, for the run loop to link after the frame.
fn plan_links(g: &Geometry, area: Rect, state: &State, buf: &Buffer) {
    let region = match (&state.overlay, state.view) {
        (None, View::Conversation) if !state.booting() => Some(g.conversation),
        (Some(Overlay::Panel), _)
            if matches!(
                state.code.panel,
                Some(crate::panel::Panel::Transcript(_) | crate::panel::Panel::Diff(_) | crate::panel::Panel::Findings(_) | crate::panel::Panel::Files { .. })
            ) =>
        {
            Some(area)
        }
        _ => None,
    };
    if let Some(r) = region {
        let cfg = LinkConfig { editor_url: state.code.links.editor_url.clone(), workspace: state.shown_workspace().map(str::to_string) };
        *state.code.link_plan.borrow_mut() = links::scan(buf, r, &cfg);
    }
}

pub(crate) fn ink(i: Ink, theme: &Theme) -> Style {
    i.style(theme)
}

/// Records that `t` was drawn at `r` this frame, for the mouse.
pub(crate) fn hit(state: &State, r: Rect, t: Target) {
    if r.width > 0 && r.height > 0 {
        state.hits.borrow_mut().push((r, t));
    }
}

/// The pointer is over `t`.
pub(crate) fn hovered(state: &State, t: &Target) -> bool {
    state.hover.as_ref() == Some(t)
}

/// A line one foreground weight up, for the row under the pointer: dimmer reads dim, dim reads as
/// body text, body text goes bright. State colours keep their hue. Never a fill.
pub(crate) fn lit(line: Line<'static>, theme: &Theme) -> Line<'static> {
    let (dim, dimmer) = (theme.dim_color(), theme.dimmer_color());
    let spans = line
        .spans
        .into_iter()
        .map(|mut s| {
            s.style = match s.style.fg {
                None | Some(Color::Reset) => s.style.add_modifier(Modifier::BOLD),
                Some(c) if c == dim => s.style.fg(Color::Reset),
                Some(c) if c == dimmer => s.style.fg(dim),
                Some(_) => s.style,
            };
            s
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Writes a row that reacts to the pointer: recorded as `t` and lit while hovered.
/// `at` is `(x, y, width)`: where the row starts and how wide it may be.
pub(crate) fn put_row(state: &State, buf: &mut Buffer, at: (u16, u16, u16), line: Line<'static>, t: Target, theme: &Theme) {
    let (x, y, max) = at;
    hit(state, Rect { x, y, width: max, height: 1 }, t.clone());
    let line = if hovered(state, &t) { lit(line, theme) } else { line };
    put(buf, x, y, &line, max);
}

pub(crate) fn note_style(tone: NoteTone, theme: &Theme) -> Style {
    match tone {
        NoteTone::Dim => ink(Ink::Dim, theme),
        NoteTone::Amber => ink(Ink::Amber, theme),
        NoteTone::Red => ink(Ink::Red, theme),
    }
}

/// Writes a line clipped to `max` columns.
pub(crate) fn put(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, max: u16) {
    buf.set_line(x, y, line, max);
}

// ---- regions ------------------------------------------------------------------------------------

/// The four fixed regions, through the shared region contract: a label and a hotkey, or no box.
pub(crate) fn region(id: RegionId) -> Region {
    match id {
        RegionId::Conversation => Region::new(id, "Conversation", 'c'),
        RegionId::Message => Region::new(id, "Message", 'i'),
        RegionId::Model => Region::new(id, "Model", 'm'),
        RegionId::Workspace => Region::new(id, "Workspace", 'w'),
        other => unreachable!("{other:?} is not a region of this screen"),
    }
}

/// How focused a box is. An overlay or an approval holds the keys, so no box does — except the
/// picker box whose dropdown is open.
pub(crate) fn focus_level(state: &State, f: Focus) -> FocusLevel {
    if !state.approvals.is_empty() {
        return FocusLevel::Unfocused;
    }
    let holder = match &state.overlay {
        Some(Overlay::ModelPicker { .. }) => Some(Focus::Model),
        Some(Overlay::WorkspacePicker { .. }) => Some(Focus::Workspace),
        Some(_) => None,
        None => Some(state.focus),
    };
    if holder == Some(f) {
        FocusLevel::Focused
    } else if state.overlay.is_none() && box_hovered(state, f) {
        FocusLevel::Hovered
    } else {
        FocusLevel::Unfocused
    }
}

/// The pointer is over box `f` (for the board: over one of its tab labels).
fn box_hovered(state: &State, f: Focus) -> bool {
    match (&state.hover, f) {
        (Some(Target::Box(h)), _) => *h == f,
        (Some(Target::Tab(_)), Focus::Board) => true,
        (Some(Target::Thought(_)), Focus::Conversation) => true,
        _ => false,
    }
}

/// The label style for a focus level: accent, bold when focused.
pub(crate) fn label_style(focus: FocusLevel, theme: &Theme) -> Style {
    theme.region_styles(focus).1
}

/// One box in the old harness's style: rounded border, `title` on the top border at the left,
/// `hotkey` on the bottom border at the right. Focus is the border colour and the label weight —
/// [`Theme::region_styles`], never a fill. Returns the area inside the border.
pub(crate) fn region_box(
    r: Rect,
    title: Vec<Span<'_>>,
    hotkey: &str,
    focus: FocusLevel,
    theme: &Theme,
    buf: &mut Buffer,
) -> Rect {
    if r.width < 4 || r.height < 2 {
        return Rect::default();
    }
    let (border, _, key) = theme.region_styles(focus);
    Block::bordered().border_type(BorderType::Rounded).border_style(border).render(r, buf);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(title);
    spans.push(Span::raw(" "));
    put(buf, r.x + 1, r.y, &Line::from(spans), r.width.saturating_sub(3));
    if !hotkey.is_empty() {
        let hk = Line::from(Span::styled(format!(" {hotkey} "), key));
        let w = hk.width() as u16;
        put(buf, r.right().saturating_sub(w + 2), r.bottom() - 1, &hk, w);
    }
    inner(r)
}

/// A plain labelled title: `Label` in the label style, then an optional dim subtitle.
pub(crate) fn titled(label: &str, sub: Option<String>, focus: FocusLevel, theme: &Theme) -> Vec<Span<'static>> {
    let mut v = vec![Span::styled(label.to_string(), label_style(focus, theme))];
    if let Some(sub) = sub.filter(|s| !s.trim().is_empty()) {
        v.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        v.push(Span::styled(sub, ink(Ink::Dim, theme)));
    }
    v
}

/// Text on a box's top border, right-aligned, if it fits after `taken` columns of title.
pub(crate) fn border_right(r: Rect, y: u16, line: &Line<'_>, taken: u16, buf: &mut Buffer) -> Option<Rect> {
    let w = line.width() as u16 + 2;
    if taken + w + 4 > r.width {
        return None;
    }
    let l = Line::from([vec![Span::raw(" ")], line.spans.clone(), vec![Span::raw(" ")]].concat());
    let x = r.right().saturating_sub(w + 2);
    put(buf, x, y, &l, w);
    Some(Rect { x, y, width: w, height: 1 })
}

/// Text on a box's bottom border at the left, leaving room for the hotkey.
pub(crate) fn border_left(r: Rect, line: &Line<'_>, buf: &mut Buffer) {
    let room = r.width.saturating_sub(12);
    let l = Line::from([vec![Span::raw(" ")], line.spans.clone(), vec![Span::raw(" ")]].concat());
    put(buf, r.x + 2, r.bottom() - 1, &l, room);
}

/// `key label · key label` in the hint weights.
pub(crate) fn hints(items: &[(&str, &str)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        }
        spans.push(Span::styled(k.to_string(), ink(Ink::Dim, theme)));
        spans.push(Span::styled(format!(" {v}"), ink(Ink::Dimmer, theme)));
    }
    Line::from(spans)
}

fn draw_too_small(area: Rect, buf: &mut Buffer, theme: &Theme) {
    let lines = [
        Line::from(Span::styled("MARLOWE", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(
            format!("needs a window of at least {MIN_COLS} × {MIN_ROWS}"),
            theme.normal(),
        )),
        Line::from(Span::styled(
            format!("this one is {} × {} — resize to continue", area.width, area.height),
            ink(Ink::Dim, theme),
        )),
    ];
    let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    for (i, l) in lines.iter().enumerate() {
        let w = l.width() as u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = top + i as u16;
        if y < area.bottom() {
            put(buf, x, y, l, area.width);
        }
    }
}

/// The braille presence meter: six cells, one row, levels 0..=4 per dot column. It assembles
/// during the boot and follows the voice envelope while Marlowe speaks.
pub(crate) fn meter(state: &State) -> String {
    let act = state.activity();
    let boot = state.wake.boot.filter(|b| b.active(state.now_ms));
    let voice = state.wake.voice_levels(state.now_ms);
    let levels: [u8; 12] = if let Some(b) = boot {
        b.meter_levels(state.now_ms)
    } else if let (Some(v), false) = (voice, act.moving()) {
        v
    } else if act.moving() {
        let t = state.now_ms.max(0) as f64 / 1000.0;
        let speed = match act {
            Activity::Thinking => 4.2,
            Activity::Writing => 6.0,
            _ => 2.4,
        };
        std::array::from_fn(|i| {
            let a = (t * speed - i as f64 * 0.55).sin() * 0.6 + (t * speed * 2.3 + i as f64).sin() * 0.4;
            (1.0 + (a * 0.5 + 0.5) * 3.0).round().clamp(1.0, 4.0) as u8
        })
    } else if act == Activity::Awaiting {
        [1, 2, 3, 4, 3, 2, 2, 3, 4, 3, 2, 1]
    } else {
        [1; 12]
    };
    // Dots in a braille cell, bottom row first: left column (7,3,2,1), right column (8,6,5,4).
    const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];
    (0..6)
        .map(|c| {
            let left: u32 = LEFT.iter().take(levels[c * 2] as usize).sum();
            let right: u32 = RIGHT.iter().take(levels[c * 2 + 1] as usize).sum();
            let bits = left | right;
            char::from_u32(0x2800 + bits).unwrap_or(' ')
        })
        .collect()
}

/// The presence meter, what Marlowe is doing, and for how long when it is working.
fn presence(state: &State, theme: &Theme) -> Line<'static> {
    let act = state.activity();
    let meter_style = match act {
        _ if state.booting() => ink(Ink::Accent, theme),
        Activity::Speaking => ink(Ink::Speech, theme),
        Activity::Offline => ink(Ink::Red, theme),
        Activity::Awaiting => ink(Ink::Amber, theme),
        a if a.moving() => ink(Ink::Accent, theme),
        _ => ink(Ink::Dim, theme),
    };
    let act_style = match act {
        Activity::Offline => ink(Ink::Red, theme),
        Activity::Awaiting => ink(Ink::Amber, theme),
        Activity::Idle | Activity::Connecting => ink(Ink::Dim, theme),
        _ => ink(Ink::Speech, theme),
    };
    let word = if state.booting() { "waking" } else { act.label() };
    let mut spans = vec![Span::styled(meter(state), meter_style), Span::raw(" "), Span::styled(word, act_style)];
    if let Some(live) = &state.live {
        let secs = (state.now_ms - live.started_ms).max(0) / 1000;
        if secs > 0 {
            let t = if secs < 60 { format!("{secs}s") } else { format!("{}m{:02}s", secs / 60, secs % 60) };
            spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
            spans.push(Span::styled(t, ink(Ink::Dim, theme)));
        }
    }
    Line::from(spans)
}

/// The Conversation box: the session title, the presence meter, and its keys when focused.
fn draw_conversation_box(g: &Geometry, state: &State, theme: &Theme, buf: &mut Buffer) {
    let r = g.conversation_box;
    let reg = region(RegionId::Conversation);
    let focus = focus_level(state, Focus::Conversation);
    let room = r.width.saturating_sub(40) as usize;
    let sub = state.title.as_deref().map(|t| text::truncate(&text::clean_maths(t), room.min(64)));
    let title = titled(reg.label(), sub, focus, theme);
    let taken = Line::from(title.clone()).width() as u16 + 3;
    region_box(r, title, &reg.hotkey_label(), focus, theme, buf);
    border_right(r, r.y, &presence(state, theme), taken, buf);
    if focus == FocusLevel::Focused {
        // With reasoning on screen, Enter folds the block the cursor is on (as the last frame drew
        // it) and Shift+arrows move the cursor between blocks.
        let v = state.viewport.get();
        let cursor = crate::state::pick_thought(&state.marks.borrow(), v.top, v.size.1 as usize, state.thought);
        let keys = match cursor {
            Some(e) => {
                let fold = if state.thought_open(e) { "close" } else { "open" };
                hints(&[("↑↓", "scroll"), ("⇧↑↓", "thoughts"), ("↵", fold), ("end", "latest"), ("i", "type")], theme)
            }
            None => hints(&[("↑↓", "scroll"), ("end", "latest"), ("i", "type")], theme),
        };
        border_left(r, &keys, buf);
    }
}

/// A full-screen view's box: its name, the presence meter, and the view's own key hints (which
/// each view draws on the bottom border).
fn draw_view_frame(main: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let (label, sub) = match state.view {
        View::Library => ("Library", None),
        View::Memory => ("Memory", None),
        View::Watch => ("Watch", None),
        View::Crew => match state.crew.open_run() {
            Some(r) => ("Run", Some(text::truncate(&text::clean_maths(&r.title), 56))),
            None => ("Crew", None),
        },
        View::Conversation => ("Conversation", None),
    };
    let title = titled(label, sub, FocusLevel::Focused, theme);
    let taken = Line::from(title.clone()).width() as u16 + 3;
    region_box(main, title, "", FocusLevel::Focused, theme, buf);
    border_right(main, main.y, &presence(state, theme), taken, buf);
}

/// The header: `* marlowe — <session>` on the left, the live summary on the right. One row, no
/// border (it has no hotkey); the summary opens the crew.
fn draw_header(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    if r.height == 0 {
        return;
    }
    let summary = state.summary();
    let sw = text::width(&summary) as u16;
    let target = Target::Summary;
    let summary_style = if hovered(state, &target) { theme.normal() } else { ink(Ink::Dim, theme) };
    let sx = r.right().saturating_sub(sw + 1);
    put(buf, sx, r.y, &Line::from(Span::styled(summary, summary_style)), sw);
    hit(state, Rect { x: sx, y: r.y, width: sw, height: 1 }, target);
    let room = r.width.saturating_sub(sw + 4) as usize;
    let title = text::truncate(&format!("marlowe — {}", state.session_label()), room.saturating_sub(2));
    let left = Line::from(vec![Span::styled("* ", ink(Ink::Accent, theme)), Span::styled(title, theme.normal())]);
    put(buf, r.x + 1, r.y, &left, room as u16);
}

/// The footer: the most-used chords on the left, the day on the right. One line, the same in
/// every view; whatever does not fit is in the ^k palette. Every chord is also a button.
fn draw_footer(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    // (key, label, keep-priority: higher stays longer, key style, what a click does).
    type Item<'a> = (&'a str, &'a str, u8, Style, Target);
    let mut items: Vec<Item> = Vec::new();
    let key = ink(Ink::Dim, theme);
    let chord = |c: Command| Target::Chord(c);
    // While an approval is up, esc answers it (deny) rather than interrupting.
    if state.is_busy() && state.approvals.is_empty() {
        items.push(("esc", "Interrupt", 9, ink(Ink::Amber, theme), chord(Command::Interrupt)));
    }
    items.extend([
        ("^n", "New", 8, key, chord(Command::NewSession)),
        ("alt-s", "Sessions", 7, key, chord(Command::Sessions)),
        ("^d", "Dispatch", 7, key, chord(Command::Dispatch)),
        ("alt-r", "Crew", 6, key, chord(Command::Crew)),
        ("^w", "Watch", 5, key, chord(Command::Watch)),
        ("^g", "Memory", 5, key, chord(Command::Memory)),
        ("^l", "Library", 4, key, chord(Command::Library)),
        ("^o", "Transcript", 3, key, chord(Command::Transcript)),
        ("⇧tab", "Mode", 3, key, chord(Command::CycleMode)),
        ("^t", "Thinking", 2, key, chord(Command::ToggleThinking)),
        ("alt-v", "Voice", 1, key, chord(Command::ToggleVoice)),
        ("^k", "palette", 10, key, Target::Palette),
    ]);
    let item_w = |items: &[Item]| -> usize {
        items.iter().map(|(k, l, ..)| text::width(k) + 1 + text::width(l)).sum::<usize>()
            + 2 * items.len().saturating_sub(1)
    };

    // The day: the date, quiet hours (amber while in them), the clock. Shrinks before the chords.
    let dim = ink(Ink::Dim, theme);
    let sep = || Span::styled(" · ", ink(Ink::Dimmer, theme));
    let mut quiet: Option<Span<'static>> = None;
    if state.wake.watch.is_some() {
        let q = state.wake.quiet();
        let now_quiet = crate::wake::clock_minute(&state.clock).is_some_and(|m| q.contains(m));
        quiet = Some(if now_quiet {
            Span::styled(format!("quiet now {}", q.label()), ink(Ink::Amber, theme))
        } else {
            Span::styled(format!("quiet {}", q.label()), dim)
        });
    }
    let date = (!state.date.is_empty()).then(|| Span::styled(state.date.clone(), dim));
    let clock = (!state.clock.is_empty()).then(|| Span::styled(state.clock.clone(), theme.normal()));
    let variants: [Vec<Option<Span<'static>>>; 3] =
        [vec![date, quiet.clone(), clock.clone()], vec![quiet, clock.clone()], vec![clock]];
    // Every view's chord stays before the date does; Thinking and Voice give way first.
    let essential: Vec<_> = items.iter().filter(|i| i.2 >= 4).cloned().collect();
    let avail = r.width.saturating_sub(2) as usize;
    let mut right = Line::default();
    for v in variants {
        let mut spans = Vec::new();
        for s in v.into_iter().flatten() {
            if !spans.is_empty() {
                spans.push(sep());
            }
            spans.push(s);
        }
        right = Line::from(spans);
        if item_w(&essential) + right.width() + 4 <= avail {
            break;
        }
    }
    let left_room = avail.saturating_sub(right.width() + 4);
    while item_w(&items) > left_room && items.len() > 1 {
        let (i, _) = items.iter().enumerate().min_by_key(|(i, it)| (it.2, std::cmp::Reverse(*i))).unwrap();
        items.remove(i);
    }
    let mut spans = Vec::new();
    let mut x = r.x + 1;
    for (i, (k, l, _, ks, t)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x += 2;
        }
        let w = (text::width(k) + 1 + text::width(l)) as u16;
        // Under the pointer the key takes Marlowe's own colour and the label reads as body text.
        let (ks, ls) = if hovered(state, t) {
            (ink(Ink::Speech, theme), theme.normal())
        } else {
            (*ks, ink(Ink::Dimmer, theme))
        };
        spans.push(Span::styled(k.to_string(), ks));
        spans.push(Span::styled(format!(" {l}"), ls));
        hit(state, Rect { x, y: r.y, width: w, height: 1 }, t.clone());
        x += w;
    }
    put(buf, r.x + 1, r.y, &Line::from(spans), left_room as u16);
    let rw = right.width() as u16;
    put(buf, r.right().saturating_sub(rw + 1), r.y, &right, rw);
}

/// What the Message box's top border says about the next message: the permission mode (when it is
/// not the default), what is queued and what is attached.
fn composer_chips(state: &State, theme: &Theme) -> Line<'static> {
    use marlowe_proto::PermissionMode;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let push = |s: Span<'static>, spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        }
        spans.push(s);
    };
    let mode = state.code.mode;
    if mode != PermissionMode::Default {
        let (label, i) = match mode {
            PermissionMode::Plan => ("plan mode", Ink::Accent),
            PermissionMode::AcceptEdits => ("accept edits", Ink::Green),
            PermissionMode::Bypass => ("bypass", Ink::Red),
            PermissionMode::DontAsk => ("don't ask", Ink::Amber),
            PermissionMode::Default => ("", Ink::Dim),
        };
        push(Span::styled(label, ink(i, theme).add_modifier(Modifier::BOLD)), &mut spans);
        spans.push(Span::styled(" ⇧tab", ink(Ink::Dimmer, theme)));
    }
    let q = state.code.queue_len();
    if q > 0 {
        push(Span::styled(format!("{q} queued"), ink(Ink::Dim, theme)), &mut spans);
    }
    let n = state.code.attachments.len();
    if n > 0 {
        push(Span::styled(format!("{n} image{}", if n == 1 { "" } else { "s" }), ink(Ink::Speech, theme)), &mut spans);
    }
    Line::from(spans)
}

/// The status line when the user has not set a command: spend and background jobs, if any.
fn builtin_status(state: &State) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if state.code.totals.usd > 0.0 {
        parts.push(text::cost(state.code.totals.usd));
    }
    let jobs = state.code.running_jobs();
    if jobs > 0 {
        parts.push(format!("{jobs} job{}", if jobs == 1 { "" } else { "s" }));
    }
    // Everything else running in the background (agents, workflows, monitors).
    let tasks = state.bg.strip(state.session.as_ref()).iter().filter(|t| t.kind != marlowe_proto::TaskKind::Job).count();
    if tasks > 0 {
        parts.push(format!("{tasks} task{}", if tasks == 1 { "" } else { "s" }));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Draws the Message box. Returns the cursor position.
fn draw_composer(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let reg = region(RegionId::Message);
    let focus = focus_level(state, Focus::Message);
    let title = titled(reg.label(), None, focus, theme);
    let taken = Line::from(title.clone()).width() as u16 + 3;
    region_box(r, title, &reg.hotkey_label(), focus, theme, buf);
    let chips = composer_chips(state, theme);
    if !chips.spans.is_empty() {
        if let Some(rect) = border_right(r, r.y, &chips, taken, buf) {
            hit(state, rect, Target::Chord(Command::CycleMode));
        }
    }

    let tw = composer_text_width(r.width);
    let x0 = r.x + 4;
    let rows = r.height.saturating_sub(2) as usize;
    put(buf, r.x + 2, r.y + 1, &Line::from(Span::styled("›", ink(Ink::Accent, theme))), 1);

    let (lines, (crow, ccol)) = state.composer.layout(tw);
    let first = (crow + 1).saturating_sub(rows);
    if state.composer.text().is_empty() {
        let hint = if !state.conn.is_online() {
            "offline — reconnecting…"
        } else if state.library.known && !state.library.has_models() {
            "press ↵ to add a model"
        } else if state.is_busy() {
            "queue a message — it goes in at the next step…"
        } else {
            "message Marlowe…"
        };
        put(buf, x0, r.y + 1, &Line::from(Span::styled(hint, ink(Ink::Dimmer, theme))), tw as u16);
        // What the keys do here, while there is nothing typed to cover.
        let keys = if state.is_busy() {
            Line::from(vec![
                Span::styled("esc", ink(Ink::Amber, theme)),
                Span::styled(" interrupt · ", ink(Ink::Dimmer, theme)),
                Span::styled("↵", ink(Ink::Dim, theme)),
                Span::styled(" queue", ink(Ink::Dimmer, theme)),
            ])
        } else {
            hints(&[("⇧↵", "newline"), ("esc", "boxes")], theme)
        };
        let kw = keys.width() as u16;
        if kw + text::width(hint) as u16 + 4 < tw as u16 {
            put(buf, r.right().saturating_sub(kw + 2), r.y + 1, &keys, kw);
        }
    } else {
        for (i, l) in lines.iter().skip(first).take(rows).enumerate() {
            let clean = text::clean(l);
            put(buf, x0, r.y + 1 + i as u16, &Line::from(Span::styled(clean, theme.normal())), tw as u16);
        }
    }

    // Bottom border: a flash or the connection's trouble, left of the hotkey.
    let status = if let Some(f) = &state.flash {
        Some(Line::from(Span::styled(text::clean(&f.text), note_style(f.tone, theme))))
    } else if let Conn::Offline { reason, attempt } = &state.conn {
        Some(Line::from(Span::styled(
            format!("offline · {} · retry {attempt}", text::clean(reason)),
            ink(Ink::Red, theme),
        )))
    } else if let Conn::Refused(msg) = &state.conn {
        Some(Line::from(Span::styled(format!("refused · {}", text::clean(msg)), ink(Ink::Red, theme))))
    } else if let Some(t) = state.code.statusline.text.as_deref().filter(|_| state.code.statusline_command().is_some()) {
        // The user's own status line, with its colours removed: the palette is fixed.
        Some(Line::from(Span::styled(text::clean(t), ink(Ink::Dim, theme))))
    } else {
        builtin_status(state).map(|t| Line::from(Span::styled(t, ink(Ink::Dimmer, theme))))
    };
    if let Some(l) = status {
        let max = r.width.saturating_sub(14) as usize;
        let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        let st = l.spans.first().map(|s| s.style).unwrap_or_default();
        border_left(r, &Line::from(Span::styled(text::truncate(&t, max), st)), buf);
    }

    let visible_row = crow.checked_sub(first)?;
    if visible_row >= rows {
        return None;
    }
    Some((x0 + ccol as u16, r.y + 1 + visible_row as u16))
}
