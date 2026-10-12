//! Path globs for rule `paths:` and permission-style patterns: `**` (any number of segments),
//! `*`, `?`, `[abc]` / `[!a-z]`, `{a,b}`. A pattern without a `/` matches at any depth (as in
//! `.gitignore`). Separators are normalised to `/`; matching ignores case on Windows.

/// Whether `path` (relative, `/` or `\` separated) matches `pattern`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        let s = s.trim_start_matches("./").to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    let path = norm(path);
    let path = path.trim_start_matches('/');
    expand_braces(&norm(pattern.trim())).iter().any(|p| {
        let p = p.trim_start_matches('/');
        let p = if p.contains('/') { p.to_string() } else { format!("**/{p}") };
        let pat: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match_segments(&pat, &segs)
    })
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| match_segments(rest, &segs[i..])),
        Some((p, rest)) => match segs.split_first() {
            Some((s, srest)) => match_one(p.as_bytes(), s.as_bytes()) && match_segments(rest, srest),
            None => false,
        },
    }
}

/// One segment: `*`, `?`, `[...]`.
fn match_one(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some(pi);
                    mark = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, len)) = class(&p[pi..], s[si]) {
                        if ok {
                            pi += len;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(st) => {
                pi = st + 1;
                mark += 1;
                si = mark;
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// A `[...]` class at the start of `p`: (matches `c`, length of the class).
fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let end = p.iter().skip(1).position(|&b| b == b']')? + 1;
    let body = &p[1..end];
    let (neg, body) = match body.first() {
        Some(b'!') | Some(b'^') => (true, &body[1..]),
        _ => (false, body),
    };
    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == b'-' {
            hit |= body[i] <= c && c <= body[i + 2];
            i += 3;
        } else {
            hit |= body[i] == c;
            i += 1;
        }
    }
    Some((hit != neg, end + 1))
}

/// `a{b,c}d` -> `abd`, `acd` (nested braces expand recursively).
fn expand_braces(p: &str) -> Vec<String> {
    let Some(open) = p.find('{') else { return vec![p.to_string()] };
    let mut depth = 0;
    let mut close = None;
    for (i, c) in p[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return vec![p.to_string()] };
    let (head, body, tail) = (&p[..open], &p[open + 1..close], &p[close + 1..]);
    let mut parts = Vec::new();
    let (mut d, mut start) = (0, 0);
    for (i, c) in body.char_indices() {
        match c {
            '{' => d += 1,
            '}' => d -= 1,
            ',' if d == 0 => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts.iter().flat_map(|part| expand_braces(&format!("{head}{part}{tail}"))).collect()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn globs_match_like_claude_code_rules() {
        assert!(glob_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(glob_match("src/**/*.rs", "src/c.rs"));
        assert!(!glob_match("src/**/*.rs", "tests/c.rs"));
        assert!(glob_match("*.md", "docs/deep/readme.md"), "no slash: any depth");
        assert!(glob_match("src/*.{ts,tsx}", "src/app.tsx"));
        assert!(!glob_match("src/*.{ts,tsx}", "src/a/app.tsx"));
        assert!(glob_match("api/v[0-9]/*.py", "api/v2/x.py"));
        assert!(glob_match("src\\**\\x.rs", "src/q/x.rs"));
        assert!(glob_match("**", "anything/at/all"));
        assert!(glob_match("Makefile", "sub/Makefile"));
    }
}
