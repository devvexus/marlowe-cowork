//! Checks a script before anything runs (WF1, WF4), so the model gets every error back at once and
//! no agent starts on a script that cannot finish:
//!
//! - it parses as the workflow dialect (no `while`, no `load`, f-strings and type hints allowed);
//! - `META = {...}` exists and is a pure literal (no names, calls or f-strings), with a
//!   `description` and, if given, `phases` as a list of strings;
//! - `def main(...)` exists at the top level and takes at most one parameter (`args`);
//! - no function reaches itself (recursion), directly or through others;
//! - no lambda or nested `def` inside a loop or comprehension reads the loop variable without
//!   binding it (Starlark closures see the variable's last value, so every agent would get the
//!   same prompt);
//! - the top level compiles: every name it uses exists.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use starlark_syntax::codemap::CodeMap;
use starlark_syntax::lexer::{Lexer, Token};

/// One problem, with the line it is on when known (1-based).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
}

impl Diagnostic {
    pub fn new(line: Option<u32>, message: impl Into<String>) -> Self {
        Self { line, message: message.into() }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(l) => write!(f, "line {l}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// All problems, one per line, for handing back to the model.
pub fn render(diags: &[Diagnostic]) -> String {
    diags.iter().map(|d| format!("- {d}")).collect::<Vec<_>>().join("\n")
}

/// A script's `META`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
}

impl Meta {
    /// Reads the literal's JSON. `whenToUse` is accepted for CC-style scripts.
    pub fn from_json(v: &Value) -> Result<Meta, String> {
        let o = v.as_object().ok_or("META must be a dict literal")?;
        let s = |k: &str| -> Result<Option<String>, String> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(format!("META[\"{k}\"] must be a string")),
            }
        };
        let description = s("description")?.filter(|d| !d.trim().is_empty()).ok_or("META needs a \"description\": one line on what the workflow does")?;
        let phases = match o.get("phases") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|p| match p {
                    Value::String(s) => Ok(s.clone()),
                    Value::Object(m) => m.get("title").or_else(|| m.get("name")).and_then(Value::as_str).map(str::to_string).ok_or_else(|| "each META phase is a string".to_string()),
                    _ => Err("each META phase is a string".to_string()),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("META[\"phases\"] must be a list of strings".into()),
        };
        Ok(Meta { name: s("name")?, description, when_to_use: s("when_to_use")?.or(s("whenToUse")?), phases })
    }
}

/// What a valid script told us about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub meta: Meta,
    /// `main`'s positional parameters (0 or 1).
    pub main_params: usize,
}

/// The dialect workflow scripts are written in.
pub fn dialect() -> starlark::syntax::Dialect {
    starlark::syntax::Dialect {
        enable_def: true,
        enable_lambda: true,
        enable_load: false,
        enable_keyword_only_arguments: true,
        enable_types: starlark::syntax::DialectTypes::ParseOnly,
        enable_load_reexport: false,
        enable_top_level_stmt: true,
        enable_f_strings: true,
        ..starlark::syntax::Dialect::Extended
    }
}

/// A starlark error as a diagnostic: its first line number and its message without the snippet.
pub fn from_starlark(e: &starlark::Error) -> Diagnostic {
    let line = e.span().map(|s| s.resolve_span().begin.line as u32 + 1);
    let msg = e.without_diagnostic().to_string();
    let msg = msg.trim().to_string();
    Diagnostic { line, message: if msg.is_empty() { e.to_string() } else { msg } }
}

/// Parses and checks `source`; the compile check of the top level is done by the engine
/// ([`crate::engine::check`]), which calls this first.
pub fn static_checks(filename: &str, source: &str) -> Result<Validated, Vec<Diagnostic>> {
    if let Err(e) = starlark::syntax::AstModule::parse(filename, source.to_owned(), &dialect()) {
        let mut d = from_starlark(&e);
        let src_line = d.line.and_then(|l| source.lines().nth(l as usize - 1)).unwrap_or("");
        if looks_like_js(src_line) || looks_like_js(source) {
            d.message.push_str(
                " (workflow scripts are Starlark, Python-like: `def main(args):`, `x = agent(\"...\")`, `[f(x) for x in xs]`, no `const`, `await`, `=>`, `{ }` blocks or `;`)",
            );
        }
        return Err(vec![d]);
    }
    let toks = lex(filename, source);
    let mut diags = Vec::new();
    let meta = match meta_literal(&toks, source) {
        Ok(v) => match Meta::from_json(&v) {
            Ok(m) => Some(m),
            Err(e) => {
                diags.push(Diagnostic::new(meta_line(&toks, source), e));
                None
            }
        },
        Err(d) => {
            diags.push(d);
            None
        }
    };
    let main_params = match main_def(&toks, source) {
        Some(n) if n <= 1 => n,
        Some(_) => {
            diags.push(Diagnostic::new(None, "`def main(args):` takes one parameter (the JSON args), or none"));
            0
        }
        None => {
            diags.push(Diagnostic::new(None, "define `def main(args):` at the top level and `return` the workflow's result from it"));
            0
        }
    };
    diags.extend(recursion(&toks, source));
    diags.extend(late_binding(&toks, source));
    match meta {
        Some(meta) if diags.is_empty() => Ok(Validated { meta, main_params }),
        _ => Err(diags),
    }
}

fn looks_like_js(s: &str) -> bool {
    ["const ", "let ", "await ", "=>", "function ", "export ", "};", "===", "async ", "`"].iter().any(|k| s.contains(k))
}

// ---------------------------------------------------------------- tokens

struct Tok {
    start: usize,
    tok: Token,
    /// Bracket depth before this token (an opening bracket has the outer depth).
    depth: usize,
}

fn lex(filename: &str, source: &str) -> Vec<Tok> {
    let codemap = CodeMap::new(filename.to_string(), source.to_string());
    let mut out = Vec::new();
    let mut depth = 0usize;
    for lexeme in Lexer::new(source, &dialect(), codemap) {
        let Ok((start, tok, _end)) = lexeme else { break };
        if matches!(tok, Token::Comment(_)) {
            continue;
        }
        let closing = matches!(tok, Token::ClosingRound | Token::ClosingSquare | Token::ClosingCurly);
        if closing {
            depth = depth.saturating_sub(1);
        }
        let opening = matches!(tok, Token::OpeningRound | Token::OpeningSquare | Token::OpeningCurly);
        out.push(Tok { start, tok, depth });
        if opening {
            depth += 1;
        }
    }
    out
}

fn line_of(source: &str, offset: usize) -> u32 {
    source[..offset.min(source.len())].bytes().filter(|b| *b == b'\n').count() as u32 + 1
}

fn ident(t: &Token) -> Option<&str> {
    match t {
        Token::Identifier(s) => Some(s.as_str()),
        _ => None,
    }
}

fn starts_statement(toks: &[Tok], i: usize) -> bool {
    i == 0 || matches!(toks[i - 1].tok, Token::Newline | Token::Indent | Token::Dedent)
}

/// Indentation level (Indent - Dedent count) before each token.
fn levels(toks: &[Tok]) -> Vec<usize> {
    let mut lvl = 0usize;
    toks.iter()
        .map(|t| {
            let here = lvl;
            match t.tok {
                Token::Indent => lvl += 1,
                Token::Dedent => lvl = lvl.saturating_sub(1),
                _ => {}
            }
            here
        })
        .collect()
}

// ---------------------------------------------------------------- META

fn meta_start(toks: &[Tok]) -> Option<usize> {
    let lv = levels(toks);
    (0..toks.len()).find(|&i| {
        lv[i] == 0
            && toks[i].depth == 0
            && starts_statement(toks, i)
            && matches!(ident(&toks[i].tok), Some("META" | "meta"))
            && toks.get(i + 1).is_some_and(|t| matches!(t.tok, Token::Equal))
    })
}

fn meta_line(toks: &[Tok], source: &str) -> Option<u32> {
    meta_start(toks).map(|i| line_of(source, toks[i].start))
}

/// The JSON of `META = {...}`, checked to be a pure literal on the tokens and then evaluated alone.
fn meta_literal(toks: &[Tok], source: &str) -> Result<Value, Diagnostic> {
    let i = meta_start(toks).ok_or_else(|| {
        Diagnostic::new(None, "the script needs `META = {\"name\": ..., \"description\": ..., \"phases\": [...]}` at the top level")
    })?;
    let line = Some(line_of(source, toks[i].start));
    let mut j = i + 2;
    let begin = toks.get(j).map(|t| t.start).ok_or_else(|| Diagnostic::new(line, "META has no value"))?;
    let mut end = source.len();
    while j < toks.len() {
        let t = &toks[j];
        if t.depth == 0 && matches!(t.tok, Token::Newline) {
            end = t.start;
            break;
        }
        let ok = match &t.tok {
            Token::String(_) | Token::Int(_) | Token::Float(_) => true,
            Token::Identifier(s) => matches!(s.as_str(), "True" | "False" | "None"),
            Token::OpeningCurly
            | Token::ClosingCurly
            | Token::OpeningSquare
            | Token::ClosingSquare
            | Token::OpeningRound
            | Token::ClosingRound
            | Token::Comma
            | Token::Colon
            | Token::Minus => true,
            Token::FString(_) => {
                return Err(Diagnostic::new(Some(line_of(source, t.start)), "META must be a pure literal: no f-strings"));
            }
            _ => false,
        };
        if !ok {
            return Err(Diagnostic::new(
                Some(line_of(source, t.start)),
                "META must be a pure literal (strings, numbers, True/False/None, lists, dicts): no names, calls or expressions",
            ));
        }
        j += 1;
    }
    let text = format!("META = {}\n", &source[begin..end]);
    let ast = starlark::syntax::AstModule::parse("META", text, &dialect()).map_err(|e| Diagnostic::new(line, from_starlark(&e).message))?;
    let module = starlark::environment::Module::new();
    let globals = starlark::environment::Globals::standard();
    let mut eval = starlark::eval::Evaluator::new(&module);
    eval.eval_module(ast, &globals).map_err(|e| Diagnostic::new(line, from_starlark(&e).message))?;
    let v = module.get("META").ok_or_else(|| Diagnostic::new(line, "META did not evaluate"))?;
    v.to_json_value().map_err(|e| Diagnostic::new(line, format!("META is not JSON: {e}")))
}

// ---------------------------------------------------------------- main

/// `main`'s positional parameter count when `def main(` is at the top level.
fn main_def(toks: &[Tok], _source: &str) -> Option<usize> {
    let lv = levels(toks);
    let i = (0..toks.len()).find(|&i| {
        lv[i] == 0 && starts_statement(toks, i) && matches!(toks[i].tok, Token::Def) && toks.get(i + 1).and_then(|t| ident(&t.tok)) == Some("main")
    })?;
    let open = i + 2;
    if !toks.get(open).is_some_and(|t| matches!(t.tok, Token::OpeningRound)) {
        return Some(0);
    }
    let inner = toks[open].depth + 1;
    let mut n = 0usize;
    let mut expect_name = true;
    let mut j = open + 1;
    while j < toks.len() && !(toks[j].depth == toks[open].depth && matches!(toks[j].tok, Token::ClosingRound)) {
        let t = &toks[j];
        if t.depth == inner {
            match &t.tok {
                Token::Identifier(_) if expect_name => {
                    // A parameter with a default does not need an argument.
                    let has_default = toks.get(j + 1).is_some_and(|t| matches!(t.tok, Token::Equal));
                    if !has_default {
                        n += 1;
                    }
                    expect_name = false;
                }
                Token::Comma => expect_name = true,
                Token::Star | Token::StarStar => {
                    // *args / **kwargs / keyword-only marker: no more positionals required.
                    return Some(n);
                }
                _ => {}
            }
        }
        j += 1;
    }
    Some(n)
}

// ---------------------------------------------------------------- recursion

/// A function body: its name and token range (exclusive end).
struct Body {
    name: String,
    line: u32,
    from: usize,
    to: usize,
}

fn bodies(toks: &[Tok], source: &str) -> Vec<Body> {
    let lv = levels(toks);
    let mut out = Vec::new();
    for i in 0..toks.len() {
        // def NAME(...) ...: body
        if matches!(toks[i].tok, Token::Def) {
            let Some(name) = toks.get(i + 1).and_then(|t| ident(&t.tok)) else { continue };
            // The body starts after the `:` that ends the header (at the def's depth).
            let mut j = i + 2;
            while j < toks.len() && !(toks[j].depth == toks[i].depth && matches!(toks[j].tok, Token::Colon)) {
                j += 1;
            }
            let from = j + 1;
            let to = if toks.get(from).is_some_and(|t| matches!(t.tok, Token::Newline)) {
                // Indented block: ends when the level drops back to the def's level.
                let mut k = from + 1;
                while k < toks.len() && !(matches!(toks[k].tok, Token::Dedent) && lv[k] == lv[i] + 1) {
                    k += 1;
                }
                k
            } else {
                let mut k = from;
                while k < toks.len() && !matches!(toks[k].tok, Token::Newline) {
                    k += 1;
                }
                k
            };
            out.push(Body { name: name.to_string(), line: line_of(source, toks[i].start), from, to });
        }
        // NAME = lambda ...: body
        if starts_statement(toks, i)
            && toks.get(i + 1).is_some_and(|t| matches!(t.tok, Token::Equal))
            && toks.get(i + 2).is_some_and(|t| matches!(t.tok, Token::Lambda))
        {
            let Some(name) = ident(&toks[i].tok) else { continue };
            let mut k = i + 3;
            while k < toks.len() && !(toks[k].depth == toks[i].depth && matches!(toks[k].tok, Token::Newline)) {
                k += 1;
            }
            out.push(Body { name: name.to_string(), line: line_of(source, toks[i].start), from: i + 3, to: k });
        }
    }
    out
}

fn recursion(toks: &[Tok], source: &str) -> Vec<Diagnostic> {
    let bodies = bodies(toks, source);
    let names: BTreeSet<&str> = bodies.iter().map(|b| b.name.as_str()).collect();
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut line: BTreeMap<&str, u32> = BTreeMap::new();
    for b in &bodies {
        line.entry(b.name.as_str()).or_insert(b.line);
        let e = edges.entry(b.name.as_str()).or_default();
        for k in b.from..b.to.min(toks.len()) {
            if let Some(id) = ident(&toks[k].tok) {
                let attr = k > 0 && matches!(toks[k - 1].tok, Token::Dot);
                if !attr && names.contains(id) {
                    e.insert(id);
                }
            }
        }
    }
    // A cycle through any function, reported once per cycle (from its smallest name).
    let mut out = Vec::new();
    let mut reported: BTreeSet<Vec<&str>> = BTreeSet::new();
    for start in edges.keys().copied() {
        let mut path = vec![start];
        if let Some(cycle) = find_cycle(start, start, &edges, &mut path, &mut BTreeSet::new()) {
            let mut key = cycle.clone();
            key.sort();
            key.dedup();
            if reported.insert(key) {
                let shown = cycle.join(" -> ");
                out.push(Diagnostic::new(
                    line.get(start).copied(),
                    format!("recursion is not allowed in workflow scripts (so every script ends): {shown}; use a `for` loop over a bounded range instead"),
                ));
            }
        }
    }
    out
}

fn find_cycle<'a>(
    start: &'a str,
    at: &'a str,
    edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    path: &mut Vec<&'a str>,
    seen: &mut BTreeSet<&'a str>,
) -> Option<Vec<&'a str>> {
    for next in edges.get(at).into_iter().flatten().copied() {
        if next == start {
            let mut c = path.clone();
            c.push(start);
            return Some(c);
        }
        if seen.insert(next) {
            path.push(next);
            if let Some(c) = find_cycle(start, next, edges, path, seen) {
                return Some(c);
            }
            path.pop();
        }
    }
    None
}

// ---------------------------------------------------------------- late binding

/// Identifiers between `for` and `in` (the loop targets).
fn loop_targets(toks: &[Tok], for_at: usize) -> (BTreeSet<String>, usize) {
    let mut names = BTreeSet::new();
    let mut j = for_at + 1;
    while j < toks.len() && !(matches!(toks[j].tok, Token::In) && toks[j].depth == toks[for_at].depth) {
        if let Some(id) = ident(&toks[j].tok) {
            names.insert(id.to_string());
        }
        j += 1;
    }
    (names, j)
}

/// For a `lambda` at `at` that ends before `limit`: its parameter names and the range of its body.
fn lambda_parts(toks: &[Tok], at: usize, limit: usize) -> (BTreeSet<String>, usize, usize) {
    let d = toks[at].depth;
    let mut params = BTreeSet::new();
    let mut j = at + 1;
    let mut expect_name = true;
    while j < limit && !(toks[j].depth == d && matches!(toks[j].tok, Token::Colon)) {
        match &toks[j].tok {
            Token::Identifier(s) if expect_name && toks[j].depth == d => {
                params.insert(s.clone());
                expect_name = false;
            }
            Token::Comma if toks[j].depth == d => expect_name = true,
            _ => {}
        }
        j += 1;
    }
    let from = j + 1;
    let mut k = from;
    while k < limit {
        let t = &toks[k];
        // The closing bracket of the enclosing group has the outer depth.
        if t.depth < d || (t.depth == d && matches!(t.tok, Token::Comma | Token::For | Token::Newline)) {
            break;
        }
        k += 1;
    }
    (params, from, k)
}

fn reads_unbound(toks: &[Tok], from: usize, to: usize, vars: &BTreeSet<String>, bound: &BTreeSet<String>) -> Option<String> {
    (from..to).find_map(|k| {
        let id = ident(&toks[k].tok)?;
        let attr = k > 0 && matches!(toks[k - 1].tok, Token::Dot);
        let kwarg = toks.get(k + 1).is_some_and(|t| matches!(t.tok, Token::Equal)) && toks[k].depth > 0;
        (!attr && !kwarg && vars.contains(id) && !bound.contains(id)).then(|| id.to_string())
    })
}

fn late_binding_message(var: &str) -> String {
    format!(
        "a lambda inside a loop reads the loop variable `{var}`; Starlark closures see its last value, so every call would get the same `{var}`. Bind it as a default (`lambda {var}={var}: agent(...)`) or use `pipeline(items, lambda prev, item, index: agent(...))`"
    )
}

fn late_binding(toks: &[Tok], source: &str) -> Vec<Diagnostic> {
    let lv = levels(toks);
    let mut out = Vec::new();
    for (f, t) in toks.iter().enumerate() {
        if !matches!(t.tok, Token::For) {
            continue;
        }
        let (vars, in_at) = loop_targets(toks, f);
        if vars.is_empty() {
            continue;
        }
        if starts_statement(toks, f) {
            // Statement loop: the indented block after `:`.
            let mut j = in_at;
            while j < toks.len() && !(toks[j].depth == t.depth && matches!(toks[j].tok, Token::Colon)) {
                j += 1;
            }
            let block_from = j + 1;
            let mut end = block_from;
            if toks.get(block_from).is_some_and(|x| matches!(x.tok, Token::Newline)) {
                end = block_from + 1;
                while end < toks.len() && !(matches!(toks[end].tok, Token::Dedent) && lv[end] == lv[f] + 1) {
                    end += 1;
                }
            } else {
                while end < toks.len() && !matches!(toks[end].tok, Token::Newline) {
                    end += 1;
                }
            }
            for k in block_from..end {
                match toks[k].tok {
                    Token::Lambda => {
                        let (params, from, to) = lambda_parts(toks, k, end);
                        if let Some(v) = reads_unbound(toks, from, to, &vars, &params) {
                            out.push(Diagnostic::new(Some(line_of(source, toks[k].start)), late_binding_message(&v)));
                        }
                    }
                    Token::Def => {
                        if let Some(b) = bodies(&toks[k..end], source).into_iter().next() {
                            let params = def_params(toks, k);
                            if let Some(v) = reads_unbound(toks, k + b.from, (k + b.to).min(end), &vars, &params) {
                                out.push(Diagnostic::new(Some(line_of(source, toks[k].start)), late_binding_message(&v)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else {
            // Comprehension: the element expression sits between the opening bracket and `for`.
            let Some(open) = (0..f).rev().find(|&k| {
                toks[k].depth + 1 == t.depth && matches!(toks[k].tok, Token::OpeningSquare | Token::OpeningRound | Token::OpeningCurly)
            }) else {
                continue;
            };
            for k in open + 1..f {
                if matches!(toks[k].tok, Token::Lambda) {
                    let (params, from, to) = lambda_parts(toks, k, f);
                    if let Some(v) = reads_unbound(toks, from, to, &vars, &params) {
                        out.push(Diagnostic::new(Some(line_of(source, toks[k].start)), late_binding_message(&v)));
                    }
                }
            }
        }
    }
    out.dedup();
    out
}

fn def_params(toks: &[Tok], def_at: usize) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let open = def_at + 2;
    if !toks.get(open).is_some_and(|t| matches!(t.tok, Token::OpeningRound)) {
        return out;
    }
    let inner = toks[open].depth + 1;
    let mut expect = true;
    let mut j = open + 1;
    while j < toks.len() && !(toks[j].depth == toks[open].depth && matches!(toks[j].tok, Token::ClosingRound)) {
        if toks[j].depth == inner {
            match &toks[j].tok {
                Token::Identifier(s) if expect => {
                    out.insert(s.clone());
                    expect = false;
                }
                Token::Comma => expect = true,
                _ => {}
            }
        }
        j += 1;
    }
    out
}
