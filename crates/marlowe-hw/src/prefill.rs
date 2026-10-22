//! The prompt read as Ollama's own output reports it: `slot ... | task N | ...` lines from the
//! llama-server runner (in `server.log`, or the log of an `ollama serve` Marlowe started).
//!
//! [`parse_line`] reads one line; [`PrefillTracker`] follows the request in flight through them.
//! A request is matched to a task by when it was sent and how big its prompt is. Nothing here
//! guesses: with no matching lines there is no snapshot, and the caller shows tokens and elapsed
//! time only. Pure: no I/O, no clocks.

/// One runner line this module reads, keyed by the slot's task.
#[derive(Debug, Clone, PartialEq)]
pub enum LogLine {
    /// `new prompt, ... task.n_tokens = N`: the whole prompt of a task, cached part included.
    NewPrompt { task: i64, total: u64 },
    /// `restored context checkpoint (... n_tokens = N, ...)`: a hybrid model's cache hit.
    Restored { task: i64, n: u64 },
    /// `cached n_tokens = N, memory_seq_rm [N, end)`: the first one after `NewPrompt` is where the
    /// read starts (what was reused); later ones are the batch boundaries it has reached.
    Cached { task: i64, n: u64 },
    /// `prompt processing, n_tokens = N, progress = p, t = s s / r tokens per second`.
    Processing { task: i64, n: u64, progress: f64, secs: f64, rate: f64 },
    /// `prompt eval time = X ms / N tokens`: the finished read, N tokens evaluated fresh.
    Eval { task: i64, ms: f64, tokens: u64 },
}

/// The number that follows `key` (spaces allowed), if there is one.
fn number_after(text: &str, key: &str) -> Option<f64> {
    let rest = text[text.find(key)? + key.len()..].trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn count_after(text: &str, key: &str) -> Option<u64> {
    number_after(text, key).map(|n| n as u64)
}

/// Reads one runner line; anything that is not one of the lines above is `None`.
pub fn parse_line(line: &str) -> Option<LogLine> {
    // `slot   operator(): id  0 | task 12 | new prompt, ...`
    let mut parts = line.splitn(3, '|');
    let (head, task, rest) = (parts.next()?, parts.next()?, parts.next()?);
    if !head.trim_start().starts_with("slot") {
        return None;
    }
    let task: i64 = task.trim().strip_prefix("task")?.trim().parse().ok()?;
    let rest = rest.trim();
    if rest.starts_with("new prompt") {
        return Some(LogLine::NewPrompt { task, total: count_after(rest, "task.n_tokens =")? });
    }
    if rest.starts_with("restored context checkpoint") {
        return Some(LogLine::Restored { task, n: count_after(rest, "n_tokens =")? });
    }
    if rest.starts_with("cached n_tokens") {
        return Some(LogLine::Cached { task, n: count_after(rest, "n_tokens =")? });
    }
    if rest.starts_with("prompt processing") {
        let after_t = &rest[rest.find(", t =")?..];
        return Some(LogLine::Processing {
            task,
            n: count_after(rest, "n_tokens =")?,
            progress: number_after(rest, "progress =")?,
            secs: number_after(after_t, "t =")?,
            rate: number_after(after_t, "s /")?,
        });
    }
    if rest.starts_with("prompt eval time") {
        return Some(LogLine::Eval { task, ms: number_after(rest, "=")?, tokens: count_after(rest, "ms /")? });
    }
    None
}

/// What the runner has said about the matched task so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefill {
    pub task: i64,
    /// The whole prompt in tokens (what the runner counted, cached part included).
    pub total: u64,
    /// Tokens served from the cache; `None` until the runner says where the read starts.
    pub cached: Option<u64>,
    /// Tokens the read has reached, cached part included.
    pub done: u64,
    /// The runner's own tokens per second so far, once it has printed one.
    pub rate: Option<f64>,
    /// Seconds the runner has been reading, as it printed them.
    pub secs: Option<f64>,
    /// The finished read: `(ms, tokens evaluated fresh)`.
    pub eval: Option<(f64, u64)>,
}

impl Prefill {
    /// Tokens read fresh and tokens reused, once the split is known (from the finished read
    /// first, else from where the read started).
    pub fn split(&self) -> Option<(u64, u64)> {
        match (self.eval, self.cached) {
            (Some((_, new)), _) => Some((new, self.total.saturating_sub(new))),
            (None, Some(c)) => Some((self.total.saturating_sub(c), c)),
            _ => None,
        }
    }
}

/// How far a runner prompt may be from the loop's estimate and still be taken for this request.
/// The estimate is characters over three, so it is loose; a crew call's prompt is told apart by
/// when it started and, mostly, by being much shorter.
const BAND: (f64, f64) = (0.6, 1.7);

/// Follows one request through the runner's lines.
#[derive(Debug, Clone)]
pub struct PrefillTracker {
    estimate: u64,
    seen: Option<Prefill>,
}

impl PrefillTracker {
    /// For a request whose prompt we estimate at `estimate` tokens (0: any size will do).
    pub fn new(estimate: u64) -> Self {
        Self { estimate, seen: None }
    }

    pub fn feed(&mut self, line: &str) {
        if let Some(l) = parse_line(line) {
            self.apply(l);
        }
    }

    fn matches(&self, total: u64) -> bool {
        let est = self.estimate as f64;
        self.estimate == 0 || (total as f64 >= est * BAND.0 && total as f64 <= est * BAND.1)
    }

    fn apply(&mut self, l: LogLine) {
        // Locked on the first prompt that fits; every other task's lines are someone else's.
        let Some(p) = &mut self.seen else {
            if let LogLine::NewPrompt { task, total } = l {
                if self.matches(total) {
                    self.seen = Some(Prefill { task, total, cached: None, done: 0, rate: None, secs: None, eval: None });
                }
            }
            return;
        };
        match l {
            LogLine::NewPrompt { task, total } if task == p.task => p.total = total,
            LogLine::Restored { task, n } | LogLine::Cached { task, n } if task == p.task => {
                p.cached.get_or_insert(n);
                p.done = p.done.max(n);
            }
            LogLine::Processing { task, n, secs, rate, .. } if task == p.task => {
                p.done = p.done.max(n);
                p.secs = Some(secs);
                p.rate = Some(rate);
            }
            LogLine::Eval { task, ms, tokens } if task == p.task => {
                p.eval = Some((ms, tokens));
                p.done = p.total;
            }
            _ => {}
        }
    }

    pub fn current(&self) -> Option<&Prefill> {
        self.seen.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from Ollama 0.32.5 server.log files (2026-10-09 and 2026-10-12), and the lines
    // Matthew pasted on 2026-10-21 (the 103.64 and 95.55 tokens-per-second ones).
    const COLD: &str = "\
slot launch_slot_: id  0 | task 0 | processing task, is_child = 0
slot   operator(): id  0 | task 0 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 7314
slot   operator(): id  0 | task 0 | cached n_tokens = 0, memory_seq_rm [0, end)
slot print_timing: id  0 | task 0 | prompt processing, n_tokens = 1024, progress = 0.14, t = 9.88 s / 103.64 tokens per second
slot   operator(): id  0 | task 0 | cached n_tokens = 1024, memory_seq_rm [1024, end)
slot print_timing: id  0 | task 0 | prompt processing, n_tokens = 2560, progress = 0.36, t = 26.79 s / 95.55 tokens per second
slot   operator(): id  0 | task 0 | cached n_tokens = 2560, memory_seq_rm [2560, end)";

    const CHECKPOINT: &str = "\
slot launch_slot_: id  0 | task 234 | processing task, is_child = 0
slot   operator(): id  0 | task 234 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 6760
slot   operator(): id  0 | task 234 | checking checkpoint with [6461, 6461] against 6465...
slot   operator(): id  0 | task 234 | restored context checkpoint (pos_min = 6461, pos_max = 6461, n_tokens = 6462, n_past = 6462, size = 174.992 MiB)
slot   operator(): id  0 | task 234 | cached n_tokens = 6462, memory_seq_rm [6462, end)
slot create_check: id  0 | task 234 | created context checkpoint 3 of 32 (pos_min = 6606, pos_max = 6606, n_tokens = 6607, size = 175.561 MiB)
slot   operator(): id  0 | task 234 | cached n_tokens = 6756, memory_seq_rm [6756, end)
slot print_timing: id  0 | task 234 | prompt eval time =     982.98 ms /   298 tokens (    3.30 ms per token,   303.19 tokens per second)
slot print_timing: id  0 | task 234 |        eval time =    1413.91 ms /   119 tokens (   11.88 ms per token,    84.16 tokens per second)";

    fn run(lines: &str, estimate: u64) -> PrefillTracker {
        let mut t = PrefillTracker::new(estimate);
        lines.lines().for_each(|l| t.feed(l));
        t
    }

    #[test]
    fn each_line_kind_parses_with_the_runners_own_spacing() {
        assert_eq!(
            parse_line("slot print_timing: id  0 | task 0 | prompt processing, n_tokens =     36, progress = 0.90, t =  13.58 s / 2.65 tokens per second"),
            Some(LogLine::Processing { task: 0, n: 36, progress: 0.90, secs: 13.58, rate: 2.65 })
        );
        assert_eq!(parse_line("slot   operator(): id  0 | task 0 | cached n_tokens = 1024, memory_seq_rm [1024, end)"), Some(LogLine::Cached { task: 0, n: 1024 }));
        assert_eq!(
            parse_line("slot print_timing: id  0 | task 0 | prompt eval time =   27153.59 ms /    40 tokens (  678.84 ms per token,     1.47 tokens per second)"),
            Some(LogLine::Eval { task: 0, ms: 27153.59, tokens: 40 })
        );
        assert_eq!(parse_line("slot get_availabl: id  0 | task -1 | selected slot by LCP similarity, sim_best = 1.000"), None);
        assert_eq!(parse_line("time=2026-10-21T20:10:03 level=INFO msg=\"x | task 1 | cached n_tokens = 5\""), None);
        assert_eq!(parse_line("slot print_timing: id  0 | task 0 |        eval time =    1413.91 ms /   119 tokens"), None, "the decode line is not the read");
    }

    #[test]
    fn a_cold_read_reports_its_real_progress_and_rate() {
        let t = run(COLD, 7_000);
        let p = t.current().unwrap();
        assert_eq!((p.total, p.cached, p.done), (7_314, Some(0), 2_560));
        assert_eq!((p.rate, p.secs), (Some(95.55), Some(26.79)));
        assert_eq!(p.split(), Some((7_314, 0)), "nothing was cached: the miss is visible");
    }

    #[test]
    fn a_checkpoint_hit_shows_what_was_reused_and_the_finished_read_is_exact() {
        let mut t = run(&CHECKPOINT.lines().take(6).collect::<Vec<_>>().join("\n"), 6_500);
        let p = t.current().unwrap();
        assert_eq!((p.total, p.cached, p.done), (6_760, Some(6_462), 6_462));
        assert_eq!(p.split(), Some((298, 6_462)));
        CHECKPOINT.lines().skip(6).for_each(|l| t.feed(l));
        let p = t.current().unwrap();
        assert_eq!((p.eval, p.done, p.split()), (Some((982.98, 298)), 6_760, Some((298, 6_462))));
    }

    #[test]
    fn other_tasks_and_prompts_of_the_wrong_size_are_not_ours() {
        // A crew call's 2,000-token prompt starts first; ours (7,314) follows.
        let mixed = "\
slot   operator(): id  1 | task 5 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 2000
slot   operator(): id  1 | task 5 | cached n_tokens = 0, memory_seq_rm [0, end)
slot   operator(): id  0 | task 6 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 7314
slot   operator(): id  0 | task 6 | cached n_tokens = 4096, memory_seq_rm [4096, end)
slot   operator(): id  1 | task 5 | cached n_tokens = 1024, memory_seq_rm [1024, end)";
        let p = run(mixed, 7_000).current().cloned().unwrap();
        assert_eq!((p.task, p.cached, p.done), (6, Some(4_096), 4_096));
        // Nothing near our size: nothing is claimed, so nothing is shown.
        assert!(run(mixed, 20_000).current().is_none());
        // No estimate: the first prompt is taken.
        assert_eq!(run(mixed, 0).current().unwrap().task, 5);
    }

    #[test]
    fn lines_before_the_prompt_or_without_one_make_no_snapshot() {
        let t = run("slot   operator(): id  0 | task 0 | cached n_tokens = 1024, memory_seq_rm [1024, end)", 1_000);
        assert!(t.current().is_none());
    }
}
