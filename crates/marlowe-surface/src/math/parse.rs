//! LaTeX maths to a tree. Two modes, one grammar.
//!
//! **Strict** is the authority on "does this parse": an unknown command, an unbalanced brace, a
//! `\left` with no `\right`, a stray `$` -- each is an error, and an error means the caller tries
//! the repair pass and then the paraphrase. **Lenient** never fails: it is the paraphrase's parser,
//! and it reads an unknown `\foo{x}` as the upright word `foo(x)` rather than as source.

use super::symbols::{self, Font};

/// TeX's atom classes, which are what decide spacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Ord,
    Op,
    Bin,
    Rel,
    Open,
    Close,
    Punct,
    Inner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Centre,
    Right,
}

/// What kind of grid an environment is, which decides its gaps and row spacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatKind {
    Matrix,
    Cases,
    Aligned,
    Gathered,
    Substack,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Atom { text: String, class: Class },
    /// Upright text from `\text{…}`: spaces in it are the author's and are kept.
    Text(String),
    Row(Vec<Node>),
    Space(usize),
    Frac { num: Box<Node>, den: Box<Node>, bar: bool },
    Sqrt { index: Option<Box<Node>>, body: Box<Node> },
    Scripts { base: Box<Node>, sub: Option<Box<Node>>, sup: Option<Box<Node>> },
    /// `\sum`, `\lim`, `\operatorname{…}`. `limits` puts scripts above and below in display.
    Op { body: Box<Node>, limits: bool },
    Accent { name: String, body: Box<Node> },
    Brace { over: bool, body: Box<Node> },
    Stack { over: Option<Box<Node>>, base: Box<Node>, under: Option<Box<Node>> },
    Arrow { glyph: char, over: Option<Box<Node>>, under: Option<Box<Node>> },
    /// `\left( … \right)`, or a bracket pair matched in the source (`auto`). Scales to its body.
    Fenced { open: char, close: char, body: Box<Node>, auto: bool },
    Middle(char),
    /// An environment's grid. `vlines` are the columns a vertical rule follows (`{cc|c}` is
    /// `[2]`), `hlines` the rows a horizontal rule precedes (`\\ \hline`), interior ones only.
    Matrix { kind: MatKind, rows: Vec<Vec<Node>>, align: Vec<Align>, vlines: Vec<usize>, hlines: Vec<usize> },
    /// `\tag{1}`: the equation's label, set at the right of its row.
    Tag(Box<Node>),
    Boxed(Box<Node>),
    Phantom(Box<Node>),
    Classed { class: Class, body: Box<Node> },
}

impl Node {
    pub(crate) fn empty() -> Node {
        Node::Row(Vec::new())
    }

    fn atom(text: impl Into<String>, class: Class) -> Node {
        Node::Atom { text: text.into(), class }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Node::Row(v) => v.iter().all(Node::is_empty),
            Node::Space(0) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError(pub &'static str);

type PResult<T> = Result<T, ParseError>;

/// Nesting deeper than this is not maths a person wrote, and a recursive parser has to stop
/// somewhere short of the stack.
const MAX_DEPTH: usize = 40;

/// Strict: `Ok` only for input that is well-formed LaTeX in the subset this module knows.
pub(crate) fn strict(src: &str) -> PResult<Node> {
    Parser::new(src, true).top()
}

/// Lenient: always a tree. The final fallback strips the syntax characters and keeps the words.
pub(crate) fn lenient(src: &str) -> Node {
    match Parser::new(src, false).top() {
        Ok(n) => n,
        Err(_) => Node::Text(strip_syntax(src)),
    }
}

/// The last-resort paraphrase: the words and symbols with every syntax character removed.
pub(crate) fn strip_syntax(src: &str) -> String {
    let mut out = String::new();
    for c in src.chars() {
        match c {
            '\\' | '{' | '}' | '$' | '&' | '#' | '%' | '^' | '_' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How a call to [`Parser::row`] ended.
#[derive(Debug, Clone, PartialEq)]
enum Term {
    End,
    Brace,
    Bracket,
    Amp,
    Newline,
    Right(char),
    EndEnv(String),
}

/// Which terminator a [`Parser::cells`] call is waiting for.
#[derive(Debug, Clone, PartialEq)]
enum Closer {
    Eof,
    Brace,
    Env(String),
}

struct Parser {
    s: Vec<char>,
    i: usize,
    strict: bool,
    depth: usize,
}

impl Parser {
    fn new(src: &str, strict: bool) -> Self {
        Parser { s: src.chars().collect(), i: 0, strict, depth: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn err<T>(&self, msg: &'static str) -> PResult<T> {
        Err(ParseError(msg))
    }

    /// Whitespace, and `%` comments to the end of the line.
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.i += 1;
            } else if c == '%' {
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == '\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("nested too deeply");
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// The command name at the cursor without consuming it: letters, or one other character.
    fn peek_command(&self) -> (String, usize) {
        debug_assert_eq!(self.peek(), Some('\\'));
        let mut j = self.i + 1;
        match self.s.get(j) {
            Some(c) if c.is_ascii_alphabetic() => {
                while matches!(self.s.get(j), Some(c) if c.is_ascii_alphabetic()) {
                    j += 1;
                }
                (self.s[self.i + 1..j].iter().collect(), j - self.i)
            }
            Some(c) => (c.to_string(), 2),
            None => (String::new(), 1),
        }
    }

    fn top(&mut self) -> PResult<Node> {
        let (rows, _, _) = self.cells(Closer::Eof, true)?;
        Ok(table_or_row(rows, MatKind::Gathered))
    }

    /// `\hline` and its booktabs cousins at the start of a row. Whether there was one.
    fn take_rules(&mut self) -> bool {
        let mut found = false;
        loop {
            self.skip_ws();
            if self.peek() != Some('\\') {
                return found;
            }
            let (name, len) = self.peek_command();
            if !matches!(name.as_str(), "hline" | "hdashline" | "midrule" | "toprule" | "bottomrule") {
                return found;
            }
            self.i += len;
            found = true;
        }
    }

    /// Cells separated by `&` and rows by `\\`, up to `closer`, and the rows a `\hline` precedes.
    fn cells(&mut self, closer: Closer, allow_table: bool) -> PResult<(Vec<Vec<Node>>, Term, Vec<usize>)> {
        let mut rows: Vec<Vec<Node>> = vec![Vec::new()];
        let mut cell: Vec<Node> = Vec::new();
        let mut hlines: Vec<usize> = Vec::new();
        loop {
            let at_row_start = cell.is_empty() && rows.last().is_some_and(Vec::is_empty);
            if allow_table && at_row_start && self.take_rules() && hlines.last() != Some(&(rows.len() - 1)) {
                hlines.push(rows.len() - 1);
            }
            let (items, term) = self.row(false)?;
            cell.extend(items);
            let done = match (&term, &closer) {
                (Term::Amp, _) | (Term::Newline, _) if !allow_table && self.strict => {
                    return self.err("alignment outside an environment");
                }
                (Term::Amp, _) => {
                    rows.last_mut().expect("never empty").push(Node::Row(std::mem::take(&mut cell)));
                    false
                }
                (Term::Newline, _) => {
                    rows.last_mut().expect("never empty").push(Node::Row(std::mem::take(&mut cell)));
                    rows.push(Vec::new());
                    false
                }
                (Term::End, Closer::Eof) => true,
                (Term::Brace, Closer::Brace) => true,
                (Term::EndEnv(name), Closer::Env(want)) => {
                    if self.strict && name != want {
                        return self.err("\\end does not match \\begin");
                    }
                    true
                }
                (Term::End, _) => {
                    if self.strict {
                        return self.err("unclosed group or environment");
                    }
                    true
                }
                _ => {
                    if self.strict {
                        return self.err("unexpected terminator");
                    }
                    // Lenient: a stray `}`, `\right` or `\end` is dropped and the cell goes on.
                    false
                }
            };
            if done {
                rows.last_mut().expect("never empty").push(Node::Row(cell));
                // A trailing `\\` before `\end` leaves an empty last row; LaTeX draws nothing there.
                if rows.len() > 1 && rows.last().is_some_and(|r| r.iter().all(Node::is_empty)) {
                    rows.pop();
                }
                // Only rules between rows are drawn: a frame above the first or below the last is
                // the bracket's job.
                let n = rows.len();
                hlines.retain(|r| *r > 0 && *r < n);
                return Ok((rows, term, hlines));
            }
        }
    }

    /// One run of atoms, up to a terminator.
    fn row(&mut self, bracket: bool) -> PResult<(Vec<Node>, Term)> {
        self.enter()?;
        let mut items: Vec<Node> = Vec::new();
        // Openers awaiting a closer: (index in `items`, glyph, is a bar).
        let mut opens: Vec<(usize, char, bool)> = Vec::new();
        // `{a \over b}`: the numerator so far, and whether it is a binomial.
        let mut infix: Option<(Vec<Node>, bool)> = None;
        let term = loop {
            self.skip_ws();
            let Some(c) = self.peek() else { break Term::End };
            match c {
                '}' => {
                    self.i += 1;
                    break Term::Brace;
                }
                ']' if bracket => {
                    self.i += 1;
                    break Term::Bracket;
                }
                '&' => {
                    self.i += 1;
                    break Term::Amp;
                }
                '^' | '_' | '\'' => {
                    opens.retain(|(i, _, _)| *i + 1 < items.len());
                    // `26.85\,^{\circ}`: a space is not a base; the scripts get an empty one.
                    let base = match items.pop() {
                        Some(Node::Space(k)) => {
                            items.push(Node::Space(k));
                            Node::atom("", Class::Ord)
                        }
                        Some(n) => n,
                        None => Node::atom("", Class::Ord),
                    };
                    let node = self.scripts(base)?;
                    items.push(node);
                }
                '\\' => {
                    let (name, len) = self.peek_command();
                    match name.as_str() {
                        "\\" => {
                            self.i += len;
                            self.skip_optional_bracket();
                            break Term::Newline;
                        }
                        "cr" => {
                            self.i += len;
                            break Term::Newline;
                        }
                        "right" => {
                            self.i += len;
                            let d = self.delimiter()?;
                            break Term::Right(d);
                        }
                        "end" => {
                            self.i += len;
                            let env = self.braced_name()?;
                            break Term::EndEnv(env);
                        }
                        "over" | "choose" | "atop" => {
                            self.i += len;
                            infix = Some((std::mem::take(&mut items), name == "choose"));
                            opens.clear();
                        }
                        "limits" | "nolimits" | "displaylimits" => {
                            self.i += len;
                            if let Some(Node::Op { limits, .. }) = items.last_mut() {
                                *limits = name != "nolimits";
                            }
                        }
                        _ => {
                            let node = self.command()?;
                            push(&mut items, &mut opens, node);
                        }
                    }
                }
                _ => {
                    let node = self.char_atom(true)?;
                    self.skip_ws();
                    if c == '{' && node.is_empty() && matches!(self.peek(), Some('^' | '_')) {
                        // `T^{\mu}{}_{\nu}`: an empty group holds the place of the scripts after it.
                        items.push(Node::atom("", Class::Ord));
                    } else {
                        push(&mut items, &mut opens, node);
                    }
                }
            }
        };
        self.leave();
        // A bar that found no partner is a relation in practice: `p(a | b)`, `D(P \| Q)`. Spaced
        // as one, it reads; set tight as TeX does, `D(P‖Q)` is hard to scan in a monospace grid.
        for (idx, _, bar) in opens {
            if bar {
                if let Some(Node::Atom { class, .. }) = items.get_mut(idx) {
                    *class = Class::Rel;
                }
            }
        }
        if let Some((num, binom)) = infix {
            let frac = Node::Frac {
                num: Box::new(Node::Row(num)),
                den: Box::new(Node::Row(items)),
                bar: !binom,
            };
            let node = if binom {
                Node::Fenced { open: '(', close: ')', body: Box::new(frac), auto: false }
            } else {
                frac
            };
            return Ok((vec![node], term));
        }
        Ok((items, term))
    }

    /// `^`, `_` and primes following a base, in any order, attached to it.
    fn scripts(&mut self, base: Node) -> PResult<Node> {
        let mut sub: Option<Node> = None;
        let mut sup: Option<Node> = None;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('\'') => {
                    let mut n = 0;
                    while self.peek() == Some('\'') {
                        self.i += 1;
                        n += 1;
                    }
                    let primes = match n {
                        1 => "′",
                        2 => "″",
                        3 => "‴",
                        _ => "⁗",
                    };
                    sup = Some(match sup.take() {
                        None => Node::atom(primes, Class::Ord),
                        Some(prev) => Node::Row(vec![prev, Node::atom(primes, Class::Ord)]),
                    });
                }
                Some(c @ ('^' | '_')) => {
                    self.i += 1;
                    let arg = self.arg()?;
                    let slot = if c == '^' { &mut sup } else { &mut sub };
                    match slot.take() {
                        None => *slot = Some(arg),
                        // `x^2^3` is an error in TeX. A prime followed by `^` is not: `f'^2`.
                        Some(prev) => {
                            let only_primes = matches!(&prev, Node::Atom { text, .. }
                                if text.chars().all(|c| matches!(c, '′' | '″' | '‴' | '⁗')));
                            if self.strict && !(c == '^' && only_primes) {
                                return self.err("double script");
                            }
                            *slot = Some(Node::Row(vec![prev, arg]));
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(Node::Scripts { base: Box::new(base), sub: sub.map(Box::new), sup: sup.map(Box::new) })
    }

    /// A single argument: a braced group, one command, or one character.
    fn arg(&mut self) -> PResult<Node> {
        self.skip_ws();
        match self.peek() {
            Some('{') => {
                self.i += 1;
                self.group()
            }
            Some('\\') => {
                let (name, _) = self.peek_command();
                if matches!(name.as_str(), "\\" | "right" | "end" | "over" | "choose") {
                    if self.strict {
                        return self.err("missing argument");
                    }
                    return Ok(Node::empty());
                }
                self.command()
            }
            Some('}' | '&' | '^' | '_') | None => {
                if self.strict {
                    return self.err("missing argument");
                }
                Ok(Node::empty())
            }
            Some(_) => self.char_atom(false),
        }
    }

    /// The inside of `{…}`, the opening brace already consumed.
    fn group(&mut self) -> PResult<Node> {
        self.enter()?;
        let (rows, _, _) = self.cells(Closer::Brace, false)?;
        self.leave();
        Ok(table_or_row(rows, MatKind::Gathered))
    }

    /// `[…]` after `\sqrt` or `\\`.
    fn optional_bracket(&mut self) -> PResult<Option<Node>> {
        self.skip_ws();
        if self.peek() != Some('[') {
            return Ok(None);
        }
        self.i += 1;
        let (items, term) = self.row(true)?;
        if term != Term::Bracket && self.strict {
            return self.err("unclosed [");
        }
        Ok(Some(Node::Row(items)))
    }

    fn skip_optional_bracket(&mut self) {
        let save = self.i;
        while matches!(self.peek(), Some(' ')) {
            self.i += 1;
        }
        if self.peek() == Some('[') {
            if let Some(n) = self.s[self.i..].iter().position(|c| *c == ']') {
                if n < 16 {
                    self.i += n + 1;
                    return;
                }
            }
        }
        self.i = save;
    }

    /// `{name}` for environments and colour names: the raw characters.
    fn braced_name(&mut self) -> PResult<String> {
        self.skip_ws();
        if self.peek() != Some('{') {
            if self.strict {
                return self.err("expected {");
            }
            return Ok(String::new());
        }
        self.i += 1;
        let mut out = String::new();
        let mut depth = 1;
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out.trim().to_string());
                    }
                }
                _ => {}
            }
            out.push(c);
        }
        if self.strict {
            return self.err("unclosed {");
        }
        Ok(out.trim().to_string())
    }

    /// The literal text of `\text{…}`, with escapes and nested maths handled.
    fn text_arg(&mut self, font: Font) -> PResult<Node> {
        self.skip_ws();
        if self.peek() != Some('{') {
            // `\text x` takes one character.
            let Some(c) = self.peek() else {
                return if self.strict { self.err("missing argument") } else { Ok(Node::empty()) };
            };
            self.i += 1;
            return Ok(Node::Text(map_font(&c.to_string(), font)));
        }
        self.i += 1;
        self.enter()?;
        let mut parts: Vec<Node> = Vec::new();
        let mut text = String::new();
        let mut depth = 1;
        let mut closed = false;
        while let Some(c) = self.peek() {
            match c {
                '{' => {
                    depth += 1;
                    self.i += 1;
                }
                '}' => {
                    self.i += 1;
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                '$' => {
                    // Maths inside text: `\text{for all $x$}`.
                    self.i += 1;
                    let start = self.i;
                    while matches!(self.peek(), Some(c) if c != '$' && c != '}') {
                        self.i += 1;
                    }
                    let inner: String = self.s[start..self.i].iter().collect();
                    if self.peek() == Some('$') {
                        self.i += 1;
                    } else if self.strict {
                        return self.err("unclosed $ in text");
                    }
                    if !text.is_empty() {
                        parts.push(Node::Text(map_font(&std::mem::take(&mut text), font)));
                    }
                    let sub = if self.strict { strict(&inner)? } else { lenient(&inner) };
                    parts.push(sub);
                }
                '\\' => {
                    let (name, len) = self.peek_command();
                    self.i += len;
                    match name.as_str() {
                        "{" | "}" | "$" | "%" | "&" | "#" | "_" => text.push_str(&name),
                        " " | "," | ";" | ":" | "quad" | "qquad" | "\\" => text.push(' '),
                        "textbackslash" | "backslash" => text.push('∖'),
                        n if symbols::text_font(n).is_some() || symbols::font(n).is_some() => {
                            let f = symbols::text_font(n).or_else(|| symbols::font(n)).unwrap_or(Font::Plain);
                            if !text.is_empty() {
                                parts.push(Node::Text(map_font(&std::mem::take(&mut text), font)));
                            }
                            parts.push(self.text_arg(f)?);
                        }
                        n => match symbols::symbol(n).or_else(|| symbols::macro_letter(n).map(|_| ("", Class::Ord))) {
                            Some((g, _)) if !g.is_empty() => text.push_str(g),
                            _ => {
                                if self.strict {
                                    return self.err("unknown command in text");
                                }
                                text.push_str(n);
                            }
                        },
                    }
                }
                '~' => {
                    self.i += 1;
                    text.push(' ');
                }
                c => {
                    self.i += 1;
                    text.push(if c == '\n' { ' ' } else { c });
                }
            }
        }
        self.leave();
        if !closed && self.strict {
            return self.err("unclosed text");
        }
        if !text.is_empty() {
            parts.push(Node::Text(map_font(&text, font)));
        }
        Ok(if parts.len() == 1 { parts.pop().expect("one") } else { Node::Row(parts) })
    }

    /// A delimiter after `\left`, `\right`, `\middle` or `\big`.
    fn delimiter(&mut self) -> PResult<char> {
        self.skip_ws();
        match self.peek() {
            Some('\\') => {
                let (name, len) = self.peek_command();
                self.i += len;
                match symbols::delimiter_command(&name) {
                    Some(d) => Ok(d),
                    None if self.strict => self.err("not a delimiter"),
                    None => Ok('.'),
                }
            }
            Some(c) => {
                self.i += 1;
                match symbols::delimiter_char(c) {
                    Some(d) => Ok(d),
                    None if self.strict => self.err("not a delimiter"),
                    None => Ok('.'),
                }
            }
            None if self.strict => self.err("missing delimiter"),
            None => Ok('.'),
        }
    }

    /// One character as an atom. `group_digits` reads `3.14` as one number, which it is in a row
    /// and is not as an argument: `x^23` is `x²3` in TeX.
    fn char_atom(&mut self, group_digits: bool) -> PResult<Node> {
        let c = self.peek().expect("caller peeked");
        self.i += 1;
        use Class::*;
        Ok(match c {
            '0'..='9' if group_digits => {
                let mut s = c.to_string();
                while let Some(d) = self.peek() {
                    let decimal_point = d == '.' && self.peek_at(1).is_some_and(|x| x.is_ascii_digit());
                    if d.is_ascii_digit() || decimal_point {
                        s.push(d);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Node::atom(s, Ord)
            }
            '+' => Node::atom("+", Bin),
            '-' => Node::atom("−", Bin),
            '*' => Node::atom("∗", Bin),
            '=' | '<' | '>' => Node::atom(c, Rel),
            ',' | ';' | ':' => Node::atom(c, Punct),
            '!' | '?' => Node::atom(c, Close),
            '(' | '[' => Node::atom(c, Open),
            ')' | ']' => Node::atom(c, Close),
            '~' => Node::Space(1),
            '{' => {
                // Only reachable from `arg`, which handles `{` itself; kept total.
                self.group()?
            }
            '#' | '$' => {
                if self.strict {
                    return self.err("stray # or $");
                }
                Node::empty()
            }
            '∑' | '∏' | '∐' | '∫' | '∬' | '∭' | '∮' | '⋃' | '⋂' | '⨁' | '⨂' => {
                Node::Op { body: Box::new(Node::atom(c, Ord)), limits: true }
            }
            c => Node::atom(c, symbols::class_of_char(c)),
        })
    }

    /// A backslash command, the cursor on the backslash.
    fn command(&mut self) -> PResult<Node> {
        let (name, len) = self.peek_command();
        self.i += len;
        if name.is_empty() {
            return if self.strict { self.err("trailing backslash") } else { Ok(Node::empty()) };
        }
        let star = |p: &mut Parser| {
            if p.peek() == Some('*') {
                p.i += 1;
                true
            } else {
                false
            }
        };
        use Class::*;
        if let Some(n) = symbols::space(&name) {
            return Ok(Node::Space(n));
        }
        if let Some((g, class)) = symbols::symbol(&name) {
            return Ok(Node::atom(g, class));
        }
        if let Some(c) = symbols::macro_letter(&name) {
            return Ok(Node::atom(c, Ord));
        }
        if let Some((g, limits)) = symbols::big_op(&name) {
            return Ok(Node::Op { body: Box::new(Node::atom(g, Ord)), limits });
        }
        if let Some((t, limits)) = symbols::function(&name) {
            return Ok(Node::Op { body: Box::new(Node::Text(t.into())), limits });
        }
        if symbols::is_ignored(&name) {
            return Ok(Node::empty());
        }
        if let Some(f) = symbols::font(&name) {
            let body = self.arg()?;
            // `\mathrm{d}` is upright. The grid sets every letter upright anyway; a typesetter
            // italicises a math letter, so the letters become text.
            if matches!(name.as_str(), "mathrm" | "mathup") {
                return Ok(upright(body));
            }
            return Ok(apply_font(body, f));
        }
        if let Some(f) = symbols::text_font(&name) {
            return self.text_arg(f);
        }
        if symbols::accent(&name).is_some()
            || matches!(name.as_str(), "overrightarrow" | "overleftarrow" | "overleftrightarrow")
        {
            let body = self.arg()?;
            return Ok(Node::Accent { name, body: Box::new(body) });
        }
        Ok(match name.as_str() {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                if name == "cfrac" {
                    self.optional_bracket()?;
                }
                let num = self.arg()?;
                let den = self.arg()?;
                Node::Frac { num: Box::new(num), den: Box::new(den), bar: true }
            }
            "binom" | "dbinom" | "tbinom" => {
                let num = self.arg()?;
                let den = self.arg()?;
                let f = Node::Frac { num: Box::new(num), den: Box::new(den), bar: false };
                Node::Fenced { open: '(', close: ')', body: Box::new(f), auto: false }
            }
            "sqrt" => {
                let index = self.optional_bracket()?.filter(|n| !n.is_empty());
                let body = self.arg()?;
                Node::Sqrt { index: index.map(Box::new), body: Box::new(body) }
            }
            "operatorname" | "operatornamewithlimits" => {
                let limits = star(self) || name == "operatornamewithlimits";
                let body = self.arg()?;
                Node::Op { body: Box::new(body), limits }
            }
            "mathop" => {
                let body = self.arg()?;
                Node::Op { body: Box::new(body), limits: true }
            }
            "mathbin" | "mathrel" | "mathord" | "mathopen" | "mathclose" | "mathpunct"
            | "mathinner" => {
                let class = match name.as_str() {
                    "mathbin" => Bin,
                    "mathrel" => Rel,
                    "mathopen" => Open,
                    "mathclose" => Close,
                    "mathpunct" => Punct,
                    "mathinner" => Inner,
                    _ => Ord,
                };
                let body = self.arg()?;
                Node::Classed { class, body: Box::new(body) }
            }
            "overbrace" | "underbrace" => {
                let body = self.arg()?;
                Node::Brace { over: name == "overbrace", body: Box::new(body) }
            }
            "overset" | "stackrel" => {
                let over = self.arg()?;
                let base = self.arg()?;
                let base = if name == "stackrel" {
                    Node::Classed { class: Rel, body: Box::new(base) }
                } else {
                    base
                };
                Node::Stack { over: Some(Box::new(over)), base: Box::new(base), under: None }
            }
            "underset" => {
                let under = self.arg()?;
                let base = self.arg()?;
                Node::Stack { over: None, base: Box::new(base), under: Some(Box::new(under)) }
            }
            "xrightarrow" | "xleftarrow" | "xRightarrow" | "xLeftarrow" | "xleftrightarrow"
            | "xLeftrightarrow" | "xmapsto" | "xhookrightarrow" | "xlongequal" => {
                let under = self.optional_bracket()?.filter(|n| !n.is_empty());
                let over = self.arg()?;
                let glyph = match name.as_str() {
                    "xleftarrow" => '←',
                    "xRightarrow" => '⇒',
                    "xLeftarrow" => '⇐',
                    "xleftrightarrow" => '↔',
                    "xLeftrightarrow" => '⇔',
                    "xmapsto" => '↦',
                    "xhookrightarrow" => '↪',
                    "xlongequal" => '=',
                    _ => '→',
                };
                let over = Some(over).filter(|n| !n.is_empty());
                Node::Arrow { glyph, over: over.map(Box::new), under: under.map(Box::new) }
            }
            "boxed" | "fbox" | "framebox" => Node::Boxed(Box::new(self.arg()?)),
            "phantom" | "hphantom" => Node::Phantom(Box::new(self.arg()?)),
            "vphantom" => {
                self.arg()?;
                Node::empty()
            }
            "smash" => {
                self.optional_bracket()?;
                self.arg()?
            }
            "color" => {
                self.braced_name()?;
                Node::empty()
            }
            "textcolor" | "colorbox" => {
                self.braced_name()?;
                self.arg()?
            }
            "fcolorbox" => {
                self.braced_name()?;
                self.braced_name()?;
                self.arg()?
            }
            "label" | "cline" | "vspace" => {
                star(self);
                self.braced_name()?;
                Node::empty()
            }
            "hspace" | "hskip" | "mspace" => {
                star(self);
                self.braced_name()?;
                Node::Space(1)
            }
            "tag" => {
                // Read as text, not as raw characters: `\tag{\alpha}` must not put `\alpha` on screen.
                let bare = star(self);
                let t = self.text_arg(Font::Plain)?;
                if bare {
                    Node::Tag(Box::new(t))
                } else {
                    Node::Tag(Box::new(Node::Row(vec![Node::Text("(".into()), t, Node::Text(")".into())])))
                }
            }
            "not" => {
                self.skip_ws();
                let next = match self.peek() {
                    Some('\\') => self.command()?,
                    Some(_) => self.char_atom(false)?,
                    None => return if self.strict { self.err("\\not at end") } else { Ok(Node::empty()) },
                };
                negate(next)
            }
            "pmod" => {
                let n = self.arg()?;
                Node::Row(vec![Node::Space(1), Node::Text("(mod ".into()), n, Node::Text(")".into())])
            }
            "pod" => {
                let n = self.arg()?;
                Node::Row(vec![Node::Space(1), Node::Text("(".into()), n, Node::Text(")".into())])
            }
            "bmod" => Node::Classed { class: Bin, body: Box::new(Node::Text("mod".into())) },
            "mod" => Node::Row(vec![Node::Space(2), Node::Text("mod".into()), Node::Space(1)]),
            "substack" => {
                self.skip_ws();
                if self.peek() != Some('{') {
                    return if self.strict { self.err("expected {") } else { Ok(Node::empty()) };
                }
                self.i += 1;
                self.enter()?;
                let (rows, _, _) = self.cells(Closer::Brace, true)?;
                self.leave();
                let rows: Vec<Vec<Node>> = rows
                    .into_iter()
                    .map(|r| vec![Node::Row(r)])
                    .collect();
                Node::Matrix { kind: MatKind::Substack, rows, align: vec![Align::Centre], vlines: Vec::new(), hlines: Vec::new() }
            }
            "left" => {
                let open = self.delimiter()?;
                self.enter()?;
                let mut body: Vec<Node> = Vec::new();
                let close = loop {
                    let (items, term) = self.row(false)?;
                    body.extend(items);
                    match term {
                        Term::Right(d) => break d,
                        Term::End => {
                            if self.strict {
                                return self.err("\\left without \\right");
                            }
                            break '.';
                        }
                        _ if self.strict => return self.err("unexpected terminator in \\left"),
                        _ => {}
                    }
                };
                self.leave();
                Node::Fenced { open, close, body: Box::new(Node::Row(body)), auto: false }
            }
            "middle" => Node::Middle(self.delimiter()?),
            "big" | "Big" | "bigg" | "Bigg" | "bigl" | "Bigl" | "biggl" | "Biggl" | "bigr"
            | "Bigr" | "biggr" | "Biggr" | "bigm" | "Bigm" | "biggm" | "Biggm" => {
                let d = self.delimiter()?;
                let class = if name.ends_with('l') {
                    Open
                } else if name.ends_with('r') {
                    Close
                } else if name.ends_with('m') {
                    Rel
                } else {
                    match d {
                        '(' | '[' | '{' | '⟨' | '⌊' | '⌈' => Open,
                        ')' | ']' | '}' | '⟩' | '⌋' | '⌉' => Close,
                        _ => Ord,
                    }
                };
                if d == '.' {
                    Node::empty()
                } else {
                    Node::atom(d, class)
                }
            }
            "begin" => self.environment()?,
            "vline" => Node::atom("|", Ord),
            _ => {
                if self.strict {
                    return self.err("unknown command");
                }
                // The paraphrase of a command this module does not know: its name, upright, with
                // any braced arguments in parentheses. `\ce{H2O}` reads `ce(H2O)`.
                let mut args: Vec<Node> = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek() != Some('{') || args.len() >= 4 {
                        break;
                    }
                    self.i += 1;
                    args.push(self.group()?);
                }
                let mut row = vec![Node::Text(name)];
                if !args.is_empty() {
                    row.push(Node::atom("(", Open));
                    for (k, a) in args.into_iter().enumerate() {
                        if k > 0 {
                            row.push(Node::atom(",", Punct));
                        }
                        row.push(a);
                    }
                    row.push(Node::atom(")", Close));
                }
                Node::Row(row)
            }
        })
    }

    /// `\begin{name}…\end{name}`, the cursor after `\begin`.
    fn environment(&mut self) -> PResult<Node> {
        let raw = self.braced_name()?;
        let name = raw.trim_end_matches('*').to_string();
        // Arguments some environments take.
        let mut spec: Option<String> = None;
        match name.as_str() {
            "array" | "subarray" | "tabular" => {
                spec = Some(self.braced_name()?);
            }
            "alignat" | "alignedat" => {
                // The column-pair count: the cells say the same.
                self.braced_name()?;
            }
            "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix" | "matrix" | "smallmatrix"
            | "psmallmatrix" | "bsmallmatrix" | "Bsmallmatrix" | "vsmallmatrix" | "Vsmallmatrix" => {
                // `pmatrix*[r]`: one alignment for every column.
                let save = self.i;
                self.skip_ws();
                if self.peek() == Some('[') {
                    if let Some(n) = self.s[self.i..].iter().take(8).position(|c| *c == ']') {
                        let inner: String = self.s[self.i + 1..self.i + n].iter().collect();
                        self.i += n + 1;
                        spec = Some(format!("*{{64}}{{{}}}", inner.trim()));
                    } else {
                        self.i = save;
                    }
                } else {
                    self.i = save;
                }
            }
            _ => {}
        }
        let (kind, open, close) = match name.as_str() {
            "matrix" | "smallmatrix" => (MatKind::Matrix, '.', '.'),
            "pmatrix" | "psmallmatrix" => (MatKind::Matrix, '(', ')'),
            "bmatrix" | "bsmallmatrix" => (MatKind::Matrix, '[', ']'),
            "Bmatrix" | "Bsmallmatrix" => (MatKind::Matrix, '{', '}'),
            "vmatrix" | "vsmallmatrix" => (MatKind::Matrix, '|', '|'),
            "Vmatrix" | "Vsmallmatrix" => (MatKind::Matrix, '‖', '‖'),
            "array" | "subarray" | "tabular" => (MatKind::Matrix, '.', '.'),
            "cases" | "dcases" | "numcases" => (MatKind::Cases, '{', '.'),
            "rcases" | "drcases" => (MatKind::Cases, '.', '}'),
            "aligned" | "align" | "alignat" | "alignedat" | "split" | "flalign" | "eqnarray" => {
                (MatKind::Aligned, '.', '.')
            }
            "gathered" | "gather" | "multline" | "equation" | "displaymath" | "math"
            | "subequations" | "center" => (MatKind::Gathered, '.', '.'),
            _ => {
                if self.strict {
                    return self.err("unknown environment");
                }
                (MatKind::Matrix, '.', '.')
            }
        };
        self.enter()?;
        let (mut rows, _, hlines) = self.cells(Closer::Env(raw.clone()), true)?;
        self.leave();
        let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for r in &mut rows {
            while r.len() < cols {
                r.push(Node::empty());
            }
        }
        let align: Vec<Align> = match kind {
            MatKind::Cases => vec![Align::Left; cols],
            MatKind::Aligned if name == "eqnarray" => (0..cols)
                .map(|c| match c % 3 {
                    0 => Align::Right,
                    1 => Align::Centre,
                    _ => Align::Left,
                })
                .collect(),
            MatKind::Aligned => (0..cols)
                .map(|c| if c % 2 == 0 { Align::Right } else { Align::Left })
                .collect(),
            MatKind::Gathered | MatKind::Substack => vec![Align::Centre; cols],
            MatKind::Matrix => match &spec {
                Some(s) => {
                    let mut v = column_spec(s).0;
                    v.resize(cols, Align::Centre);
                    v
                }
                None => vec![Align::Centre; cols],
            },
        };
        let mut vlines = match (&spec, kind) {
            (Some(s), MatKind::Matrix) => column_spec(s).1,
            _ => Vec::new(),
        };
        vlines.retain(|c| *c > 0 && *c < cols);
        vlines.dedup();
        // In an alignment, the cell after `&` starts with the relation: `a &= b`. TeX inserts an
        // empty atom so the relation is spaced as binary, and so does this.
        if kind == MatKind::Aligned {
            for r in &mut rows {
                for (c, cell) in r.iter_mut().enumerate() {
                    if c % 2 == 1 {
                        let inner = std::mem::replace(cell, Node::empty());
                        *cell = lead_with_empty_atom(inner);
                    }
                }
            }
        }
        let matrix = Node::Matrix { kind, rows, align, vlines, hlines };
        Ok(if open == '.' && close == '.' {
            matrix
        } else {
            Node::Fenced { open, close, body: Box::new(matrix), auto: false }
        })
    }
}

/// Push an atom, matching bracket pairs so they can scale with what they enclose.
///
/// `(`, `[`, `\{`, `\langle` and friends open; any closer closes the nearest opener, whatever its
/// shape, because `[0, 1)` is a half-open interval and not an error. A bar `|` or `‖` closes an
/// open bar of the same kind and otherwise opens one; a bar left unmatched stays an ordinary
/// symbol, so `p(a | b)` is untouched.
fn push(items: &mut Vec<Node>, opens: &mut Vec<(usize, char, bool)>, node: Node) {
    if node.is_empty() {
        return;
    }
    let (glyph, class) = match &node {
        Node::Atom { text, class } if text.chars().count() == 1 => {
            (text.chars().next().expect("one"), *class)
        }
        _ => {
            items.push(node);
            return;
        }
    };
    let is_bar = matches!(glyph, '|' | '‖') && class == Class::Ord;
    if is_bar {
        if let Some(&(idx, open, true)) = opens.last() {
            if open == glyph {
                opens.pop();
                wrap(items, idx, glyph, glyph);
                return;
            }
        }
        opens.push((items.len(), glyph, true));
        items.push(node);
        return;
    }
    match class {
        Class::Open => {
            opens.push((items.len(), glyph, false));
            items.push(node);
        }
        Class::Close => {
            // Bars still open inside the bracket never found a partner: `p(a | b)`. They space as
            // relations, the same as an unpaired bar at the end of a row.
            while let Some(&(idx, _, true)) = opens.last() {
                opens.pop();
                if let Some(Node::Atom { class, .. }) = items.get_mut(idx) {
                    *class = Class::Rel;
                }
            }
            match opens.pop() {
                Some((idx, open, _)) => wrap(items, idx, open, glyph),
                None => items.push(node),
            }
        }
        _ => items.push(node),
    }
}

fn wrap(items: &mut Vec<Node>, idx: usize, open: char, close: char) {
    let inner: Vec<Node> = items.drain(idx + 1..).collect();
    items.pop();
    items.push(Node::Fenced { open, close, body: Box::new(Node::Row(inner)), auto: true });
}

fn table_or_row(mut rows: Vec<Vec<Node>>, kind: MatKind) -> Node {
    if rows.len() == 1 && rows[0].len() == 1 {
        return rows.pop().and_then(|mut r| r.pop()).expect("one cell");
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(1);
    let kind = if cols > 1 { MatKind::Aligned } else { kind };
    for r in &mut rows {
        while r.len() < cols {
            r.push(Node::empty());
        }
        if kind == MatKind::Aligned {
            for (c, cell) in r.iter_mut().enumerate() {
                if c % 2 == 1 {
                    let inner = std::mem::replace(cell, Node::empty());
                    *cell = lead_with_empty_atom(inner);
                }
            }
        }
    }
    let align = (0..cols)
        .map(|c| match kind {
            MatKind::Aligned if c % 2 == 0 => Align::Right,
            MatKind::Aligned => Align::Left,
            _ => Align::Centre,
        })
        .collect();
    Node::Matrix { kind, rows, align, vlines: Vec::new(), hlines: Vec::new() }
}

/// An array's column specification as alignments and the columns a vertical rule follows:
/// `{cc|c}` is centre ×3 with a rule after column 2. `p{2cm}` and friends are left-aligned
/// columns, `@{…}` and `!{…}` are skipped, and `*{3}{c}` repeats.
pub(crate) fn column_spec(spec: &str) -> (Vec<Align>, Vec<usize>) {
    fn group(chars: &[char], i: &mut usize) -> String {
        while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
            *i += 1;
        }
        if chars.get(*i) != Some(&'{') {
            return chars.get(*i).map(|c| {
                *i += 1;
                c.to_string()
            }).unwrap_or_default();
        }
        *i += 1;
        let (start, mut depth) = (*i, 1);
        while let Some(c) = chars.get(*i) {
            *i += 1;
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return chars[start..*i - 1].iter().collect();
                    }
                }
                _ => {}
            }
        }
        chars[start..].iter().collect()
    }
    fn walk(chars: &[char], aligns: &mut Vec<Align>, bars: &mut Vec<usize>, depth: usize) {
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            i += 1;
            match c {
                'l' => aligns.push(Align::Left),
                'c' => aligns.push(Align::Centre),
                'r' => aligns.push(Align::Right),
                'p' | 'm' | 'b' | 'X' => {
                    aligns.push(Align::Left);
                    if c != 'X' {
                        group(chars, &mut i);
                    }
                }
                '|' => {
                    if bars.last() != Some(&aligns.len()) {
                        bars.push(aligns.len());
                    }
                }
                '@' | '!' | '>' | '<' => {
                    group(chars, &mut i);
                }
                '*' => {
                    let n: usize = group(chars, &mut i).trim().parse().unwrap_or(1);
                    let body: Vec<char> = group(chars, &mut i).chars().collect();
                    if depth < 4 {
                        for _ in 0..n.min(64) {
                            walk(&body, aligns, bars, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let chars: Vec<char> = spec.chars().collect();
    let (mut aligns, mut bars) = (Vec::new(), Vec::new());
    walk(&chars, &mut aligns, &mut bars, 0);
    if aligns.len() > 256 {
        aligns.truncate(256);
    }
    bars.retain(|b| *b <= aligns.len());
    (aligns, bars)
}

/// `= b` becomes `{}= b`, flattened so the relation sees the empty atom as its left neighbour.
fn lead_with_empty_atom(inner: Node) -> Node {
    let mut v = vec![Node::atom("", Class::Ord)];
    match inner {
        Node::Row(items) => v.extend(items),
        other => v.push(other),
    }
    Node::Row(v)
}

fn map_font(s: &str, font: Font) -> String {
    s.chars().map(|c| symbols::font_char(font, c).unwrap_or(c)).collect()
}

/// `\mathbb{R}`: the font applied to every letter it can reach.
fn apply_font(node: Node, font: Font) -> Node {
    if font == Font::Plain {
        return node;
    }
    match node {
        Node::Atom { text, class } => Node::Atom { text: map_font(&text, font), class },
        Node::Text(t) => Node::Text(map_font(&t, font)),
        Node::Row(v) => Node::Row(v.into_iter().map(|n| apply_font(n, font)).collect()),
        Node::Scripts { base, sub, sup } => Node::Scripts {
            base: Box::new(apply_font(*base, font)),
            sub,
            sup,
        },
        Node::Accent { name, body } => Node::Accent { name, body: Box::new(apply_font(*body, font)) },
        Node::Fenced { open, close, body, auto } => {
            Node::Fenced { open, close, body: Box::new(apply_font(*body, font)), auto }
        }
        other => other,
    }
}

/// `\mathrm{KL}`: letters become upright text, everything else is untouched.
fn upright(node: Node) -> Node {
    match node {
        Node::Atom { text, class: Class::Ord } if text.chars().all(char::is_alphabetic) && !text.is_empty() => {
            Node::Text(text)
        }
        Node::Row(v) => {
            // A run of letters is one word, so `\mathrm{KL}` is the text `KL`, not `K` and `L`.
            let mut out: Vec<Node> = Vec::new();
            for n in v.into_iter().map(upright) {
                match (out.last_mut(), n) {
                    (Some(Node::Text(prev)), Node::Text(t)) => prev.push_str(&t),
                    (_, n) => out.push(n),
                }
            }
            Node::Row(out)
        }
        Node::Scripts { base, sub, sup } => Node::Scripts { base: Box::new(upright(*base)), sub, sup },
        other => other,
    }
}

/// `\not=` is `≠`. Where Unicode has no negated form, a combining long solidus is overlaid.
fn negate(node: Node) -> Node {
    match node {
        Node::Atom { text, class } => {
            let neg = match text.as_str() {
                "=" => Some("≠"),
                "∈" => Some("∉"),
                "∋" => Some("∌"),
                "≡" => Some("≢"),
                "<" => Some("≮"),
                ">" => Some("≯"),
                "≤" => Some("≰"),
                "≥" => Some("≱"),
                "∼" => Some("≁"),
                "≅" => Some("≇"),
                "≈" => Some("≉"),
                "⊂" => Some("⊄"),
                "⊃" => Some("⊅"),
                "⊆" => Some("⊈"),
                "⊇" => Some("⊉"),
                "∣" => Some("∤"),
                "∥" => Some("∦"),
                "∃" => Some("∄"),
                "→" => Some("↛"),
                "←" => Some("↚"),
                "⇒" => Some("⇏"),
                "⇔" => Some("⇎"),
                "≺" => Some("⊀"),
                "≻" => Some("⊁"),
                _ => None,
            };
            match neg {
                Some(n) => Node::Atom { text: n.into(), class },
                None => Node::Atom { text: format!("{text}\u{338}"), class },
            }
        }
        other => other,
    }
}
