//! The work block: everything the model did in one assistant turn (its reasoning and its tool
//! calls) as ONE block in the transcript, and the lines that stay outside it (an edit, a sent file).
//!
//! The entries stay flat in [`State::entries`], in the order they happened; [`roles`] groups each
//! contiguous run of reasoning and tool entries (the text of the assistant ends a run) at layout
//! time. The first of them draws the block and the rest draw nothing of their own, so the cache
//! keeps one rows-list per entry and a streaming frame still lays out one block. A short sentence
//! of text right before a run that holds a call is the run's title ([`preamble`]).
//!
//! **Collapsed** (the default), two rows that update live: a header with the clock, the tool count
//! and the tokens, then the latest reasoning as one dim line; and the most recent call, in the colour
//! of its state (amber while the model streams its arguments, blue running, green done, red failed),
//! or `3 running · bash, read_file, grep` when calls overlap. **Expanded** (a click on the header, `^t`
//! or the Conversation box's cursor and Enter): the turn in order, each thought and each call on its
//! own row, a click on a call opening its output.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::{markdown, Theme};
use ratatui::text::{Line, Span};

use super::ink;
use crate::layout_cache::Hasher;
use crate::state::{Entry, Mark, RowHit, State, Target, ToolStatus};
use crate::text;

/// What an entry is to the work block of its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Role {
    /// Not part of any block.
    Plain,
    /// Draws the block of these entries (its own index first).
    First(Vec<usize>),
    /// Drawn by the block above.
    Member,
    /// A short line of text that titles the block under it, drawn in that block's header.
    Title,
}

/// The roles of the transcript's entries, and the first entry of the block no later turn has
/// closed (the one the turn in flight is writing).
pub(super) fn roles(entries: &[Entry]) -> (Vec<Role>, Option<usize>) {
    fn close(out: &mut [Role], cur: &mut Vec<usize>) {
        if let Some(&first) = cur.first() {
            out[first] = Role::First(std::mem::take(cur));
        }
    }
    let mut out = vec![Role::Plain; entries.len()];
    let mut cur = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e {
            // Text ends the run; a blank one says nothing and ends nothing.
            Entry::Prose { text } if !text.trim().is_empty() => close(&mut out, &mut cur),
            Entry::Thinking { .. } | Entry::Tool { .. } => {
                if !cur.is_empty() {
                    out[i] = Role::Member;
                }
                cur.push(i);
            }
            // What a turn does not hold: the next one starts a block of its own.
            Entry::User { .. } | Entry::News { .. } | Entry::Usage { .. } | Entry::Timings(_) | Entry::Brief { .. } | Entry::Project { .. } | Entry::Seq(_) => {
                close(&mut out, &mut cur)
            }
            _ => {}
        }
    }
    let open = cur.first().copied();
    close(&mut out, &mut cur);
    for i in 0..entries.len() {
        if let Role::First(members) = &out[i] {
            if let Some(t) = preamble(entries, i, members) {
                out[t] = Role::Title;
            }
        }
    }
    (out, open)
}

/// The entry that titles the block whose first entry is `first`: the text right before it, when it
/// is one short sentence (at most 100 characters, one line, not a question) and the block holds a
/// call. Longer text, a question, or text at the end of a turn stays text.
pub(super) fn preamble(entries: &[Entry], first: usize, members: &[usize]) -> Option<usize> {
    let at = first.checked_sub(1)?;
    let Some(Entry::Prose { text }) = entries.get(at) else { return None };
    if !members.iter().any(|&m| matches!(entries.get(m), Some(Entry::Tool { .. }))) {
        return None;
    }
    let t = text.trim();
    let sentence = !t.is_empty()
        && t.chars().count() <= 100
        && !t.contains('\n')
        && !t.ends_with('?')
        && !t.contains(['`', '*', '#', '|', '$', '\\'])
        // One sentence: a stop inside it, followed by more words, makes it two.
        && !t.char_indices().any(|(i, c)| matches!(c, '.' | '!' | '?' | '…') && t[i + c.len_utf8()..].starts_with(char::is_whitespace));
    sentence.then_some(at)
}

/// A title's words: the sentence without its closing stop or ellipsis.
fn title_words(text: &str) -> String {
    let t = text::clean(text.trim());
    t.trim_end_matches(['.', '…', ':', '!']).trim_end().to_string()
}

/// Whether entry `e` (with `role`) draws no row at all.
pub(super) fn silent(state: &State, e: &Entry, role: &Role) -> bool {
    match (e, role) {
        (Entry::Tool { call_id, .. }, Role::Member) => !has_diff(state, call_id),
        (Entry::Thinking { .. }, Role::Member) => true,
        (Entry::Prose { .. }, Role::Title) => true,
        _ => false,
    }
}

fn has_diff(state: &State, call_id: &str) -> bool {
    state.code.tool_detail.get(call_id).is_some_and(|d| d.diff.is_some())
}

/// What the block's rows depend on, besides the entries themselves.
pub(super) fn key(state: &State, first: usize, members: &[usize], live: bool, h: &mut Hasher) {
    h.u64(u64::from(live) | u64::from(state.thought_open(first)) << 1 | u64::from(state.show_thinking) << 2);
    if let Some(Entry::Prose { text }) = preamble(&state.entries, first, members).and_then(|t| state.entries.get(t)) {
        h.str(text);
    }
    if live {
        h.u64((state.now_ms / 1000) as u64);
    }
    for &m in members {
        h.u64(m as u64);
        if let Some(e) = state.entries.get(m) {
            h.entry(e);
            if let Entry::Tool { call_id, .. } = e {
                let d = state.code.tool_detail.get(call_id);
                h.str(d.map_or("", |d| d.tool.as_str()));
                h.u64(u64::from(state.code.preparing.iter().any(|k| k == call_id)));
                let t = state.code.tool_times.get(call_id);
                h.u64(t.map_or(0, |t| t.0 as u64));
                h.u64(t.and_then(|t| t.1).map_or(0, |t| t as u64));
            }
        }
    }
}

// ---- the calls ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// The model is still streaming the call's arguments.
    Streaming,
    Running,
    Done(String),
    Failed(String),
}

struct Call<'a> {
    id: &'a str,
    name: String,
    args: String,
    phase: Phase,
    ms: Option<u64>,
}

fn call<'a>(state: &State, call_id: &'a str, summary: &str, status: &ToolStatus) -> Call<'a> {
    let line = text::clean(summary.lines().next().unwrap_or("").trim());
    let name = state
        .code
        .tool_detail
        .get(call_id)
        .map(|d| d.tool.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| line.split_whitespace().next().unwrap_or("").to_string());
    let args = line.strip_prefix(name.as_str()).unwrap_or(&line).trim().to_string();
    let phase = match status {
        ToolStatus::Running if state.code.preparing.iter().any(|k| k == call_id) => Phase::Streaming,
        ToolStatus::Running => Phase::Running,
        ToolStatus::Done(s) => Phase::Done(text::clean(s.lines().next().unwrap_or(""))),
        ToolStatus::Failed(s) if s.trim().is_empty() => Phase::Failed("failed".into()),
        ToolStatus::Failed(s) => Phase::Failed(text::clean(s.lines().next().unwrap_or(""))),
    };
    let ms = state.code.tool_times.get(call_id).map(|&(start, end)| (end.unwrap_or(state.now_ms) - start).max(0) as u64);
    Call { id: call_id, name, args, phase, ms }
}

/// The state's words, in the state's colour; a result or an error is cut to `max` columns.
fn state_spans(c: &Call, max: usize, theme: &Theme) -> Vec<Span<'static>> {
    let took = |ms: Option<u64>, min: u64| ms.filter(|m| *m >= min).map(|m| Span::styled(format!(" · {}", super::strip::took(m)), ink(Ink::Dimmer, theme)));
    match &c.phase {
        Phase::Streaming => vec![Span::styled("streaming", ink(Ink::Amber, theme))],
        Phase::Running => std::iter::once(Span::styled("running", ink(Ink::Info, theme))).chain(took(c.ms, 1000)).collect(),
        Phase::Done(s) => {
            let mut v = vec![Span::styled("✓", ink(Ink::Green, theme))];
            if !s.trim().is_empty() {
                v.push(Span::styled(format!(" {}", text::truncate(s, max)), ink(Ink::Dim, theme)));
            }
            v.extend(took(c.ms, 1000));
            v
        }
        Phase::Failed(s) => {
            let mut v = vec![Span::styled(format!("✗ {}", text::truncate(s, max)), ink(Ink::Red, theme))];
            v.extend(took(c.ms, 1000));
            v
        }
    }
}

fn spans_width(v: &[Span<'_>]) -> usize {
    v.iter().map(|s| text::width(&s.content)).sum()
}

// ---- the block ----------------------------------------------------------------------------------

/// The block's tool calls, its reasoning's totals, and the latest thought.
struct Facts<'a> {
    calls: Vec<Call<'a>>,
    tokens: u64,
    ms: u64,
    latest: String,
}

fn facts<'a>(state: &'a State, members: &[usize]) -> Facts<'a> {
    let mut f = Facts { calls: Vec::new(), tokens: 0, ms: 0, latest: String::new() };
    let mut spans: Vec<(i64, i64)> = Vec::new();
    for &m in members {
        match state.entries.get(m) {
            Some(Entry::Thinking { text: t, tokens, ms, .. }) => {
                f.tokens += if *tokens > 0 { *tokens } else { (t.chars().count() as u64).div_ceil(4) };
                f.ms += ms;
                if let Some(l) = t.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                    f.latest = text::clean(l).split_whitespace().collect::<Vec<_>>().join(" ");
                }
            }
            Some(Entry::Tool { call_id, summary, status }) => {
                if let Some(&(start, end)) = state.code.tool_times.get(call_id.as_str()) {
                    spans.push((start, end.unwrap_or(state.now_ms).max(start)));
                }
                f.calls.push(call(state, call_id, summary, status));
            }
            _ => {}
        }
    }
    // Calls that overlap count once: the clock is the turn's, not the sum of its calls.
    spans.sort_unstable();
    let mut reach = i64::MIN;
    for (start, end) in spans {
        f.ms += (end - start.max(reach)).max(0) as u64;
        reach = reach.max(end);
    }
    f
}

/// The block's rows. `first` is the entry that draws it; `live` is whether its turn is in flight.
#[allow(clippy::too_many_arguments)]
pub(super) fn block_lines(
    state: &State,
    first: usize,
    members: &[usize],
    live: bool,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
    marks: &mut Vec<Mark>,
    hits: &mut Vec<RowHit>,
) {
    let f = facts(state, members);
    let open = state.thought_open(first);
    let title = match preamble(&state.entries, first, members).and_then(|t| state.entries.get(t)) {
        Some(Entry::Prose { text }) => Some(title_words(text)),
        _ => None,
    };
    marks.push(Mark { entry: first, line: out.len() });
    out.push(match &title {
        Some(t) => titled_header(t, &f, open, width, theme),
        None => header(&f, open, live, width, theme),
    });
    if open {
        // A block of one thought and no call says its numbers in the header already.
        let solo = f.calls.is_empty() && members.len() == 1;
        narrative(state, members, solo, width, theme, out, hits);
    } else if !f.calls.is_empty() {
        // The latest call is one more row to click: it opens the block like its header.
        hits.push(RowHit { line: out.len(), x: 0, w: width as u16, target: Target::Thought(first) });
        out.push(latest_call(&f, width, theme));
    }
}

/// `▸ Gathering recent changes · 6.1s · 3 tools`: the model's own words in its speech colour, the
/// numbers dim. Live or settled it reads the same; the last call is under it when folded.
fn titled_header(title: &str, f: &Facts, open: bool, width: usize, theme: &Theme) -> Line<'static> {
    let marker = if open { chrome::DISCLOSURE_OPEN } else { chrome::DISCLOSURE_CLOSED };
    let mut stats = String::new();
    if f.ms >= 1000 {
        stats.push_str(&format!(" · {}", super::strip::took(f.ms)));
    }
    match f.calls.len() {
        0 => {}
        1 => stats.push_str(" · 1 tool"),
        n => stats.push_str(&format!(" · {n} tools")),
    }
    let room = width.saturating_sub(2 + text::width(&stats)).max(8);
    Line::from(vec![
        Span::styled(format!("{marker} "), ink(Ink::Dimmer, theme)),
        Span::styled(text::truncate(title, room), ink(Ink::Speech, theme)),
        Span::styled(stats, ink(Ink::Dim, theme)),
    ])
}

fn header(f: &Facts, open: bool, live: bool, width: usize, theme: &Theme) -> Line<'static> {
    let marker = if open { chrome::DISCLOSURE_OPEN } else { chrome::DISCLOSURE_CLOSED };
    let clock = if f.ms >= 1000 { Some(super::strip::took(f.ms)) } else { None };
    let tools = match f.calls.len() {
        0 => None,
        1 => Some("1 tool".to_string()),
        n => Some(format!("{n} tools")),
    };
    let tokens = (f.tokens > 0).then(|| format!("{} tokens", text::compact(f.tokens)));
    if !live {
        // Settled: `thought 12s · 3 tools`, or the tokens when no tool was used.
        let mut words = "thought".to_string();
        if let Some(c) = &clock {
            words.push_str(&format!(" {c}"));
        }
        match (tools, tokens) {
            (Some(t), _) => words.push_str(&format!(" · {t}")),
            // No tool: the header carries the reasoning's own numbers, as it always did.
            (None, Some(t)) => {
                words.push_str(&format!(" · {t}"));
                if f.ms >= 500 && f.tokens * 1000 / f.ms > 0 {
                    words.push_str(&format!(" · {} tok/s", f.tokens * 1000 / f.ms));
                }
            }
            (None, None) => {}
        }
        return Line::from(vec![Span::styled(format!("{marker} "), ink(Ink::Dimmer, theme)), Span::styled(words, ink(Ink::Dimmer, theme))]);
    }
    let mut spans = vec![Span::styled(format!("{marker} "), ink(Ink::Hover, theme)), Span::styled("thinking", ink(Ink::Body, theme))];
    for p in clock.into_iter().chain(tools).chain(tokens) {
        spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(p, ink(Ink::Dim, theme)));
    }
    // Collapsed, the end of the latest thought rides on the header, so it moves as the model writes.
    let room = width.saturating_sub(spans_width(&spans) + 2);
    if !open && !f.latest.is_empty() && room >= 8 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(text::truncate_left(&f.latest, room), ink(Ink::Dim, theme)));
    }
    Line::from(spans)
}

/// The most recent call (or the ones running together) under a collapsed header.
fn latest_call(f: &Facts, width: usize, theme: &Theme) -> Line<'static> {
    let lead = || Span::styled("  └ ", ink(Ink::Dimmer, theme));
    let running: Vec<&Call> = f.calls.iter().filter(|c| c.phase == Phase::Running).collect();
    if running.len() > 1 {
        // Each tool once, in the order they began, with a count when it runs more than once.
        let mut tools: Vec<(&str, usize)> = Vec::new();
        for c in &running {
            match tools.iter_mut().find(|t| t.0 == c.name) {
                Some(t) => t.1 += 1,
                None => tools.push((&c.name, 1)),
            }
        }
        let names = tools.iter().map(|(n, k)| if *k > 1 { format!("{n} ×{k}") } else { n.to_string() }).collect::<Vec<_>>().join(", ");
        let head = format!("{} running", running.len());
        let room = width.saturating_sub(4 + text::width(&head) + 3);
        return Line::from(vec![
            lead(),
            Span::styled(head, ink(Ink::Info, theme)),
            Span::styled(" · ", ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&names, room), ink(Ink::Dim, theme)),
        ]);
    }
    let Some(c) = f.calls.last() else { return Line::default() };
    let left = width.saturating_sub(4 + text::width(&c.name) + 3);
    let st = state_spans(c, left.saturating_sub(2 + text::width(&c.args).min(left / 2) + 8).max(12), theme);
    let room = left.saturating_sub(2 + spans_width(&st));
    let mut spans = vec![lead(), Span::styled(c.name.clone(), ink(Ink::Body, theme))];
    if !c.args.is_empty() && room >= 4 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(text::truncate(&c.args, room), ink(Ink::Dim, theme)));
    }
    spans.push(Span::raw("   "));
    spans.extend(st);
    Line::from(spans)
}

/// The turn in order: reasoning as it was written, each call on a row of its own.
fn narrative(state: &State, members: &[usize], solo: bool, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>, hits: &mut Vec<RowHit>) {
    let rule = format!("{} ", chrome::QUOTE_RULE);
    let body = width.saturating_sub(2);
    for &m in members {
        match state.entries.get(m) {
            Some(Entry::Thinking { text: t, tokens, live, ms, read }) => {
                let n = if *tokens > 0 { *tokens } else { (t.chars().count() as u64).div_ceil(4) };
                let mut stats = vec![format!("{} tok", text::thousands(n))];
                if *ms >= 1000 {
                    stats.insert(0, super::strip::took(*ms));
                }
                if *ms >= 500 && n * 1000 / ms > 0 {
                    stats.push(format!("{} tok/s", n * 1000 / ms));
                }
                if n > 0 && !*live && !solo {
                    out.push(Line::from(vec![
                        Span::styled(rule.clone(), ink(Ink::Dimmer, theme)),
                        Span::styled(stats.join(" · "), ink(Ink::Dimmer, theme)),
                    ]));
                }
                if let Some(r) = read {
                    let mut spans = vec![Span::styled(rule.clone(), ink(Ink::Dimmer, theme))];
                    spans.extend(super::strip::read_spans(r, theme));
                    out.push(Line::from(spans));
                }
                let rows: Vec<Line<'static>> = if markdown::has_maths(t) {
                    // Reasoning is shown as written, except that its maths is typeset.
                    markdown::render_text(t.trim(), body, theme, ink(Ink::Dim, theme), *live)
                } else {
                    text::wrap(&text::clean(t.trim()), body).into_iter().map(|l| Line::from(Span::styled(l, ink(Ink::Dim, theme)))).collect()
                };
                for l in rows {
                    let mut spans = vec![Span::styled(rule.clone(), ink(Ink::Dimmer, theme))];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
                }
            }
            Some(Entry::Tool { call_id, summary, status }) => {
                let c = call(state, call_id, summary, status);
                let marker = if matches!(c.phase, Phase::Failed(_)) { Ink::Red } else { Ink::Accent };
                let left = width.saturating_sub(2 + text::width(&c.name) + 3);
                let st = state_spans(&c, left.saturating_sub(2 + text::width(&c.args).min(left / 2) + 8).max(12), theme);
                let room = left.saturating_sub(2 + spans_width(&st));
                let mut spans = vec![Span::styled("› ", ink(marker, theme)), Span::styled(c.name.clone(), ink(Ink::Body, theme))];
                if !c.args.is_empty() && room >= 4 {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(text::truncate(&c.args, room), ink(Ink::Dim, theme)));
                }
                spans.push(Span::raw("   "));
                spans.extend(st);
                hits.push(RowHit { line: out.len(), x: 0, w: Line::from(spans.clone()).width().min(width) as u16, target: Target::Tool(c.id.to_string()) });
                out.push(Line::from(spans));
            }
            _ => {}
        }
    }
}

// ---- outside the block ----------------------------------------------------------------------------

/// `edited src/lib.rs +12 −3`, a row per file an edit call changed; a click opens the diff.
pub(super) fn edit_lines(state: &State, call_id: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>, hits: &mut Vec<RowHit>) {
    let Some(diff) = state.code.tool_detail.get(call_id).and_then(|d| d.diff.as_ref()) else { return };
    for f in &diff.files {
        let (add, del) = f.stats();
        let counts = format!(" +{add} −{del}");
        let path = text::truncate_left(&text::clean(&f.path), width.saturating_sub(7 + text::width(&counts)).max(8));
        let spans = vec![
            Span::styled("edited ", ink(Ink::Dim, theme)),
            Span::styled(path, ink(Ink::Body, theme)),
            Span::styled(format!(" +{add}"), ink(Ink::Green, theme)),
            Span::styled(format!(" −{del}"), ink(Ink::Red, theme)),
        ];
        let line = Line::from(spans);
        hits.push(RowHit { line: out.len(), x: 0, w: line.width().min(width) as u16, target: Target::Tool(call_id.to_string()) });
        out.push(line);
    }
}

/// A sent file as one row: its name in the accent, clickable, and its full path in parentheses at
/// the right, cut from the left to fit. `idx` is the entry; the row is a mark for the cursor.
pub(super) fn file_line(idx: usize, file: &marlowe_proto::SentFile, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>, marks: &mut Vec<Mark>, hits: &mut Vec<RowHit>) {
    let name = text::truncate(&text::clean(&file.name), width.saturating_sub(4).max(8));
    let name_w = text::width(&name);
    let mut spans = vec![Span::styled(name, ink(Ink::Accent, theme))];
    // Two columns between the name and the path, and the parentheses.
    let room = width.saturating_sub(name_w + 2);
    if room >= 8 {
        let path = format!("({})", text::truncate_left(&text::clean(&file.path), room - 2));
        spans.push(Span::raw(" ".repeat(width - name_w - text::width(&path))));
        spans.push(Span::styled(path, ink(Ink::Dim, theme)));
    }
    marks.push(Mark { entry: idx, line: out.len() });
    hits.push(RowHit { line: out.len(), x: 0, w: name_w as u16, target: Target::SentFile(idx) });
    out.push(Line::from(spans));
}

/// The file row under the Conversation box's cursor: the name lit (the keys are on the box's border).
pub(super) fn file_cursor(mut line: Line<'static>, theme: &Theme) -> Line<'static> {
    if let Some(name) = line.spans.first_mut() {
        name.style = ink(Ink::Hover, theme);
    }
    line
}
