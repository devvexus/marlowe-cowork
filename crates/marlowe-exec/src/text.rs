//! Text helpers shared by the file tools: numbering, ranges, glob matching, bounded cuts, and
//! the secret-shape checks behind the write guard (design 2.1 B6). The `read` prefix itself lives
//! in `marlowe_code::edit::numbering`, re-exported here.

pub use marlowe_code::edit::engine::MAX_EDIT_SITES_NAMED;
pub use marlowe_code::edit::numbering::{count_lines, line_at, number_lines, strip_line_numbers, LINE_NUMBER_WIDTH};

/// Characters of one line `read` returns before cutting it (minified files, data dumps).
pub const MAX_LINE_CHARS: usize = 2_000;

/// How many lines one `read` returns when no range is given.
pub const READ_WINDOW_LINES: usize = 2_000;

/// Byte ceiling on one `read` window, measured on the numbered text actually returned.
pub const READ_WINDOW_BYTES: usize = 32 * 1024;

/// The most one text window takes on a large context window.
pub const READ_WINDOW_BYTES_MAX: usize = 100 * 1024;

/// How much of a file `read`/`grep` hold in memory.
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Consecutive numbered lines in `content` that make it a pasted `read` window.
pub const NUMBERED_CONTENT_LINES: usize = 2;

/// Files one walk enumerates.
pub const WALK_FILE_CAP: usize = 2_000;

/// Directories a walk does not descend into unless named as the base.
pub const WALK_SKIP: [&str; 6] = [".git", "target", "node_modules", ".venv", "__pycache__", "dist"];

/// Result lines (matches plus context) `grep` emits before it only counts.
pub const MAX_MATCH_LINES: usize = 200;

/// Bytes kept of one emitted `grep` line.
pub const MAX_MATCH_LINE_BYTES: usize = 512;

/// Largest `grep` `context`; larger values are refused.
pub const MAX_GREP_CONTEXT: i64 = 20;

/// Regex compile ceiling (NFA and lazy DFA).
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Paths a notice names before "and N more".
pub const NAMES_IN_A_NOTICE: usize = 10;

/// Tokens estimate for `bytes` of text (3 bytes per token, as the context assembler does).
pub fn estimate_tokens_of(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX)
}

pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// One emitted line, bounded, with the cut marked.
pub fn clip_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_string();
    }
    let cut = floor_boundary(line, MAX_MATCH_LINE_BYTES);
    format!("{}…[line continues, {} more bytes]", &line[..cut], line.len() - cut)
}

/// Head and tail of `text` with the omission stated, when it exceeds `max` bytes.
pub fn head_and_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head_end = floor_boundary(text, max / 2);
    let tail_start = ceil_boundary(text, text.len() - max / 2);
    format!(
        "{}\n…[{} bytes omitted from the middle]…\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

pub fn take_lines(text: &str, n: usize) -> String {
    text.split_inclusive('\n').take(n).collect()
}

/// Cut to at most `cap` bytes on a line boundary; returns the text and the whole lines kept.
pub fn cut_to_whole_lines(text: &str, cap: usize) -> (String, usize) {
    let hard = floor_boundary(text, cap);
    let end = match text[..hard].rfind('\n') {
        Some(i) => i + 1,
        None => hard,
    };
    let end = if end == 0 { hard } else { end };
    (text[..end].to_string(), text[..end].lines().count())
}

/// `first-last`, 1-based inclusive. Every malformed range is an error, never "the whole file".
/// Returns the text and the first line's absolute number.
pub fn slice_lines(text: &str, range: &str) -> Result<(String, usize), String> {
    let malformed = |detail: &str| {
        Err(format!(
            "`range` must be `first-last`, 1-based and inclusive, e.g. \"20-60\" — {detail}. Omit \
             `range` to read the whole file."
        ))
    };
    let Some((a, b)) = range.split_once('-') else {
        return malformed(&format!("\"{range}\" has no `-`"));
    };
    let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
        return malformed(&format!("\"{range}\" is not two whole numbers"));
    };
    if a == 0 {
        return malformed("lines are numbered from 1, not 0");
    }
    if b < a {
        return malformed(&format!("\"{range}\" runs backwards; did you mean \"{b}-{a}\"?"));
    }
    Ok((
        text.split_inclusive('\n')
            .skip(a - 1)
            .take((b - a).saturating_add(1))
            .collect::<String>(),
        a,
    ))
}

/// Case-insensitive glob over `/`-separated paths. `*` and `?` stay inside one segment; `**`
/// spans any number of segments, and `**/` also matches zero of them (so `**/Cargo.toml` matches
/// `Cargo.toml`); `{a,b}` alternates (not nested). `\` in either side is read as `/`.
pub fn glob_match(pattern: &str, subject: &str) -> bool {
    let s: Vec<char> = subject.to_lowercase().replace('\\', "/").chars().collect();
    expand_braces(&pattern.to_lowercase().replace('\\', "/"))
        .iter()
        .any(|p| match_chars(&p.chars().collect::<Vec<_>>(), &s))
}

/// `a{b,c}d{e,f}` -> the four alternatives. An unclosed `{` is literal.
fn expand_braces(p: &str) -> Vec<String> {
    let Some(open) = p.find('{') else { return vec![p.to_string()] };
    let Some(len) = p[open..].find('}') else { return vec![p.to_string()] };
    let (head, body, tail) = (&p[..open], &p[open + 1..open + len], &p[open + len + 1..]);
    let tails = expand_braces(tail);
    body.split(',')
        .flat_map(|alt| tails.iter().map(move |t| format!("{head}{alt}{t}")))
        .take(256)
        .collect()
}

/// One pattern token.
#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Lit(char),
    /// `?`: one character other than `/`.
    One,
    /// `*`: any run without `/`.
    Star,
    /// `**`: any run, `/` included.
    GlobStar,
    /// `**/`: zero or more whole segments (empty, or any run ending in `/`).
    Segments,
}

fn tokens(p: &[char]) -> Vec<Tok> {
    let mut out = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        out.push(match p[i] {
            '*' if p.get(i + 1) == Some(&'*') => {
                i += 1;
                if p.get(i + 1) == Some(&'/') {
                    i += 1;
                    Tok::Segments
                } else {
                    Tok::GlobStar
                }
            }
            '*' => Tok::Star,
            '?' => Tok::One,
            c => Tok::Lit(c),
        });
        i += 1;
    }
    out
}

/// Dynamic programme over (token, subject) positions: O(|p|·|s|), no backtracking blowup.
fn match_chars(p: &[char], s: &[char]) -> bool {
    let n = s.len();
    // `next[j]`: the tokens after the current one match `s[j..]`.
    let mut next = vec![false; n + 1];
    next[n] = true;
    for tok in tokens(p).into_iter().rev() {
        let mut cur = vec![false; n + 1];
        let mut seg_tail = false;
        for j in (0..=n).rev() {
            cur[j] = match tok {
                Tok::Lit(c) => j < n && s[j] == c && next[j + 1],
                Tok::One => j < n && s[j] != '/' && next[j + 1],
                Tok::Star => next[j] || (j < n && s[j] != '/' && cur[j + 1]),
                Tok::GlobStar => next[j] || (j < n && cur[j + 1]),
                Tok::Segments => {
                    // Some k > j with s[k-1] == '/' and next[k].
                    seg_tail = seg_tail || (j < n && s[j] == '/' && next[j + 1]);
                    next[j] || seg_tail
                }
            };
        }
        next = cur;
    }
    next[0]
}

/// Why `replacing` did not match, phrased so the model can act on it.
pub fn replacing_miss(existing: &str, replacing: &str) -> String {
    if let Some(stripped) = strip_line_numbers(replacing) {
        return match existing.find(&stripped) {
            Some(at) => format!(
                "`replacing` carries the line-number prefix `read` printed ({LINE_NUMBER_WIDTH} \
                 characters, then a tab, in front of every line). `edit` matches the file byte for \
                 byte, so strip them. Stripped, your text IS in the file, starting at line {}.",
                line_at(existing, at)
            ),
            None => format!(
                "`replacing` carries the line-number prefix `read` printed, which `edit` never \
                 accepts — strip it. Even stripped the text was not found, so `read` the file \
                 again and copy the snippet from what comes back. The file is {} bytes, {} lines.",
                existing.len(),
                count_lines(existing)
            ),
        };
    }
    if existing.contains("\r\n") && !replacing.contains('\r') && replacing.contains('\n') {
        let unix = existing.replace("\r\n", "\n");
        if let Some(at) = unix.find(replacing) {
            return format!(
                "`replacing` was not found because the file uses CRLF line endings and your text \
                 uses LF. Apart from that it is there, at line {}. `read` the file and copy the \
                 snippet from what comes back rather than retyping it.",
                line_at(&unix, at)
            );
        }
    }
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    if !replacing.trim().is_empty() && squash(existing).contains(&squash(replacing)) {
        return format!(
            "`replacing` was not found. The file DOES contain that text apart from whitespace, so \
             the indentation or line breaks differ — it must match byte for byte. `read` the file \
             and copy the snippet exactly as it comes back. The file is {} bytes, {} lines.",
            existing.len(),
            count_lines(existing)
        );
    }
    format!(
        "`replacing` was not found in the file, which is {} bytes and {} lines. It must match \
         byte for byte, including indentation. `read` the file first and copy the snippet from \
         what it returns.",
        existing.len(),
        count_lines(existing)
    )
}

/// Why a path looks like it holds secrets (`.env`, private keys, credential stores), if it does.
/// Writing one needs approval in every mode (design 2.1 B6). Templates (`.env.example`) do not
/// count.
pub fn secret_path(path: &str) -> Option<&'static str> {
    let norm = path.replace('\\', "/").to_ascii_lowercase();
    let name = norm.rsplit('/').next().unwrap_or(&norm);
    let template =
        name.split('.').skip(1).any(|seg| ["example", "sample", "template", "dist", "defaults"].contains(&seg));
    if (name == ".env" || name.starts_with(".env.") || name.ends_with(".env")) && !template {
        return Some("an environment file");
    }
    if ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].contains(&name) {
        return Some("an SSH private key");
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if ["pem", "key", "p12", "pfx", "jks", "keystore", "ppk", "asc", "gpg"].contains(&ext) && !template {
        return Some("a key or certificate store");
    }
    if [".npmrc", ".pypirc", ".netrc", "_netrc", ".git-credentials", ".dockercfg", "kubeconfig"].contains(&name)
        || norm.ends_with(".aws/credentials")
        || norm.ends_with(".docker/config.json")
    {
        return Some("a credential store");
    }
    let stem = name.split('.').next().unwrap_or(name);
    if ["credentials", "secrets", "secret", "service-account", "service_account"].contains(&stem) && !template {
        return Some("a credentials file");
    }
    None
}

/// The kind of credential `content` appears to contain (a private key block, a cloud or API
/// token), if any. Writing one needs approval in every mode (design 2.1 B6).
pub fn secret_content(content: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let pats = PATTERNS.get_or_init(|| {
        [
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "a private key"),
            (r"\bAKIA[0-9A-Z]{16}\b", "an AWS access key"),
            (r"\bsk-ant-[A-Za-z0-9_\-]{20,}", "an Anthropic API key"),
            (r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{32,}", "an OpenAI-style API key"),
            (r"\bsk-or-v1-[0-9a-f]{32,}", "an OpenRouter API key"),
            (r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", "a GitHub token"),
            (r"\bgithub_pat_[A-Za-z0-9_]{60,}", "a GitHub token"),
            (r"\bxox[abprs]-[A-Za-z0-9\-]{10,}", "a Slack token"),
            (r"\bhf_[A-Za-z0-9]{30,}\b", "a Hugging Face token"),
            (r"\bAIza[0-9A-Za-z_\-]{35}\b", "a Google API key"),
        ]
        .into_iter()
        .filter_map(|(p, why)| regex::Regex::new(p).ok().map(|r| (r, why)))
        .collect()
    });
    pats.iter().find(|(r, _)| r.is_match(content)).map(|(_, why)| *why)
}

/// Keeps at most [`MAX_LINE_CHARS`] characters of each line, marking the cut.
pub fn clip_long_lines(text: &str) -> (String, usize) {
    let mut clipped = 0usize;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if body.chars().count() <= MAX_LINE_CHARS {
            out.push_str(line);
            continue;
        }
        clipped += 1;
        let cut: usize = body.char_indices().nth(MAX_LINE_CHARS).map_or(body.len(), |(i, _)| i);
        out.push_str(&body[..cut]);
        out.push_str(&format!("…[line truncated, {} more characters]", body[cut..].chars().count()));
        out.push_str(&line[body.len()..]);
    }
    (out, clipped)
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn secret_shaped_paths_and_contents_are_named() {
        for p in [".env", "app/.env.local", "C:\\x\\prod.env", "id_ed25519", "certs/server.pem", "deploy.key", ".npmrc", "home/.aws/credentials", "credentials.json", "secrets.yaml"] {
            assert!(secret_path(p).is_some(), "{p}");
        }
        for p in [".env.example", "src/env.rs", "README.md", "id_ed25519.pub", "keys.rs", "secret_santa.py", "credentials.example.json"] {
            assert!(secret_path(p).is_none(), "{p}");
        }
        assert_eq!(secret_content("x = 'AKIAABCDEFGHIJKLMNOP'"), Some("an AWS access key"));
        assert_eq!(secret_content("-----BEGIN OPENSSH PRIVATE KEY-----\nabc"), Some("a private key"));
        assert!(secret_content(&format!("token: ghp_{}", "a".repeat(36))).is_some());
        assert!(secret_content("let sk = compute_key();").is_none());
    }

    #[test]
    fn long_lines_are_clipped_on_char_boundaries() {
        let (t, n) = clip_long_lines(&format!("short\n{}\r\nend", "é".repeat(MAX_LINE_CHARS + 5)));
        assert_eq!(n, 1);
        assert!(t.contains("…[line truncated, 5 more characters]\r\nend"), "{t}");
    }
}

#[cfg(test)]
mod glob_tests {
    use super::glob_match;

    #[test]
    fn globstar_spans_any_depth_including_none() {
        for s in ["Cargo.toml", "crates/a/Cargo.toml", "crates/a/b/c/Cargo.toml"] {
            assert!(glob_match("**/Cargo.toml", s), "{s}");
        }
        assert!(!glob_match("**/Cargo.toml", "crates/a/Cargo.toml.bak"));
        assert!(!glob_match("**/Cargo.toml", "crates/aCargo.toml"));
        assert!(glob_match("crates/**/*.rs", "crates/x.rs"));
        assert!(glob_match("crates/**/*.rs", "crates/a/src/x.rs"));
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("**", "a/b/c"));
        assert!(glob_match("a/**/b/**/c.txt", "a/x/b/y/z/c.txt"));
        assert!(glob_match("a/**/b/**/c.txt", "a/b/c.txt"));
    }

    #[test]
    fn star_and_question_stay_in_one_segment() {
        assert!(glob_match("src/*.rs", "src/b.rs"));
        assert!(!glob_match("src/*.rs", "src/deep/c.rs"));
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?", "/"));
        assert!(glob_match("*.MD", "readme.md"), "case-insensitive");
        assert!(glob_match(r"crates\*\Cargo.toml", "crates/a/Cargo.toml"), "backslash is a separator");
    }

    #[test]
    fn braces_alternate_and_pathological_patterns_stay_fast() {
        assert!(glob_match("**/*.{ts,tsx}", "web/app.tsx"));
        assert!(glob_match("**/*.{ts,tsx}", "app.ts"));
        assert!(!glob_match("**/*.{ts,tsx}", "app.js"));
        assert!(glob_match("a{", "a{"), "an unclosed brace is literal");
        let subject = "a".repeat(200);
        let t = std::time::Instant::now();
        assert!(!glob_match(&format!("{}b", "**a".repeat(40)), &subject));
        assert!(!glob_match(&format!("{}b", "*a".repeat(40)), &subject));
        assert!(t.elapsed().as_millis() < 500, "no exponential backtracking");
    }
}
