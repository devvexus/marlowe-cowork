//! Directives in the user's message: `ultrathink` (C14) and `+500k` token targets (WF8), parsed
//! the same way for the TUI and `-p`.
//!
//! - `ultrathink` raises that turn's effort to `Max`, for that turn only ([`TurnDirectives`], read
//!   by the loop).
//! - `+<n>k` / `+<n>m` (also `+1.5m`) is a token target for the work the message asks for: the
//!   [`TokenPool`] counts the output tokens of the main loop, its subagents and every workflow
//!   against it (one pool), the conductor is kept working until the target is nearly spent
//!   ([`DirectiveMiddleware`]), and a workflow's `budget` reads the same pool.
//!
//! Neither token is cut from the text: the model sees what the user wrote. Both are ignored inside
//! code fences and inline code, where they are quoted rather than meant.

use std::sync::{Arc, Mutex};

use crate::convo::Conversation;
use crate::middleware::{FinalFlow, StepInfo, ToolMiddleware, TurnFlow};

/// The most a `+<n>` target may ask for (a typo like `+99999m` is not a plan).
pub const MAX_TARGET_TOKENS: u64 = 200_000_000;
/// The conductor is pushed on while more than this share of the target is unspent.
pub const KEEP_GOING_ABOVE: f64 = 0.10;
/// Times one turn is pushed on because its token target is not reached.
pub const MAX_TARGET_NUDGES: u32 = 30;

/// What a user message asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Directives {
    pub ultrathink: bool,
    /// Output tokens to spend on the request (`+500k` = 500,000).
    pub token_target: Option<u64>,
}

/// Reads the directives in `text` (outside code).
pub fn parse(text: &str) -> Directives {
    let prose = strip_code(text);
    let mut out = Directives::default();
    for word in prose.split_whitespace() {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '.');
        // Trailing sentence punctuation (`+1.5m.`) is not part of the number.
        let bare = bare.trim_end_matches('.');
        if bare.eq_ignore_ascii_case("ultrathink") {
            out.ultrathink = true;
        } else if let Some(n) = target_of(bare) {
            // The last one wins, as a correction would.
            out.token_target = Some(n);
        }
    }
    out
}

/// `+500k`, `+1.5m`, `+20K`: the token count, or `None` for anything else (`+5`, `+x`, `+1kb`).
fn target_of(word: &str) -> Option<u64> {
    let rest = word.strip_prefix('+')?;
    let unit = rest.chars().last()?.to_ascii_lowercase();
    let (digits, mult) = match unit {
        'k' => (&rest[..rest.len() - 1], 1_000.0),
        'm' => (&rest[..rest.len() - 1], 1_000_000.0),
        _ => return None,
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') || digits.matches('.').count() > 1 {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    let tokens = (n * mult).round();
    (tokens >= 1.0 && tokens <= MAX_TARGET_TOKENS as f64).then_some(tokens as u64)
}

/// `text` with fenced blocks and inline code replaced by spaces.
fn strip_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            out.push('\n');
            continue;
        }
        if fenced {
            out.push('\n');
            continue;
        }
        let mut inline = false;
        for c in line.chars() {
            if c == '`' {
                inline = !inline;
                out.push(' ');
            } else if inline {
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

/// The directives of the turn now running, in `Conversation::ext` (replaced at the start of every
/// turn, so `ultrathink` never carries over).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnDirectives {
    pub ultrathink: bool,
}

#[derive(Debug, Default)]
struct PoolState {
    total: Option<u64>,
    spent: u64,
    nudges: u32,
}

/// The session's output-token pool (a service). `start` arms it from a `+500k`; the loop, its
/// subagents and workflows `charge` what they generate; `remaining` is what is left.
#[derive(Clone, Default)]
pub struct TokenPool(Arc<Mutex<PoolState>>);

impl std::fmt::Debug for TokenPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("TokenPool").field("total", &g.total).field("spent", &g.spent).finish()
    }
}

impl TokenPool {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Arms the pool for a new request: `total` output tokens, none spent.
    pub fn start(&self, total: u64) {
        *self.lock() = PoolState { total: Some(total), spent: 0, nudges: 0 };
    }

    /// Disarms it (a request without a target).
    pub fn clear(&self) {
        *self.lock() = PoolState::default();
    }

    /// Counts generated tokens against the target (nothing when none is armed).
    pub fn charge(&self, output_tokens: u64) {
        let mut g = self.lock();
        if g.total.is_some() {
            g.spent = g.spent.saturating_add(output_tokens);
        }
    }

    pub fn total(&self) -> Option<u64> {
        self.lock().total
    }

    pub fn spent(&self) -> u64 {
        self.lock().spent
    }

    /// Tokens left; `None` when no target is armed.
    pub fn remaining(&self) -> Option<u64> {
        let g = self.lock();
        g.total.map(|t| t.saturating_sub(g.spent))
    }

    /// Whether the conductor should be kept working: a target is armed, more than
    /// [`KEEP_GOING_ABOVE`] of it is unspent and the turn has not been pushed on too often.
    fn keep_going(&self) -> Option<(u64, u64)> {
        let mut g = self.lock();
        let total = g.total?;
        let left = total.saturating_sub(g.spent);
        if (left as f64) <= total as f64 * KEEP_GOING_ABOVE || g.nudges >= MAX_TARGET_NUDGES {
            return None;
        }
        g.nudges += 1;
        Some((g.spent, total))
    }
}

/// Applies the directives of each user message: records `ultrathink` for the loop, arms or clears
/// the [`TokenPool`], and keeps the conductor working toward a target.
pub struct DirectiveMiddleware {
    pool: TokenPool,
}

impl DirectiveMiddleware {
    pub fn new(pool: TokenPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

impl ToolMiddleware for DirectiveMiddleware {
    fn name(&self) -> &str {
        "directives"
    }

    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        let d = parse(text);
        convo.ext.insert(TurnDirectives { ultrathink: d.ultrathink });
        match d.token_target {
            Some(total) => self.pool.start(total),
            None => self.pool.clear(),
        }
        TurnFlow::Continue
    }

    fn before_final(&self, _info: StepInfo, _convo: &mut Conversation, _text: &str) -> FinalFlow {
        match self.pool.keep_going() {
            Some((spent, total)) => FinalFlow::Continue(format!(
                "<system-reminder>\nThe user set a token target for this request: {spent} of {total} output tokens used. Do \
                 not stop yet: keep working, verify what you did, and take on the next piece of the request. Finish only \
                 when the work is complete and checked, or the target is nearly spent.\n</system-reminder>"
            )),
            None => FinalFlow::Accept,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::SessionId;

    #[test]
    fn parity_c14_ultrathink_is_found_as_a_word_and_not_in_code() {
        assert!(parse("please ultrathink about this").ultrathink);
        assert!(parse("Ultrathink: why does it hang?").ultrathink);
        assert!(!parse("the `ultrathink` keyword is documented").ultrathink);
        assert!(!parse("```\nultrathink\n```\nplain").ultrathink);
        assert!(!parse("ultrathinking is not it").ultrathink);
        assert!(!parse("nothing here").ultrathink);
    }

    #[test]
    fn parity_wf8_targets_parse_as_k_and_m_and_reject_the_rest() {
        let t = |s: &str| parse(s).token_target;
        assert_eq!(t("refactor it +500k"), Some(500_000));
        assert_eq!(t("+20k fix the bug"), Some(20_000));
        assert_eq!(t("go +2m"), Some(2_000_000));
        assert_eq!(t("go +1.5m."), Some(1_500_000));
        assert_eq!(t("go +20K"), Some(20_000));
        assert_eq!(t("c++ 500k"), None);
        assert_eq!(t("a + 500k"), None);
        assert_eq!(t("+5"), None);
        assert_eq!(t("+1kb"), None);
        assert_eq!(t("+0k"), None);
        assert_eq!(t("+99999m"), None);
        assert_eq!(t("run `+500k` literally"), None);
        assert_eq!(t("first +10k then +30k"), Some(30_000));
    }

    #[test]
    fn parity_wf8_the_pool_counts_one_budget_and_keeps_the_conductor_going() {
        let pool = TokenPool::new();
        let mw = DirectiveMiddleware::new(pool.clone());
        let mut convo = Conversation::new(SessionId("s".into()), None);
        let mut text = "do the migration +20k".to_string();
        assert_eq!(mw.before_turn(&mut convo, &mut text), TurnFlow::Continue);
        assert_eq!(text, "do the migration +20k", "the text is the user's, untouched");
        assert_eq!((pool.total(), pool.remaining()), (Some(20_000), Some(20_000)));
        pool.charge(5_000);
        pool.charge(5_000);
        assert_eq!(pool.remaining(), Some(10_000));
        let info = StepInfo { turn: 0, step: 1 };
        assert!(matches!(mw.before_final(info, &mut convo, "done"), FinalFlow::Continue(r) if r.contains("10000 of 20000")));
        pool.charge(9_000);
        assert_eq!(mw.before_final(info, &mut convo, "done"), FinalFlow::Accept, "within 10% of the target: the turn may end");
        pool.charge(5_000);
        assert_eq!(pool.remaining(), Some(0), "the pool saturates at its total");
        // The next message has no target: the pool is disarmed and charges are ignored.
        let mut next = "thanks".to_string();
        mw.before_turn(&mut convo, &mut next);
        assert_eq!(pool.total(), None);
        pool.charge(1_000);
        assert_eq!(pool.spent(), 0);
    }

    #[test]
    fn ultrathink_lasts_one_turn() {
        let mw = DirectiveMiddleware::new(TokenPool::new());
        let mut convo = Conversation::new(SessionId("s".into()), None);
        mw.before_turn(&mut convo, &mut "ultrathink about it".to_string());
        assert!(convo.ext.get::<TurnDirectives>().unwrap().ultrathink);
        mw.before_turn(&mut convo, &mut "and now plainly".to_string());
        assert!(!convo.ext.get::<TurnDirectives>().unwrap().ultrathink);
    }
}
