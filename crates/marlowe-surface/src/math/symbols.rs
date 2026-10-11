//! The tables: what each command means, as a glyph and a TeX atom class.
//!
//! Curated, not exhaustive. A command missing from here is not guessed at. The strict parse fails,
//! the repair pass gets one try, and then the paraphrase shows the command's *name* as upright text.
//! The source never appears either way.

use super::parse::Class;

/// A symbol command: its glyph and how it spaces.
pub(crate) fn symbol(name: &str) -> Option<(&'static str, Class)> {
    use Class::*;
    let found = GREEK
        .iter()
        .map(|(n, g)| (*n, *g, Ord))
        .chain(ORD.iter().map(|(n, g)| (*n, *g, Ord)))
        .chain(BIN.iter().map(|(n, g)| (*n, *g, Bin)))
        .chain(REL.iter().map(|(n, g)| (*n, *g, Rel)))
        .chain(ARROWS.iter().map(|(n, g)| (*n, *g, Rel)))
        .chain(OPEN.iter().map(|(n, g)| (*n, *g, Open)))
        .chain(CLOSE.iter().map(|(n, g)| (*n, *g, Close)))
        .chain(PUNCT.iter().map(|(n, g)| (*n, *g, Punct)))
        .find(|(n, _, _)| *n == name)?;
    Some((found.1, found.2))
}

/// The class a raw Unicode character carries when a model writes it directly inside maths:
/// `$α ≤ β$`. Read off the same tables, so `≤` typed and `\leq` typed space identically.
pub(crate) fn class_of_char(c: char) -> Class {
    use Class::*;
    // A bar is an ordinary symbol until the parser pairs it: `\lvert` shares its glyph.
    if matches!(c, '|' | '‖') {
        return Ord;
    }
    let mut buf = [0u8; 4];
    let s: &str = c.encode_utf8(&mut buf);
    if BIN.iter().any(|(_, g)| *g == s) || matches!(c, '+' | '−' | '∗') {
        return Bin;
    }
    if REL.iter().chain(ARROWS.iter()).any(|(_, g)| *g == s) || matches!(c, '=' | '<' | '>') {
        return Rel;
    }
    if OPEN.iter().any(|(_, g)| *g == s) || matches!(c, '(' | '[') {
        return Open;
    }
    if CLOSE.iter().any(|(_, g)| *g == s) || matches!(c, ')' | ']') {
        return Close;
    }
    if matches!(c, ',' | ';' | ':') {
        return Punct;
    }
    Ord
}

/// Big operators: the glyph, and whether limits go above and below in display.
pub(crate) fn big_op(name: &str) -> Option<(&'static str, bool)> {
    BIG.iter().find(|(n, _, _)| *n == name).map(|(_, g, l)| (*g, *l))
}

/// Upright operator names: `\sin`, `\lim`. The text and whether limits go below in display.
pub(crate) fn function(name: &str) -> Option<(&'static str, bool)> {
    FUNCTIONS.iter().find(|(n, _, _)| *n == name).map(|(_, t, l)| (*t, *l))
}

/// Every operator name, for the currency heuristic: `$\max x$` contains `max`, which is a word.
pub(crate) fn is_function_word(w: &str) -> bool {
    FUNCTIONS.iter().any(|(_, t, _)| *t == w) || w == "mod"
}

/// Whether `w` is a proper prefix of an operator name, for the streaming currency heuristic.
pub(crate) fn is_function_prefix(w: &str) -> bool {
    FUNCTIONS.iter().any(|(_, t, _)| t.starts_with(w)) || "mod".starts_with(w)
}

/// Spacing commands, in columns. `\!` is a negative thin space and has no terminal form.
pub(crate) fn space(name: &str) -> Option<usize> {
    Some(match name {
        "," | ":" | ";" | ">" | " " | "thinspace" | "medspace" | "thickspace" | "enspace"
        | "nobreakspace" => 1,
        "quad" => 2,
        "qquad" => 4,
        "!" | "negthinspace" | "negmedspace" | "negthickspace" => 0,
        _ => return None,
    })
}

/// Commands that typeset nothing in a terminal and take no argument.
pub(crate) fn is_ignored(name: &str) -> bool {
    matches!(
        name,
        "displaystyle"
            | "textstyle"
            | "scriptstyle"
            | "scriptscriptstyle"
            | "nonumber"
            | "notag"
            | "hline"
            | "hdashline"
            | "mathstrut"
            | "strut"
            | "allowbreak"
            | "nobreak"
            | "relax"
            | "protect"
            | "normalsize"
            | "small"
            | "large"
            | "Large"
            | "LARGE"
            | "huge"
            | "Huge"
            | "footnotesize"
            | "scriptsize"
            | "tiny"
            | "centering"
            | "noindent"
            | "newline"
    )
}

/// A delimiter after `\left`, `\right`, `\middle` or `\big…`, given as a command name.
pub(crate) fn delimiter_command(name: &str) -> Option<char> {
    Some(match name {
        "{" | "lbrace" => '{',
        "}" | "rbrace" => '}',
        "langle" => '⟨',
        "rangle" => '⟩',
        "lfloor" => '⌊',
        "rfloor" => '⌋',
        "lceil" => '⌈',
        "rceil" => '⌉',
        "lvert" | "rvert" | "vert" => '|',
        "|" | "lVert" | "rVert" | "Vert" => '‖',
        "lbrack" => '[',
        "rbrack" => ']',
        "lgroup" => '⟮',
        "rgroup" => '⟯',
        "backslash" => '∖',
        "uparrow" => '↑',
        "downarrow" => '↓',
        "updownarrow" => '↕',
        "Uparrow" => '⇑',
        "Downarrow" => '⇓',
        _ => return None,
    })
}

/// A delimiter given as a plain character.
pub(crate) fn delimiter_char(c: char) -> Option<char> {
    Some(match c {
        '(' | ')' | '[' | ']' | '|' | '/' => c,
        '<' => '⟨',
        '>' => '⟩',
        '.' => '.',
        '‖' | '⟨' | '⟩' | '⌊' | '⌋' | '⌈' | '⌉' => c,
        _ => return None,
    })
}

/// Common macros a model writes as though they were standard. Expanded before parsing a command.
pub(crate) fn macro_letter(name: &str) -> Option<char> {
    Some(match name {
        "R" | "Reals" | "reals" => 'ℝ',
        "N" | "naturals" => 'ℕ',
        "Z" | "integers" => 'ℤ',
        "Q" => 'ℚ',
        "C" | "complex" => 'ℂ',
        "E" => '𝔼',
        _ => return None,
    })
}

/// The combining mark for an accent command, and whether the multi-cell form is drawn as a row
/// above (`true`) or below (`false`) the body.
pub(crate) fn accent(name: &str) -> Option<(char, bool)> {
    Some(match name {
        "hat" | "widehat" => ('\u{302}', true),
        "check" | "widecheck" => ('\u{30C}', true),
        "tilde" | "widetilde" => ('\u{303}', true),
        "acute" => ('\u{301}', true),
        "grave" => ('\u{300}', true),
        "dot" => ('\u{307}', true),
        "ddot" => ('\u{308}', true),
        "dddot" => ('\u{20DB}', true),
        "breve" => ('\u{306}', true),
        "bar" => ('\u{304}', true),
        "vec" => ('\u{20D7}', true),
        "mathring" => ('\u{30A}', true),
        "overline" => ('\u{305}', true),
        "underline" => ('\u{332}', false),
        "cancel" | "bcancel" | "xcancel" | "sout" => ('\u{336}', true),
        _ => return None,
    })
}

/// What the row above a multi-cell body looks like, for an accent with no single-cell form.
pub(crate) fn accent_row(name: &str, w: usize) -> String {
    let w = w.max(1);
    let centred = |c: char| {
        let left = (w - 1) / 2;
        format!("{}{}{}", " ".repeat(left), c, " ".repeat(w - 1 - left))
    };
    match name {
        // An underscore sits at the bottom of its cell, so a run of them on the row above draws a
        // rule just over the body -- the overline, without a box-drawing glyph.
        "bar" | "overline" => "_".repeat(w),
        "underline" => "‾".repeat(w),
        "vec" | "overrightarrow" => format!("{}→", "⎯".repeat(w - 1)),
        "overleftarrow" => format!("←{}", "⎯".repeat(w - 1)),
        "overleftrightarrow" => {
            if w >= 2 {
                format!("←{}→", "⎯".repeat(w - 2))
            } else {
                "↔".into()
            }
        }
        "tilde" | "widetilde" => centred('˜'),
        "dot" => centred('˙'),
        "ddot" => centred('¨'),
        "check" | "widecheck" => centred('ˇ'),
        "breve" => centred('˘'),
        "acute" => centred('´'),
        "grave" => centred('`'),
        "mathring" => centred('˚'),
        "dddot" => centred('…'),
        _ => centred('ˆ'),
    }
}

/// A sub- or superscript as true Unicode script glyphs, or [`None`] if any character lacks one.
/// All-or-nothing: `x^{2n}` mapped partially would read `x²n`, which is `x² · n`.
pub(crate) fn script_char(c: char, sup: bool) -> Option<char> {
    let table: &[(char, char)] = if sup { SUPERSCRIPT } else { SUBSCRIPT };
    table.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// A font command applied to one character. `None` means the font has no form of it; the caller
/// keeps the plain character, because an upright `θ` inside `\mathbf` is still `θ`.
pub(crate) fn font_char(font: Font, c: char) -> Option<char> {
    let off = |base: u32, from: char| char::from_u32(base + (c as u32 - from as u32));
    match font {
        Font::Plain => None,
        Font::Bold => match c {
            'A'..='Z' => off(0x1D400, 'A'),
            'a'..='z' => off(0x1D41A, 'a'),
            '0'..='9' => off(0x1D7CE, '0'),
            'Α'..='Ω' if c != '\u{3A2}' => off(0x1D6A8, 'Α'),
            'α'..='ω' => off(0x1D6C2, 'α'),
            _ => None,
        },
        Font::Blackboard => match c {
            'C' => Some('ℂ'),
            'H' => Some('ℍ'),
            'N' => Some('ℕ'),
            'P' => Some('ℙ'),
            'Q' => Some('ℚ'),
            'R' => Some('ℝ'),
            'Z' => Some('ℤ'),
            'A'..='Z' => off(0x1D538, 'A'),
            'a'..='z' => off(0x1D552, 'a'),
            '0'..='9' => off(0x1D7D8, '0'),
            _ => None,
        },
        // Eight capitals and three lower-case letters live in Letterlike Symbols for historical
        // reasons; the rest are in Mathematical Alphanumeric Symbols.
        Font::Script => match c {
            'B' => Some('\u{212C}'),
            'E' => Some('\u{2130}'),
            'F' => Some('\u{2131}'),
            'H' => Some('\u{210B}'),
            'I' => Some('\u{2110}'),
            'L' => Some('\u{2112}'),
            'M' => Some('\u{2133}'),
            'R' => Some('\u{211B}'),
            'A'..='Z' => off(0x1D49C, 'A'),
            'e' => Some('\u{212F}'),
            'g' => Some('\u{210A}'),
            'o' => Some('\u{2134}'),
            'a'..='z' => off(0x1D4B6, 'a'),
            _ => None,
        },
        Font::Fraktur => match c {
            'C' => Some('ℭ'),
            'H' => Some('ℌ'),
            'I' => Some('ℑ'),
            'R' => Some('ℜ'),
            'Z' => Some('ℨ'),
            'A'..='Z' => off(0x1D504, 'A'),
            'a'..='z' => off(0x1D51E, 'a'),
            _ => None,
        },
        Font::Sans => match c {
            'A'..='Z' => off(0x1D5A0, 'A'),
            'a'..='z' => off(0x1D5BA, 'a'),
            '0'..='9' => off(0x1D7E2, '0'),
            _ => None,
        },
        Font::Mono => match c {
            'A'..='Z' => off(0x1D670, 'A'),
            'a'..='z' => off(0x1D68A, 'a'),
            '0'..='9' => off(0x1D7F6, '0'),
            _ => None,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Font {
    Plain,
    Bold,
    Blackboard,
    Script,
    Fraktur,
    Sans,
    Mono,
}

/// Font commands. Upright and italic map to [`Font::Plain`]: a terminal sets every letter upright,
/// and Unicode italic letters would make copied text unsearchable for no visible gain.
pub(crate) fn font(name: &str) -> Option<Font> {
    Some(match name {
        "mathbb" | "mathds" | "mathbbm" | "Bbb" => Font::Blackboard,
        "mathbf" | "boldsymbol" | "bm" | "pmb" | "mathbfit" => Font::Bold,
        "mathcal" | "mathscr" => Font::Script,
        "mathfrak" | "frak" => Font::Fraktur,
        "mathsf" => Font::Sans,
        "mathtt" => Font::Mono,
        "mathrm" | "mathit" | "mathnormal" | "mathup" => Font::Plain,
        _ => return None,
    })
}

/// Text-mode commands whose argument is literal text.
pub(crate) fn text_font(name: &str) -> Option<Font> {
    Some(match name {
        "text" | "textrm" | "textup" | "textnormal" | "textit" | "emph" | "mbox" | "hbox"
        | "textmd" => Font::Plain,
        "textbf" => Font::Bold,
        "textsf" => Font::Sans,
        "texttt" => Font::Mono,
        _ => return None,
    })
}

const GREEK: &[(&str, &str)] = &[
    ("alpha", "α"), ("beta", "β"), ("gamma", "γ"), ("delta", "δ"), ("epsilon", "ϵ"),
    ("varepsilon", "ε"), ("zeta", "ζ"), ("eta", "η"), ("theta", "θ"), ("vartheta", "ϑ"),
    ("iota", "ι"), ("kappa", "κ"), ("varkappa", "ϰ"), ("lambda", "λ"), ("mu", "μ"), ("nu", "ν"),
    ("xi", "ξ"), ("omicron", "ο"), ("pi", "π"), ("varpi", "ϖ"), ("rho", "ρ"), ("varrho", "ϱ"),
    ("sigma", "σ"), ("varsigma", "ς"), ("tau", "τ"), ("upsilon", "υ"), ("phi", "ϕ"),
    ("varphi", "φ"), ("chi", "χ"), ("psi", "ψ"), ("omega", "ω"), ("digamma", "ϝ"),
    ("Alpha", "Α"), ("Beta", "Β"), ("Gamma", "Γ"), ("Delta", "Δ"), ("Epsilon", "Ε"),
    ("Zeta", "Ζ"), ("Eta", "Η"), ("Theta", "Θ"), ("Iota", "Ι"), ("Kappa", "Κ"),
    ("Lambda", "Λ"), ("Mu", "Μ"), ("Nu", "Ν"), ("Xi", "Ξ"), ("Omicron", "Ο"), ("Pi", "Π"),
    ("Rho", "Ρ"), ("Sigma", "Σ"), ("Tau", "Τ"), ("Upsilon", "Υ"), ("Phi", "Φ"), ("Chi", "Χ"),
    ("Psi", "Ψ"), ("Omega", "Ω"), ("varGamma", "Γ"), ("varDelta", "Δ"), ("varTheta", "Θ"),
    ("varLambda", "Λ"), ("varPi", "Π"), ("varSigma", "Σ"), ("varPhi", "Φ"), ("varPsi", "Ψ"),
    ("varOmega", "Ω"),
];

const ORD: &[(&str, &str)] = &[
    ("infty", "∞"), ("partial", "∂"), ("nabla", "∇"), ("ell", "ℓ"), ("hbar", "ℏ"),
    ("hslash", "ℏ"), ("imath", "ı"), ("jmath", "ȷ"), ("wp", "℘"), ("Re", "ℜ"), ("Im", "ℑ"),
    ("aleph", "ℵ"), ("beth", "ℶ"), ("gimel", "ℷ"), ("forall", "∀"), ("exists", "∃"),
    ("nexists", "∄"), ("emptyset", "∅"), ("varnothing", "∅"), ("neg", "¬"), ("lnot", "¬"),
    ("top", "⊤"), ("bot", "⊥"), ("angle", "∠"), ("measuredangle", "∡"), ("triangle", "△"),
    ("square", "□"), ("Box", "□"), ("blacksquare", "■"), ("Diamond", "◇"), ("lozenge", "◊"),
    ("checkmark", "✓"), ("S", "§"), ("P", "¶"), ("copyright", "©"), ("pounds", "£"),
    ("flat", "♭"), ("sharp", "♯"), ("natural", "♮"), ("prime", "′"), ("degree", "°"),
    ("surd", "√"), ("ldots", "…"), ("dots", "…"), ("dotsc", "…"), ("dotso", "…"),
    // **Not U+22EF.** That is §B6's tool-line marker and `chrome` reserves it. Three dot operators
    // say the same thing in glyphs the harness does not draw.
    ("cdots", "⋅⋅⋅"), ("dotsb", "⋅⋅⋅"), ("dotsm", "⋅⋅⋅"), ("dotsi", "⋅⋅⋅"),
    ("vdots", "⋮"), ("ddots", "⋱"), ("iddots", "⋰"), ("mho", "℧"), ("eth", "ð"),
    ("clubsuit", "♣"), ("diamondsuit", "♢"), ("heartsuit", "♡"), ("spadesuit", "♠"),
    ("bigstar", "★"), ("backslash", "∖"), ("vert", "|"), ("Vert", "‖"), ("|", "‖"),
    ("$", "$"), ("%", "%"), ("&", "&"), ("#", "#"), ("_", "_"), ("textbackslash", "∖"),
    ("dagger", "†"), ("ddagger", "‡"), ("textdagger", "†"), ("dag", "†"), ("ddag", "‡"),
    ("circledR", "®"), ("yen", "¥"), ("euro", "€"), ("complement", "∁"), ("Finv", "Ⅎ"),
    ("varkappa", "ϰ"), ("infin", "∞"), ("O", "Ø"), ("o", "ø"), ("l", "ł"), ("L", "Ł"),
    ("ss", "ß"), ("ae", "æ"), ("AE", "Æ"), ("i", "ı"), ("j", "ȷ"),
];

const BIN: &[(&str, &str)] = &[
    ("pm", "±"), ("mp", "∓"), ("times", "×"), ("div", "÷"),
    // U+22C5 DOT OPERATOR rather than U+00B7: `·` is the harness's own separator glyph, and
    // `2 · 3` in a reply would read a shade more like chrome than like arithmetic.
    ("cdot", "⋅"), ("ast", "∗"), ("star", "⋆"), ("circ", "∘"), ("bullet", "∙"),
    ("oplus", "⊕"), ("ominus", "⊖"), ("otimes", "⊗"), ("oslash", "⊘"), ("odot", "⊙"),
    ("wedge", "∧"), ("land", "∧"), ("vee", "∨"), ("lor", "∨"), ("cap", "∩"), ("cup", "∪"),
    ("sqcap", "⊓"), ("sqcup", "⊔"), ("setminus", "∖"), ("smallsetminus", "∖"), ("uplus", "⊎"),
    ("amalg", "⨿"), ("wr", "≀"), ("diamond", "⋄"), ("triangleleft", "◁"),
    ("triangleright", "▷"), ("lhd", "⊲"), ("rhd", "⊳"), ("unlhd", "⊴"), ("unrhd", "⊵"),
    ("boxplus", "⊞"), ("boxtimes", "⊠"), ("boxminus", "⊟"), ("boxdot", "⊡"),
    ("circledast", "⊛"), ("circledcirc", "⊚"), ("ltimes", "⋉"), ("rtimes", "⋊"),
    ("intercal", "⊺"), ("bigcirc", "◯"), ("dotplus", "∔"), ("divideontimes", "⋇"),
    ("curlywedge", "⋏"), ("curlyvee", "⋎"),
];

const REL: &[(&str, &str)] = &[
    ("le", "≤"), ("leq", "≤"), ("leqq", "≦"), ("leqslant", "⩽"), ("ge", "≥"), ("geq", "≥"),
    ("geqq", "≧"), ("geqslant", "⩾"), ("ne", "≠"), ("neq", "≠"), ("lt", "<"), ("gt", ">"),
    ("approx", "≈"), ("approxeq", "≊"), ("thickapprox", "≈"), ("equiv", "≡"), ("sim", "∼"),
    ("thicksim", "∼"), ("simeq", "≃"), ("cong", "≅"), ("propto", "∝"), ("varpropto", "∝"),
    ("ll", "≪"), ("gg", "≫"), ("lll", "⋘"), ("ggg", "⋙"), ("prec", "≺"), ("succ", "≻"),
    ("preceq", "⪯"), ("succeq", "⪰"), ("subset", "⊂"), ("supset", "⊃"), ("subseteq", "⊆"),
    ("supseteq", "⊇"), ("subsetneq", "⊊"), ("supsetneq", "⊋"), ("sqsubset", "⊏"),
    ("sqsupset", "⊐"), ("sqsubseteq", "⊑"), ("sqsupseteq", "⊒"), ("in", "∈"), ("isin", "∈"),
    ("ni", "∋"), ("owns", "∋"), ("notin", "∉"), ("perp", "⊥"), ("parallel", "∥"),
    ("nparallel", "∦"), ("mid", "∣"), ("nmid", "∤"), ("vdash", "⊢"), ("dashv", "⊣"),
    ("models", "⊨"), ("vDash", "⊨"), ("Vdash", "⊩"), ("asymp", "≍"), ("doteq", "≐"),
    ("triangleq", "≜"), ("coloneqq", "≔"), ("coloneq", "≔"), ("eqqcolon", "≕"),
    ("eqcolon", "≕"), ("defeq", "≝"), ("eqdef", "≝"), ("lesssim", "≲"), ("gtrsim", "≳"),
    ("lessapprox", "⪅"), ("gtrapprox", "⪆"), ("nleq", "≰"), ("ngeq", "≱"), ("nless", "≮"),
    ("ngtr", "≯"), ("nsim", "≁"), ("ncong", "≇"), ("nequiv", "≢"), ("bowtie", "⋈"),
    ("Join", "⋈"), ("smile", "⌣"), ("frown", "⌢"), ("between", "≬"), ("pitchfork", "⋔"),
    ("therefore", "∴"), ("because", "∵"), ("vcentcolon", ":"), ("ratio", "∶"),
    ("approxident", "≋"), ("backsim", "∽"), ("eqsim", "≂"), ("lneq", "⪇"), ("gneq", "⪈"),
    ("nsubseteq", "⊈"), ("nsupseteq", "⊉"), ("trianglelefteq", "⊴"),
    ("trianglerighteq", "⊵"), ("precsim", "≾"), ("succsim", "≿"), ("leftmodels", "⫤"),
    ("multimap", "⊸"), ("circeq", "≗"), ("bumpeq", "≏"), ("Bumpeq", "≎"),
];

const ARROWS: &[(&str, &str)] = &[
    ("to", "→"), ("gets", "←"), ("rightarrow", "→"), ("leftarrow", "←"),
    ("leftrightarrow", "↔"), ("Rightarrow", "⇒"), ("Leftarrow", "⇐"), ("Leftrightarrow", "⇔"),
    ("longrightarrow", "⟶"), ("longleftarrow", "⟵"), ("longleftrightarrow", "⟷"),
    ("Longrightarrow", "⟹"), ("Longleftarrow", "⟸"), ("Longleftrightarrow", "⟺"),
    ("implies", "⇒"), ("impliedby", "⇐"), ("iff", "⇔"), ("mapsto", "↦"),
    ("longmapsto", "⟼"), ("uparrow", "↑"), ("downarrow", "↓"), ("updownarrow", "↕"),
    ("Uparrow", "⇑"), ("Downarrow", "⇓"), ("Updownarrow", "⇕"), ("nearrow", "↗"),
    ("searrow", "↘"), ("nwarrow", "↖"), ("swarrow", "↙"), ("hookrightarrow", "↪"),
    ("hookleftarrow", "↩"), ("rightharpoonup", "⇀"), ("rightharpoondown", "⇁"),
    ("leftharpoonup", "↼"), ("leftharpoondown", "↽"), ("rightleftharpoons", "⇌"),
    ("leftrightharpoons", "⇋"), ("leftrightarrows", "⇆"), ("rightleftarrows", "⇄"),
    ("rightrightarrows", "⇉"), ("leftleftarrows", "⇇"), ("twoheadrightarrow", "↠"),
    ("twoheadleftarrow", "↞"), ("rightsquigarrow", "⇝"), ("leadsto", "⇝"),
    ("circlearrowleft", "↺"), ("circlearrowright", "↻"), ("curvearrowleft", "↶"),
    ("curvearrowright", "↷"), ("nrightarrow", "↛"), ("nleftarrow", "↚"),
    ("nRightarrow", "⇏"), ("nLeftarrow", "⇍"), ("nLeftrightarrow", "⇎"),
    ("nleftrightarrow", "↮"), ("dashrightarrow", "⇢"), ("dashleftarrow", "⇠"),
    ("Lsh", "↰"), ("Rsh", "↱"), ("upharpoonright", "↾"), ("restriction", "↾"),
    ("upharpoonleft", "↿"), ("downharpoonright", "⇂"), ("downharpoonleft", "⇃"),
    ("leftarrowtail", "↢"), ("rightarrowtail", "↣"), ("looparrowright", "↬"),
    ("Rrightarrow", "⇛"), ("Lleftarrow", "⇚"),
];

const OPEN: &[(&str, &str)] = &[
    ("langle", "⟨"), ("lbrace", "{"), ("{", "{"), ("lbrack", "["), ("lfloor", "⌊"),
    ("lceil", "⌈"), ("lvert", "|"), ("lVert", "‖"), ("lgroup", "⟮"), ("ulcorner", "⌜"),
    ("llcorner", "⌞"), ("lmoustache", "⎰"), ("llbracket", "⟦"),
];

const CLOSE: &[(&str, &str)] = &[
    ("rangle", "⟩"), ("rbrace", "}"), ("}", "}"), ("rbrack", "]"), ("rfloor", "⌋"),
    ("rceil", "⌉"), ("rvert", "|"), ("rVert", "‖"), ("rgroup", "⟯"), ("urcorner", "⌝"),
    ("lrcorner", "⌟"), ("rmoustache", "⎱"), ("rrbracket", "⟧"),
];

const PUNCT: &[(&str, &str)] = &[("colon", ":"), ("cdotp", "⋅"), ("ldotp", "."), ("comma", ",")];

const BIG: &[(&str, &str, bool)] = &[
    ("sum", "∑", true), ("prod", "∏", true), ("coprod", "∐", true), ("int", "∫", true),
    ("iint", "∬", true), ("iiint", "∭", true), ("oint", "∮", true), ("oiint", "∯", true),
    ("intop", "∫", true), ("smallint", "∫", true),
    ("bigcup", "⋃", true), ("bigcap", "⋂", true), ("bigsqcup", "⨆", true), ("bigvee", "⋁", true),
    ("bigwedge", "⋀", true), ("bigoplus", "⨁", true), ("bigotimes", "⨂", true),
    ("bigodot", "⨀", true), ("biguplus", "⨄", true),
];

/// `(command, text, limits in display)`. The last block is not standard LaTeX but is what models
/// write as though it were; rendering it beats a paraphrase of a perfectly clear intent.
const FUNCTIONS: &[(&str, &str, bool)] = &[
    ("sin", "sin", false), ("cos", "cos", false), ("tan", "tan", false), ("cot", "cot", false),
    ("sec", "sec", false), ("csc", "csc", false), ("arcsin", "arcsin", false),
    ("arccos", "arccos", false), ("arctan", "arctan", false), ("arccot", "arccot", false),
    ("sinh", "sinh", false), ("cosh", "cosh", false), ("tanh", "tanh", false),
    ("coth", "coth", false), ("sech", "sech", false), ("csch", "csch", false),
    ("log", "log", false), ("ln", "ln", false), ("lg", "lg", false), ("exp", "exp", false),
    ("ker", "ker", false), ("dim", "dim", false), ("deg", "deg", false), ("arg", "arg", false),
    ("hom", "hom", false), ("lim", "lim", true), ("liminf", "lim inf", true),
    ("limsup", "lim sup", true), ("max", "max", true), ("min", "min", true),
    ("sup", "sup", true), ("inf", "inf", true), ("det", "det", true), ("gcd", "gcd", true),
    ("Pr", "Pr", true), ("injlim", "inj lim", true), ("projlim", "proj lim", true),
    ("argmax", "argmax", true), ("argmin", "argmin", true), ("Tr", "Tr", false),
    ("tr", "tr", false), ("rank", "rank", false), ("diag", "diag", false),
    ("sgn", "sgn", false), ("sign", "sign", false), ("Var", "Var", false),
    ("Cov", "Cov", false), ("erf", "erf", false), ("lcm", "lcm", false),
];

/// Unicode superscripts. There is no superscript `q`, so `x^q` goes to a row above in display and
/// to explicit notation inline. `⊤` maps to `ᵀ`: `QK^\top` is a transpose and reads as one.
const SUPERSCRIPT: &[(char, char)] = &[
    ('0', '⁰'), ('1', '¹'), ('2', '²'), ('3', '³'), ('4', '⁴'), ('5', '⁵'), ('6', '⁶'),
    ('7', '⁷'), ('8', '⁸'), ('9', '⁹'), ('+', '⁺'), ('-', '⁻'), ('−', '⁻'), ('=', '⁼'),
    ('(', '⁽'), (')', '⁾'), ('a', 'ᵃ'), ('b', 'ᵇ'), ('c', 'ᶜ'), ('d', 'ᵈ'), ('e', 'ᵉ'),
    ('f', 'ᶠ'), ('g', 'ᵍ'), ('h', 'ʰ'), ('i', 'ⁱ'), ('j', 'ʲ'), ('k', 'ᵏ'), ('l', 'ˡ'),
    ('m', 'ᵐ'), ('n', 'ⁿ'), ('o', 'ᵒ'), ('p', 'ᵖ'), ('r', 'ʳ'), ('s', 'ˢ'), ('t', 'ᵗ'),
    ('u', 'ᵘ'), ('v', 'ᵛ'), ('w', 'ʷ'), ('x', 'ˣ'), ('y', 'ʸ'), ('z', 'ᶻ'), ('A', 'ᴬ'),
    ('B', 'ᴮ'), ('D', 'ᴰ'), ('E', 'ᴱ'), ('G', 'ᴳ'), ('H', 'ᴴ'), ('I', 'ᴵ'), ('J', 'ᴶ'),
    ('K', 'ᴷ'), ('L', 'ᴸ'), ('M', 'ᴹ'), ('N', 'ᴺ'), ('O', 'ᴼ'), ('P', 'ᴾ'), ('R', 'ᴿ'),
    ('T', 'ᵀ'), ('U', 'ᵁ'), ('V', 'ⱽ'), ('W', 'ᵂ'), ('°', '°'), ('∘', '°'), ('𝖳', 'ᵀ'), ('′', '′'), ('″', '″'),
    ('⊤', 'ᵀ'), ('β', 'ᵝ'), ('γ', 'ᵞ'), ('δ', 'ᵟ'), ('θ', 'ᶿ'), ('φ', 'ᵠ'), ('ϕ', 'ᵠ'),
    ('χ', 'ᵡ'), ('ε', 'ᵋ'), ('ι', 'ᶥ'), ('∗', '*'), ('*', '*'), ('†', '†'),
    // Already superscript: `e^{-x^2}` reads `e⁻ˣ²`, as it would be typed.
    ('⁰', '⁰'), ('¹', '¹'), ('²', '²'), ('³', '³'), ('⁴', '⁴'), ('⁵', '⁵'), ('⁶', '⁶'),
    ('⁷', '⁷'), ('⁸', '⁸'), ('⁹', '⁹'), ('ⁿ', 'ⁿ'), ('ⁱ', 'ⁱ'), ('ᵀ', 'ᵀ'), ('⁻', '⁻'),
    ('⁺', '⁺'),
];

/// Subscripts are sparser than superscripts -- no `b`, `c`, `d`, `f`, `g`, `w`, `y`, `z`, and no
/// capitals at all. That asymmetry is Unicode's.
const SUBSCRIPT: &[(char, char)] = &[
    ('0', '₀'), ('1', '₁'), ('2', '₂'), ('3', '₃'), ('4', '₄'), ('5', '₅'), ('6', '₆'),
    ('7', '₇'), ('8', '₈'), ('9', '₉'), ('+', '₊'), ('-', '₋'), ('−', '₋'), ('=', '₌'),
    ('(', '₍'), (')', '₎'), ('a', 'ₐ'), ('e', 'ₑ'), ('h', 'ₕ'), ('i', 'ᵢ'), ('j', 'ⱼ'),
    ('k', 'ₖ'), ('l', 'ₗ'), ('m', 'ₘ'), ('n', 'ₙ'), ('o', 'ₒ'), ('p', 'ₚ'), ('r', 'ᵣ'),
    ('s', 'ₛ'), ('t', 'ₜ'), ('u', 'ᵤ'), ('v', 'ᵥ'), ('x', 'ₓ'), ('β', 'ᵦ'), ('γ', 'ᵧ'),
    ('ρ', 'ᵨ'), ('φ', 'ᵩ'), ('ϕ', 'ᵩ'), ('χ', 'ᵪ'),
    ('₀', '₀'), ('₁', '₁'), ('₂', '₂'), ('₃', '₃'), ('₄', '₄'), ('₅', '₅'), ('₆', '₆'),
    ('₇', '₇'), ('₈', '₈'), ('₉', '₉'), ('ᵢ', 'ᵢ'), ('ⱼ', 'ⱼ'), ('ₖ', 'ₖ'), ('ₙ', 'ₙ'),
];

/// Every glyph the tables can emit, for the safety test.
#[cfg(test)]
pub(crate) fn all_glyphs() -> Vec<char> {
    let mut out = Vec::new();
    for t in [GREEK, ORD, BIN, REL, ARROWS, OPEN, CLOSE, PUNCT] {
        for (_, g) in t {
            out.extend(g.chars());
        }
    }
    for (_, g, _) in BIG {
        out.extend(g.chars());
    }
    for (_, t, _) in FUNCTIONS {
        out.extend(t.chars());
    }
    for (_, v) in SUPERSCRIPT.iter().chain(SUBSCRIPT.iter()) {
        out.push(*v);
    }
    out
}
