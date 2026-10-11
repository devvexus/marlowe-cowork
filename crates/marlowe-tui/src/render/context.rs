//! The context meter on the Model box and the "Context & hardware" planner popup. Glyphs and
//! foreground colours only: the bars are `━ ╍ ─ ┊`, never a fill.

use marlowe_proto::{ctx_label, ChangeWhen, ContextPlan, ContextPolicy, Engine, Fit, KvType, LayerRole, PlanModel};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{hit, hovered, ink, lit, put, put_row};
use crate::context::{Meter, PlanField};
use crate::state::{Overlay, State, Target};
use crate::text;

/// Where the compaction trigger sits on the meter.
const TRIGGER: f64 = 0.7;
const WARN: f64 = 0.9;

fn gib(mib: f64) -> String {
    format!("{:.1}", mib / 1024.0)
}

/// The ink of the used part of a bar at fraction `f` of the window.
fn used_ink(f: f64) -> Ink {
    if f >= WARN {
        Ink::Red
    } else if f >= TRIGGER {
        Ink::Amber
    } else {
        Ink::Accent
    }
}

/// `━━━━━━┊────`: used cells coloured by where they sit, the compaction point, the rest.
fn meter_bar(used: u64, window: u32, cells: usize, theme: &Theme) -> Vec<Span<'static>> {
    let frac = if window == 0 { 0.0 } else { (used as f64 / f64::from(window)).min(1.0) };
    let filled = (frac * cells as f64).round() as usize;
    let mark = (TRIGGER * cells as f64).round() as usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |c: &str, i: Ink| match spans.last_mut() {
        Some(last) if last.style == ink(i, theme) => last.content.to_mut().push_str(c),
        _ => spans.push(Span::styled(c.to_string(), ink(i, theme))),
    };
    for i in 0..cells {
        if i < filled {
            push("━", used_ink(i as f64 / cells as f64));
        } else if i == mark {
            push("┊", Ink::Dim);
        } else {
            push("─", Ink::Dimmer);
        }
    }
    spans
}

/// The meter row, fitted to `w` columns: the bar goes first, then the slot count, then the KV
/// type; the window and the warnings stay.
pub fn meter_line(m: &Meter, w: usize, theme: &Theme) -> Line<'static> {
    let numbers = vec![
        Span::styled(text::compact(m.used), theme.normal()),
        Span::styled(format!(" / {}", ctx_label(m.window)), ink(Ink::Dim, theme)),
    ];
    // (priority, full text, short text, ink): lower priority goes first when narrow.
    let mut notes: Vec<(u8, String, String, Ink)> = Vec::new();
    if let Some(k) = m.kv {
        let s = format!("kv {}", k.short());
        notes.push((1, s.clone(), s, Ink::Dim));
    }
    if let Some(n) = m.slots {
        let s = format!("{n} slot{}", if n == 1 { "" } else { "s" });
        notes.push((0, s.clone(), s, Ink::Dim));
    }
    if let Some((n, t)) = m.partial {
        notes.push((9, format!("{n}/{t} on GPU"), format!("{n}/{t} GPU"), Ink::Amber));
    }
    if let Some(p) = m.pending {
        notes.push((8, format!("{} pending", p.short()), format!("{} next", p.short()), Ink::Amber));
    }
    if m.cloud {
        notes.push((7, "cloud".into(), "cloud".into(), Ink::Dim));
    } else if !m.loaded {
        notes.push((6, "not loaded".into(), "idle".into(), Ink::Dimmer));
    }
    let nums: usize = numbers.iter().map(|s| text::width(&s.content)).sum();
    let width = |label: bool, bar: usize, notes: &[(u8, String, String, Ink)], short: bool| -> usize {
        let n: usize = notes.iter().map(|(_, f, s, _)| 3 + text::width(if short { s } else { f })).sum();
        usize::from(label) * 4 + if bar > 0 { bar + 1 } else { 0 } + nums + n
    };
    // The bar goes first, then the slot count, then the KV type; then short forms, then the label.
    const MIN_BAR: usize = 6;
    let mut bar = 0;
    if width(true, MIN_BAR, &notes, false) <= w {
        bar = (w - width(true, 0, &notes, false) - 1).min(24);
    }
    while bar == 0 && width(true, 0, &notes, false) > w {
        match notes.iter().enumerate().filter(|(_, n)| n.0 < 2).min_by_key(|(_, n)| n.0).map(|(i, _)| i) {
            Some(i) => {
                notes.remove(i);
            }
            None => break,
        }
    }
    let short = bar == 0 && width(true, 0, &notes, false) > w;
    let label = bar > 0 || width(true, 0, &notes, short) <= w;
    let mut spans = if label { vec![Span::styled("ctx ", ink(Ink::Dim, theme))] } else { Vec::new() };
    if bar > 0 {
        spans.extend(meter_bar(m.used, m.window, bar, theme));
        spans.push(Span::raw(" "));
    }
    spans.extend(numbers);
    for (_, full, brief, i) in notes {
        spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(if short { brief } else { full }, ink(i, theme)));
    }
    Line::from(spans)
}

/// Draws the meter on the Model box's second row and records it as a button.
pub fn draw_meter(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(m) = state.meter() else { return };
    let w = r.width.saturating_sub(2);
    let mut line = meter_line(&m, w as usize, theme);
    let t = Target::ContextMeter;
    hit(state, Rect { x: r.x, y: r.y, width: r.width, height: 1 }, t.clone());
    if hovered(state, &t) && state.overlay.is_none() {
        line = lit(line, theme);
    }
    put(buf, r.x + 1, r.y, &line, w);
}

// ---- the planner --------------------------------------------------------------------------------

fn panel(area: Rect, w: u16, h: u16, title: &str, theme: &Theme, buf: &mut Buffer) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(1));
    let r = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 3, width: w, height: h };
    Clear.render(r, buf);
    let border = ink(Ink::Accent, theme);
    Block::bordered().border_type(BorderType::Rounded).border_style(border).render(r, buf);
    put(buf, r.x + 2, r.y, &Line::from(vec![Span::raw(" "), Span::styled(title.to_string(), border), Span::raw(" ")]), w.saturating_sub(10));
    let esc = Line::from(Span::styled(" esc ", ink(Ink::Dim, theme)));
    put(buf, r.right().saturating_sub(7), r.y, &esc, 5);
    Rect { x: r.x + 2, y: r.y + 1, width: w.saturating_sub(4), height: h.saturating_sub(2) }
}

/// A line of labelled facts: `GPU    …`.
fn fact(label: &str, spans: Vec<Span<'static>>, theme: &Theme) -> Line<'static> {
    let mut v = vec![Span::styled(format!("{label:<7}"), ink(Ink::Accent, theme))];
    v.extend(spans);
    Line::from(v)
}

fn dim(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), ink(Ink::Dim, theme))
}

fn body(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), theme.normal())
}

fn sep(theme: &Theme) -> Span<'static> {
    Span::styled(" · ", ink(Ink::Dimmer, theme))
}

fn kib(b: u64) -> String {
    format!("{} KiB", (b as f64 / 1024.0).round() as u64)
}

/// The layer strip: `┃` keeps a KV cache, `╎` a sliding window, `·` recurrent state.
fn strip(layers: &[LayerRole], w: usize, theme: &Theme) -> Line<'static> {
    let main: Vec<LayerRole> = layers.iter().copied().filter(|r| !matches!(r, LayerRole::Mtp)).collect();
    let mtp = layers.len() - main.len();
    let step = main.len().div_ceil(w.saturating_sub(16).max(8)).max(1);
    let mut spans: Vec<Span<'static>> = vec![Span::raw("       ")];
    for chunk in main.chunks(step) {
        let role = if chunk.contains(&LayerRole::Attention) {
            LayerRole::Attention
        } else if chunk.contains(&LayerRole::SlidingWindow) {
            LayerRole::SlidingWindow
        } else {
            chunk[0]
        };
        let (g, i) = match role {
            LayerRole::Attention => ("┃", Ink::Accent),
            LayerRole::SlidingWindow => ("╎", Ink::Accent),
            LayerRole::Recurrent => ("·", Ink::Dim),
            _ => (" ", Ink::Dimmer),
        };
        match spans.last_mut() {
            Some(last) if last.style == ink(i, theme) => last.content.to_mut().push_str(g),
            _ => spans.push(Span::styled(g.to_string(), ink(i, theme))),
        }
    }
    if mtp > 0 {
        spans.push(dim(format!(" +{}", if mtp == 1 { "MTP".to_string() } else { format!("{mtp} MTP") }), theme));
    }
    Line::from(spans)
}

/// `one at 46k, two at 20k, or three at 11k`.
fn spread(m: &PlanModel, kv: KvType) -> String {
    let words = ["one", "two", "three", "four", "five", "six", "seven", "eight"];
    let mut parts = Vec::new();
    let mut last = u32::MAX;
    for n in 1..=8u16 {
        let c = m.max_ctx(kv, n, 1024).unwrap_or(0);
        if c < 4096 {
            break;
        }
        if c < last {
            parts.push(format!("{} at {}", words[n as usize - 1], ctx_label(c)));
            last = c;
        }
        if parts.len() == 3 {
            break;
        }
    }
    match parts.len() {
        0 => "not one agent at 4k".into(),
        1 => parts.remove(0),
        _ => {
            let tail = parts.pop().unwrap_or_default();
            format!("{}, or {tail}", parts.join(", "))
        }
    }
}

/// The usage bar for the policy being edited: fixed costs, agents, spare (or overflow).
fn usage_bar(m: &PlanModel, d: &ContextPolicy, cells: usize, theme: &Theme) -> Vec<Span<'static>> {
    let Some(budget) = m.budget_mib() else { return vec![dim("no GPU measured", theme)] };
    let fixed = m.fixed_mib(d.max_ctx);
    let agents = f64::from(d.parallel) * m.per_agent_mib(d.max_ctx, d.kv_type);
    let total = (fixed + agents).max(budget);
    let at = |mib: f64| ((mib / total) * cells as f64).round() as usize;
    let (f, a, b) = (at(fixed), at(fixed + agents), at(budget));
    let mut spans: Vec<Span<'static>> = Vec::new();
    for i in 0..cells {
        let (g, i_) = if i < f.min(b) {
            ("━", Ink::Body)
        } else if i < a.min(b) {
            ("╍", Ink::Accent)
        } else if i < b {
            ("─", Ink::Dimmer)
        } else {
            ("━", Ink::Red)
        };
        match spans.last_mut() {
            Some(last) if last.style == ink(i_, theme) => last.content.to_mut().push_str(g),
            _ => spans.push(Span::styled(g.to_string(), ink(i_, theme))),
        }
    }
    let need = fixed + agents;
    spans.push(dim(format!("  {} of {} GiB", gib(need), gib(budget)), theme));
    if need <= budget {
        spans.push(sep(theme));
        spans.push(dim(format!("{} spare", gib(budget - need)), theme));
    } else {
        spans.push(sep(theme));
        spans.push(Span::styled(format!("{} over", gib(need - budget)), ink(Ink::Red, theme)));
    }
    spans
}

/// The one sentence that says what this setting means.
fn verdict(m: &PlanModel, d: &ContextPolicy, theme: &Theme) -> (String, Style) {
    let Some(budget) = m.budget_mib() else {
        return ("No NVIDIA GPU was measured: this plan cannot check the setting.".into(), ink(Ink::Amber, theme));
    };
    let left = budget - m.fixed_mib(d.max_ctx);
    let kv = d.kv_type;
    match m.fit(d.max_ctx, kv, d.parallel) {
        Fit::Fits { spare_mib } => (
            format!(
                "Loads fully on the GPU with {} GiB for agents: {} ({}). This setting leaves {} GiB spare.",
                gib(left.max(0.0)),
                spread(m, kv),
                kv,
                gib(spare_mib)
            ),
            theme.normal(),
        ),
        Fit::Partial { gpu_layers, total_layers, slowdown } => (
            format!(
                "Over the budget: Ollama would keep {gpu_layers}/{total_layers} layers on the GPU and move the rest to the CPU, about {slowdown:.0}× slower writing. Nothing fails visibly; it is just slow."
            ),
            ink(Ink::Amber, theme),
        ),
        Fit::NoFit { gpu_layers, total_layers } => (
            format!(
                "Does not fit: only {gpu_layers}/{total_layers} layers would stay on the GPU. Lower the context, use q4_0, or run fewer agents."
            ),
            ink(Ink::Red, theme),
        ),
        Fit::Unknown => ("No GPU measurement.".into(), ink(Ink::Dim, theme)),
    }
}

/// A control: `label ◂ value ▸`, accent while focused, a button that focuses it.
fn control(label: &str, value: String, focused: bool, enabled: bool, theme: &Theme) -> Vec<Span<'static>> {
    let (l, v, a) = match (focused, enabled) {
        (_, false) => (ink(Ink::Dimmer, theme), ink(Ink::Dim, theme), ink(Ink::Dimmer, theme)),
        (true, _) => (ink(Ink::Accent, theme), ink(Ink::Accent, theme).add_modifier(Modifier::BOLD), ink(Ink::Accent, theme)),
        (false, _) => (ink(Ink::Dim, theme), theme.normal(), ink(Ink::Dimmer, theme)),
    };
    vec![
        Span::styled(format!("{label} "), l),
        Span::styled("◂ ", a),
        Span::styled(value, v),
        Span::styled(" ▸", a),
    ]
}

/// The planner popup.
pub fn planner(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(Overlay::ContextPlan { model, field, draft, review }) = &state.overlay else { return };
    let w = area.width.saturating_sub(4).min(104);
    let tw = w.saturating_sub(4) as usize;
    let title = format!("Context & hardware · {}", text::clean(&model.model_id));
    let plan = state.context.plan_for(model);
    let mut lines: Vec<(Line<'static>, Option<Target>)> = Vec::new();
    let push = |lines: &mut Vec<(Line<'static>, Option<Target>)>, l: Line<'static>| lines.push((l, None));
    match (plan, draft) {
        (Some(p), Some(d)) if *review => review_lines(state, p, d, tw, theme, &mut lines),
        (Some(p), Some(d)) => plan_lines(state, p, d, *field, tw, theme, &mut lines),
        _ => {
            let (msg, st) = match &state.context.error {
                Some(e) => (e.clone(), ink(Ink::Red, theme)),
                None if state.conn.is_online() => ("reading the model file, the runner log and the GPU…".to_string(), ink(Ink::Dim, theme)),
                None => ("offline — the planner needs the daemon".to_string(), ink(Ink::Amber, theme)),
            };
            for l in text::wrap(&msg, tw) {
                push(&mut lines, Line::from(Span::styled(l, st)));
            }
        }
    }
    let h = lines.len() as u16 + 2;
    super::overlay::scrim(area, theme, buf);
    let inner = panel(area, w, h, &title, theme, buf);
    for (i, (l, t)) in lines.into_iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            break;
        }
        match t {
            Some(t) => put_row(state, buf, (inner.x, y, inner.width), l, t, theme),
            None => put(buf, inner.x, y, &l, inner.width),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_lines(
    state: &State,
    p: &ContextPlan,
    d: &ContextPolicy,
    field: PlanField,
    tw: usize,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<Target>)>,
) {
    let push = |out: &mut Vec<(Line<'static>, Option<Target>)>, l: Line<'static>| out.push((l, None));
    let local = p.in_effect.engine == Engine::Ollama;
    let Some(m) = p.plan.as_ref().filter(|_| local) else {
        push(out, fact("Model", vec![body(format!("hosted · up to {} tokens of context", text::thousands(u64::from(p.ctx_limit))), theme)], theme));
        push(out, Line::from(dim("Nothing loads on this machine: the planner sets the window agents get, nothing else.", theme)));
        push(out, Line::from(""));
        let mut c = control("max context", ctx_label(d.max_ctx), true, true, theme);
        c.push(dim("   ↵ review", theme));
        out.push((Line::from(c), Some(Target::PlanField(PlanField::Ctx))));
        return;
    };
    // GPU.
    match &m.gpu {
        Some(g) => {
            let name = g.name.trim_start_matches("NVIDIA ").trim_start_matches("GeForce ").to_string();
            push(
                out,
                fact(
                    "GPU",
                    vec![
                        body(format!("{name} {} GiB", gib(g.total_mib)), theme),
                        sep(theme),
                        dim(format!("other apps {}", gib(g.others_mib())), theme),
                        sep(theme),
                        dim(format!("free for Marlowe {}", gib(g.free_for_marlowe_mib())), theme),
                        sep(theme),
                        dim(format!("keep {}", gib(g.headroom_mib)), theme),
                    ],
                    theme,
                ),
            );
        }
        None => push(out, fact("GPU", vec![Span::styled("no NVIDIA GPU measured (NVML unavailable)", ink(Ink::Amber, theme))], theme)),
    }
    // Load.
    let mut load = vec![body(format!("weights {}", gib(m.weights_main_mib)), theme)];
    let what: Vec<String> = [p.arch.quant.clone(), p.arch.params.map(|n| format!("{:.1}B", n as f64 / 1e9))].into_iter().flatten().collect();
    if !what.is_empty() {
        load.push(dim(format!(" ({})", what.join(" ")), theme));
    }
    if m.weights_draft_mib > 0.0 {
        load.push(sep(theme));
        load.push(dim(format!("draft MTP {}", gib(m.weights_draft_mib)), theme));
    }
    if m.vision_mib > 0.0 {
        load.push(sep(theme));
        load.push(dim(format!("vision ~{}", gib(m.vision_mib)), theme));
    }
    load.push(sep(theme));
    load.push(dim(format!("compute {}", gib(m.compute_mib(d.max_ctx))), theme));
    load.push(sep(theme));
    load.push(dim(format!("driver {}", gib(m.driver_mib)), theme));
    load.push(body(format!(" = {} fixed", gib(m.fixed_mib(d.max_ctx))), theme));
    if !m.calibrated {
        load.push(Span::styled("  (estimate until a load is measured)", ink(Ink::Dimmer, theme)));
    }
    push(out, fact("Load", load, theme));
    // Architecture.
    push(out, fact("Arch", vec![body(p.arch.summary.clone(), theme)], theme));
    if !p.arch.layers.is_empty() {
        push(out, strip(&p.arch.layers, tw, theme));
    }
    if m.state_b > 0 {
        let mib = m.state_b as f64 / 1048576.0;
        let kind = p.arch.state_kind.clone().unwrap_or_else(|| "recurrent".into());
        let mut st = vec![body(format!("{mib:.0} MiB per agent"), theme), dim(format!(", fixed at any context ({kind})"), theme)];
        if m.state_copies > 1 {
            st.push(dim(format!("; ×{} with the draft = {:.0} MiB", m.state_copies, mib * f64::from(m.state_copies)), theme));
        }
        push(out, fact("State", st, theme));
    }
    // KV per token.
    let mut kv = vec![dim("per token  ", theme)];
    for (i, t) in KvType::ALL.iter().enumerate() {
        if i > 0 {
            kv.push(sep(theme));
        }
        let st = if *t == d.kv_type { ink(Ink::Accent, theme) } else { theme.normal() };
        kv.push(Span::styled(format!("{} {}", t.ollama_name(), kib(m.kv_main_token_b[t.index()])), st));
    }
    if m.draft_kv_token_b > 0 {
        kv.push(dim(format!("  (+ draft {} f16)", kib(m.draft_kv_token_b)), theme));
    }
    if let Some(sw) = p.arch.sliding_window {
        kv.push(dim(format!("  · window {sw} on {} MiB/agent", m.swa_agent_b[d.kv_type.index()] / 1048576), theme));
    }
    push(out, fact("KV", kv, theme));
    // Host RAM.
    let mut ram = vec![dim(format!("pinned {}", gib(m.host_fixed_mib)), theme)];
    if m.checkpoint_mib > 0.0 {
        ram.push(sep(theme));
        ram.push(dim(format!("checkpoints ≤{}", gib(m.checkpoint_mib * 32.0 * f64::from(d.parallel))), theme));
    }
    ram.push(sep(theme));
    ram.push(dim("prompt cache ≤8.0", theme));
    if let (Some(t), Some(f)) = (p.ram_total_mib, p.ram_free_mib) {
        ram.push(sep(theme));
        let need = m.host_mib(d.parallel);
        let st = if need > f { ink(Ink::Amber, theme) } else { theme.normal() };
        ram.push(Span::styled(format!("{} of {} GiB free", gib(f), gib(t)), st));
    }
    push(out, fact("RAM", ram, theme));
    push(out, Line::from(""));

    // The table.
    let kvi = d.kv_type.index();
    push(
        out,
        Line::from(Span::styled(
            format!("  {:>8}   {:<18}{:>5}{:>6}{:>6}   {}", "context", format!("per agent ({})", d.kv_type), "f16", "q8_0", "q4_0", "← agents that fit"),
            ink(Ink::Dimmer, theme),
        )),
    );
    let rows = state.plan_rows();
    for (i, ctx) in rows.iter().copied().enumerate() {
        if ctx > p.ctx_limit {
            continue;
        }
        let sel = ctx == d.max_ctx;
        let marker = if sel { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let cst = if sel { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
        let mut spans = vec![
            marker,
            Span::styled(format!("{:>8}", ctx_label(ctx)), cst),
            dim(format!("   {:<18}", format!("{} GiB", gib(m.per_agent_mib(ctx, d.kv_type)))), theme),
        ];
        let agents = KvType::ALL.map(|t| m.agents(ctx, t).unwrap_or(0));
        let note = if agents[kvi] == 0 {
            match m.fit(ctx, d.kv_type, 1) {
                Fit::Partial { gpu_layers, total_layers, slowdown } => Some((
                    format!("partial offload · {gpu_layers}/{total_layers} on GPU · ~{slowdown:.0}× slower"),
                    Ink::Amber,
                )),
                Fit::NoFit { .. } => Some(("does not fit".to_string(), Ink::Red)),
                _ => None,
            }
        } else {
            None
        };
        for (j, n) in agents.iter().enumerate() {
            let s = if *n == 0 { "–".to_string() } else { n.to_string() };
            let w = if j == 0 { 5 } else { 6 };
            let st = match (j == kvi, *n == 0) {
                (true, false) => ink(Ink::Accent, theme),
                (_, true) => ink(Ink::Dimmer, theme),
                _ => theme.normal(),
            };
            spans.push(Span::styled(format!("{s:>w$}"), st));
        }
        if let Some((n, i_)) = note {
            spans.push(Span::styled(format!("   {n}"), ink(i_, theme)));
        }
        out.push((Line::from(spans), Some(Target::PlanCell(i, None))));
    }
    let single = |m: &PlanModel, t: KvType| m.max_ctx(t, 1, 1024).filter(|c| *c >= 1024).map_or("–".to_string(), ctx_label);
    let mut best = vec![dim(
        format!("  max one agent: {} ({}) · {} (q4_0)", single(m, d.kv_type), d.kv_type, single(m, KvType::Q4_0)),
        theme,
    )];
    if m.has_draft {
        let nd = m.without_draft();
        best.push(dim(format!(" · without the draft: {} · {}", single(&nd, d.kv_type), single(&nd, KvType::Q4_0)), theme));
    }
    push(out, Line::from(best));
    push(out, Line::from(""));

    // The controls.
    let mut c = control("max context", ctx_label(d.max_ctx), field == PlanField::Ctx, true, theme);
    c.push(Span::raw("   "));
    c.extend(control("kv", d.kv_type.ollama_name().to_string(), field == PlanField::Kv, true, theme));
    c.push(Span::raw("   "));
    c.extend(control("agents at once", d.parallel.to_string(), field == PlanField::Parallel, true, theme));
    c.push(dim(" (you + crew)", theme));
    if p.auto.is_some() {
        c.push(Span::styled("   a", ink(Ink::Accent, theme)));
        c.push(dim(" auto", theme));
    }
    out.push((Line::from(c), Some(Target::PlanField(field.next()))));
    let bar_w = tw.saturating_sub(34).clamp(10, 48);
    push(out, Line::from(usage_bar(m, d, bar_w, theme)));
    let (v, st) = verdict(m, d, theme);
    for l in text::wrap(&v, tw) {
        push(out, Line::from(Span::styled(l, st)));
    }
    for wn in &p.warnings {
        push(out, Line::from(Span::styled(text::truncate(&format!("! {}", text::clean(wn)), tw), ink(Ink::Amber, theme))));
    }
    if d.parallel > 1 || p.in_effect.parallel.unwrap_or(1) > 1 {
        push(out, Line::from(dim("Each agent gets its own slot of max context; agents may ask for less, never more.", theme)));
    }
    push(out, Line::from(""));
    out.push((
        Line::from(vec![
            Span::styled("↵", ink(Ink::Accent, theme)),
            dim(" review changes", theme),
            Span::styled("   ←→ adjust · ↑↓ context · tab field · esc close", ink(Ink::Dimmer, theme)),
        ]),
        Some(Target::Button(0)),
    ));
}

fn review_lines(state: &State, p: &ContextPlan, d: &ContextPolicy, tw: usize, theme: &Theme, out: &mut Vec<(Line<'static>, Option<Target>)>) {
    let push = |out: &mut Vec<(Line<'static>, Option<Target>)>, l: Line<'static>| out.push((l, None));
    push(out, Line::from(vec![body(format!("Saving {} for {}:", marlowe_proto::policy_label(d), p.model.model_id), theme)]));
    push(out, Line::from(""));
    let changes = state.context_review();
    let restart = changes.iter().any(|c| c.when == ChangeWhen::Restart);
    let room = tw.saturating_sub(13).max(20);
    for c in &changes {
        let (mark, label, st) = match c.when {
            ChangeWhen::Now => ("✓ ", "now      ", ink(Ink::Green, theme)),
            ChangeWhen::Restart => ("! ", "restart  ", ink(Ink::Amber, theme)),
        };
        for (k, l) in text::wrap(&text::clean(&c.text), room).into_iter().enumerate() {
            let lead = if k == 0 {
                vec![Span::raw("  "), Span::styled(mark, st), Span::styled(label, st)]
            } else {
                vec![Span::raw(" ".repeat(13))]
            };
            push(out, Line::from([lead, vec![body(l, theme)]].concat()));
        }
        if let Some(det) = &c.detail {
            for l in text::wrap(&text::clean(det), room) {
                push(out, Line::from(vec![Span::raw(" ".repeat(13)), dim(l, theme)]));
            }
        }
    }
    if restart {
        push(out, Line::from(""));
        for l in text::wrap("Restarting Ollama always asks first (allow or deny). Until it restarts, the meter shows the change as pending.", tw) {
            push(out, Line::from(dim(l, theme)));
        }
    }
    push(out, Line::from(""));
    let key = |k: &str| Span::styled(k.to_string(), ink(Ink::Accent, theme));
    if restart {
        out.push((Line::from(vec![key("s"), dim(" save (server change later)", theme)]), Some(Target::Button(0))));
        out.push((Line::from(vec![key("r"), dim(" save and restart Ollama…", theme)]), Some(Target::Button(1))));
    } else {
        out.push((Line::from(vec![key("s"), dim(" save", theme)]), Some(Target::Button(0))));
    }
    out.push((Line::from(vec![key("esc"), dim(" back", theme)]), Some(Target::Button(2))));
}
