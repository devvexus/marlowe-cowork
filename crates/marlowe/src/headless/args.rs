//! `marlowe -p` flags (design 2.7 H4). Claude Code's spellings are accepted beside Marlowe's own.

use std::time::Duration;

use marlowe_proto::PermissionMode;

pub const USAGE: &str = "\
marlowe -p [PROMPT] [flags]               one non-interactive turn (stdin is appended to the prompt)

  --output-format text|json|stream-json   what stdout carries (default text)
  --input-format text|stream-json         stream-json: user messages and control messages on stdin
  --include-partial-messages              stream-json: also emit text deltas as stream_event lines
  --verbose                               progress lines on stderr
  --standalone                            run an in-process daemon instead of the shared one
  --no-memory                             with --standalone: no memory service
  -c, --continue                          continue the newest session of this workspace
  -r, --resume ID                         continue session ID
  -w, --worktree NAME                     work in a git worktree of this repo (created or reused)
  --workspace DIR                         the workspace (default: the current directory)
  --add-dir DIR                           another directory the session may use (repeatable)
  --model REF                             provider/model, or a model id in the library
  --append-system-prompt TEXT             added to the system prompt for this run
  --allowed-tools LIST                    rules that are allowed without asking: Bash(git diff:*),Read
  --disallowed-tools LIST                 rules that are denied outright
  --permission-mode MODE                  plan | default | accept_edits | bypass | dont_ask
  --permission-prompt-tool TOOL           mcp__server__tool, or `stdio` to ask the SDK client
  --fail-on-ask                           exit 2 at the first approval nothing allowed
  --max-steps N (--max-turns)             model calls per turn
  --max-budget-usd X                      stop (exit 3) when the cost passes X
  --mcp-config FILE|JSON                  MCP servers for this run (repeatable)
  --strict-mcp-config                     use only --mcp-config (implies --standalone)
  --settings FILE|JSON                    settings for this run (repeatable)
  --idle-timeout SECS                     give up after this long with no activity (default 1800)

Exit codes: 0 done, 1 error, 2 approval needed under --fail-on-ask, 3 budget, 130 cancelled.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Text,
    StreamJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    No,
    Continue,
    Id(String),
}

#[derive(Debug, Clone)]
pub struct Args {
    pub prompt: Option<String>,
    pub output: OutputFormat,
    pub input: InputFormat,
    pub partial: bool,
    pub verbose: bool,
    pub standalone: bool,
    pub no_memory: bool,
    pub resume: Resume,
    pub worktree: Option<String>,
    pub workspace: Option<String>,
    pub add_dirs: Vec<String>,
    pub model: Option<String>,
    pub append_system_prompt: Option<String>,
    pub allowed: Vec<String>,
    pub disallowed: Vec<String>,
    pub mode: Option<PermissionMode>,
    pub prompt_tool: Option<String>,
    pub fail_on_ask: bool,
    pub max_steps: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub mcp_config: Vec<String>,
    pub strict_mcp: bool,
    pub settings: Vec<String>,
    pub idle_timeout: Duration,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            prompt: None,
            output: OutputFormat::Text,
            input: InputFormat::Text,
            partial: false,
            verbose: false,
            standalone: false,
            no_memory: false,
            resume: Resume::No,
            worktree: None,
            workspace: None,
            add_dirs: Vec::new(),
            model: None,
            append_system_prompt: None,
            allowed: Vec::new(),
            disallowed: Vec::new(),
            mode: None,
            prompt_tool: None,
            fail_on_ask: false,
            max_steps: None,
            max_budget_usd: None,
            mcp_config: Vec::new(),
            strict_mcp: false,
            settings: Vec::new(),
            idle_timeout: Duration::from_secs(1800),
        }
    }
}

/// Splits `Bash(git diff:*),Read Edit(src/**)` on commas and spaces outside parentheses.
pub fn split_rules(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in list.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' | ' ' | '\t' | '\n' if depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `bypassPermissions`, `acceptEdits`, `dontAsk` (Claude Code) and Marlowe's own spellings.
pub fn parse_mode(s: &str) -> Result<PermissionMode, String> {
    let norm: String = s.trim().chars().filter(|c| *c != '_' && *c != '-').collect::<String>().to_ascii_lowercase();
    Ok(match norm.as_str() {
        "plan" => PermissionMode::Plan,
        "default" => PermissionMode::Default,
        "acceptedits" => PermissionMode::AcceptEdits,
        "bypass" | "bypasspermissions" | "auto" => PermissionMode::Bypass,
        "dontask" => PermissionMode::DontAsk,
        _ => return Err(format!("unknown permission mode {s:?} (plan, default, accept_edits, bypass, dont_ask)")),
    })
}

impl Args {
    pub fn parse(args: &[String]) -> Result<Args, String> {
        let mut a = Args::default();
        let mut words: Vec<String> = Vec::new();
        let mut it = args.iter().peekable();
        let mut print = false;
        while let Some(arg) = it.next() {
            let mut value = |flag: &str| -> Result<String, String> {
                it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))
            };
            // `--flag=value`
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let mut take = |name: &str| -> Result<String, String> {
                match inline.clone() {
                    Some(v) => Ok(v),
                    None => value(name),
                }
            };
            match flag.as_str() {
                "-p" | "--print" => print = true,
                "--output-format" => {
                    a.output = match take("--output-format")?.as_str() {
                        "text" => OutputFormat::Text,
                        "json" => OutputFormat::Json,
                        "stream-json" => OutputFormat::StreamJson,
                        o => return Err(format!("--output-format {o:?}: use text, json or stream-json")),
                    }
                }
                "--input-format" => {
                    a.input = match take("--input-format")?.as_str() {
                        "text" => InputFormat::Text,
                        "stream-json" => InputFormat::StreamJson,
                        o => return Err(format!("--input-format {o:?}: use text or stream-json")),
                    }
                }
                "--include-partial-messages" => a.partial = true,
                "--verbose" => a.verbose = true,
                "--standalone" => a.standalone = true,
                "--no-memory" => a.no_memory = true,
                "-c" | "--continue" => a.resume = Resume::Continue,
                "-r" | "--resume" => a.resume = Resume::Id(take("--resume")?),
                "-w" | "--worktree" => a.worktree = Some(take("--worktree")?),
                "--workspace" | "--cwd" => a.workspace = Some(take("--workspace")?),
                "--add-dir" => a.add_dirs.push(take("--add-dir")?),
                "--model" => a.model = Some(take("--model")?),
                "--append-system-prompt" => a.append_system_prompt = Some(take("--append-system-prompt")?),
                "--allowed-tools" | "--allowedTools" => a.allowed.extend(split_rules(&take("--allowed-tools")?)),
                "--disallowed-tools" | "--disallowedTools" => a.disallowed.extend(split_rules(&take("--disallowed-tools")?)),
                "--permission-mode" => a.mode = Some(parse_mode(&take("--permission-mode")?)?),
                "--permission-prompt-tool" => a.prompt_tool = Some(take("--permission-prompt-tool")?),
                "--fail-on-ask" => a.fail_on_ask = true,
                "--max-steps" | "--max-turns" => {
                    let v = take("--max-steps")?;
                    a.max_steps = Some(v.parse().map_err(|_| format!("--max-steps {v:?} is not a number"))?);
                }
                "--max-budget-usd" => {
                    let v = take("--max-budget-usd")?;
                    let n: f64 = v.parse().map_err(|_| format!("--max-budget-usd {v:?} is not a number"))?;
                    if !(n.is_finite() && n >= 0.0) {
                        return Err("--max-budget-usd must be zero or more".into());
                    }
                    a.max_budget_usd = Some(n);
                }
                "--mcp-config" => a.mcp_config.push(take("--mcp-config")?),
                "--strict-mcp-config" => a.strict_mcp = true,
                "--settings" => a.settings.push(take("--settings")?),
                "--idle-timeout" => {
                    let v = take("--idle-timeout")?;
                    a.idle_timeout = Duration::from_secs(v.parse().map_err(|_| format!("--idle-timeout {v:?} is not a number of seconds"))?);
                }
                "--" => {
                    words.extend(it.by_ref().cloned());
                }
                f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown flag {f}")),
                _ => words.push(arg.clone()),
            }
        }
        if !print {
            return Err("not a print invocation (-p)".into());
        }
        if !words.is_empty() {
            a.prompt = Some(words.join(" "));
        }
        if a.input == InputFormat::StreamJson && a.output != OutputFormat::StreamJson {
            return Err("--input-format stream-json needs --output-format stream-json".into());
        }
        if a.partial && a.output != OutputFormat::StreamJson {
            return Err("--include-partial-messages needs --output-format stream-json".into());
        }
        if a.prompt_tool.as_deref() == Some("stdio") && (a.input != InputFormat::StreamJson || a.output != OutputFormat::StreamJson) {
            return Err("--permission-prompt-tool stdio needs stream-json on both sides".into());
        }
        if a.strict_mcp {
            // The shared daemon serves other sessions; "only these servers" is a property of a
            // process of its own.
            a.standalone = true;
        }
        if a.no_memory && !a.standalone {
            return Err("--no-memory goes with --standalone (the shared daemon keeps memory on)".into());
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Args, String> {
        Args::parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn a_plain_print_call() {
        let a = parse("-p fix the build").unwrap();
        assert_eq!(a.prompt.as_deref(), Some("fix the build"));
        assert_eq!((a.output, a.input), (OutputFormat::Text, InputFormat::Text));
        assert!(!a.standalone && a.resume == Resume::No);
    }

    #[test]
    fn every_flag_has_a_home() {
        let a = parse(
            "--print go --output-format stream-json --input-format stream-json --include-partial-messages --verbose --standalone --no-memory \
             -c --workspace C:/w --add-dir C:/a --add-dir C:/b --model ollama/x --append-system-prompt be-brief \
             --permission-mode bypassPermissions --permission-prompt-tool mcp__s__t --fail-on-ask --max-turns 7 --max-budget-usd=2.5 \
             --mcp-config a.json --settings s.toml --idle-timeout 30 -w feature",
        )
        .unwrap();
        assert_eq!(a.prompt.as_deref(), Some("go"));
        assert!(a.partial && a.verbose && a.standalone && a.no_memory && a.fail_on_ask);
        assert_eq!(a.resume, Resume::Continue);
        assert_eq!(a.add_dirs, ["C:/a", "C:/b"]);
        assert_eq!(a.model.as_deref(), Some("ollama/x"));
        assert_eq!(a.append_system_prompt.as_deref(), Some("be-brief"));
        assert_eq!(a.mode, Some(PermissionMode::Bypass));
        assert_eq!(a.prompt_tool.as_deref(), Some("mcp__s__t"));
        assert_eq!((a.max_steps, a.max_budget_usd), (Some(7), Some(2.5)));
        assert_eq!((a.mcp_config.len(), a.settings.len()), (1, 1));
        assert_eq!(a.idle_timeout, Duration::from_secs(30));
        assert_eq!(a.worktree.as_deref(), Some("feature"));
        assert_eq!(parse("-p x -r abc").unwrap().resume, Resume::Id("abc".into()));
    }

    #[test]
    fn rule_lists_split_outside_parentheses() {
        assert_eq!(split_rules("Bash(git diff:*),Read Edit(src/**)"), ["Bash(git diff:*)", "Read", "Edit(src/**)"]);
        assert_eq!(split_rules("Bash(a, b)"), ["Bash(a, b)"], "a comma inside a rule stays in it");
        let a = parse("-p x --allowed-tools Bash(git_log:*),Read --disallowed-tools Bash(rm:*)").unwrap();
        assert_eq!(a.allowed, ["Bash(git_log:*)", "Read"]);
        assert_eq!(a.disallowed, ["Bash(rm:*)"]);
        // Repeatable.
        assert_eq!(parse("-p x --allowedTools Read --allowed-tools Glob").unwrap().allowed, ["Read", "Glob"]);
    }

    #[test]
    fn permission_modes_accept_both_spellings() {
        for (s, m) in [
            ("plan", PermissionMode::Plan),
            ("default", PermissionMode::Default),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("accept_edits", PermissionMode::AcceptEdits),
            ("bypassPermissions", PermissionMode::Bypass),
            ("auto", PermissionMode::Bypass),
            ("dontAsk", PermissionMode::DontAsk),
            ("dont_ask", PermissionMode::DontAsk),
        ] {
            assert_eq!(parse_mode(s).unwrap(), m, "{s}");
        }
        assert!(parse_mode("yolo").is_err());
    }

    #[test]
    fn combinations_that_cannot_work_are_refused_up_front() {
        assert!(parse("-p --input-format stream-json").unwrap_err().contains("stream-json"));
        assert!(parse("-p --include-partial-messages").is_err());
        assert!(parse("-p --permission-prompt-tool stdio").is_err());
        assert!(parse("-p --no-memory").is_err());
        assert!(parse("-p --bogus").unwrap_err().contains("unknown flag"));
        assert!(parse("-p --max-turns lots").is_err());
        assert!(parse("-p --max-budget-usd -1").is_err());
        assert!(parse("-p --output-format xml").is_err());
        assert!(parse("--model x").unwrap_err().contains("-p"));
        assert!(parse("-p --model").unwrap_err().contains("needs a value"));
        assert!(parse("-p --strict-mcp-config").unwrap().standalone, "strict MCP implies a process of its own");
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        let a = parse("-p --verbose -- --not-a-flag text").unwrap();
        assert_eq!(a.prompt.as_deref(), Some("--not-a-flag text"));
    }
}
