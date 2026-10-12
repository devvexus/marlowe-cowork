//! The one-time converter for Claude Code's JavaScript workflows (`.claude\workflows\*.js`,
//! 9.2-q): their `meta` is read here so the listing can show it, and `/workflow convert <name>`
//! hands the model [`conversion_prompt`], which writes `.marlowe\workflows\<name>.star` and
//! checks it with `workflow{script_path, validate_only:true}` until it is clean. The JavaScript
//! file is never modified and never run.

use serde_json::Value;

/// The `meta` object of a CC workflow (`export const meta = {...}`) as JSON, when it is a literal.
pub fn js_meta(source: &str) -> Option<Value> {
    let at = source.find("meta")?;
    let rest = &source[at..];
    let eq = rest.find('=')?;
    let after = rest[eq + 1..].trim_start();
    if !after.starts_with('{') {
        return None;
    }
    let obj = balanced(after)?;
    serde_json::from_str(&js_object_to_json(obj)?).ok()
}

/// The `{...}` at the start of `s`, braces matched outside strings.
fn balanced(s: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// A JavaScript object literal (unquoted keys, `'...'` and backtick strings without `${}`,
/// trailing commas, comments) as JSON text; `None` when it holds anything else.
fn js_object_to_json(src: &str) -> Option<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            '"' | '\'' | '`' => {
                let q = c;
                let mut s = String::new();
                i += 1;
                while i < chars.len() && chars[i] != q {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        s.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if q == '`' && chars[i] == '$' && chars.get(i + 1) == Some(&'{') {
                        return None;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                out.push_str(&serde_json::to_string(&s).ok()?);
                i += 1;
            }
            ',' => {
                // Drop a trailing comma.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if !matches!(chars.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let mut w = String::new();
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    w.push(chars[i]);
                    i += 1;
                }
                match w.as_str() {
                    "true" | "false" | "null" => out.push_str(&w),
                    _ => {
                        // A key: the next non-space is `:`.
                        let mut j = i;
                        while j < chars.len() && chars[j].is_whitespace() {
                            j += 1;
                        }
                        if chars.get(j) != Some(&':') {
                            return None;
                        }
                        out.push_str(&serde_json::to_string(&w).ok()?);
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// What the model is told to do for `/workflow convert <name>`.
pub fn conversion_prompt(name: &str, js_path: &str, target: &str) -> String {
    format!(
        "Convert the Claude Code JavaScript workflow `{name}` ({js_path}) into a Marlowe Starlark workflow at {target}. \
Read the JavaScript first; do not modify or run it.\n\
Starlark is Python-like. Map it this way:\n\
- `export const meta = {{...}}` -> `META = {{...}}` (a pure literal; `whenToUse` may stay or become `when_to_use`).\n\
- The script body -> `def main(args):` returning the result; `args` is the same JSON.\n\
- `await agent(prompt, {{label, phase, schema, model, effort, agentType, isolation}})` -> `agent(prompt, label=..., phase=..., schema=..., model=..., effort=..., agent_type=..., isolation=...)` (no await).\n\
- `await parallel([() => agent(a), () => agent(b)])` -> `parallel([lambda: agent(a), lambda: agent(b)])`. In a loop or comprehension bind the variable: `[lambda f=f: agent(f) for f in files]`, or use `pipeline`.\n\
- `pipeline(items, (prev, item, i) => ...)` -> `pipeline(items, lambda prev, item, i: ...)`; a multi-line stage becomes a `def`.\n\
- Template literals -> f-strings (`f\"Review {{path}}\"`); `JSON.stringify(x)` -> `json.encode(x)`; `JSON.parse` -> `json.decode`.\n\
- `x.length` -> `len(x)`, `arr.map(f)` -> `[f(v) for v in arr]`, `arr.filter(f)` -> `[v for v in arr if f(v)]`, `null` -> `None`, `===` -> `==`, `&&`/`||`/`!` -> `and`/`or`/`not`.\n\
- `while` loops and recursion do not exist: use `for i in range(N):` with `break`; `try`/`catch` does not exist: a failed agent returns `None`, check for it.\n\
- `budget.total`, `budget.spent()`, `budget.remaining()`, `phase()`, `log()` and `workflow(name, args)` keep their names.\n\
Write the file with the write tool, then call `workflow{{script_path:\"{target}\", validate_only:true}}` and fix every error it reports until it says the script is valid. \
Then tell the user the workflow can run as `/workflow {name}`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_cc_meta_literal_reads_as_json() {
        let js = "// review\nexport const meta = {\n  name: 'portfolio-assess',\n  description: \"Assess each system\", // why\n  whenToUse: `for portfolios`,\n  phases: [{title: 'collect'}, 'score',],\n};\nconst r = await agent(`x ${y}`);\n";
        assert_eq!(
            js_meta(js).unwrap(),
            json!({"name": "portfolio-assess", "description": "Assess each system", "whenToUse": "for portfolios", "phases": [{"title": "collect"}, "score"]})
        );
        assert!(js_meta("export const meta = { name: makeName() };").is_none());
        assert!(js_meta("export const meta = { d: `x ${y}` };").is_none());
    }
}
