//! The "initializing mainframe" lines: a step shows when it really starts and flips when it
//! really ends. One line shape ([`SeqLine`], drawn by `render::seq`) serves the boot, the
//! shutdown ([`Farewell`]), research channels coming online and a crew run spinning up
//! ([`Block`], an in-transcript entry). Nothing here waits or invents a step.

use marlowe_proto::{Event, PhaseEvent, PhaseKind, PhaseState};

use crate::wake::Health;

/// One line: a health glyph, a label, leader dots and a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqLine {
    pub health: Health,
    pub label: String,
    pub value: String,
    /// The step has ended (its glyph is no longer waiting).
    pub done: bool,
}

impl SeqLine {
    pub fn pending(label: impl Into<String>) -> Self {
        SeqLine { health: Health::Pending, label: label.into(), value: String::new(), done: false }
    }

    pub fn finished(label: impl Into<String>, value: impl Into<String>, health: Health) -> Self {
        SeqLine { health, label: label.into(), value: value.into(), done: true }
    }
}

/// `13.1 GB` from MiB.
fn gb(mib: u64) -> String {
    format!("{:.1} GB", mib as f64 / 1024.0)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---- shutdown -----------------------------------------------------------------------------------

/// The daemon's close, as it happens. Started when the TUI asks the daemon to stop; fed every
/// event until the daemon says `ShuttingDown`, the connection ends, a key skips it, or the
/// timeouts below pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Farewell {
    began_ms: i64,
    lines: Vec<(String, SeqLine)>,
    /// The daemon announced a close (`Closing` or a shutdown phase): it is stopping.
    closing: bool,
    shut: bool,
    skipped: bool,
}

impl Farewell {
    /// How long the daemon may stay silent before the close is taken as not happening (another
    /// window is attached, or it keeps running).
    pub const QUIET_MS: i64 = 1_500;
    /// The most a close is waited for.
    pub const HARD_MS: i64 = 20_000;

    pub fn new(now_ms: i64) -> Self {
        Farewell { began_ms: now_ms, lines: Vec::new(), closing: false, shut: false, skipped: false }
    }

    pub fn lines(&self) -> impl Iterator<Item = &SeqLine> {
        self.lines.iter().map(|(_, l)| l)
    }

    /// The daemon has finished: the goodnight line may show.
    pub fn shut(&self) -> bool {
        self.shut
    }

    /// Any key: leave now.
    pub fn skip(&mut self) {
        self.skipped = true;
    }

    /// The connection ended: the daemon is gone, which is the close finishing.
    pub fn disconnected(&mut self) {
        self.shut = true;
    }

    pub fn done(&self, now_ms: i64) -> bool {
        let waited = now_ms - self.began_ms;
        self.skipped || self.shut || waited >= Self::HARD_MS || (!self.closing && waited >= Self::QUIET_MS)
    }

    pub fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::Closing { .. } => self.closing = true,
            Event::ShuttingDown => {
                self.closing = true;
                self.shut = true;
            }
            Event::Phase(p) if p.kind == PhaseKind::Shutdown => {
                self.closing = true;
                self.phase(p);
            }
            _ => {}
        }
    }

    fn phase(&mut self, p: &PhaseEvent) {
        let stage = p.stage.as_deref().unwrap_or("");
        // One line per step; an unload is one line per model.
        let key = format!("{stage}:{}", if stage == "unload" { p.detail.as_deref().unwrap_or("") } else { "" });
        let label = match stage {
            "pause" => "pausing crew".to_string(),
            "save" => "saving sessions".to_string(),
            "unload" => format!("unloading {}", p.detail.as_deref().unwrap_or("models")),
            "journal" => "journal".to_string(),
            other => other.to_string(),
        };
        // Unloads run one after another: an end closes the oldest unload still open.
        let found = if stage == "unload" && p.state == PhaseState::End {
            self.lines.iter().position(|(k, l)| k.starts_with("unload:") && !l.done)
        } else {
            self.lines.iter().position(|(k, _)| *k == key)
        };
        let at = match found {
            Some(i) => i,
            None => {
                self.lines.push((key, SeqLine::pending(label)));
                self.lines.len() - 1
            }
        };
        if p.state != PhaseState::End {
            return;
        }
        let failed = p.detail.as_deref().is_some_and(|d| d.starts_with("failed"));
        let value = match (stage, p.done) {
            ("pause", Some(n)) if n > 0 => plural(n, "run", "runs"),
            ("pause", _) => "no runs".into(),
            ("save", Some(n)) if n > 0 => plural(n, "session", "sessions"),
            ("save", _) => "saved".into(),
            ("unload" | "journal", _) if failed => p.detail.clone().unwrap_or_default(),
            ("unload", Some(mib)) => format!("{} freed", gb(mib)),
            ("unload", None) => "unloaded".into(),
            ("journal", _) => "sealed".into(),
            _ => p.detail.clone().unwrap_or_else(|| "done".into()),
        };
        let health = if failed { Health::Degraded } else { Health::Ok };
        self.lines[at].1 = SeqLine::finished(self.lines[at].1.label.clone(), value, health);
    }
}

/// `Goodnight, sir.` with the profile's address; `Goodnight.` when it has none.
pub fn goodnight(address: Option<&str>) -> String {
    match address.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("Goodnight, {a}."),
        None => "Goodnight.".into(),
    }
}

// ---- in-transcript sequences ---------------------------------------------------------------------

/// What a transcript sequence is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A crew run spinning up: one line, its steps as they finish.
    Spinup,
    /// Research channels coming online: one line per channel.
    Channels,
}

impl BlockKind {
    /// The sequence a phase belongs to, if it is one.
    pub fn of(p: &PhaseEvent) -> Option<BlockKind> {
        match p.kind {
            PhaseKind::Spinup => Some(BlockKind::Spinup),
            PhaseKind::Channel => Some(BlockKind::Channels),
            _ => None,
        }
    }
}

/// A sequence in the transcript, updated in place by the run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub run: String,
    pub kind: BlockKind,
    pub lines: Vec<SeqLine>,
    /// Spin-up steps in the order they finished: `sandbox ✓`.
    steps: Vec<String>,
    /// Per channel, the results so far (parallel to `lines`).
    counts: Vec<u64>,
}

/// The steps of a spin-up; the line is done when all have ended.
const SPINUP_STEPS: usize = 4;

impl Block {
    /// Starts a sequence from its first phase (a spin-up's role is in `detail`).
    pub fn open(run: &str, p: &PhaseEvent) -> Option<Block> {
        let kind = BlockKind::of(p)?;
        let lines = match kind {
            BlockKind::Spinup => vec![SeqLine::pending(format!("spinning up {}", p.detail.as_deref().unwrap_or("agent")))],
            BlockKind::Channels => Vec::new(),
        };
        Some(Block { run: run.into(), kind, lines, steps: Vec::new(), counts: Vec::new() })
    }

    /// Whether `p` continues this sequence. A spin-up's opening `Start` (no stage) begins a new
    /// one once the last has finished (a run resumed after a pause spins up again).
    pub fn takes(&self, run: &str, p: &PhaseEvent) -> bool {
        if self.run != run || BlockKind::of(p) != Some(self.kind) {
            return false;
        }
        let opens = self.kind == BlockKind::Spinup && p.state == PhaseState::Start && p.stage.is_none();
        !(opens && self.lines.iter().all(|l| l.done))
    }

    /// Applies one phase. Returns whether the sequence changed.
    pub fn apply(&mut self, p: &PhaseEvent) -> bool {
        match self.kind {
            BlockKind::Spinup => self.spin(p),
            BlockKind::Channels => self.channel(p),
        }
    }

    fn spin(&mut self, p: &PhaseEvent) -> bool {
        if p.state != PhaseState::End {
            return false;
        }
        let Some(stage) = p.stage.as_deref() else { return false };
        let step = match stage {
            "tools" => format!("{} tools ✓", p.done.unwrap_or(0)),
            "budget" => format!("budget {} ✓", p.detail.as_deref().unwrap_or("-")),
            other => format!("{other} ✓"),
        };
        self.steps.push(step);
        let line = &mut self.lines[0];
        line.value = self.steps.join(" · ");
        if self.steps.len() >= SPINUP_STEPS {
            line.health = Health::Ok;
            line.done = true;
        }
        true
    }

    fn channel(&mut self, p: &PhaseEvent) -> bool {
        let Some(name) = p.stage.as_deref() else { return false };
        let at = match self.lines.iter().position(|l| l.label == name) {
            Some(i) => i,
            None => {
                self.lines.push(SeqLine::pending(name));
                self.counts.push(0);
                self.lines.len() - 1
            }
        };
        self.lines[at] = match p.state {
            PhaseState::Start => SeqLine { value: "online".into(), ..SeqLine::pending(name) },
            PhaseState::Progress => {
                let why = p.detail.as_deref().unwrap_or("retrying");
                SeqLine { health: Health::Degraded, label: name.into(), value: format!("{why} ⟳"), done: false }
            }
            PhaseState::End if p.detail.as_deref().is_some_and(|d| d.starts_with("failed")) => {
                SeqLine::finished(name, p.detail.clone().unwrap_or_default(), Health::Down)
            }
            PhaseState::End => {
                self.counts[at] += p.done.unwrap_or(0);
                SeqLine::finished(name, format!("{} ✓", self.counts[at]), Health::Ok)
            }
        };
        true
    }
}
