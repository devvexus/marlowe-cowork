//! **The parsed formula as Typst math markup**, for the image path (`marlowe-typeset`).
//!
//! The image path reads the same tree as the grid, so a formula is typeset as an image exactly
//! when the strict parse (or its one repair) accepted it. Nothing here re-reads LaTeX.
//!
//! Every character that Typst's math syntax gives a meaning to is escaped, and text goes in a
//! string literal, so no model-written formula can reach Typst as code. A construct with no
//! faithful Typst form returns [`None`], and that formula stays on the character grid.

use super::layout::{self, Mode};
use super::parse::{Align, Class, MatKind, Node};

/// The body of a display equation (what goes between `$ ` and ` $`), or [`None`].
pub(crate) fn markup(n: &Node) -> Option<String> {
    let n = unwrap_row(n);
    if let Node::Matrix { kind: kind @ (MatKind::Aligned | MatKind::Gathered), rows, align, .. } = n {
        // A top-level alignment is the equation's own: `&` aligns and `\` breaks, as in LaTeX.
        // A `\tag` goes in a column of its own after the widest row, left-aligned.
        let aligned = *kind == MatKind::Aligned;
        // `eqnarray`'s three columns are one equation; `align`'s pairs are separate ones, which
        // LaTeX sets apart and Typst's alignment columns do not.
        let eqnarray = align.get(1) == Some(&Align::Centre);
        let width = rows.iter().map(Vec::len).max().unwrap_or(1);
        let tag_col = width.next_multiple_of(2) + 1;
        let mut lines = Vec::with_capacity(rows.len());
        for row in rows {
            let mut tag = None;
            let mut cells = Vec::with_capacity(row.len());
            for (k, c) in row.iter().enumerate() {
                let (rest, t) = untag(c);
                tag = tag.or(t);
                let s = emit(&rest)?;
                cells.push(if aligned && !eqnarray && k > 0 && k % 2 == 0 && !s.trim().is_empty() { format!("quad {s}") } else { s });
            }
            let mut line = cells.join(if aligned { " & " } else { " " });
            if let Some(t) = tag {
                let t = emit(&t)?;
                if aligned {
                    let amps = tag_col - (cells.len() - 1);
                    line.push_str(&format!("{} quad {t}", " &".repeat(amps)));
                } else {
                    line.push_str(&format!(" wide {t}"));
                }
            }
            lines.push(line);
        }
        return Some(lines.join(" \\\n"));
    }
    emit(n)
}

/// A cell without its `\tag`, and the tag.
fn untag(cell: &Node) -> (Node, Option<Node>) {
    match cell {
        Node::Tag(t) => (Node::Row(Vec::new()), Some((**t).clone())),
        Node::Row(items) => {
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

/// A row holding exactly one non-empty node is that node.
fn unwrap_row(n: &Node) -> &Node {
    if let Node::Row(v) = n {
        let mut it = v.iter().filter(|x| !x.is_empty());
        if let (Some(only), None) = (it.next(), it.next()) {
            return unwrap_row(only);
        }
    }
    n
}

/// Characters with a meaning in Typst math. Each is written escaped, as itself.
fn special(c: char) -> bool {
    matches!(
        c,
        '\\' | '(' | ')' | '[' | ']' | '{' | '}' | '/' | '^' | '_' | '&' | '#' | '$' | '"' | ','
            | ';' | ':' | '\'' | '*' | '~' | '@' | '|' | '<' | '>' | '=' | '-' | '+' | '!' | '?'
            | '.' | '`' | '%'
    )
}

fn atom(text: &str, class: Class) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let number = !text.is_empty() && text.chars().all(|c| c.is_ascii_digit() || c == '.');
    for c in text.chars() {
        match c {
            '′' => out.push('\''),
            '″' => out.push_str("''"),
            '‴' => out.push_str("'''"),
            // `3.14` is a number to Typst too; anywhere else a dot is escaped.
            '.' if number => out.push('.'),
            c if special(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    // An unpaired bar is spaced as a relation by the grid (`p(a | b)`); say so to Typst.
    if matches!(text, "|" | "‖") && class == Class::Rel {
        return format!("class(\"relation\", {out})");
    }
    // Ellipses are ordinary in TeX. Typst's Unicode class for `⋯` leaves a following `+` unary.
    if matches!(text, "⋯" | "…") {
        return format!("class(\"normal\", {out})");
    }
    out
}

fn string(t: &str) -> String {
    let mut out = String::with_capacity(t.len() + 2);
    out.push('"');
    for c in t.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn space(k: usize) -> &'static str {
    match k {
        0 => "",
        1 => "thin",
        2 | 3 => "quad",
        _ => "wide",
    }
}

fn class_name(c: Class) -> &'static str {
    match c {
        Class::Ord | Class::Inner => "normal",
        Class::Op => "large",
        Class::Bin => "binary",
        Class::Rel => "relation",
        Class::Open => "opening",
        Class::Close => "closing",
        Class::Punct => "punctuation",
    }
}

/// A delimiter as it is written inside `lr(…)`, or `""` for the null delimiter.
fn delim(c: char) -> String {
    if c == '.' {
        return String::new();
    }
    atom(&c.to_string(), Class::Ord)
}

/// A node that a script can attach to without grouping.
fn simple_base(n: &Node) -> bool {
    match n {
        Node::Atom { text, .. } => text.chars().count() <= 1 || text.chars().all(|c| c.is_ascii_digit()),
        Node::Text(_)
        | Node::Op { .. }
        | Node::Fenced { .. }
        | Node::Accent { .. }
        | Node::Frac { .. }
        | Node::Sqrt { .. } => true,
        Node::Row(v) => {
            let mut it = v.iter().filter(|x| !x.is_empty());
            matches!((it.next(), it.next()), (Some(only), None) if simple_base(only))
        }
        _ => false,
    }
}

fn is_primes(n: &Node) -> Option<usize> {
    match n {
        Node::Atom { text, .. } if !text.is_empty() => {
            let mut k = 0;
            for c in text.chars() {
                k += match c {
                    '′' => 1,
                    '″' => 2,
                    '‴' => 3,
                    '⁗' => 4,
                    _ => return None,
                };
            }
            Some(k)
        }
        _ => None,
    }
}

fn group(s: String) -> String {
    format!("({s})")
}

fn emit(n: &Node) -> Option<String> {
    Some(match n {
        Node::Atom { text, class } => atom(text, *class),
        Node::Text(t) if t.is_empty() => String::new(),
        Node::Text(t) => string(t),
        Node::Row(items) => {
            // Items are separated by a space, which Typst ignores between maths atoms but keeps
            // beside text: `"Var" (X)` would set a gap LaTeX does not. A string starts and ends
            // with a quote, so it can sit flush against anything without merging into a name.
            let mut out = String::new();
            let mut prev_text = true;
            for it in items {
                let s = match it {
                    Node::Space(k) => space(*k).to_string(),
                    other => emit(other)?,
                };
                if s.is_empty() {
                    continue;
                }
                let text = matches!(it, Node::Text(_));
                if !out.is_empty() && !prev_text && !text {
                    out.push(' ');
                }
                out.push_str(&s);
                prev_text = text;
            }
            out
        }
        Node::Space(k) => space(*k).to_string(),
        Node::Frac { num, den, bar: true } => format!("frac({}, {})", emit(num)?, emit(den)?),
        Node::Frac { num, den, bar: false } => {
            format!("mat(delim: #none, {}; {})", cell(num)?, cell(den)?)
        }
        Node::Sqrt { index: None, body } => format!("sqrt({})", emit(body)?),
        Node::Sqrt { index: Some(i), body } => format!("root({}, {})", emit(i)?, emit(body)?),
        Node::Scripts { base, sub, sup } => scripts(base, sub.as_deref(), sup.as_deref())?,
        Node::Op { body, limits } => op(body, *limits)?,
        Node::Accent { name, body } => accent(name, body)?,
        Node::Brace { over, body } => {
            format!("{}({})", if *over { "overbrace" } else { "underbrace" }, emit(body)?)
        }
        Node::Stack { over, base, under } => {
            let mut s = format!("limits({})", emit(base)?);
            if let Some(u) = under {
                s.push_str(&format!("_({})", emit(u)?));
            }
            if let Some(o) = over {
                s.push_str(&format!("^({})", emit(o)?));
            }
            s
        }
        Node::Arrow { glyph, over, under } => {
            let mut s = format!("stretch({})", atom(&glyph.to_string(), Class::Rel));
            if let Some(u) = under {
                s.push_str(&format!("_({})", emit(u)?));
            }
            if let Some(o) = over {
                s.push_str(&format!("^({})", emit(o)?));
            }
            s
        }
        Node::Fenced { open, close, body, auto } => fenced(*open, *close, body, *auto)?,
        Node::Middle(c) => format!("mid({})", delim(*c)),
        Node::Matrix { kind, rows, align, vlines, hlines } => matrix(*kind, rows, align, vlines, hlines, "#none")?,
        Node::Tag(t) => format!("wide {}", emit(t)?),
        Node::Boxed(b) => format!("#box(stroke: 0.05em + fg, inset: 0.25em, ${}$)", emit(b)?),
        Node::Phantom(b) => format!("#hide(${}$)", emit(b)?),
        Node::Classed { class, body } => format!("class(\"{}\", {})", class_name(*class), emit(body)?),
    })
}

/// A matrix cell: never empty, because `mat(a, , b)` is a syntax error.
fn cell(n: &Node) -> Option<String> {
    let s = emit(n)?;
    Some(if s.trim().is_empty() { "\"\"".to_string() } else { s })
}

fn scripts(base: &Node, sub: Option<&Node>, sup: Option<&Node>) -> Option<String> {
    // `\underbrace{…}_{label}` is Typst's `underbrace(…, label)`.
    if let Node::Brace { over, body } = unwrap_row(base) {
        let label = if *over { sup } else { sub };
        let other = if *over { sub } else { sup };
        if let (Some(l), None) = (label, other) {
            let f = if *over { "overbrace" } else { "underbrace" };
            return Some(format!("{f}({}, {})", emit(body)?, emit(l)?));
        }
    }
    let b = emit(base)?;
    // `{}_\nu`: an empty base still holds the scripts' place. `{T^{ab}}_c`: scripts on scripts
    // follow them, as an empty base after the first set does, rather than stacking under them.
    let nested = matches!(unwrap_row(base), Node::Scripts { .. });
    let b = if b.trim().is_empty() {
        "\"\"".to_string()
    } else if nested {
        format!("{b} \"\"")
    } else {
        b
    };
    // Primes are Typst's own `'`, which it places as LaTeX does.
    let (primes, sup) = match sup {
        Some(s) => match is_primes(s) {
            Some(k) => (k, None),
            None => match s {
                Node::Row(v) if v.len() == 2 && is_primes(&v[0]).is_some() => {
                    (is_primes(&v[0]).unwrap_or(0), Some(&v[1]))
                }
                _ => (0, Some(s)),
            },
        },
        None => (0, None),
    };
    let sub_s = match sub {
        Some(n) => Some(emit(n)?),
        None => None,
    };
    let sup_s = match sup {
        Some(n) => Some(emit(n)?),
        None => None,
    };
    if simple_base(base) || nested {
        let mut s = b;
        s.push_str(&"'".repeat(primes));
        if let Some(x) = sub_s {
            s.push_str(&format!("_{}", group(x)));
        }
        if let Some(x) = sup_s {
            s.push_str(&format!("^{}", group(x)));
        }
        return Some(s);
    }
    let mut args = vec![if primes > 0 { format!("{b}{}", "'".repeat(primes)) } else { b }];
    if let Some(x) = sub_s {
        args.push(format!("b: {x}"));
    }
    if let Some(x) = sup_s {
        args.push(format!("t: {x}"));
    }
    Some(format!("attach({})", args.join(", ")))
}

fn op(body: &Node, limits: bool) -> Option<String> {
    match unwrap_row(body) {
        Node::Atom { text, .. } => {
            let g = atom(text, Class::Op);
            // Integrals take their bounds beside the sign in LaTeX, which is Typst's default.
            if matches!(text.as_str(), "∫" | "∬" | "∭" | "∮" | "∯") {
                return Some(g);
            }
            Some(if limits { format!("limits({g})") } else { format!("scripts({g})") })
        }
        other => {
            // An operator name is upright text, whatever it was written with.
            let name = match other {
                Node::Text(t) => t.clone(),
                n => layout::lay(n, Mode::Linear).line(),
            };
            let lim = if limits { ", limits: #true" } else { "" };
            Some(format!("op({}{lim})", string(&name)))
        }
    }
}

fn accent(name: &str, body: &Node) -> Option<String> {
    let b = emit(body)?;
    let f = match name {
        "overline" => return Some(format!("overline({b})")),
        "underline" => return Some(format!("underline({b})")),
        "cancel" | "sout" => return Some(format!("cancel({b})")),
        "bcancel" => return Some(format!("cancel(inverted: #true, {b})")),
        "xcancel" => return Some(format!("cancel(cross: #true, {b})")),
        "overrightarrow" => "\u{20D7}",
        "overleftarrow" => "\u{20D6}",
        "overleftrightarrow" => "\u{20E1}",
        // `\bar` is a macron; LaTeX draws it short, over one letter.
        n => match super::symbols::accent(n) {
            Some((mark, true)) => return Some(format!("accent({b}, \"{mark}\")")),
            _ => return None,
        },
    };
    Some(format!("accent({b}, \"{f}\")"))
}

fn fenced(open: char, close: char, body: &Node, auto: bool) -> Option<String> {
    // Brackets written plainly do not grow in LaTeX; only `\left` and `\right` ones do.
    if auto {
        return Some(format!("{} {} {}", delim(open), emit(body)?, delim(close)).trim().to_string());
    }
    let inner = unwrap_row(body);
    match inner {
        Node::Frac { num, den, bar: false } if open == '(' && close == ')' => {
            return Some(format!("binom({}, {})", emit(num)?, emit(den)?));
        }
        Node::Matrix { kind: MatKind::Cases, rows, .. } => {
            let reverse = match (open, close) {
                ('{', '.') => false,
                ('.', '}') => true,
                _ => return None,
            };
            let mut lines = Vec::with_capacity(rows.len());
            for row in rows {
                let cells: Option<Vec<String>> = row.iter().map(emit).collect();
                lines.push(cells?.join(" & "));
            }
            let rev = if reverse { "reverse: #true, " } else { "" };
            return Some(format!("cases({rev}{})", lines.join(", ")));
        }
        Node::Matrix { kind: MatKind::Matrix, rows, align, vlines, hlines } => {
            let d = match (open, close) {
                ('(', ')') => Some("\"(\""),
                ('[', ']') => Some("\"[\""),
                ('{', '}') => Some("\"{\""),
                ('|', '|') => Some("\"|\""),
                ('‖', '‖') => Some("\"‖\""),
                _ => None,
            };
            if let Some(d) = d {
                return matrix(MatKind::Matrix, rows, align, vlines, hlines, d);
            }
        }
        _ => {}
    }
    let (o, c) = (delim(open), delim(close));
    let b = emit(body)?;
    Some(format!("lr({o} {b} {c})"))
}

fn matrix(
    kind: MatKind,
    rows: &[Vec<Node>],
    align: &[Align],
    vlines: &[usize],
    hlines: &[usize],
    d: &str,
) -> Option<String> {
    // A nested alignment: Typst's `cases` aligns its rows at `&` the way an equation does, and
    // with no delimiter it is `aligned`.
    if kind == MatKind::Aligned && rows.iter().any(|r| r.len() > 1) {
        let mut lines = Vec::with_capacity(rows.len());
        for row in rows {
            let cells: Option<Vec<String>> = row.iter().map(emit).collect();
            lines.push(cells?.join(" & "));
        }
        return Some(format!("cases(delim: #none, {})", lines.join(", ")));
    }
    let uniform = align.first().filter(|a| align.iter().all(|x| x == *a));
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(row.len());
        for (k, n) in row.iter().enumerate() {
            let c = cell(n)?;
            // `mat` has one alignment for every column. A column of its own (`{lcr}`) aligns at
            // an alignment point: before the cell for left, after it for right.
            cells.push(match (uniform, align.get(k)) {
                (None, Some(Align::Left)) => format!("& {c}"),
                (None, Some(Align::Right)) => format!("{c} &"),
                _ => c,
            });
        }
        lines.push(cells.join(", "));
    }
    let al = match uniform {
        Some(Align::Left) => "align: #left, ",
        Some(Align::Right) => "align: #right, ",
        _ => "",
    };
    let gap = if kind == MatKind::Substack { "row-gap: #0.1em, " } else { "" };
    let list = |v: &[usize]| v.iter().map(|x| format!("{x}, ")).collect::<String>();
    let augment = match (vlines.is_empty(), hlines.is_empty()) {
        (true, true) => String::new(),
        _ => format!("augment: #(vline: ({}), hline: ({})), ", list(vlines), list(hlines)),
    };
    Some(format!("mat(delim: {d}, {al}{gap}{augment}{})", lines.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::markup;

    fn t(src: &str) -> String {
        markup(&parse::strict(src).expect("parses")).expect("has a Typst form")
    }

    #[test]
    fn constructs_map_to_typst_functions() {
        assert_eq!(t(r"\frac{a}{b}"), "frac(a, b)");
        assert_eq!(t(r"\sqrt[3]{x}"), "root(3, x)");
        assert_eq!(t(r"x_i^2"), "x_(i)^(2)");
        assert_eq!(t(r"\sum_{i=1}^n i"), "limits(∑)_(i \\= 1)^(n) i");
        assert_eq!(t(r"\hat{y}"), "accent(y, \"\u{302}\")");
        assert_eq!(t(r"\mathrm{KL}"), "\"KL\"");
        assert_eq!(t(r"\binom{n}{k}"), "binom(n, k)");
        assert_eq!(t(r"\left( x \right)"), "lr(\\( x \\))");
        assert_eq!(t(r"f'(x)"), r"f' \( x \)");
        assert_eq!(t(r"\text{Var}(X)"), r#""Var"\( X \)"#);
    }

    #[test]
    fn environments_map_to_mat_cases_and_alignment() {
        assert_eq!(t(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}"), "mat(delim: \"(\", a, b; c, d)");
        assert_eq!(t(r"\begin{cases} 1 & x > 0 \\ 0 & x \le 0 \end{cases}"), "cases(1 & x \\> 0, 0 & x ≤ 0)");
        assert_eq!(t(r"\begin{aligned} a &= b \\ &= c \end{aligned}"), "a & \\= b \\\n & \\= c");
    }

    #[test]
    fn engineering_constructs_have_faithful_typst_forms() {
        // The augmented matrix's rule and a block matrix's horizontal rule.
        assert_eq!(
            t(r"\left[\begin{array}{cc|c} 1 & 2 & 3 \\ 4 & 5 & 6 \end{array}\right]"),
            "mat(delim: \"[\", augment: #(vline: (2, ), hline: ()), 1, 2, 3; 4, 5, 6)"
        );
        assert!(t(r"\begin{array}{c|c} A & B \\ \hline C & D \end{array}").contains("augment: #(vline: (1, ), hline: (1, ))"));
        // Per-column alignment at alignment points; a uniform one as `align`.
        assert_eq!(t(r"\begin{array}{lr} a & b \end{array}"), "mat(delim: #none, & a, b &)");
        assert_eq!(t(r"\begin{pmatrix*}[r] 1 & 2 \end{pmatrix*}"), "mat(delim: \"(\", align: #right, 1, 2)");
        assert_eq!(t(r"\begin{Vmatrix} a \\ b \end{Vmatrix}"), "mat(delim: \"‖\", a; b)");
        // A nested alignment is `cases` with no delimiter, which aligns at `&`.
        assert_eq!(
            t(r"\left\{ \begin{aligned} x &= 1 \\ y &= 2 \end{aligned} \right."),
            "lr(\\{ cases(delim: #none, x & \\= 1, y & \\= 2) )"
        );
        // Tags take a column of their own after the widest row.
        assert_eq!(t(r"\begin{align} a &= b \tag{1} \\ c &= d \end{align}"), "a & \\= b & & quad \"(\"\"1\"\")\" \\\nc & \\= d");
        // `align`'s pairs are set apart, as LaTeX sets them.
        assert_eq!(t(r"\begin{align} a &= b & c &= d \end{align}"), "a & \\= b & quad c & \\= d");
        // Mixed tensor indices stagger, with or without the empty group.
        assert_eq!(t(r"T^{\mu}{}_{\nu}"), "T^(μ) \"\"_(ν)");
        assert_eq!(t(r"{T^{a}}_{c}"), "T^(a) \"\"_(c)");
    }

    #[test]
    fn column_specs_give_alignments_and_rules() {
        use super::super::parse::{column_spec, Align::*};
        assert_eq!(column_spec("cc|c"), (vec![Centre, Centre, Centre], vec![2]));
        assert_eq!(column_spec("|l|r|"), (vec![Left, Right], vec![0, 1, 2]));
        assert_eq!(column_spec("@{}l*{3}{r}p{2cm}"), (vec![Left, Right, Right, Right, Left], vec![]));
        assert_eq!(column_spec("c||c"), (vec![Centre, Centre], vec![1]));
    }

    #[test]
    fn nothing_a_model_writes_reaches_typst_as_code() {
        // `#` would start code; inside text it is a string character, in maths it is escaped.
        let s = t(r#"\text{#import "x"} + \#"#);
        assert_eq!(s, "\"#import \\\"x\\\"\"\\+ \\#");
    }
}
