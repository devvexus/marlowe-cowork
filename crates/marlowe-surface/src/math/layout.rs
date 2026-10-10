//! **A box-layout engine on a character grid.**
//!
//! Every node becomes a [`Grid`]: rows of cells with a baseline. A row of nodes is laid side by
//! side with baselines aligned; a fraction stacks numerator, a vinculum and denominator; a big
//! operator stacks its limits; a matrix is a grid of grids with brackets built to its height.
//!
//! Three modes. **Display** uses two dimensions. **Linear** is the one-line form inline maths needs
//! so a paragraph keeps its line height: `a/b`, `√(x + 1)`, `xᵢ²`. **Script** is linear without
//! the spacing around relations, as TeX sets sub- and superscripts.

use unicode_normalization::UnicodeNormalization;
use unicode_width::UnicodeWidthChar;

use super::parse::{Align, Class, MatKind, Node};
use super::symbols;

/// The fraction bar. Not `─` (U+2500): box drawing is reserved for harness borders by
/// [`crate::chrome`], and a model must not be able to draw one by writing `\frac`.
pub(crate) const VINCULUM: char = '⎯';

const NO_MARK: char = '\0';

/// One terminal cell: a glyph and up to two combining marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    pub c: char,
    pub marks: [char; 2],
}

impl Cell {
    const BLANK: Cell = Cell { c: ' ', marks: [NO_MARK; 2] };
    /// The right half of a double-width character. Emits nothing.
    const CONT: Cell = Cell { c: NO_MARK, marks: [NO_MARK; 2] };

    fn new(c: char) -> Cell {
        Cell { c, marks: [NO_MARK; 2] }
    }

    fn add_mark(&mut self, m: char) -> bool {
        if self.c == ' ' || self.c == NO_MARK {
            return false;
        }
        for slot in &mut self.marks {
            if *slot == NO_MARK {
                *slot = m;
                return true;
            }
        }
        false
    }

    fn marked(&self) -> bool {
        self.marks[0] != NO_MARK
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Grid {
    rows: Vec<Vec<Cell>>,
    base: usize,
}

impl Grid {
    fn empty() -> Grid {
        Grid { rows: vec![Vec::new()], base: 0 }
    }

    pub(crate) fn width(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }

    pub(crate) fn height(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn baseline(&self) -> usize {
        self.base
    }

    /// One row of text. A combining mark joins the cell before it; a wide character takes two.
    fn text(s: &str) -> Grid {
        let mut row: Vec<Cell> = Vec::new();
        for c in s.chars() {
            match c.width() {
                Some(0) => {
                    if let Some(last) = row.iter_mut().rev().find(|x| x.c != NO_MARK) {
                        last.add_mark(c);
                    }
                }
                Some(2) => {
                    row.push(Cell::new(c));
                    row.push(Cell::CONT);
                }
                Some(_) => row.push(Cell::new(c)),
                None => {}
            }
        }
        Grid { rows: vec![row], base: 0 }
    }

    fn blank(w: usize, h: usize, base: usize) -> Grid {
        Grid { rows: vec![vec![Cell::BLANK; w]; h.max(1)], base: base.min(h.max(1) - 1) }
    }

    fn above(&self) -> usize {
        self.base
    }

    fn below(&self) -> usize {
        self.height() - self.base - 1
    }

    fn padded(mut self, above: usize, below: usize) -> Grid {
        let w = self.width();
        let top = above.saturating_sub(self.above());
        let bot = below.saturating_sub(self.below());
        if top > 0 {
            let mut rows = vec![vec![Cell::BLANK; w]; top];
            rows.append(&mut self.rows);
            self.rows = rows;
        }
        for _ in 0..bot {
            self.rows.push(vec![Cell::BLANK; w]);
        }
        self.base = above;
        self
    }

    fn widened(mut self, w: usize, align: Align) -> Grid {
        let have = self.width();
        if have >= w {
            return self;
        }
        let extra = w - have;
        let left = match align {
            Align::Left => 0,
            Align::Right => extra,
            Align::Centre => extra / 2,
        };
        for row in &mut self.rows {
            let mut r = vec![Cell::BLANK; left];
            r.append(row);
            r.resize(w, Cell::BLANK);
            *row = r;
        }
        self
    }

    /// Side by side, baselines aligned.
    fn hcat(parts: Vec<Grid>) -> Grid {
        if parts.is_empty() {
            return Grid::empty();
        }
        let above = parts.iter().map(Grid::above).max().unwrap_or(0);
        let below = parts.iter().map(Grid::below).max().unwrap_or(0);
        let mut rows: Vec<Vec<Cell>> = vec![Vec::new(); above + below + 1];
        for p in parts {
            for (r, row) in p.padded(above, below).rows.into_iter().enumerate() {
                rows[r].extend(row);
            }
        }
        Grid { rows, base: above }
    }

    /// One above another, each aligned within the widest. `base` is the result's baseline row.
    fn vstack(parts: Vec<(Grid, Align)>, base: usize) -> Grid {
        let w = parts.iter().map(|(g, _)| g.width()).max().unwrap_or(0);
        let mut rows = Vec::new();
        for (g, a) in parts {
            rows.extend(g.widened(w, a).rows);
        }
        if rows.is_empty() {
            return Grid::empty();
        }
        let base = base.min(rows.len() - 1);
        Grid { rows, base }
    }

    fn blit(&mut self, src: &Grid, row: usize, col: usize) {
        for (r, line) in src.rows.iter().enumerate() {
            for (c, cell) in line.iter().enumerate() {
                if let Some(dst) = self.rows.get_mut(row + r).and_then(|x| x.get_mut(col + c)) {
                    *dst = *cell;
                }
            }
        }
    }

    fn line_string(row: &[Cell]) -> String {
        let mut s = String::with_capacity(row.len());
        for cell in row {
            if cell.c == NO_MARK {
                continue;
            }
            s.push(cell.c);
            for m in cell.marks {
                if m != NO_MARK {
                    s.push(m);
                }
            }
        }
        s.trim_end().to_string()
    }

    /// The rows as strings, trailing blanks trimmed.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.rows.iter().map(|r| Grid::line_string(r)).collect()
    }

    /// The first row as a string: the whole of a linear layout.
    pub(crate) fn line(&self) -> String {
        Grid::line_string(&self.rows[0]).trim_start().to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Display,
    Linear,
    Script,
}

impl Mode {
    fn flat(self) -> bool {
        self != Mode::Display
    }
}

pub(crate) fn lay(n: &Node, m: Mode) -> Grid {
    match n {
        Node::Atom { text, .. } => Grid::text(text),
        Node::Text(t) => Grid::text(t),
        Node::Row(items) => lay_row(items, m),
        Node::Space(k) => Grid::blank(*k, 1, 0),
        Node::Frac { num, den, bar } => lay_frac(num, den, *bar, m),
        Node::Sqrt { index, body } => lay_sqrt(index.as_deref(), body, m),
        Node::Scripts { base, sub, sup } => lay_scripts(base, sub.as_deref(), sup.as_deref(), m),
        Node::Op { body, .. } => lay_op(body, m),
        Node::Accent { name, body } => lay_accent(name, body, m),
        Node::Brace { over, body } => lay_brace(*over, body, m),
        Node::Stack { over, base, under } => lay_stack(over.as_deref(), base, under.as_deref(), m),
        Node::Arrow { glyph, over, under } => lay_arrow(*glyph, over.as_deref(), under.as_deref(), m),
        Node::Fenced { open, close, body, .. } => lay_fenced(*open, *close, body, m),
        Node::Middle(c) => Grid::text(&c.to_string()),
        Node::Matrix { kind, rows, align, vlines, hlines } => lay_matrix(*kind, rows, align, vlines, hlines, m, false),
        Node::Tag(t) => Grid::hcat(vec![Grid::blank(TAG_GAP, 1, 0), lay(t, m)]),
        Node::Boxed(b) => lay_boxed(b, m),
        Node::Phantom(b) => {
            let g = lay(b, m);
            Grid::blank(g.width(), g.height(), g.base)
        }
        Node::Classed { body, .. } => lay(body, m),
    }
}

/// One line per row of a top-level alignment, or one line for anything else. The fallback when a
/// display is wider than the pane, and the paraphrase's form.
pub(crate) fn linear_rows(n: &Node) -> Vec<String> {
    match n {
        Node::Matrix { kind: kind @ (MatKind::Aligned | MatKind::Gathered), rows, .. } => rows
            .iter()
            .map(|r| flat_row(*kind, r, &[], Mode::Linear).line())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![lay(n, Mode::Linear).line()],
    }
}

/// Whether a formula holds a matrix or array of two or more rows, anywhere: such a display is
/// clipped when too wide rather than set on one line.
pub(crate) fn has_matrix(n: &Node) -> bool {
    match n {
        Node::Matrix { kind: MatKind::Matrix, rows, .. } if rows.len() > 1 => true,
        Node::Matrix { rows, .. } => rows.iter().flatten().any(has_matrix),
        Node::Row(v) => v.iter().any(has_matrix),
        Node::Frac { num, den, .. } => has_matrix(num) || has_matrix(den),
        Node::Sqrt { body, .. }
        | Node::Op { body, .. }
        | Node::Accent { body, .. }
        | Node::Brace { body, .. }
        | Node::Fenced { body, .. }
        | Node::Classed { body, .. } => has_matrix(body),
        Node::Boxed(b) | Node::Phantom(b) | Node::Tag(b) => has_matrix(b),
        Node::Scripts { base, sub, sup } => {
            has_matrix(base) || sub.as_deref().is_some_and(has_matrix) || sup.as_deref().is_some_and(has_matrix)
        }
        Node::Stack { over, base, under } => {
            has_matrix(base) || over.as_deref().is_some_and(has_matrix) || under.as_deref().is_some_and(has_matrix)
        }
        _ => false,
    }
}

/// The spacing classes of a node as seen from its left and right.
fn classes(n: &Node) -> (Class, Class) {
    use Class::*;
    match n {
        Node::Atom { class, .. } | Node::Classed { class, .. } => (*class, *class),
        Node::Scripts { base, .. } | Node::Stack { base, .. } => classes(base),
        Node::Op { .. } => (Op, Op),
        Node::Frac { .. } | Node::Matrix { .. } => (Inner, Inner),
        Node::Fenced { .. } => (Open, Close),
        Node::Arrow { .. } | Node::Middle(_) => (Rel, Rel),
        _ => (Ord, Ord),
    }
}

fn atom_text(n: &Node) -> Option<&str> {
    match n {
        Node::Atom { text, .. } => Some(text),
        _ => None,
    }
}

struct Item<'a> {
    node: &'a Node,
    l: Class,
    r: Class,
    space_before: usize,
}

/// TeX's inter-atom spacing, reduced to a character grid: one column or none.
fn auto_space(a: Class, b: Class, m: Mode) -> usize {
    use Class::*;
    if m == Mode::Script {
        return 0;
    }
    match (a, b) {
        (Open, _) | (_, Close) => 0,
        (Rel, Rel) => 0,
        (Rel, Punct) => 0,
        (Rel, _) | (_, Rel) => 1,
        (Bin, _) | (_, Bin) => 1,
        (Punct, _) => 1,
        (Op, Ord | Op | Inner) => 1,
        (Ord | Close | Inner, Op) => 1,
        (Inner, Ord | Inner | Open) | (Ord | Close, Inner) => usize::from(m == Mode::Display),
        _ => 0,
    }
}

fn lay_row(items: &[Node], m: Mode) -> Grid {
    use Class::*;
    let mut its: Vec<Item> = Vec::new();
    let mut pending = 0usize;
    for n in items {
        match n {
            Node::Space(k) => pending += k,
            n if n.is_empty() => {}
            n => {
                let (l, r) = classes(n);
                its.push(Item { node: n, l, r, space_before: pending });
                pending = 0;
            }
        }
    }
    let trailing = pending;
    if its.is_empty() {
        return Grid::blank(trailing, 1, 0);
    }
    // A binary operator with no left operand is a sign: `-x`, `a = -b`, `(-1)`.
    let mut unary_at: Vec<usize> = Vec::new();
    for k in 0..its.len() {
        if its[k].l == Bin {
            let prev = k.checked_sub(1).map(|p| its[p].r);
            let next = its.get(k + 1).map(|x| x.l);
            let unary = matches!(prev, None | Some(Bin | Op | Rel | Open | Punct))
                || matches!(next, None | Some(Rel | Close | Punct));
            if unary {
                its[k].l = Ord;
                its[k].r = Ord;
                unary_at.push(k);
            }
        }
    }
    let mut grids: Vec<Grid> = its.iter().map(|it| lay(it.node, m)).collect();

    // Linear: `\frac{a}{b}x` is `(a/b)x`, not `a/bx`.
    if m.flat() {
        for k in 0..its.len() {
            if let Node::Frac { num, den, bar: true } = its[k].node {
                if vulgar(num, den).is_some() {
                    continue;
                }
                let touches = |c: Option<Class>| matches!(c, Some(Ord | Op | Open | Close | Inner));
                let prev = k.checked_sub(1).map(|p| its[p].r);
                let next = its.get(k + 1).map(|x| x.l);
                if touches(prev) || touches(next) {
                    let g = std::mem::replace(&mut grids[k], Grid::empty());
                    grids[k] = parens(g);
                }
            }
        }
    }

    // `\middle|` stretches to the tallest thing beside it.
    if m == Mode::Display && its.iter().any(|it| matches!(it.node, Node::Middle(_))) {
        let others = grids.iter().zip(&its).filter(|(_, it)| !matches!(it.node, Node::Middle(_)));
        let above = others.clone().map(|(g, _)| g.above()).max().unwrap_or(0);
        let below = others.map(|(g, _)| g.below()).max().unwrap_or(0);
        for (k, it) in its.iter().enumerate() {
            if let Node::Middle(c) = it.node {
                grids[k] = delimiter(*c, above + below + 1, above, true);
            }
        }
    }

    let heights: Vec<usize> = grids.iter().map(Grid::height).collect();
    let mut parts: Vec<Grid> = Vec::with_capacity(grids.len() * 2 + 1);
    for k in 0..its.len() {
        let gap = if k == 0 {
            its[0].space_before
        } else {
            let mut auto = auto_space(its[k - 1].r, its[k].l, m);
            // `1,000` is a number, not a list.
            if its[k - 1].r == Punct
                && atom_text(its[k - 1].node) == Some(",")
                && k >= 2
                && atom_text(its[k - 2].node).is_some_and(|t| t.ends_with(|c: char| c.is_ascii_digit()))
                && atom_text(its[k].node).is_some_and(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            {
                auto = 0;
            }
            // A sign hugs what it negates: `−∑`, not `− ∑`.
            if unary_at.contains(&(k - 1)) {
                auto = 0;
            }
            if m == Mode::Display && auto == 0 {
                let (prev_tall, next_tall) = (heights[k - 1] > 1, heights[k] > 1);
                // Two tall things never touch: `⎞⎛` reads as one bracket. After a tall bracket the
                // next symbol gets air, and so does an operator name before one: `softmax ⎛`.
                if (prev_tall && next_tall)
                    || (prev_tall
                        && matches!(its[k - 1].r, Close | Inner)
                        && matches!(its[k].l, Ord | Op | Inner | Open))
                    || (next_tall && its[k - 1].r == Op && its[k].l == Open)
                {
                    auto = 1;
                }
            }
            auto.max(its[k].space_before)
        };
        if gap > 0 {
            parts.push(Grid::blank(gap, 1, 0));
        }
        parts.push(std::mem::replace(&mut grids[k], Grid::empty()));
    }
    if trailing > 0 {
        parts.push(Grid::blank(trailing, 1, 0));
    }
    Grid::hcat(parts)
}

fn parens(g: Grid) -> Grid {
    Grid::hcat(vec![Grid::text("("), g, Grid::text(")")])
}

/// `½` for the fractions Unicode has a glyph for.
fn vulgar(num: &Node, den: &Node) -> Option<char> {
    let digit = |n: &Node| -> Option<String> {
        match n {
            Node::Atom { text, .. } => Some(text.clone()),
            Node::Row(v) if v.len() == 1 => match &v[0] {
                Node::Atom { text, .. } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        }
    };
    let (n, d) = (digit(num)?, digit(den)?);
    const VULGAR: [(&str, &str, char); 15] = [
        ("1", "2", '½'), ("1", "3", '⅓'), ("2", "3", '⅔'), ("1", "4", '¼'), ("3", "4", '¾'),
        ("1", "5", '⅕'), ("2", "5", '⅖'), ("3", "5", '⅗'), ("4", "5", '⅘'), ("1", "6", '⅙'),
        ("5", "6", '⅚'), ("1", "8", '⅛'), ("3", "8", '⅜'), ("5", "8", '⅝'), ("7", "8", '⅞'),
    ];
    VULGAR.iter().find(|(a, b, _)| *a == n && *b == d).map(|(_, _, g)| *g)
}

/// Whether a fraction part or a radicand needs parentheses in linear form: it has an operator,
/// a relation, punctuation or spacing at its top level, or is itself a fraction.
fn compound(n: &Node) -> bool {
    use Class::*;
    match n {
        Node::Row(v) => {
            let eff: Vec<&Node> = v.iter().filter(|x| !x.is_empty()).collect();
            if eff.len() == 1 {
                return compound(eff[0]);
            }
            eff.iter().enumerate().any(|(k, x)| {
                let (l, _) = classes(x);
                // A leading sign is not an operator: `-1` stays `-1`.
                (l == Bin && k > 0)
                    || matches!(l, Rel | Punct | Op)
                    || matches!(x, Node::Space(_) | Node::Frac { bar: true, .. })
                    || matches!(x, Node::Text(t) if t.contains(' '))
            })
        }
        Node::Frac { bar: true, .. } => true,
        Node::Text(t) => t.trim().contains(' '),
        _ => false,
    }
}

fn lay_frac(num: &Node, den: &Node, bar: bool, m: Mode) -> Grid {
    if m.flat() {
        if bar {
            if let Some(v) = vulgar(num, den) {
                return Grid::text(&v.to_string());
            }
        }
        let wrap = |n: &Node, den: bool| {
            let g = lay(n, m);
            // `n!/k!(n − k)!` reads as `(n!/k!)(n − k)!`; a denominator with a space in it is
            // bracketed even when it has no operator at the top.
            if compound(n) || (den && g.line().contains(' ')) {
                parens(g)
            } else {
                g
            }
        };
        let sep = if bar { "/" } else { ", " };
        return Grid::hcat(vec![wrap(num, false), Grid::text(sep), wrap(den, bar)]);
    }
    let n = lay(num, Mode::Display);
    let d = lay(den, Mode::Display);
    let w = n.width().max(d.width()).max(1);
    if bar {
        let nh = n.height();
        let rule = Grid::text(&VINCULUM.to_string().repeat(w));
        Grid::vstack(vec![(n, Align::Centre), (rule, Align::Centre), (d, Align::Centre)], nh)
    } else {
        let nh = n.height();
        Grid::vstack(vec![(n, Align::Centre), (d, Align::Centre)], nh.saturating_sub(1))
    }
}

/// Single atoms, scripted atoms and bracketed groups need no parentheses under a linear radical.
fn simple_radicand(n: &Node) -> bool {
    match n {
        Node::Row(v) => {
            let eff: Vec<&Node> = v.iter().filter(|x| !x.is_empty()).collect();
            eff.len() == 1 && simple_radicand(eff[0])
        }
        Node::Atom { .. } | Node::Fenced { .. } => true,
        Node::Scripts { base, .. } => matches!(**base, Node::Atom { .. }),
        _ => false,
    }
}

fn to_script(g: &Grid, sup: bool) -> Option<Grid> {
    if g.height() != 1 || g.width() == 0 {
        return None;
    }
    let mut s = String::new();
    for cell in &g.rows[0] {
        if cell.marked() || cell.c == NO_MARK {
            return None;
        }
        s.push(symbols::script_char(cell.c, sup)?);
    }
    Some(Grid::text(&s))
}

fn lay_sqrt(index: Option<&Node>, body: &Node, m: Mode) -> Grid {
    let idx = index.map(|i| lay(i, Mode::Script));
    let idx_text = idx.as_ref().map(Grid::line);
    let sign = match idx_text.as_deref() {
        None => Some("√".to_string()),
        Some("3") => Some("∛".to_string()),
        Some("4") => Some("∜".to_string()),
        Some(_) => idx.as_ref().and_then(|g| to_script(g, true)).map(|g| format!("{}√", g.line())),
    };
    let b = lay(body, m);
    if m.flat() {
        let inner = if simple_radicand(body) { b } else { parens(b) };
        return match sign {
            Some(s) => Grid::hcat(vec![Grid::text(&s), inner]),
            // An index Unicode cannot superscript: `x^(1/k)` says the same thing.
            None => Grid::hcat(vec![
                parens(inner),
                Grid::text(&format!("^(1/{})", idx_text.unwrap_or_default())),
            ]),
        };
    }
    let h = b.height();
    let w = b.width().max(1);
    let overline = "_".repeat(w);
    // `√2`, `√dₖ`: a single symbol needs no vinculum to show where the radicand ends, and the extra
    // row would make every denominator holding one a row taller.
    if h == 1 && simple_radicand(body) && index.is_none() {
        return Grid::hcat(vec![Grid::text("√"), b]);
    }
    if h == 1 {
        let (lead, top_left) = match &sign {
            Some(s) => (Grid::text(s), String::new()),
            // The index sits up and to the left of the radical sign, where a reader looks for it.
            None => (Grid::text("√"), idx_text.clone().unwrap_or_default()),
        };
        let lw = lead.width();
        let pad = Grid::text(&top_left).width();
        let mut g = Grid::blank(pad + lw + b.width(), 2, 1);
        g.blit(&Grid::text(&top_left), 0, 0);
        g.blit(&Grid::text(&overline), 0, pad + lw);
        g.blit(&lead, 1, pad);
        g.blit(&b, 1, pad + lw);
        return g;
    }
    // Tall radicand: a diagonal up the left side, the tick at the bottom.
    let iw = match &sign {
        Some(s) if s != "√" => Grid::text(s).width().saturating_sub(1),
        None => idx.as_ref().map_or(0, Grid::width),
        _ => 0,
    };
    let label = match &sign {
        Some(s) if s != "√" => s.trim_end_matches('√').to_string(),
        None => idx_text.unwrap_or_default(),
        _ => String::new(),
    };
    let mut g = Grid::blank(iw + h + b.width(), h + 1, 1 + b.base);
    g.blit(&Grid::text(&overline), 0, iw + h);
    for r in 1..=h {
        let glyph = if r == h { "√" } else { "/" };
        g.blit(&Grid::text(glyph), r, iw + h - r);
    }
    if !label.is_empty() {
        g.blit(&Grid::text(&label), h.saturating_sub(1).max(1), 0);
    }
    g.blit(&b, 1, iw + h);
    g
}

fn wants_limits(base: &Node) -> bool {
    match base {
        Node::Op { limits, .. } => *limits,
        Node::Brace { .. } => true,
        _ => false,
    }
}

/// `_θ` or `_(KL)`: explicit notation for a script with no Unicode form. Only a single symbol goes
/// bare: `D_KL` invites reading `K` as the subscript and `L` as what follows it.
fn explicit(mark: char, g: &Grid) -> Grid {
    let s = g.line();
    if s.chars().count() == 1 {
        Grid::text(&format!("{mark}{s}"))
    } else {
        Grid::text(&format!("{mark}({s})"))
    }
}

/// A limit set above or below a big operator in display: script-sized, except that a
/// `\substack` keeps its lines stacked, as LaTeX sets them.
fn lay_limit(n: &Node) -> Grid {
    let mut inner = n;
    while let Node::Row(v) = inner {
        let mut it = v.iter().filter(|x| !x.is_empty());
        match (it.next(), it.next()) {
            (Some(only), None) => inner = only,
            _ => break,
        }
    }
    match inner {
        Node::Matrix { kind: MatKind::Substack, rows, .. } => {
            let lines: Vec<(Grid, Align)> = rows
                .iter()
                .map(|r| (Grid::hcat(r.iter().map(|c| lay(c, Mode::Script)).collect()), Align::Centre))
                .collect();
            let base = lines.len().saturating_sub(1) / 2;
            Grid::vstack(lines, base)
        }
        _ => lay(n, Mode::Script),
    }
}

fn lay_scripts(base: &Node, sub: Option<&Node>, sup: Option<&Node>, m: Mode) -> Grid {
    let b = lay(base, m);
    if m == Mode::Display && wants_limits(base) {
        let sub_g = sub.map(lay_limit);
        let sup_g = sup.map(lay_limit);
        let mut parts = Vec::new();
        let mut row = b.base;
        if let Some(g) = sup_g {
            row += g.height();
            parts.push((g, Align::Centre));
        }
        parts.push((b, Align::Centre));
        if let Some(g) = sub_g {
            parts.push((g, Align::Centre));
        }
        return Grid::vstack(parts, row);
    }
    let sub_g = sub.map(|s| lay(s, Mode::Script));
    let sup_g = sup.map(|s| lay(s, Mode::Script));
    // A brace's label is not a subscript: `(1 + ⋯ + 1)_n`, not `1 + ⋯ + 1ₙ`.
    let labelled = matches!(base, Node::Brace { .. });
    let b = if labelled && m.flat() { parens(b) } else { b };
    let sup_u = sup_g.as_ref().filter(|_| !labelled).and_then(|g| to_script(g, true));
    let sub_u = sub_g.as_ref().filter(|_| !labelled).and_then(|g| to_script(g, false));
    let sub_ok = sub_g.is_none() || sub_u.is_some();
    let sup_ok = sup_g.is_none() || sup_u.is_some();
    if m.flat() || (b.height() == 1 && sub_ok && sup_ok) {
        // One row: true script glyphs first, explicit notation after, subscript before
        // superscript -- so `x_θ` with a square reads `x²_θ`, not the ambiguous `x_θ²`.
        let mut parts = vec![b];
        if let Some(u) = sub_u.clone() {
            parts.push(u);
        }
        if let Some(u) = sup_u.clone() {
            parts.push(u);
        }
        if let (Some(g), None) = (&sub_g, &sub_u) {
            parts.push(explicit('_', g));
        }
        if let (Some(g), None) = (&sup_g, &sup_u) {
            parts.push(explicit('^', g));
        }
        return Grid::hcat(parts);
    }
    let top_extra = usize::from(sup_g.is_some() && sup_u.is_none());
    let bot_extra = usize::from(sub_g.is_some() && sub_u.is_none());
    let sup_place = sup_u.or(sup_g);
    let sub_place = sub_u.or(sub_g);
    let sw = sup_place.as_ref().map_or(0, Grid::width).max(sub_place.as_ref().map_or(0, Grid::width));
    let h = top_extra + b.height() + bot_extra;
    let mut g = Grid::blank(b.width() + sw, h, top_extra + b.base);
    g.blit(&b, top_extra, 0);
    if let Some(s) = &sup_place {
        let row = if top_extra == 1 { 0 } else { top_extra };
        g.blit(s, row, b.width());
    }
    if let Some(s) = &sub_place {
        let row = if bot_extra == 1 { h - 1 } else { top_extra + b.height() - 1 };
        g.blit(s, row, b.width());
    }
    g
}

fn lay_op(body: &Node, m: Mode) -> Grid {
    if m == Mode::Display && matches!(body, Node::Atom { text, .. } if text == "∫") {
        // The two halves of the integral sign, which the terminal font draws to join.
        let mut g = Grid::blank(1, 2, 1);
        g.blit(&Grid::text("⌠"), 0, 0);
        g.blit(&Grid::text("⌡"), 1, 0);
        return g;
    }
    lay(body, m)
}

fn accent_mark(name: &str) -> Option<(char, bool)> {
    match name {
        "overrightarrow" => Some(('\u{20D7}', true)),
        "overleftarrow" => Some(('\u{20D6}', true)),
        "overleftrightarrow" => Some(('\u{20E1}', true)),
        n => symbols::accent(n),
    }
}

fn precompose(c: char, mark: char) -> Option<char> {
    let s: String = [c, mark].iter().collect::<String>().nfc().collect();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(p), None) if p.width() == Some(1) => Some(p),
        _ => None,
    }
}

fn lay_accent(name: &str, body: &Node, m: Mode) -> Grid {
    let mut b = lay(body, m);
    let Some((mark, above)) = accent_mark(name) else { return b };
    let single = b.width() == 1 && b.height() == 1 && b.rows[0][0].c != NO_MARK;
    if single {
        let cell = b.rows[0][0];
        if !cell.marked() {
            if let Some(p) = precompose(cell.c, mark) {
                return Grid::text(&p.to_string());
            }
        }
        if b.rows[0][0].add_mark(mark) {
            return b;
        }
    }
    let per_cell = m.flat() || matches!(name, "cancel" | "bcancel" | "xcancel" | "sout");
    if per_cell {
        for row in &mut b.rows {
            for cell in row.iter_mut() {
                cell.add_mark(mark);
            }
        }
        return b;
    }
    let row = Grid::text(&symbols::accent_row(name, b.width()));
    if above {
        let base = b.base + 1;
        Grid::vstack(vec![(row, Align::Left), (b, Align::Left)], base)
    } else {
        let base = b.base;
        Grid::vstack(vec![(b, Align::Left), (row, Align::Left)], base)
    }
}

fn lay_brace(over: bool, body: &Node, m: Mode) -> Grid {
    let b = lay(body, m);
    if m.flat() {
        return b;
    }
    let w = b.width().max(1);
    let row = if w == 1 {
        (if over { "‾" } else { "_" }).to_string()
    } else if over {
        format!("⎡{}⎤", "‾".repeat(w - 2))
    } else {
        format!("⎣{}⎦", "_".repeat(w - 2))
    };
    let row = Grid::text(&row);
    if over {
        let base = b.base + 1;
        Grid::vstack(vec![(row, Align::Left), (b, Align::Left)], base)
    } else {
        let base = b.base;
        Grid::vstack(vec![(b, Align::Left), (row, Align::Left)], base)
    }
}

fn flat_scripts(b: Grid, over: Option<Grid>, under: Option<Grid>) -> Grid {
    let mut parts = vec![b];
    if let Some(u) = &under {
        parts.push(to_script(u, false).unwrap_or_else(|| explicit('_', u)));
    }
    if let Some(o) = &over {
        parts.push(to_script(o, true).unwrap_or_else(|| explicit('^', o)));
    }
    Grid::hcat(parts)
}

fn lay_stack(over: Option<&Node>, base: &Node, under: Option<&Node>, m: Mode) -> Grid {
    let b = lay(base, m);
    let o = over.map(|n| lay(n, Mode::Script));
    let u = under.map(|n| lay(n, Mode::Script));
    if m.flat() {
        return flat_scripts(b, o, u);
    }
    let mut parts = Vec::new();
    let mut row = b.base;
    if let Some(g) = o {
        row += g.height();
        parts.push((g, Align::Centre));
    }
    parts.push((b, Align::Centre));
    if let Some(g) = u {
        parts.push((g, Align::Centre));
    }
    Grid::vstack(parts, row)
}

fn lay_arrow(glyph: char, over: Option<&Node>, under: Option<&Node>, m: Mode) -> Grid {
    let o = over.map(|n| lay(n, Mode::Script));
    let u = under.map(|n| lay(n, Mode::Script));
    if m.flat() {
        return flat_scripts(Grid::text(&glyph.to_string()), o, u);
    }
    let w = o.as_ref().map_or(0, Grid::width).max(u.as_ref().map_or(0, Grid::width)) + 2;
    let line = |c: char, n: usize| c.to_string().repeat(n);
    let shaft = match glyph {
        '←' => format!("←{}", line(VINCULUM, w - 1)),
        '↔' => format!("←{}→", line(VINCULUM, w - 2)),
        '⇒' => format!("{}⇒", line('=', w - 1)),
        '⇐' => format!("⇐{}", line('=', w - 1)),
        '⇔' => format!("⇐{}⇒", line('=', w - 2)),
        '↦' => format!("↦{}→", line(VINCULUM, w - 2)),
        '↪' => format!("↪{}→", line(VINCULUM, w - 2)),
        '=' => line('=', w),
        _ => format!("{}→", line(VINCULUM, w - 1)),
    };
    let mut parts = Vec::new();
    let mut row = 0;
    if let Some(g) = o {
        row = g.height();
        parts.push((g, Align::Centre));
    }
    parts.push((Grid::text(&shaft), Align::Centre));
    if let Some(g) = u {
        parts.push((g, Align::Centre));
    }
    Grid::vstack(parts, row)
}

/// A delimiter built to `h` rows from the bracket-piece glyphs, or a single glyph at height one.
fn delimiter(c: char, h: usize, base: usize, left: bool) -> Grid {
    if c == '.' {
        return Grid::blank(0, h, base);
    }
    if h <= 1 {
        return Grid::text(&c.to_string());
    }
    let pieces = |t: char, mid: char, b: char| -> Vec<char> {
        let mut v = vec![t];
        v.extend(std::iter::repeat_n(mid, h - 2));
        v.push(b);
        v
    };
    let col: Vec<char> = match c {
        '(' => pieces('⎛', '⎜', '⎝'),
        ')' => pieces('⎞', '⎟', '⎠'),
        '[' => pieces('⎡', '⎢', '⎣'),
        ']' => pieces('⎤', '⎥', '⎦'),
        '⌈' => pieces('⎡', '⎢', '⎢'),
        '⌉' => pieces('⎤', '⎥', '⎥'),
        '⌊' => pieces('⎢', '⎢', '⎣'),
        '⌋' => pieces('⎥', '⎥', '⎦'),
        '{' | '}' => {
            let is_left = c == '{';
            if h == 2 {
                if is_left {
                    vec!['⎰', '⎱']
                } else {
                    vec!['⎱', '⎰']
                }
            } else {
                let mid = if base > 0 && base < h - 1 { base } else { h / 2 };
                (0..h)
                    .map(|r| match (r, is_left) {
                        (0, true) => '⎧',
                        (0, false) => '⎫',
                        (r, true) if r == h - 1 => '⎩',
                        (r, false) if r == h - 1 => '⎭',
                        (r, true) if r == mid => '⎨',
                        (r, false) if r == mid => '⎬',
                        _ => '⎪',
                    })
                    .collect()
            }
        }
        '|' => vec!['⎪'; h],
        '‖' => vec!['‖'; h],
        '⟨' | '⟩' => {
            let (top, bot) = if c == '⟨' { ('⟋', '⟍') } else { ('⟍', '⟋') };
            (0..h)
                .map(|r| {
                    if h % 2 == 1 && r == h / 2 {
                        c
                    } else if r < h / 2 {
                        top
                    } else {
                        bot
                    }
                })
                .collect()
        }
        _ => (0..h).map(|r| if r == base { c } else { ' ' }).collect(),
    };
    let _ = left;
    let mut g = Grid::blank(1, h, base);
    for (r, ch) in col.into_iter().enumerate() {
        g.rows[r][0] = Cell::new(ch);
    }
    g
}

fn lay_fenced(open: char, close: char, body: &Node, m: Mode) -> Grid {
    let glyph = |c: char| if c == '.' { Grid::empty() } else { Grid::text(&c.to_string()) };
    if m.flat() {
        // `\binom{n}{k}` has no linear form of its own; `C(n, k)` is the one people write.
        if let Node::Frac { bar: false, num, den } = body {
            return Grid::hcat(vec![
                Grid::text("C("),
                lay(num, m),
                Grid::text(", "),
                lay(den, m),
                Grid::text(")"),
            ]);
        }
        let inner = match body {
            Node::Matrix { kind, rows, align, vlines, hlines } => lay_matrix(*kind, rows, align, vlines, hlines, m, true),
            b => lay(b, m),
        };
        return Grid::hcat(vec![glyph(open), inner, glyph(close)]);
    }
    let b = lay(body, Mode::Display);
    if b.height() == 1 {
        return Grid::hcat(vec![glyph(open), b, glyph(close)]);
    }
    let (h, base) = (b.height(), b.base);
    Grid::hcat(vec![delimiter(open, h, base, true), b, delimiter(close, h, base, false)])
}

/// Columns between a `\tag` and the widest row of its alignment.
const TAG_GAP: usize = 4;
/// The augmented matrix's rule and its crossing with a horizontal one: the same glyph as a `|`
/// delimiter built to height, so a `vmatrix` and an augmented matrix draw one kind of line.
const VRULE: char = '⎪';

/// A `\tag` at the top level of a cell's row, taken out so it can be set in a column of its own.
fn split_tag(cell: &Node) -> (Node, Option<Node>) {
    match cell {
        Node::Tag(t) => (Node::empty(), Some((**t).clone())),
        Node::Row(items) if items.iter().any(|n| matches!(n, Node::Tag(_))) => {
            let mut tag = None;
            let rest = items
                .iter()
                .filter(|n| match n {
                    Node::Tag(t) => {
                        tag = Some((**t).clone());
                        false
                    }
                    _ => true,
                })
                .cloned()
                .collect();
            (Node::Row(rest), tag)
        }
        other => (other.clone(), None),
    }
}

/// Every row's tag, if any row of an alignment or gather has one, and the rows without them.
fn split_tags(kind: MatKind, rows: &[Vec<Node>]) -> Option<(Vec<Vec<Node>>, Vec<Option<Node>>)> {
    if !matches!(kind, MatKind::Aligned | MatKind::Gathered) {
        return None;
    }
    let has = |n: &Node| match n {
        Node::Tag(_) => true,
        Node::Row(v) => v.iter().any(|x| matches!(x, Node::Tag(_))),
        _ => false,
    };
    if !rows.iter().flatten().any(has) {
        return None;
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut tags = Vec::with_capacity(rows.len());
    for r in rows {
        let mut tag = None;
        let cells = r
            .iter()
            .map(|c| {
                let (rest, t) = split_tag(c);
                tag = tag.take().or(t);
                rest
            })
            .collect();
        out.push(cells);
        tags.push(tag);
    }
    Some((out, tags))
}

fn lay_matrix(
    kind: MatKind,
    rows: &[Vec<Node>],
    align: &[Align],
    vlines: &[usize],
    hlines: &[usize],
    m: Mode,
    bracketed: bool,
) -> Grid {
    if m.flat() {
        let mut parts: Vec<Grid> = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            if r > 0 {
                parts.push(Grid::text("; "));
            }
            parts.push(Grid::text(&flat_row(kind, row, vlines, m).line()));
        }
        let g = Grid::hcat(parts);
        // A bare `matrix` has no brackets of its own, and a list of rows needs some.
        return if kind == MatKind::Matrix && !bracketed {
            Grid::hcat(vec![Grid::text("["), g, Grid::text("]")])
        } else {
            g
        };
    }
    let (untagged, tags) = match split_tags(kind, rows) {
        Some((r, t)) => (std::borrow::Cow::Owned(r), t),
        None => (std::borrow::Cow::Borrowed(rows), Vec::new()),
    };
    let cells: Vec<Vec<Grid>> =
        untagged.iter().map(|r| r.iter().map(|c| lay(c, Mode::Display)).collect()).collect();
    let ncols = cells.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for r in &cells {
        for (c, g) in r.iter().enumerate() {
            widths[c] = widths[c].max(g.width());
        }
    }
    // The columns before column `c`: a rule takes a column with a space either side.
    let gap = |c: usize| -> usize {
        if c == 0 {
            return 0;
        }
        if vlines.contains(&c) {
            return 3;
        }
        // `eqnarray`'s `a &=& b`: the relation is a column of its own, set one space either side.
        let eqnarray = kind == MatKind::Aligned && align.get(1) == Some(&Align::Centre);
        if eqnarray {
            // The relation's cell already leads with its space (`{}=`).
            return usize::from(c % 3 != 1);
        }
        match kind {
            MatKind::Aligned => {
                if c % 2 == 1 {
                    0
                } else {
                    3
                }
            }
            _ => 2,
        }
    };
    let mut rule_x: Vec<usize> = Vec::new();
    let mut x = 0;
    for c in 0..ncols {
        x += gap(c);
        if vlines.contains(&c) {
            rule_x.push(x - 2);
        }
        x += widths[c];
    }
    let tag_grids: Vec<Option<Grid>> = tags.iter().map(|t| t.as_ref().map(|t| lay(t, Mode::Display))).collect();
    let tag_w = tag_grids.iter().flatten().map(Grid::width).max().unwrap_or(0);
    let mut row_grids: Vec<Grid> = Vec::new();
    for (k, r) in cells.into_iter().enumerate() {
        let above = r.iter().map(Grid::above).max().unwrap_or(0);
        let below = r.iter().map(Grid::below).max().unwrap_or(0);
        let mut parts = Vec::new();
        let n = r.len();
        for (c, g) in r.into_iter().enumerate() {
            if gap(c) > 0 {
                parts.push(Grid::blank(gap(c), 1, 0));
            }
            let a = align.get(c).copied().unwrap_or(Align::Centre);
            parts.push(g.padded(above, below).widened(widths[c], a));
        }
        // A short row still reaches every column, so the tags line up.
        for c in n..ncols {
            parts.push(Grid::blank(gap(c) + widths[c], 1, 0));
        }
        if tag_w > 0 {
            parts.push(Grid::blank(TAG_GAP, 1, 0));
            let t = tag_grids.get(k).cloned().flatten().unwrap_or_else(|| Grid::blank(0, 1, 0));
            parts.push(t.widened(tag_w, Align::Right));
        }
        row_grids.push(Grid::hcat(parts));
    }
    let total_w = row_grids.iter().map(Grid::width).max().unwrap_or(0);
    let body_w = if tag_w > 0 { total_w - tag_w - TAG_GAP } else { total_w };
    let mut stacked: Vec<(Grid, Align)> = Vec::new();
    for (k, g) in row_grids.iter().enumerate() {
        if k > 0 && hlines.contains(&k) {
            let mut rule = Grid::text(&VINCULUM.to_string().repeat(body_w));
            for x in &rule_x {
                rule.rows[0][*x] = Cell::new(VRULE);
            }
            stacked.push((rule, Align::Left));
        } else if k > 0 && kind != MatKind::Substack && (g.height() > 2 || row_grids[k - 1].height() > 2) {
            // A blank row between rows where one holds a fraction or anything else three or more
            // rows tall, so two vinculums or a denominator and the next numerator never touch. A
            // row two tall is a dropped script (`f_y`), which is its own spacing.
            stacked.push((Grid::blank(total_w, 1, 0), Align::Left));
        }
        stacked.push((g.clone(), Align::Left));
    }
    let h: usize = stacked.iter().map(|(g, _)| g.height()).sum();
    let mut g = Grid::vstack(stacked, h.saturating_sub(1) / 2);
    // The augmented matrix's rules run the full height, through the gaps between tall rows.
    for x in &rule_x {
        for row in &mut g.rows {
            if let Some(cell) = row.get_mut(*x) {
                *cell = Cell::new(VRULE);
            }
        }
    }
    g
}

/// One row of a grid environment on one line: matrix cells comma-separated (a column rule is a
/// `|`), an alignment's cells run together (its `&` sits before the relation) with a gap between
/// column pairs.
fn flat_row(kind: MatKind, row: &[Node], vlines: &[usize], m: Mode) -> Grid {
    let mut parts: Vec<Grid> = Vec::new();
    for (c, cell) in row.iter().enumerate() {
        if c > 0 && !cell.is_empty() {
            let prev_has_comma = parts.last().is_some_and(|g| g.line().ends_with(','));
            let sep = match kind {
                MatKind::Matrix if vlines.contains(&c) => " | ",
                MatKind::Matrix | MatKind::Substack => ", ",
                MatKind::Cases if prev_has_comma => " ",
                MatKind::Cases => ", ",
                MatKind::Aligned if c % 2 == 0 => "  ",
                MatKind::Aligned | MatKind::Gathered => "",
            };
            if !sep.is_empty() {
                parts.push(Grid::text(sep));
            }
        }
        parts.push(lay(cell, m));
    }
    Grid::hcat(parts)
}

fn lay_boxed(body: &Node, m: Mode) -> Grid {
    let b = lay(body, m);
    if m.flat() {
        return Grid::hcat(vec![Grid::text("["), b, Grid::text("]")]);
    }
    let w = b.width() + 2;
    let h = b.height() + 2;
    let mut g = Grid::blank(w + 2, h, b.base + 1);
    g.blit(&Grid::text(&"_".repeat(w)), 0, 1);
    g.blit(&Grid::text(&"‾".repeat(w)), h - 1, 1);
    for r in 1..h - 1 {
        g.blit(&Grid::text("⎸"), r, 0);
        g.blit(&Grid::text("⎹"), r, w + 1);
    }
    g.blit(&b, 1, 2);
    g
}

#[cfg(test)]
pub(crate) fn layout_glyphs() -> Vec<char> {
    let mut v: Vec<char> = "⎛⎜⎝⎞⎟⎠⎡⎢⎣⎤⎥⎦⎧⎨⎩⎫⎬⎭⎪⎰⎱‖⟋⟍⌠⌡√∛∜_‾⎸⎹→←⇒⇐↦↪=ˆ˜˙¨ˇ˘´`˚…".chars().collect();
    v.push(VINCULUM);
    v
}
