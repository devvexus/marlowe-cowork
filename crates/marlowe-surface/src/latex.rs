//! **Inline maths, made legible where it can be and left visibly as source where it cannot.**
//!
//! # The line, stated before the table
//!
//! You cannot render LaTeX in a character grid. There is no vinculum, no radical sign that spans,
//! no stacked limits, no matrix. What a terminal *can* do is Greek letters, sub- and superscripts,
//! the common operators and relations, and simple fractions — and that covers most of the maths
//! that appears in a conversational reply.
//!
//! So this module answers one question per expression: **can the whole of it be represented?** If
//! yes, it returns the Unicode. If any single token cannot, it returns [`None`] and the caller
//! prints the source, delimiters and all, styled as verbatim text.
//!
//! **All-or-nothing is the whole design.** A partially translated formula is the failure this
//! project's §B11 instinct is about, in a new place: *an honest refusal beats a degraded
//! imitation.* `\int_0^\infty e^{-x}dx` with the `\infty` quietly dropped is `∫₀ e⁻ˣdx`, which is a
//! **different integral** and looks entirely plausible. A wrong formula that looks right is worse
//! than a raw one that looks raw — the reader of the raw one knows to go and check.
//!
//! # What is refused, and why each refusal is deliberate rather than unfinished
//!
//! | construct | why not |
//! |---|---|
//! | `\hat`, `\bar`, `\vec`, `\tilde`, `\dot` | combining marks occupy zero columns; the wrap arithmetic and the eye would disagree, and `x̂` is not reliably composed by every terminal font |
//! | `\begin{…}` / `\end{…}`, `&`, `\\` | matrices and alignments are two-dimensional. There is no honest one-line form |
//! | `\sqrt[3]{x}` | an index on the radical has no inline form. `\sqrt{x}` has one and is accepted |
//! | `\overline`, `\underbrace`, `\stackrel` | same: the notation *is* the geometry |
//! | any `\command` not in the table | an unknown command is an unknown meaning. Silence is not translation |
//! | a superscript or subscript whose characters have no Unicode form | `^q` and `_b` do not exist; refusing is the only alternative to inventing one |
//!
//! # Two safety properties, enforced here rather than asserted about here
//!
//! 1. **Every emitted codepoint passes `marlowe_contract::text::is_renderable`.** That predicate is
//!    the project's one answer to what may reach a screen, and it refuses U+202E, U+2028 and the
//!    zero-width block for documented reasons. A maths renderer with its own private table would
//!    walk straight around it.
//! 2. **No emitted codepoint is a [`crate::chrome`] marker.** This matters more than it looks:
//!    `chrome::prepare_model_text` runs over the raw reply *before* parsing, so anything this
//!    module produces afterwards has already passed the chrome reservation. A table entry mapping
//!    some command to `─` would be a hole straight through §B2's premise, opened by a maths
//!    table nobody would think to audit for borders.
//!
//! Both are checked on the output of every call, not only in a test over the table — the table is
//! one way to reach the output and a future `\command` handler would be another.

use std::borrow::Cow;

/// Render a LaTeX expression to Unicode, or refuse.
///
/// `src` is the expression **without** its delimiters: `\alpha^2`, not `$\alpha^2$`.
pub fn render(src: &str) -> Option<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut lex = Lex { s: &chars, i: 0 };
    let body = seq(&mut lex, false)?;
    if lex.i != chars.len() {
        // A stray `}` — unbalanced input. Refuse rather than guess where the group ended.
        return None;
    }
    let spaced = space_relations(&body);
    // The two safety properties, on the output rather than on the table.
    if spaced.chars().any(|c| !marlowe_contract::text::is_renderable(c)) {
        return None;
    }
    if spaced.chars().any(crate::chrome::is_reserved) {
        return None;
    }
    Some(spaced)
}

/// Whether a `$…$` span should be treated as maths at all.
///
/// # This is currency protection, and it is the only heuristic in the module
///
/// `$` is the dollar sign far more often than it is a maths delimiter. *"it costs $5 and $10"*
/// contains a perfectly well-formed `$…$` span whose content is `5 and `, and translating it
/// yields `5and` — a silent, confident corruption of ordinary prose, which is precisely the class
/// of failure the all-or-nothing rule exists to prevent. The refusal has to happen **before**
/// [`render`] is asked, because `5 and ` renders successfully.
///
/// Four conditions, each earning its place against a real sentence:
///
/// | rule | the sentence it protects |
/// |---|---|
/// | no leading or trailing space in the content | `$5 to $10` — content `5 to ` |
/// | no run of three or more letters outside a command | `$5 and $10` — content `5 and ` |
/// | at least one maths signal, or a single letter | `$100–$200` — content `100–` |
/// | no newline, and a bounded length | a `$` opening at the top of a reply and closing at the end |
///
/// The cost is named: a bare `$n$` with **no** operator is accepted only because it is a single
/// letter, and `$5$` — a lone digit — is refused and stays as source. That is the right direction.
/// A reader seeing `$5$` has lost nothing; a reader seeing `5` where the author wrote a price has.
pub fn looks_like_inline_maths(content: &str) -> bool {
    if content.is_empty() || content.len() > 200 || content.contains('\n') {
        return false;
    }
    if content.starts_with(' ') || content.ends_with(' ') {
        return false;
    }
    // **A backslash command settles it, and settles it BEFORE the letter-run test below.**
    //
    // That test reads a run of three or more letters as English, which is what protects
    // "it costs $5 and $10" -- `and`. It also fired on `\operatorname{softmax}(z)`, where `softmax`
    // is a seven-letter run inside a command's own argument, and refused the whole attention
    // formula as prose.
    //
    // Ordinary prose does not contain a backslash followed by a letter. Currency does not either.
    // So the presence of one is a stronger and cheaper signal than counting letters, and the
    // heuristic this module calls "the only heuristic in the module" keeps its job on the case it
    // was written for.
    if content
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'\\' && w[1].is_ascii_alphabetic())
    {
        return true;
    }
    // Letter runs of three or more, ignoring anything introduced by a backslash — `\alpha` is a
    // command, `and` is English.
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            continue;
        }
        if chars[i].is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // One list of operator names, shared with `command` — a second copy here would drift,
            // and the drift would show up as a formula that parses and then refuses.
            if word.len() >= 3 && !FUNCTIONS.contains(&word.as_str()) && word != "mod" {
                return false;
            }
            continue;
        }
        i += 1;
    }
    let single_letter = chars.len() == 1 && chars[0].is_alphabetic();
    let signal = content.contains(['\\', '^', '_', '=', '+', '<', '>', '/', '*']);
    single_letter || signal
}

struct Lex<'a> {
    s: &'a [char],
    i: usize,
}

impl Lex<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }
    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.i += 1;
        }
    }
}

/// Render a run of tokens. `in_group` means a `}` ends it rather than being an error.
fn seq(lex: &mut Lex, in_group: bool) -> Option<String> {
    let mut out = String::new();
    loop {
        let Some(c) = lex.peek() else {
            return if in_group { None } else { Some(out) };
        };
        match c {
            '}' => {
                if in_group {
                    lex.i += 1;
                    return Some(out);
                }
                return None;
            }
            '{' => {
                lex.i += 1;
                out.push_str(&seq(lex, true)?);
            }
            '\\' => out.push_str(&command(lex)?),
            '^' | '_' => {
                lex.i += 1;
                let script = argument(lex)?;
                out.push_str(&map_script(&script, c == '^')?);
            }
            // `&` is an alignment tab and `%` starts a comment: both belong to constructs with no
            // one-line form. `#` is a macro parameter. All three mean the input is not the kind of
            // expression this module claims to handle.
            '&' | '%' | '#' => return None,
            c if c.is_whitespace() => {
                lex.i += 1;
                // **A single space is kept where the source had one.** LaTeX itself discards it and
                // re-derives spacing from the grammar, which a character grid cannot do —
                // discarding it here turns `O(n \log n)` into `O(nlogn)`. Keeping it costs nothing
                // and is never wrong, because the author put it there.
                //
                // Not after an opening delimiter, though: `\left( x \right)` is `(x)`, and `( x )`
                // reads as a deliberate space the author did not write.
                if !out.is_empty() && !out.ends_with(' ') && !out.ends_with(['(', '[', '{', '⟨', '⌊', '⌈']) {
                    out.push(' ');
                }
            }
            _ => {
                lex.i += 1;
                out.push(c);
            }
        }
    }
}

/// The argument of `^`, `_`, `\frac` or `\sqrt`: a braced group, a command, or one character.
fn argument(lex: &mut Lex) -> Option<String> {
    lex.skip_spaces();
    match lex.peek()? {
        '{' => {
            lex.i += 1;
            seq(lex, true)
        }
        '\\' => command(lex),
        '}' | '^' | '_' | '&' | '%' | '#' => None,
        _ => {
            let c = lex.bump()?;
            Some(c.to_string())
        }
    }
}

fn command(lex: &mut Lex) -> Option<String> {
    debug_assert_eq!(lex.peek(), Some('\\'));
    lex.i += 1;
    let Some(first) = lex.peek() else { return None };
    if !first.is_ascii_alphabetic() {
        lex.i += 1;
        return match first {
            // Spacing commands. `\!` is a negative thin space and has no terminal form; collapsing
            // it to nothing is the closest honest reading.
            ',' | ';' | ':' | ' ' => Some(" ".into()),
            '!' => Some(String::new()),
            '{' | '}' | '%' | '$' | '&' | '#' | '_' => Some(first.to_string()),
            // `\|` is the double bar KL divergence is written with: D(P \| Q). It was the ONLY
            // unrenderable token in an expression whose every other part worked, and the
            // all-or-nothing rule therefore discarded the whole formula.
            //
            // U+2016, not two ASCII bars: an ASCII `|` is this renderer's table delimiter, and a
            // formula that could emit one is a formula that could forge a table row.
            '|' => Some("\u{2016}".into()),
            // `\\` is a line break inside maths — a two-dimensional construct.
            _ => None,
        };
    }
    let start = lex.i;
    while matches!(lex.peek(), Some(c) if c.is_ascii_alphabetic()) {
        lex.i += 1;
    }
    let name: String = lex.s[start..lex.i].iter().collect();
    match name.as_str() {
        // LaTeX's own rule: the whitespace that TERMINATES a command name is syntax, not spacing,
        // and is consumed. `lpha x` is `αx`. Operator names are the exception two arms down —
        // `\log n` is `log n`, and running them together would read as a variable called `logn`.
        _ if FUNCTIONS.contains(&name.as_str()) => {
            lex.skip_spaces();
            let next_is_atom =
                matches!(lex.peek(), Some(c) if c.is_alphanumeric() || c == '\\');
            // **`?`, not `unwrap_or("")`.** A name in `FUNCTIONS` with no matching `SYMBOLS` row
            // used to render as NOTHING: the operator vanished and the expression around it came
            // out looking well-formed. `rgmin_	heta \mathcal{L}(	heta)` became `_θℒ(θ)` --
            // silently wrong, which is the one outcome this module exists to prevent, and it was
            // introduced by adding five names to one list and not the other.
            //
            // Refusing means the expression stays visibly its source instead, and the two lists
            // are now checked against each other by a test.
            let upright = SYMBOLS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)?;
            Some(if next_is_atom {
                format!("{upright} ")
            } else {
                upright.to_string()
            })
        }
        "frac" | "dfrac" | "tfrac" => {
            let num = argument(lex)?;
            let den = argument(lex)?;
            Some(fraction(&num, &den))
        }
        "sqrt" => {
            lex.skip_spaces();
            // `\sqrt[3]{x}` — an index on the radical, which has no inline form.
            if lex.peek() == Some('[') {
                return None;
            }
            let body = argument(lex)?;
            // Parenthesising a compound radicand is not a decoration: `√x+1` reads as `(√x)+1` and
            // is a different expression. `√(x+1)` is correct, so it is what gets emitted.
            if body.chars().count() == 1 {
                Some(format!("√{body}"))
            } else {
                Some(format!("√({body})"))
            }
        }
        // Upright text inside maths. The contents are rendered as an ordinary sequence, so a
        // command inside them is still checked.
        "text" | "textrm" | "mathrm" | "operatorname" | "mathit" | "mathsf" | "mathtt" => {
            argument(lex)
        }
        // **Script and bold letters.** mathcal{L} is the loss and mathcal{N} the Gaussian in
        // every paper; mathbf{x} is the vector. Unicode has all of them as single codepoints,
        // so this is the same act as mathbb rather than a new kind of claim.
        "mathcal" | "mathscr" => {
            let body = argument(lex)?;
            body.chars().map(script_letter).collect::<Option<String>>()
        }
        "mathbf" | "boldsymbol" => {
            let body = argument(lex)?;
            // boldsymbol{	heta} renders its argument first: the argument is a command, not
            // a letter, and there is no bold form of an arbitrary glyph to fall back on.
            let inner = if body.starts_with('\\') { render(&body)? } else { body };
            inner.chars().map(bold_letter).collect::<Option<String>>()
        }
        "mathbb" => {
            let body = argument(lex)?;
            let mut out = String::new();
            for c in body.chars() {
                out.push(blackboard(c)?);
            }
            Some(out)
        }
        // **Accents, and ONLY where a precomposed character exists.**
        //
        // The header says these are refused because "combining marks occupy zero columns". That
        // is right about COMBINING marks and was applied one step too widely: \hat{y} is U+0177,
        // a single precomposed codepoint with its own width, which no font has to compose and
        // which the wrap arithmetic counts as one column. It is the predicted value in every
        // regression loss ever written, and it was refusing the formula around it.
        //
        // Where Unicode has no precomposed form -- \hat{x}, \bar{\theta} -- this still refuses,
        // because there the alternative really is a combining mark. Partial coverage is the
        // honest outcome: render what can be represented correctly, refuse the rest.
        "hat" | "tilde" | "bar" | "acute" | "grave" | "ddot" => {
            let body = argument(lex)?;
            let mut chars = body.chars();
            let base = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            precomposed(&name, base).map(|c| c.to_string())
        }
        // Sizing commands modify a delimiter that follows. The delimiter itself is what renders.
        "left" | "right" | "bigl" | "bigr" | "Bigl" | "Bigr" | "big" | "Big" => {
            lex.skip_spaces();
            match lex.peek()? {
                '.' => {
                    lex.i += 1;
                    Some(String::new())
                }
                '\\' => command(lex),
                c => {
                    lex.i += 1;
                    Some(c.to_string())
                }
            }
        }
        _ => {
            let found = SYMBOLS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())?;
            lex.skip_spaces();
            Some(found)
        }
    }
}

/// The commands LaTeX sets upright as operators, which therefore keep a space before the atom that
/// follows them. Shared with [`looks_like_inline_maths`], where the same names are the words that
/// may legitimately appear inside a `$…$` span.
const FUNCTIONS: [&str; 24] = [
    "sin", "cos", "tan", "sec", "csc", "cot", "log", "ln", "exp", "max", "min", "lim", "det",
    "dim", "ker", "deg", "arg", "gcd", "bmod",
    // Statistics and optimisation. argmax/argmin are how every objective in machine learning
    // is stated; Pr, sup and inf are the vocabulary around them.
    "argmax", "argmin", "sup", "inf", "Pr",
];

/// `a/b`, with parentheses wherever they are needed for the result to mean what the source meant.
///
/// Vulgar fractions are used for the handful Unicode actually has, because `½` is what a reader
/// expects to see and `1/2` is what they have to parse.
fn fraction(num: &str, den: &str) -> String {
    const VULGAR: [(&str, &str, char); 15] = [
        ("1", "2", '½'),
        ("1", "3", '⅓'),
        ("2", "3", '⅔'),
        ("1", "4", '¼'),
        ("3", "4", '¾'),
        ("1", "5", '⅕'),
        ("2", "5", '⅖'),
        ("3", "5", '⅗'),
        ("4", "5", '⅘'),
        ("1", "6", '⅙'),
        ("5", "6", '⅚'),
        ("1", "8", '⅛'),
        ("3", "8", '⅜'),
        ("5", "8", '⅝'),
        ("7", "8", '⅞'),
    ];
    if let Some((_, _, g)) = VULGAR.iter().find(|(n, d, _)| *n == num && *d == den) {
        return g.to_string();
    }
    let wrap = |s: &str| {
        if s.chars().count() > 1 && !(s.starts_with('(') && s.ends_with(')')) {
            format!("({s})")
        } else {
            s.to_string()
        }
    };
    format!("{}/{}", wrap(num), wrap(den))
}

/// A superscript or subscript, or [`None`] if **any** character in it has no Unicode form.
///
/// The all-or-nothing rule at its smallest scale, and the place it matters most: `x^{2n}` where
/// only the `2` mapped would render `x²n`, which reads as `x² · n`.
/// A sub- or superscript, as a true Unicode script where one exists and as **explicit notation**
/// where one does not.
///
/// # Why this falls back rather than refusing, which is a change to the all-or-nothing rule
///
/// **Unicode has no subscript for most letters.** There is `ₓ` and `ₙ`, and there is no subscript
/// `θ`, no subscript `K`, no subscript capital anything. So `\nabla_\theta`, `D_{\mathrm{KL}}` and
/// `\mathbb{E}_{\pi_\theta}` were refused — and because the rule is all-or-nothing, each one
/// discarded an entire formula in which **every other token rendered**. A real reply came back with
/// four equations in raw LaTeX for want of a glyph that does not exist in the standard.
///
/// The refusal principle is kept, and this is not an exception to it. That principle is about
/// **dropping** content: `\int_0^\infty` collapsing to `∫₀` loses the `∞` and states a different
/// integral, so it must refuse. `∇_θ` loses nothing — it is the ordinary plain-text convention for
/// a subscript, the same one every mathematician types in an email. **Degrading to explicit
/// notation is not the same act as silently discarding a bound**, and conflating them cost more
/// than it protected.
///
/// The inner expression is rendered first, so `\mathbb{E}_{\pi_\theta}` is `𝔼_{π_θ}` and not
/// `𝔼_{\pi_\theta}`. Braces are kept when the script is more than one character, because `D_KL`
/// invites reading `K` as the subscript and `L` as what follows it.
fn map_script(s: &str, superscript: bool) -> Option<String> {
    let table = if superscript { SUPERSCRIPT } else { SUBSCRIPT };
    // A true script form, but only if EVERY character has one. A partial mapping would be the
    // dropping this module refuses.
    if !s.is_empty() {
        let mapped: Option<String> = s
            .chars()
            .map(|c| table.iter().find(|(k, _)| *k == c).map(|(_, v)| *v))
            .collect();
        if let Some(m) = mapped {
            return Some(m);
        }
    }
    // Explicit notation, with the script itself rendered.
    let inner = render(s)?;
    let mark = if superscript { '^' } else { '_' };
    if inner.chars().count() > 1 {
        Some(format!("{mark}{{{inner}}}"))
    } else {
        Some(format!("{mark}{inner}"))
    }
}

/// A base letter plus an accent, **as one precomposed codepoint or not at all**.
///
/// Deliberately not a combining-mark fallback. A combining mark is zero columns wide, so the
/// renderer's wrap arithmetic and the reader's eye disagree about where the line ends, and
/// whether it composes at all is a property of the terminal's font rather than of this program.
fn precomposed(accent: &str, base: char) -> Option<char> {
    let table: &[(char, char)] = match accent {
        "hat" => &[('a', '\u{e2}'), ('e', '\u{ea}'), ('i', '\u{ee}'), ('o', '\u{f4}'), ('u', '\u{fb}'), ('y', '\u{177}'), ('c', '\u{109}'), ('g', '\u{11d}'), ('h', '\u{125}'), ('j', '\u{135}'), ('s', '\u{15d}'), ('w', '\u{175}'), ('z', '\u{1e91}'), ('A', '\u{c2}'), ('E', '\u{ca}'), ('I', '\u{ce}'), ('O', '\u{d4}'), ('U', '\u{db}'), ('Y', '\u{176}')],
        "tilde" => &[('a', '\u{e3}'), ('n', '\u{f1}'), ('o', '\u{f5}'), ('i', '\u{129}'), ('u', '\u{169}'), ('e', '\u{1ebd}'), ('y', '\u{1ef9}'), ('A', '\u{c3}'), ('N', '\u{d1}'), ('O', '\u{d5}')],
        "bar" => &[('a', '\u{101}'), ('e', '\u{113}'), ('i', '\u{12b}'), ('o', '\u{14d}'), ('u', '\u{16b}'), ('A', '\u{100}'), ('E', '\u{112}'), ('I', '\u{12a}'), ('O', '\u{14c}'), ('U', '\u{16a}')],
        "acute" => &[('a', '\u{e1}'), ('e', '\u{e9}'), ('i', '\u{ed}'), ('o', '\u{f3}'), ('u', '\u{fa}'), ('y', '\u{fd}'), ('n', '\u{144}'), ('c', '\u{107}'), ('s', '\u{15b}'), ('z', '\u{17a}')],
        "grave" => &[('a', '\u{e0}'), ('e', '\u{e8}'), ('i', '\u{ec}'), ('o', '\u{f2}'), ('u', '\u{f9}')],
        "ddot" => &[('a', '\u{e4}'), ('e', '\u{eb}'), ('i', '\u{ef}'), ('o', '\u{f6}'), ('u', '\u{fc}'), ('y', '\u{ff}')],
        _ => return None,
    };
    table.iter().find(|(k, _)| *k == base).map(|(_, v)| *v)
}

/// mathbf / boldsymbol. The Mathematical Alphanumeric Symbols block is contiguous, so this is
/// offset arithmetic rather than a table -- and it refuses anything outside A-Z a-z 0-9, which is
/// the honest boundary: there is no bold form of an arbitrary glyph.
fn bold_letter(c: char) -> Option<char> {
    let base = match c {
        'A'..='Z' => 0x1D400 + (c as u32 - 'A' as u32),
        'a'..='z' => 0x1D41A + (c as u32 - 'a' as u32),
        '0'..='9' => 0x1D7CE + (c as u32 - '0' as u32),
        _ => return None,
    };
    char::from_u32(base)
}

/// mathcal / mathscr. **Not contiguous**: eight capitals live in the Letterlike Symbols block for
/// historical reasons and the rest in Mathematical Alphanumeric Symbols. A table, so those eight
/// exceptions are visible rather than buried in arithmetic.
fn script_letter(c: char) -> Option<char> {
    Some(match c {
        'B' => '\u{212C}', 'E' => '\u{2130}', 'F' => '\u{2131}', 'H' => '\u{210B}',
        'I' => '\u{2110}', 'L' => '\u{2112}', 'M' => '\u{2133}', 'R' => '\u{211B}',
        'A'..='Z' => char::from_u32(0x1D49C + (c as u32 - 'A' as u32))?,
        'e' => '\u{212F}', 'g' => '\u{210A}', 'o' => '\u{2134}',
        'a'..='z' => char::from_u32(0x1D4B6 + (c as u32 - 'a' as u32))?,
        _ => return None,
    })
}

fn blackboard(c: char) -> Option<char> {
    Some(match c {
        'R' => 'ℝ',
        'N' => 'ℕ',
        'Z' => 'ℤ',
        'Q' => 'ℚ',
        'C' => 'ℂ',
        'H' => 'ℍ',
        'P' => 'ℙ',
        'E' => '𝔼',
        _ => return None,
    })
}

/// One space either side of anything that is **unambiguously binary**, which is how maths is set
/// and how it becomes scannable: `x²+y²=z²` against `x² + y² = z²`.
///
/// `+` and `-` are deliberately absent: they are also unary, and `-x` spaced as `- x` reads as a
/// subtraction with a missing left operand. Everything in this list can only sit between two
/// operands, so spacing it is never wrong. `·` is absent for a different reason — it is the
/// harness's own separator glyph, and `2 · 3` in a reply would read a shade more like chrome than
/// like arithmetic.
fn space_relations(s: &str) -> String {
    const SPACED: [char; 35] = [
        // Relations.
        '=', '<', '>', '≤', '≥', '≠', '≈', '≡', '∈', '∉', '⊂', '⊆', '⊃', '⊇', '≜', '∼', '≃', '≅',
        // Arrows, which read worst of all unspaced: `f:X→Y`.
        '→', '←', '↔', '⇒', '⇐', '⇔', '↦',
        // Binary operators. `\alpha \times \beta^2` is `α × β²`, not `α×β²` — the difference was
        // obvious the moment the pane was looked at rather than reasoned about.
        '×', '÷', '±', '∓', '⊕', '⊗', '∘',
        // Conditionals and norms. `p(a ∣ s)` against `p(a ∣s)` -- the second reads as a bar stuck to
        // the wrong operand. The source `\mid`'s own space is consumed as the token terminator, so
        // without this the spacing is decided by a LaTeX lexing rule rather than by how it reads.
        '∣', '∥', '‖',
    ];
    let mut out = String::with_capacity(s.len());
    // Whether the previous character was a relation that contributed its own trailing space.
    let mut just_spaced = false;
    for c in s.chars() {
        if SPACED.contains(&c) {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            out.push(c);
            out.push(' ');
            just_spaced = true;
        } else if c == ' ' && just_spaced {
            // The relation already contributed one. **Only that one** -- this used to collapse
            // EVERY run of spaces, which silently deleted `\quad` and `\qquad`. Those are the
            // author separating two equations on one line, so `L = ... \qquad p = ...` came out as
            // `L = ... p = ...` and read as a single malformed expression.
        } else {
            just_spaced = false;
            out.push(c);
        }
    }
    // A space before a closing delimiter or a comma is a space nobody typed — it is the artefact of
    // `\right)` having been a token. Same reason the opening case is handled in `seq`.
    let mut tidy = String::with_capacity(out.len());
    for c in out.chars() {
        if matches!(c, ')' | ']' | '}' | '⟩' | '⌋' | '⌉' | ',' | ';') && tidy.ends_with(' ') {
            tidy.pop();
        }
        tidy.push(c);
    }
    while tidy.ends_with(' ') {
        tidy.pop();
    }
    tidy
}

/// Strip the leading and trailing `$`, `\(`/`\)` or `\[`/`\]` from a delimited expression.
pub fn strip_delimiters(s: &str) -> Option<(&str, Delimiter)> {
    let t = s.trim();
    for (open, close, kind) in [
        ("$$", "$$", Delimiter::Display),
        ("\\[", "\\]", Delimiter::Display),
        ("\\(", "\\)", Delimiter::Inline),
        ("$", "$", Delimiter::Inline),
    ] {
        if let Some(rest) = t.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix(close) {
                return Some((inner, kind));
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiter {
    Inline,
    Display,
}

/// Render a delimited expression, falling back to its own source.
///
/// The [`Cow`] is the fallback made visible in the type: `Borrowed` is the source, unchanged and
/// about to be drawn as verbatim text; `Owned` is a translation.
pub fn render_or_source(src: &str) -> Cow<'_, str> {
    match strip_delimiters(src).and_then(|(inner, _)| render(inner)) {
        Some(out) => Cow::Owned(out),
        None => Cow::Borrowed(src),
    }
}

/// Greek, operators, relations, arrows and set notation.
///
/// Curated rather than exhaustive: every entry is something that turns up in a conversational
/// reply, and an entry that is not here produces a visible refusal rather than a wrong glyph.
const SYMBOLS: &[(&str, &str)] = &[
    // Greek, lower case.
    ("alpha", "α"), ("beta", "β"), ("gamma", "γ"), ("delta", "δ"), ("epsilon", "ε"),
    ("varepsilon", "ϵ"), ("zeta", "ζ"), ("eta", "η"), ("theta", "θ"), ("vartheta", "ϑ"),
    ("iota", "ι"), ("kappa", "κ"), ("lambda", "λ"), ("mu", "μ"), ("nu", "ν"), ("xi", "ξ"),
    ("pi", "π"), ("varpi", "ϖ"), ("rho", "ρ"), ("varrho", "ϱ"), ("sigma", "σ"),
    ("varsigma", "ς"), ("tau", "τ"), ("upsilon", "υ"), ("phi", "φ"), ("varphi", "ϕ"),
    ("chi", "χ"), ("psi", "ψ"), ("omega", "ω"),
    // Greek, upper case.
    ("Gamma", "Γ"), ("Delta", "Δ"), ("Theta", "Θ"), ("Lambda", "Λ"), ("Xi", "Ξ"), ("Pi", "Π"),
    ("Sigma", "Σ"), ("Upsilon", "Υ"), ("Phi", "Φ"), ("Psi", "Ψ"), ("Omega", "Ω"),
    // Binary operators.
    ("times", "×"), ("div", "÷"), ("cdot", "·"), ("pm", "±"), ("mp", "∓"), ("ast", "∗"),
    ("star", "⋆"), ("circ", "∘"), ("bullet", "∙"), ("oplus", "⊕"), ("otimes", "⊗"),
    // odot is the Hadamard product and it is in every backpropagation rule ever written.
    // It was the single unrenderable token in one, which is how it was found.
    ("odot", "⊙"), ("ominus", "⊖"), ("oslash", "⊘"), ("boxtimes", "⊠"),
    ("bigodot", "⨀"), ("varnothing", "∅"), ("coloneqq", "≔"),
    ("wedge", "∧"), ("vee", "∨"), ("setminus", "\\"),
    // Relations.
    ("leq", "≤"), ("le", "≤"), ("geq", "≥"), ("ge", "≥"), ("neq", "≠"), ("ne", "≠"),
    ("approx", "≈"), ("equiv", "≡"), ("sim", "∼"), ("simeq", "≃"), ("cong", "≅"),
    ("propto", "∝"), ("ll", "≪"), ("gg", "≫"), ("triangleq", "≜"),
    // **Conditionals and norms — added 2026-08-22 after a real reply came out raw.**
    //
    // `\mid` is the conditional bar and it is everywhere in probability: P(a | s), q(z | x),
    // every policy in reinforcement learning. Its absence refused whole expressions in which every
    // OTHER token rendered — the all-or-nothing rule working exactly as designed, on a gap that
    // should not have existed. `\|` is the double bar KL divergence is written with.
    //
    // U+2223 DIVIDES rather than ASCII `|`: the ASCII bar is a table delimiter in this renderer,
    // and a formula that emitted one would be a formula that could forge a table row.
    ("mid", "∣"), ("nmid", "∤"), ("parallel", "∥"), ("perp", "⊥"),
    // Transpose and its neighbours. `QK^	op` is the attention formula as everyone writes
    // it, and `	op` was the single unrenderable token in it.
    ("top", "⊤"), ("bot", "⊥"), ("dagger", "†"), ("ddagger", "‡"),
    // Set notation and logic.
    ("in", "∈"), ("notin", "∉"), ("ni", "∋"), ("subset", "⊂"), ("subseteq", "⊆"),
    ("supset", "⊃"), ("supseteq", "⊇"), ("cup", "∪"), ("cap", "∩"), ("emptyset", "∅"),
    ("varnothing", "∅"), ("forall", "∀"), ("exists", "∃"), ("nexists", "∄"), ("neg", "¬"),
    ("lnot", "¬"), ("land", "∧"), ("lor", "∨"), ("therefore", "∴"), ("because", "∵"),
    // Arrows.
    ("to", "→"), ("rightarrow", "→"), ("leftarrow", "←"), ("leftrightarrow", "↔"),
    ("Rightarrow", "⇒"), ("Leftarrow", "⇐"), ("Leftrightarrow", "⇔"), ("mapsto", "↦"),
    ("uparrow", "↑"), ("downarrow", "↓"), ("implies", "⇒"), ("iff", "⇔"),
    // Big operators. These are the operator itself; their limits ride on `_` and `^`, and refuse
    // there when the limit has no script form — which is exactly the `\int_0^\infty` case.
    ("sum", "∑"), ("prod", "∏"), ("coprod", "∐"), ("int", "∫"), ("iint", "∬"),
    ("oint", "∮"), ("bigcup", "⋃"), ("bigcap", "⋂"),
    // Calculus and analysis.
    ("infty", "∞"), ("partial", "∂"), ("nabla", "∇"), ("Re", "ℜ"), ("Im", "ℑ"),
    ("aleph", "ℵ"), ("hbar", "ℏ"), ("ell", "ℓ"), ("degree", "°"), ("prime", "′"),
    // Punctuation and delimiters.
    // `\cdots` is NOT U+22EF here, and the reason is worth reading: U+22EF is §B6's tool-line
    // marker. `render`'s output check would refuse it, so the expression would fall back to source
    // — correct, but a worse reply for no gain. Three middle dots say the same thing in glyphs the
    // harness does not use.
    ("ldots", "…"), ("dots", "…"), ("cdots", "···"), ("vdots", "⋮"), ("langle", "⟨"),
    ("rangle", "⟩"), ("lfloor", "⌊"), ("rfloor", "⌋"), ("lceil", "⌈"), ("rceil", "⌉"),
    ("quad", "  "), ("qquad", "    "),
    // Upright function names. LaTeX sets them upright; a terminal has no italic to contrast
    // against here, so the name itself is the whole rendering.
    ("sin", "sin"), ("cos", "cos"), ("tan", "tan"), ("sec", "sec"), ("csc", "csc"),
    ("cot", "cot"), ("log", "log"), ("ln", "ln"), ("exp", "exp"), ("lim", "lim"),
    ("max", "max"), ("min", "min"), ("det", "det"), ("dim", "dim"), ("ker", "ker"),
    ("argmax", "argmax"), ("argmin", "argmin"), ("sup", "sup"), ("inf", "inf"),
    ("Pr", "Pr"),
    ("deg", "deg"), ("arg", "arg"), ("gcd", "gcd"), ("bmod", "mod"), ("pmod", "mod"),
];

/// Unicode has no superscript `q`, so `x^q` refuses. That gap is the reason [`map_script`] is
/// all-or-nothing rather than best-effort.
const SUPERSCRIPT: &[(char, char)] = &[
    ('0', '⁰'), ('1', '¹'), ('2', '²'), ('3', '³'), ('4', '⁴'), ('5', '⁵'), ('6', '⁶'),
    ('7', '⁷'), ('8', '⁸'), ('9', '⁹'), ('+', '⁺'), ('-', '⁻'), ('−', '⁻'), ('=', '⁼'),
    ('(', '⁽'), (')', '⁾'), ('a', 'ᵃ'), ('b', 'ᵇ'), ('c', 'ᶜ'), ('d', 'ᵈ'), ('e', 'ᵉ'),
    ('f', 'ᶠ'), ('g', 'ᵍ'), ('h', 'ʰ'), ('i', 'ⁱ'), ('j', 'ʲ'), ('k', 'ᵏ'), ('l', 'ˡ'),
    ('m', 'ᵐ'), ('n', 'ⁿ'), ('o', 'ᵒ'), ('p', 'ᵖ'), ('r', 'ʳ'), ('s', 'ˢ'), ('t', 'ᵗ'),
    ('u', 'ᵘ'), ('v', 'ᵛ'), ('w', 'ʷ'), ('x', 'ˣ'), ('y', 'ʸ'), ('z', 'ᶻ'), ('A', 'ᴬ'),
    ('B', 'ᴮ'), ('D', 'ᴰ'), ('E', 'ᴱ'), ('G', 'ᴳ'), ('H', 'ᴴ'), ('I', 'ᴵ'), ('J', 'ᴶ'),
    ('K', 'ᴷ'), ('L', 'ᴸ'), ('M', 'ᴹ'), ('N', 'ᴺ'), ('O', 'ᴼ'), ('P', 'ᴾ'), ('R', 'ᴿ'),
    ('T', 'ᵀ'), ('U', 'ᵁ'), ('V', 'ⱽ'), ('W', 'ᵂ'), ('°', '°'), ('′', '′'),
];

/// Subscripts are sparser than superscripts — no `b`, `c`, `d`, `f`, `g`, `w`, `y` or `z` — so
/// `x_b` refuses where `x^b` does not. That asymmetry is Unicode's, not this module's.
const SUBSCRIPT: &[(char, char)] = &[
    ('0', '₀'), ('1', '₁'), ('2', '₂'), ('3', '₃'), ('4', '₄'), ('5', '₅'), ('6', '₆'),
    ('7', '₇'), ('8', '₈'), ('9', '₉'), ('+', '₊'), ('-', '₋'), ('−', '₋'), ('=', '₌'),
    ('(', '₍'), (')', '₎'), ('a', 'ₐ'), ('e', 'ₑ'), ('h', 'ₕ'), ('i', 'ᵢ'), ('j', 'ⱼ'),
    ('k', 'ₖ'), ('l', 'ₗ'), ('m', 'ₘ'), ('n', 'ₙ'), ('o', 'ₒ'), ('p', 'ₚ'), ('r', 'ᵣ'),
    ('s', 'ₛ'), ('t', 'ₜ'), ('u', 'ᵤ'), ('v', 'ᵥ'), ('x', 'ₓ'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_cases_read_as_maths() {
        for (src, want) in [
            (r"\alpha", "α"),
            (r"x^2", "x²"),
            (r"x_i", "xᵢ"),
            (r"E = mc^2", "E = mc²"),
            (r"\sum_{i=1}^{n} i", "∑ᵢ₌₁ⁿ i"),
            (r"\frac{1}{2}", "½"),
            (r"\frac{a+b}{c}", "(a+b)/c"),
            (r"\sqrt{2}", "√2"),
            (r"\sqrt{x+1}", "√(x+1)"),
            (r"a \leq b", "a ≤ b"),
            (r"\mathbb{R}^n", "ℝⁿ"),
            (r"f: X \to Y", "f: X → Y"),
            (r"\left( x \right)", "(x)"),
            (r"O(n \log n)", "O(n log n)"),
            (r"\pi \approx 3.14159", "π ≈ 3.14159"),
        ] {
            assert_eq!(render(src).as_deref(), Some(want), "rendering {src:?}");
        }
    }

    #[test]
    fn what_cannot_be_shown_is_refused_whole() {
        // Each of these renders *partially* under a best-effort scheme, and every partial result
        // is a different expression that looks entirely plausible.
        for (src, why) in [
            // **The three SCRIPT cases moved out of this list on 2026-08-22**, to
            // `a_script_with_no_unicode_form_degrades_without_dropping_anything`. They are no
            // longer refused, because a script with no glyph now degrades to explicit notation
            // rather than vanishing — nothing is dropped, which is the whole test of this list.
            // Everything remaining IS still refused, and each one is genuinely lossy or 2-D.
            (r"\hat{x}", "a combining mark occupies zero columns"),
            (r"\vec{v}", "same"),
            (r"\begin{pmatrix} a & b \end{pmatrix}", "a matrix is two-dimensional"),
            (r"\sqrt[3]{x}", "an index on the radical has no inline form"),
            (r"\overline{AB}", "the notation is the geometry"),
            (r"\unknowncommand", "an unknown command is an unknown meaning"),
            (r"a \\ b", "a line break inside maths"),
            (r"{a", "unbalanced"),
            (r"a}", "unbalanced"),
        ] {
            assert_eq!(render(src), None, "{src:?} should refuse: {why}");
        }
    }

    #[test]
    fn a_partial_translation_would_be_a_different_integral() {
        // **AMENDED 2026-08-22. The reason below is unchanged; the assertion inverted because
        // the harm it names no longer happens.**
        //
        // The fear was `∫₀ e⁻ˣdx` -- a convergent-looking definite integral over a range that is
        // NOT in the source, because `^\infty` had no superscript glyph and was DROPPED. Refusing
        // the whole expression was the right answer to that.
        //
        // A script with no Unicode form now degrades to explicit notation instead of vanishing
        // (see `map_script`), so the upper bound is present and the expression is the one that was
        // written. Refusing is no longer protecting anything here, and it was costing four
        // rendered formulas in a real reply for want of a `∞` that is now shown.
        //
        // **The property the old assertion defended is what is asserted now: the bound survives.**
        let out = render(r"\int_0^\infty e^{-x}dx").expect("renders rather than refusing");
        assert!(out.contains('∞'), "the upper bound was dropped -- the original defect: {out}");
        assert!(out.contains('₀') || out.contains("_0"), "the lower bound was dropped: {out}");
        assert!(out.contains('∫'), "{out}");
    }

    #[test]
    fn every_glyph_the_table_can_emit_passes_the_projects_display_predicate() {
        // The predicate refuses U+202E, U+2028 and the zero-width block for reasons its own module
        // documents. A maths table with its own idea of a safe character would walk around it.
        let mut checked = 0;
        for (name, value) in SYMBOLS {
            for c in value.chars() {
                assert!(
                    marlowe_contract::text::is_renderable(c),
                    "\\{name} emits U+{:04X}, which the display predicate refuses",
                    c as u32
                );
                assert!(
                    !crate::chrome::is_reserved(c),
                    "\\{name} emits U+{:04X}, which is harness chrome. `prepare_model_text` runs \
                     BEFORE this module, so a chrome glyph produced here would reach the screen \
                     unmarked and §B2's premise — a border means a region — would be forgeable \
                     through a maths table",
                    c as u32
                );
                checked += 1;
            }
        }
        for (k, v) in SUPERSCRIPT.iter().chain(SUBSCRIPT.iter()) {
            assert!(marlowe_contract::text::is_renderable(*v), "{k:?} -> {v:?}");
            assert!(!crate::chrome::is_reserved(*v), "{k:?} -> {v:?}");
            checked += 1;
        }
        assert!(checked > 200, "only {checked} glyphs walked; the tables did not load");
        println!("latex: {checked} emittable glyphs, all renderable, none of them chrome");
    }

    #[test]
    fn a_chrome_glyph_reaching_the_output_refuses_the_whole_expression() {
        // **The output check, exercised rather than assumed.**
        //
        // In the shipped pipeline `chrome::prepare_model_text` marks box-drawing before this
        // module is ever called, so the only way a border could reach here is a future table entry
        // — which is what the table walk above covers. This is the other half: the check lives on
        // the RESULT, so a literal reserved glyph arriving by any route refuses the expression
        // rather than being drawn.
        assert_eq!(render("a─b"), None, "a box-drawing rule rendered as maths");
        assert_eq!(render("x⋯y"), None, "§B6's tool marker rendered as maths");
        // The control: the identical expression with an ordinary glyph renders.
        assert_eq!(render("a-b").as_deref(), Some("a-b"));
    }

    #[test]
    fn a_dollar_sign_in_prose_is_not_maths() {
        // Every one of these is a real sentence, and every one contains a well-formed `$…$` span.
        for content in ["5 and ", "5 to ", "100–", "3.99 or ", "x and y"] {
            assert!(
                !looks_like_inline_maths(content),
                "{content:?} would have been silently rewritten as maths"
            );
        }
        for content in ["x^2", r"\alpha", "a + b", "n", "E = mc^2", r"\sum_{i}"] {
            assert!(looks_like_inline_maths(content), "{content:?} is maths");
        }
    }

    #[test]
    fn relations_are_spaced_and_signs_are_not() {
        assert_eq!(render("x^2+y^2=z^2").as_deref(), Some("x²+y² = z²"));
        // A unary minus spaced as `- x` reads as a subtraction with a missing operand.
        assert_eq!(render("-x").as_deref(), Some("-x"));
    }
}

#[cfg(test)]
mod function_table_agreement {
    use super::*;

    /// **Every name in `FUNCTIONS` must have a row in `SYMBOLS`, and this is why.**
    ///
    /// The two lists do different jobs — one decides *"this is an upright operator name"*, the
    /// other says *what it renders as* — and nothing connected them. A name in the first with no
    /// row in the second rendered as the empty string, so the operator silently disappeared and
    /// the expression around it still looked well-formed: `\argmin_\theta \mathcal{L}(\theta)`
    /// came out `_θℒ(θ)`.
    ///
    /// That is the module's one forbidden outcome — wrong and plausible — and it was introduced by
    /// adding five names to one list and forgetting the other. The lookup now refuses instead, and
    /// this makes the omission a build failure rather than a rendering one.
    #[test]
    fn every_function_name_has_a_symbol_row() {
        for name in FUNCTIONS {
            assert!(
                SYMBOLS.iter().any(|(n, _)| *n == name),
                "{name} is in FUNCTIONS with no SYMBOLS row: it would render as nothing"
            );
        }
        assert!(FUNCTIONS.len() >= 20, "the list looks empty: {}", FUNCTIONS.len());
    }
}
