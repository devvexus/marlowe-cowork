//! **One deterministic repair pass**, for the slips models actually make.
//!
//! Each fix is mechanical and named, and the pass runs once: a formula that is still malformed
//! afterwards goes to the paraphrase rather than to a second guess. The fixes, in order:
//!
//! | slip | fix |
//! |---|---|
//! | a stray `$` inside the span | removed |
//! | a trailing `\`, `^` or `_` with nothing after it | removed |
//! | an unmatched `}` | removed |
//! | an unclosed `{` | closed at the end |
//! | an `\end{x}` with no `\begin{x}` | removed |
//! | a `\begin{x}` with no `\end{x}` | closed at the end, innermost first |
//! | a `\right` with no `\left` | becomes a plain delimiter |
//! | a `\left` with no `\right` | closed at the end with `\right.` |

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `\name` or `\c`, as written (with the backslash).
    Cmd(String),
    Open,
    Close,
    Char(char),
}

fn lex(src: &str) -> Vec<Tok> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' => {
                let mut j = i + 1;
                if j < cs.len() && cs[j].is_ascii_alphabetic() {
                    while j < cs.len() && cs[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                } else if j < cs.len() {
                    j += 1;
                }
                out.push(Tok::Cmd(cs[i..j].iter().collect()));
                i = j;
            }
            '{' => {
                out.push(Tok::Open);
                i += 1;
            }
            '}' => {
                out.push(Tok::Close);
                i += 1;
            }
            _ => {
                out.push(Tok::Char(c));
                i += 1;
            }
        }
    }
    out
}

fn unlex(toks: &[Tok]) -> String {
    let mut out = String::new();
    for t in toks {
        match t {
            Tok::Cmd(s) => out.push_str(s),
            Tok::Open => out.push('{'),
            Tok::Close => out.push('}'),
            Tok::Char(c) => out.push(*c),
        }
    }
    out
}

/// The name in `\begin{name}` / `\end{name}` starting at `toks[i]` (the command), if well-formed.
fn env_name(toks: &[Tok], i: usize) -> Option<(String, usize)> {
    if toks.get(i + 1) != Some(&Tok::Open) {
        return None;
    }
    let mut name = String::new();
    let mut j = i + 2;
    while let Some(Tok::Char(c)) = toks.get(j) {
        name.push(*c);
        j += 1;
    }
    if toks.get(j) == Some(&Tok::Close) {
        Some((name, j + 1))
    } else {
        None
    }
}

pub(crate) fn repair(src: &str) -> String {
    let mut toks = lex(src);

    // A stray `$`. An escaped `\$` is a `Cmd` and survives.
    toks.retain(|t| *t != Tok::Char('$'));

    // A trailing backslash, or a script with nothing to attach.
    while let Some(last) = toks.last() {
        let trailing_ws = matches!(last, Tok::Char(c) if c.is_whitespace());
        let dangling = matches!(last, Tok::Char('^' | '_'))
            || matches!(last, Tok::Cmd(c) if c == "\\" || c == "\\\\");
        if trailing_ws || dangling {
            toks.pop();
        } else {
            break;
        }
    }

    // Braces: drop an unmatched `}`, close an unclosed `{`.
    let mut depth = 0usize;
    let mut kept = Vec::with_capacity(toks.len());
    for t in toks {
        match t {
            Tok::Open => {
                depth += 1;
                kept.push(t);
            }
            Tok::Close if depth == 0 => {}
            Tok::Close => {
                depth -= 1;
                kept.push(t);
            }
            _ => kept.push(t),
        }
    }
    for _ in 0..depth {
        kept.push(Tok::Close);
    }
    let mut toks = kept;

    // Environments: drop an `\end` with no `\begin`, close a `\begin` left open.
    let mut stack: Vec<String> = Vec::new();
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Cmd(c) if c == "\\begin" => {
                if let Some((name, next)) = env_name(&toks, i) {
                    stack.push(name);
                    out.extend(toks[i..next].iter().cloned());
                    i = next;
                    continue;
                }
            }
            Tok::Cmd(c) if c == "\\end" => {
                if let Some((name, next)) = env_name(&toks, i) {
                    if let Some(pos) = stack.iter().rposition(|n| *n == name) {
                        // Close anything opened inside it first, innermost first.
                        while stack.len() > pos + 1 {
                            let inner = stack.pop().expect("len checked");
                            out.extend(lex(&format!("\\end{{{inner}}}")));
                        }
                        stack.pop();
                        out.extend(toks[i..next].iter().cloned());
                    }
                    i = next;
                    continue;
                }
            }
            _ => {}
        }
        out.push(toks[i].clone());
        i += 1;
    }
    while let Some(name) = stack.pop() {
        out.extend(lex(&format!("\\end{{{name}}}")));
    }
    toks = out;

    // `\left` / `\right`, counted per brace group so `{\left(}` does not borrow an outer `\right`.
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut lefts: Vec<usize> = vec![0];
    for t in toks {
        match &t {
            Tok::Open => {
                lefts.push(0);
                out.push(t);
            }
            Tok::Close => {
                let open = lefts.pop().unwrap_or(0);
                for _ in 0..open {
                    out.extend(lex("\\right."));
                }
                if lefts.is_empty() {
                    lefts.push(0);
                }
                out.push(t);
            }
            Tok::Cmd(c) if c == "\\left" => {
                *lefts.last_mut().expect("never empty") += 1;
                out.push(t);
            }
            Tok::Cmd(c) if c == "\\right" => {
                let top = lefts.last_mut().expect("never empty");
                if *top > 0 {
                    *top -= 1;
                    out.push(t);
                }
                // Unmatched: drop the command; the delimiter after it stays as a plain character.
            }
            _ => out.push(t),
        }
    }
    for _ in 0..lefts.iter().sum::<usize>() {
        out.extend(lex("\\right."));
    }
    unlex(&out)
}

#[cfg(test)]
mod tests {
    use super::repair;

    #[test]
    fn each_named_slip_is_fixed() {
        for (bad, good) in [
            (r"\frac{a}{b", r"\frac{a}{b}"),
            (r"a}+b", r"a+b"),
            (r"x = 1$", r"x = 1"),
            (r"\left( x", r"\left( x\right."),
            (r"x \right)", r"x )"),
            (r"\begin{aligned} a &= b", r"\begin{aligned} a &= b\end{aligned}"),
            (r"a \end{cases}", r"a "),
            (r"x^", r"x"),
            (r"x \\", r"x"),
        ] {
            assert_eq!(repair(bad), good, "repairing {bad:?}");
        }
    }

    #[test]
    fn a_well_formed_formula_is_untouched() {
        for s in [r"\frac{a}{b}", r"\left( \frac{1}{2} \right)", r"\begin{pmatrix} a \\ b \end{pmatrix}", r"\$5"] {
            assert_eq!(repair(s), s);
        }
    }
}
