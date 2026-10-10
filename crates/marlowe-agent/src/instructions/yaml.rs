//! A lenient YAML subset for Claude Code-format frontmatter (agents, commands, skills, rules,
//! output styles). It covers what those files use in practice: block maps and sequences nested
//! by indentation (including sequences of maps, as in a `hooks:` block), flow sequences and maps,
//! single- and double-quoted scalars, `|` and `>` block scalars, and comments. Scalars stay
//! strings; callers read booleans and numbers with [`Yaml::as_bool`] and [`Yaml::as_u32`].
//! Anchors, aliases, tags and multi-document streams are not supported: such text is read as a
//! plain string, never executed.

/// A parsed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Null,
    Str(String),
    Seq(Vec<Yaml>),
    /// Keys in file order; a repeated key keeps its last value.
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    /// The value under `key` in a map.
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(m) => m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The first present key among `keys` (spelling variants such as `allowed-tools` /
    /// `allowedTools`).
    pub fn get_any(&self, keys: &[&str]) -> Option<&Yaml> {
        keys.iter().find_map(|k| self.get(k))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }

    /// `true/false/yes/no/on/off`, case-insensitive.
    pub fn as_bool(&self) -> Option<bool> {
        match self.as_str()?.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" => Some(true),
            "false" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_str()?.trim().parse().ok()
    }

    /// A list of strings: a sequence of scalars, or one scalar split on top-level commas
    /// (`tools: Read, Grep, Agent(a, b)` keeps `Agent(a, b)` whole). Empty for null.
    pub fn list(&self) -> Vec<String> {
        match self {
            Yaml::Null => Vec::new(),
            Yaml::Str(s) => split_top_level(s, ',').into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            Yaml::Seq(items) => items.iter().filter_map(|i| i.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).collect(),
            Yaml::Map(_) => Vec::new(),
        }
    }

    /// Map keys in file order (empty for a non-map).
    pub fn keys(&self) -> Vec<&str> {
        match self {
            Yaml::Map(m) => m.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// The value as JSON (scalars as strings; `true`/`false` and numbers keep their type), for
    /// blocks handed on to other tracks (an agent's `hooks:`).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Yaml::Null => Value::Null,
            Yaml::Str(s) => match s.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => s
                    .parse::<i64>()
                    .map(Value::from)
                    .or_else(|_| s.parse::<f64>().map_err(|_| ()).and_then(|f| serde_json::Number::from_f64(f).map(Value::Number).ok_or(())))
                    .unwrap_or_else(|_| Value::String(s.clone())),
            },
            Yaml::Seq(items) => Value::Array(items.iter().map(Yaml::to_json).collect()),
            Yaml::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }
}

/// Splits `---` frontmatter from the body. Accepts a BOM and CRLF line ends. Without a closed
/// frontmatter block the whole text is the body.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t.strip_prefix("---\r\n").or_else(|| t.strip_prefix("---\n")) else {
        return (None, t);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).trim_end() == "---" {
            let front = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(front), body);
        }
        offset += line.len();
    }
    (None, t)
}

/// Parses frontmatter text (without the `---` fences). An empty text is an empty map.
pub fn parse(src: &str) -> Result<Yaml, String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut p = Parser { lines };
    let (v, next) = p.block(0, 0)?;
    if let Some(j) = p.next_content(next) {
        return Err(format!("line {}: unexpected indentation", j + 1));
    }
    Ok(match v {
        Yaml::Null => Yaml::Map(Vec::new()),
        v => v,
    })
}

/// Parses the frontmatter of `text`; `(Map(empty), whole text)` when it has none.
pub fn parse_document(text: &str) -> Result<(Yaml, &str), String> {
    match split_frontmatter(text) {
        (Some(front), body) => Ok((parse(front)?, body)),
        (None, body) => Ok((Yaml::Map(Vec::new()), body)),
    }
}

struct Parser<'a> {
    lines: Vec<&'a str>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn is_dash(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl<'a> Parser<'a> {
    fn next_content(&self, mut i: usize) -> Option<usize> {
        while i < self.lines.len() {
            if !is_blank_or_comment(self.lines[i]) {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn text(&self, i: usize) -> &'a str {
        self.lines[i].trim_start_matches(' ').trim_end()
    }

    /// A node starting at the first content line at or after `i` whose indent is at least `min`.
    fn block(&mut self, i: usize, min: usize) -> Result<(Yaml, usize), String> {
        let Some(j) = self.next_content(i) else { return Ok((Yaml::Null, self.lines.len())) };
        let ind = indent_of(self.lines[j]);
        if ind < min {
            return Ok((Yaml::Null, j));
        }
        let t = self.text(j);
        if is_dash(t) {
            return self.seq(j, ind);
        }
        if split_key(t).is_some() {
            return self.map(j, ind, None);
        }
        // A plain scalar, possibly folded over more-indented lines.
        let mut s = inline(t)?;
        let mut k = j + 1;
        while let Some(n) = self.next_content(k) {
            if indent_of(self.lines[n]) < ind {
                break;
            }
            if let Yaml::Str(cur) = &mut s {
                cur.push(' ');
                cur.push_str(self.text(n));
            }
            k = n + 1;
        }
        Ok((s, k))
    }

    /// A block map whose keys sit at column `ind`. `first` overrides the first line's text (a map
    /// that starts on a sequence item's `- ` line).
    fn map(&mut self, start: usize, ind: usize, first: Option<&'a str>) -> Result<(Yaml, usize), String> {
        let mut out: Vec<(String, Yaml)> = Vec::new();
        let mut i = start;
        let mut first = first;
        loop {
            let (line_no, t) = match first.take() {
                Some(t) => (start, t),
                None => {
                    let Some(j) = self.next_content(i) else { break };
                    if indent_of(self.lines[j]) != ind {
                        if indent_of(self.lines[j]) > ind {
                            return Err(format!("line {}: unexpected indentation", j + 1));
                        }
                        i = j;
                        break;
                    }
                    (j, self.text(j))
                }
            };
            let Some((key, rest)) = split_key(t) else {
                i = line_no;
                break;
            };
            let rest = strip_comment(rest).trim();
            let (value, next) = if rest.is_empty() {
                match self.next_content(line_no + 1) {
                    Some(k) if indent_of(self.lines[k]) > ind => self.block(k, ind + 1)?,
                    Some(k) if indent_of(self.lines[k]) == ind && is_dash(self.text(k)) => self.seq(k, ind)?,
                    _ => (Yaml::Null, line_no + 1),
                }
            } else if rest.starts_with('|') || rest.starts_with('>') {
                self.block_scalar(line_no, ind, rest)
            } else if (rest.starts_with('[') && !balanced(rest, '[', ']')) || (rest.starts_with('{') && !balanced(rest, '{', '}')) {
                // A flow collection continued over the following lines.
                let (open, close) = if rest.starts_with('[') { ('[', ']') } else { ('{', '}') };
                let mut joined = rest.to_string();
                let mut k = line_no + 1;
                while k < self.lines.len() && !balanced(&joined, open, close) {
                    joined.push(' ');
                    joined.push_str(self.lines[k].trim());
                    k += 1;
                }
                (inline(&joined)?, k)
            } else {
                (inline(rest)?, line_no + 1)
            };
            out.retain(|(k, _)| *k != key);
            out.push((key, value));
            i = next;
        }
        Ok((Yaml::Map(out), i))
    }

    fn seq(&mut self, start: usize, ind: usize) -> Result<(Yaml, usize), String> {
        let mut out = Vec::new();
        let mut i = start;
        while let Some(j) = self.next_content(i) {
            let line = self.lines[j];
            if indent_of(line) != ind || !is_dash(self.text(j)) {
                i = j;
                break;
            }
            let after = &self.text(j)[1..];
            let rest = after.trim_start();
            let col = ind + 1 + (after.len() - rest.len());
            let rest = rest.trim_end();
            let (item, next) = if rest.is_empty() || rest.starts_with('#') {
                self.block(j + 1, ind + 1)?
            } else if is_dash(rest) {
                // `- - x`: a nested sequence on one line; read the inner item only.
                (Yaml::Seq(vec![inline(rest[1..].trim())?]), j + 1)
            } else if split_key(rest).is_some() && !rest.starts_with('[') && !rest.starts_with('{') {
                self.map(j, col, Some(rest))?
            } else if rest.starts_with('|') || rest.starts_with('>') {
                self.block_scalar(j, ind, rest)
            } else {
                (inline(strip_comment(rest).trim())?, j + 1)
            };
            out.push(item);
            i = next;
            if i >= self.lines.len() {
                break;
            }
        }
        Ok((Yaml::Seq(out), i))
    }

    /// `|` / `>` with optional `-`/`+` chomping, content more indented than `ind`.
    fn block_scalar(&self, line_no: usize, ind: usize, header: &str) -> (Yaml, usize) {
        let fold = header.starts_with('>');
        let strip = header.contains('-');
        let keep = header.contains('+');
        let mut k = line_no + 1;
        let mut body: Vec<&str> = Vec::new();
        let mut block_ind: Option<usize> = None;
        while k < self.lines.len() {
            let l = self.lines[k];
            if l.trim().is_empty() {
                body.push("");
                k += 1;
                continue;
            }
            let li = indent_of(l);
            if li <= ind {
                break;
            }
            let bi = *block_ind.get_or_insert(li);
            body.push(if l.len() >= bi { &l[bi.min(li)..] } else { l.trim_start() });
            k += 1;
        }
        // Trailing blank lines belong to the chomping, not the content.
        let mut trailing = 0;
        while body.last() == Some(&"") {
            body.pop();
            trailing += 1;
        }
        let mut s = if fold {
            let mut out = String::new();
            for (n, l) in body.iter().enumerate() {
                if l.is_empty() {
                    out.push('\n');
                } else {
                    if n > 0 && !body[n - 1].is_empty() && !out.ends_with('\n') {
                        out.push(' ');
                    }
                    out.push_str(l.trim_end());
                }
            }
            out
        } else {
            body.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
        };
        if !strip && !s.is_empty() {
            s.push('\n');
            if keep {
                s.push_str(&"\n".repeat(trailing));
            }
        }
        (Yaml::Str(s), k)
    }
}

/// `key: rest` or `key:` (rest empty). Keys may be quoted. `None` when the line is not a map
/// entry (a URL like `http://x` is a scalar: the colon must be followed by a space or the end).
fn split_key(t: &str) -> Option<(String, &str)> {
    if t.starts_with('"') || t.starts_with('\'') {
        let q = t.chars().next()?;
        let end = t[1..].find(q)? + 1;
        let key = t[1..end].to_string();
        let after = t[end + 1..].trim_start();
        let rest = after.strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        return Some((key, rest));
    }
    if t.starts_with('[') || t.starts_with('{') || t.starts_with('#') || is_dash(t) {
        return None;
    }
    let mut depth = 0i32;
    for (i, c) in t.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth <= 0 => {
                let rest = &t[i + 1..];
                if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                    let key = t[..i].trim();
                    if key.is_empty() {
                        return None;
                    }
                    return Some((key.to_string(), rest));
                }
            }
            ' ' if t[i..].trim_start().starts_with('#') => return None,
            _ => {}
        }
    }
    None
}

/// Drops a ` # comment` outside quotes.
fn strip_comment(s: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && (prev == ' ' || prev == '\t' || i == 0) => return &s[..i],
            None => {}
        }
        prev = c;
    }
    s
}

fn balanced(s: &str, open: char, close: char) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == open => depth += 1,
            None if c == close => depth -= 1,
            None => {}
        }
    }
    depth <= 0
}

/// Splits on `sep` outside quotes and brackets.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if matches!(c, '(' | '[' | '{') => depth += 1,
            None if matches!(c, ')' | ']' | '}') => depth -= 1,
            None if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            None => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// An inline value: a flow sequence or map, a quoted scalar, or a plain scalar.
fn inline(s: &str) -> Result<Yaml, String> {
    let s = s.trim();
    if s.is_empty() || s == "~" || s == "null" {
        return Ok(Yaml::Null);
    }
    if let Some(inner) = s.strip_prefix('[') {
        let inner = inner.trim_end().strip_suffix(']').ok_or_else(|| format!("unclosed `[` in `{s}`"))?;
        if inner.trim().is_empty() {
            return Ok(Yaml::Seq(Vec::new()));
        }
        return split_top_level(inner, ',').into_iter().filter(|p| !p.trim().is_empty()).map(inline).collect::<Result<Vec<_>, _>>().map(Yaml::Seq);
    }
    if let Some(inner) = s.strip_prefix('{') {
        let inner = inner.trim_end().strip_suffix('}').ok_or_else(|| format!("unclosed `{{` in `{s}`"))?;
        let mut out = Vec::new();
        for part in split_top_level(inner, ',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match split_key(part) {
                Some((k, v)) => out.push((k, inline(v)?)),
                None => out.push((part.to_string(), Yaml::Null)),
            }
        }
        return Ok(Yaml::Map(out));
    }
    if s.starts_with('"') {
        return Ok(Yaml::Str(double_quoted(s)));
    }
    if let Some(inner) = s.strip_prefix('\'') {
        let end = find_single_close(inner).unwrap_or(inner.len());
        return Ok(Yaml::Str(inner[..end].replace("''", "'")));
    }
    Ok(Yaml::Str(strip_comment(s).trim().to_string()))
}

fn find_single_close(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

fn double_quoted(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Yaml {
        Yaml::Str(v.into())
    }

    #[test]
    fn scalars_lists_and_quotes() {
        let y = parse("name: code-reviewer\ntools: Read, Grep, Agent(a, b)\nmodel: \"sonnet\"\nq: 'it''s'\nempty:\n# c\nflag: true # yes").unwrap();
        assert_eq!(y.get("name"), Some(&s("code-reviewer")));
        assert_eq!(y.get("tools").unwrap().list(), ["Read", "Grep", "Agent(a, b)"]);
        assert_eq!(y.get("model"), Some(&s("sonnet")));
        assert_eq!(y.get("q"), Some(&s("it's")));
        assert_eq!(y.get("empty"), Some(&Yaml::Null));
        assert_eq!(y.get("flag").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn flow_and_block_sequences_and_nested_maps() {
        let y = parse(
            "allowed-tools: [\"Bash\", Read]\nlist:\n  - Read\n  - Bash(ls *)\ncompact:\n- a\n- b\nbudget: {tokens: 600000, steps: 80}\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"./check.sh\"\n",
        )
        .unwrap();
        assert_eq!(y.get("allowed-tools").unwrap().list(), ["Bash", "Read"]);
        assert_eq!(y.get("list").unwrap().list(), ["Read", "Bash(ls *)"]);
        assert_eq!(y.get("compact").unwrap().list(), ["a", "b"]);
        assert_eq!(y.get("budget").unwrap().get("steps"), Some(&s("80")));
        let hook = &y.get("hooks").unwrap().get("PreToolUse").unwrap();
        let Yaml::Seq(items) = hook else { panic!("{hook:?}") };
        assert_eq!(items[0].get("matcher"), Some(&s("Bash")));
        let inner = items[0].get("hooks").unwrap();
        assert_eq!(inner.to_json(), serde_json::json!([{"type": "command", "command": "./check.sh"}]));
    }

    #[test]
    fn block_scalars_keep_or_fold_lines() {
        let y = parse("description: |\n  line one\n  line two\n\nnext: >-\n  folded\n  text\n").unwrap();
        assert_eq!(y.get("description"), Some(&s("line one\nline two\n")));
        assert_eq!(y.get("next"), Some(&s("folded text")));
    }

    #[test]
    fn frontmatter_splits_with_crlf_and_bom() {
        let (f, b) = split_frontmatter("\u{feff}---\r\nname: x\r\n---\r\nbody\r\n");
        assert_eq!(f, Some("name: x\r\n"));
        assert_eq!(b, "body\r\n");
        assert_eq!(split_frontmatter("no front"), (None, "no front"));
        assert_eq!(split_frontmatter("---\nunclosed"), (None, "---\nunclosed"));
    }

    #[test]
    fn urls_and_colons_inside_values_stay_scalars() {
        let y = parse("url: https://example.com/a:b\ndescription: Use when: the user asks").unwrap();
        assert_eq!(y.get("url"), Some(&s("https://example.com/a:b")));
        assert_eq!(y.get("description"), Some(&s("Use when: the user asks")));
    }
}
