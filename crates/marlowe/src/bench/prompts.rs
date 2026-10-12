//! The tool-call prompts behind gate A4 (design 1): 200 deterministic requests, each with one
//! right tool and the values its arguments must carry. They are plain instructions, not
//! trick questions: the point is whether a model names the tool, fills the schema and keeps
//! the values it was given, the failure class repair exists for.

/// One request and what a correct call to it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    /// The tool a correct answer calls.
    pub tool: &'static str,
    /// Strings that must all appear (case-insensitively) in the call's arguments.
    pub expect: Vec<String>,
}

const PATHS: [&str; 10] = [
    "src/main.rs",
    "Cargo.toml",
    "README.md",
    "docs/ARCHITECTURE.md",
    "crates/core/src/lib.rs",
    "tests/test_api.py",
    "config/settings.toml",
    "scripts/build.ps1",
    "data/notes.txt",
    "src/utils/strings.rs",
];

const GLOBS: [&str; 10] = [
    "**/*.rs", "src/**/*.py", "*.toml", "docs/*.md", "tests/**/test_*.py", "**/*.json", "crates/*/Cargo.toml", "**/*.tsx", "scripts/*.ps1", "**/README*",
];

const TERMS: [&str; 10] = [
    "TODO", "fn main", "import numpy", "class Config", "unwrap()", "def test_", "panic!", "FIXME", "async fn", "retry_count",
];

const COMMANDS: [&str; 10] = [
    "git status",
    "cargo test -p core",
    "ls -la",
    "python --version",
    "npm run build",
    "pytest -x -q",
    "git log --oneline -5",
    "node --version",
    "cargo check --workspace",
    "git diff --stat",
];

const SNIPPETS: [(&str, &str); 10] = [
    ("version = 1", "version = 2"),
    ("debug = false", "debug = true"),
    ("MAX_RETRIES = 3", "MAX_RETRIES = 5"),
    ("timeout: 30", "timeout: 60"),
    ("let limit = 10;", "let limit = 25;"),
    ("name = demo", "name = prod"),
    ("port = 8080", "port = 9090"),
    ("level = info", "level = warn"),
    ("workers = 2", "workers = 8"),
    ("mode = fast", "mode = safe"),
];

const URLS: [&str; 10] = [
    "https://example.com/docs/intro",
    "https://example.org/changelog",
    "https://docs.rs/serde/latest/serde/",
    "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html",
    "https://example.com/api/v2/status",
    "https://example.net/blog/2026/release-notes",
    "https://www.rfc-editor.org/rfc/rfc9110",
    "https://example.com/pricing",
    "https://example.org/faq",
    "https://example.com/security.txt",
];

const TOPICS: [&str; 10] = [
    "diffusion", "retrieval augmented generation", "speculative decoding", "mixture of experts", "state space models", "tokenization", "reward modelling", "quantization", "long context", "distillation",
];

const WORDS: [(&str, &str, &str); 10] = [
    ("parser", "tests", "docs"),
    ("cache", "metrics", "retry"),
    ("schema", "migration", "seed"),
    ("lexer", "fuzzing", "bench"),
    ("auth", "logging", "limits"),
    ("router", "handlers", "errors"),
    ("loader", "config", "cli"),
    ("encoder", "decoder", "tokenizer"),
    ("planner", "executor", "reporter"),
    ("index", "search", "ranking"),
];

/// How many distinct prompts a tool has.
const VARIANTS: usize = 30;

/// The `i`-th prompt for `tool`, or `None` for a tool without a template.
fn one(tool: &str, i: usize) -> Option<Prompt> {
    let ph = |n: usize| (i / 10 + i) % n;
    let p = PATHS[i % 10];
    let s = |t: &str| t.to_string();
    Some(match tool {
        "read" => Prompt {
            text: [format!("Read the file {p}."), format!("Show me what is in {p}."), format!("Open {p} and look at its first 40 lines.")][ph(3)].clone(),
            tool: "read",
            expect: vec![s(p)],
        },
        "glob" => {
            let g = GLOBS[i % 10];
            Prompt {
                text: [format!("List the files that match {g}."), format!("Find every file matching the pattern {g}."), format!("Which files fit {g}?")][ph(3)].clone(),
                tool: "glob",
                expect: vec![s(g)],
            }
        }
        "grep" => {
            let t = TERMS[i % 10];
            Prompt {
                text: [format!("Search the code for `{t}`."), format!("Where does `{t}` appear in the repository?"), format!("grep for `{t}` and show me the matches.")][ph(3)].clone(),
                tool: "grep",
                expect: vec![s(t)],
            }
        }
        "bash" => {
            let c = COMMANDS[i % 10];
            Prompt {
                text: [format!("Run `{c}`."), format!("Execute this in the shell: `{c}`"), format!("I need the output of `{c}`.")][ph(3)].clone(),
                tool: "bash",
                expect: vec![s(c)],
            }
        }
        "write" => {
            let word = WORDS[i % 10].0;
            Prompt {
                text: [
                    format!("Create the file {p} containing exactly the text `{word} ready`."),
                    format!("Write a new file at {p} whose whole content is `{word} ready`."),
                    format!("Make {p} with the single line `{word} ready` in it."),
                ][ph(3)]
                .clone(),
                tool: "write",
                expect: vec![s(p), format!("{word} ready")],
            }
        }
        "edit" => {
            let (old, new) = SNIPPETS[i % 10];
            Prompt {
                text: [
                    format!("In {p}, replace `{old}` with `{new}`."),
                    format!("Edit {p}: change `{old}` into `{new}`."),
                    format!("Update {p} so that `{old}` reads `{new}` instead."),
                ][ph(3)]
                .clone(),
                tool: "edit",
                expect: vec![s(p), s(old), s(new)],
            }
        }
        "multi_edit" => {
            let (a, b) = SNIPPETS[i % 10];
            let (c, d) = SNIPPETS[(i + 3) % 10];
            Prompt {
                text: [
                    format!("In {p}, make two changes in one go: replace `{a}` with `{b}`, and replace `{c}` with `{d}`."),
                    format!("Apply both edits to {p} together: `{a}` -> `{b}` and `{c}` -> `{d}`."),
                ][ph(2)]
                .clone(),
                tool: "multi_edit",
                expect: vec![s(p), s(a), s(b), s(c), s(d)],
            }
        }
        "python" => {
            let n = 20 + i % 10 * 7;
            let code = format!("print(sum(range({n})))");
            Prompt {
                text: [format!("Use the python tool to run `{code}`."), format!("Run this Python snippet: `{code}`")][ph(2)].clone(),
                tool: "python",
                expect: vec![format!("range({n})")],
            }
        }
        "todo" => {
            let (a, b, c) = WORDS[i % 10];
            Prompt {
                text: [
                    format!("Track this work in a todo list: first the {a}, then the {b}, then the {c}."),
                    format!("Put three items on the task list: {a}, {b} and {c}."),
                    format!("Write a todo list for me with {a}, {b} and {c} as separate items."),
                ][ph(3)]
                .clone(),
                tool: "todo",
                expect: vec![s(a), s(b), s(c)],
            }
        }
        "web_fetch" => {
            let u = URLS[i % 10];
            Prompt {
                text: [format!("Fetch {u} and tell me what it says."), format!("Open the page {u}."), format!("What is on {u}? Fetch it.")][ph(3)].clone(),
                tool: "web_fetch",
                expect: vec![s(u)],
            }
        }
        "arxiv" => {
            let t = TOPICS[i % 10];
            Prompt {
                text: [format!("Find arXiv papers about {t}."), format!("Search arXiv for recent work on {t}.")][ph(2)].clone(),
                tool: "arxiv",
                expect: vec![s(t.split(' ').next().unwrap_or(t))],
            }
        }
        "ask_user" => {
            let (a, b, _) = WORDS[i % 10];
            Prompt {
                text: [
                    format!("Before you start, ask me whether the {a} or the {b} should come first."),
                    format!("Ask me a question with two options: {a} and {b}."),
                ][ph(2)]
                .clone(),
                tool: "ask_user",
                expect: vec![s(a), s(b)],
            }
        }
        "symbols" => {
            let t = TERMS[i % 10].split(' ').last().unwrap_or("main");
            Prompt { text: [format!("List the symbols named like `{t}`."), format!("Which symbols match `{t}`?")][ph(2)].clone(), tool: "symbols", expect: vec![s(t)] }
        }
        "references" => {
            let w = WORDS[i % 10].0;
            Prompt {
                text: [format!("Find every reference to the symbol `{w}`."), format!("Where is `{w}` used?")][ph(2)].clone(),
                tool: "references",
                expect: vec![s(w)],
            }
        }
        _ => return None,
    })
}

/// Up to `n` prompts across the tools in `available` (those the session actually offers),
/// round-robin so every tool is covered before any repeats. Always the same list for the same
/// inputs.
pub fn tool_prompts(available: &[String], n: usize) -> Vec<Prompt> {
    const ORDER: [&str; 13] = ["read", "grep", "bash", "edit", "glob", "write", "todo", "web_fetch", "multi_edit", "python", "ask_user", "arxiv", "references"];
    let tools: Vec<&str> = ORDER.iter().copied().chain(["symbols"]).filter(|t| available.iter().any(|a| a == t)).collect();
    let mut out: Vec<Prompt> = Vec::new();
    if tools.is_empty() {
        return out;
    }
    'fill: for round in 0..VARIANTS {
        for t in &tools {
            if out.len() >= n {
                break 'fill;
            }
            if let Some(p) = one(t, round) {
                if !out.iter().any(|q| q.text == p.text) {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<String> {
        ["read", "grep", "bash", "edit", "glob", "write", "todo", "web_fetch", "multi_edit", "python", "ask_user", "arxiv"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn two_hundred_distinct_prompts_cover_every_available_tool() {
        let p = tool_prompts(&all(), 200);
        assert_eq!(p.len(), 200);
        let texts: std::collections::BTreeSet<&str> = p.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts.len(), 200, "prompts repeat");
        for t in all() {
            let n = p.iter().filter(|x| x.tool == t).count();
            assert!(n >= 10, "{t} has only {n} prompts");
        }
        assert!(p.iter().all(|x| !x.expect.is_empty()));
    }

    #[test]
    fn the_list_is_stable_and_follows_the_available_tools() {
        assert_eq!(tool_prompts(&all(), 200), tool_prompts(&all(), 200));
        let only_shell = tool_prompts(&["bash".to_string()], 25);
        assert_eq!(only_shell.len(), 25);
        assert!(only_shell.iter().all(|p| p.tool == "bash"));
        assert!(tool_prompts(&["nothing".to_string()], 10).is_empty());
        // A smaller n is a prefix, so a smoke run is a subset of the full one.
        assert_eq!(tool_prompts(&all(), 30), tool_prompts(&all(), 200)[..30].to_vec());
    }
}
