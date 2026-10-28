//! Projects in the client: a copy of each project's state that follows the daemon's patch stream,
//! the card that waits for an answer, and the entry that tells the project's story in the
//! conversation.
//!
//! Everything drawn comes from the project's JSON ([`marlowe_project::ProjectState`]) through the
//! generic view-model and [`marlowe_project::derive`]: nothing here names a project kind, a phase
//! or a step. The painters are in `render/projects*.rs`, the keys and clicks in `projects_input.rs`.
//!
//! A project that runs is drawn after every entry of the conversation; when it ends it settles
//! into the transcript as an [`Entry::Project`] at the time it ended.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use marlowe_project::{derive, Gate, GateKind, GateStatus, ProjectState, ProjectStatus, Registry, Replica};
use marlowe_proto::{Event, ProjectSummary, Request};
use serde::Deserialize;

use crate::render::projects::BLine;
use crate::state::{Entry, State};

/// How long the client lets patches pile up before it reads the state again.
const REPARSE_MS: i64 = 250;
/// Projects of the open conversation the client loads when it attaches.
const LOAD_AT_MOST: usize = 8;
/// The report file is read up to this many bytes.
const REPORT_BYTES: u64 = 4 << 20;
/// How long a card shows "answering" before it is offered again (the daemon never answered).
pub const ANSWER_WAIT_MS: i64 = 8_000;
/// The answer to `ListProjects` when this daemon runs no projects.
pub const NO_PROJECTS: &str = "projects are not running in this daemon";

/// The builtin registry, built once.
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Registry::builtin)
}

/// The deliverable's file as the client read it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDoc {
    /// Where it is.
    pub path: String,
    /// Its text, when the client could read it.
    pub text: Option<String>,
}

/// An answer sent and not yet seen in the state.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// The gate answered.
    pub gate: String,
    /// The option chosen.
    pub choice: String,
    /// When it was sent (client clock, ms).
    pub at_ms: i64,
}

/// One project as the client holds it.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The JSON copy that follows the patch stream.
    pub replica: Replica,
    /// The copy read as a [`ProjectState`] (it can lag the replica by [`REPARSE_MS`]).
    pub state: ProjectState,
    /// The revision `state` was read at.
    pub parsed_rev: u64,
    /// When `state` was last read (client clock, ms).
    parsed_at: Option<i64>,
    /// A bumped number the transcript cache keys on.
    pub version: u64,
    /// The report, once the project ended and has one.
    pub report: Option<ReportDoc>,
    /// An answer in flight.
    pub pending: Option<Pending>,
    /// The client saw this project running (so it settles at the time it ends).
    pub seen_live: bool,
    /// While the project runs again after it ended (a reopen): how many relays of it the
    /// transcript held when it went live, so the result of this run settles after its own relay
    /// and not after an earlier one's. `None` once the run's result is in the transcript.
    pub relays_seen: Option<usize>,
}

impl Loaded {
    /// The card that waits for the user.
    pub fn open_gate(&self) -> Option<&Gate> {
        self.state.gates.iter().find(|g| g.status == GateStatus::Open)
    }

    /// A plan or a checkin card is open.
    pub fn needs_you(&self) -> bool {
        matches!(self.state.status, ProjectStatus::AwaitingPlan | ProjectStatus::AwaitingCheckin)
    }

    /// Nothing will run.
    pub fn is_final(&self) -> bool {
        self.state.status.is_terminal()
    }

    /// The deliverable's path, from the item the last content step made.
    pub fn report_path(&self) -> Option<String> {
        derive::result_item(&self.state).and_then(|i| i.data.get("path")).and_then(|v| v.as_str()).map(str::to_string)
    }

    /// The card, unless an answer to it is on its way.
    pub fn answering(&self, now_ms: i64) -> Option<&Pending> {
        let g = self.open_gate()?;
        self.pending.as_ref().filter(|p| p.gate == g.id && now_ms - p.at_ms < ANSWER_WAIT_MS)
    }

    fn reparse(&mut self, now_ms: i64) -> bool {
        match ProjectState::deserialize(&self.replica.state) {
            Ok(st) => {
                self.state = st;
                self.parsed_rev = self.replica.rev;
                self.parsed_at = Some(now_ms);
                true
            }
            Err(_) => false,
        }
    }

    fn stale(&self) -> bool {
        self.parsed_rev != self.replica.rev
    }
}

/// What a click or a hover can land on. Carried inside `Target::Proj`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjTarget {
    /// A card option (project, option id).
    Option(String, String),
    /// A plan card's `revise`.
    Revise(String),
    /// `cancel` on a running project: asks first.
    Cancel(String),
    /// `y` of the cancel question.
    CancelYes(String),
    /// `n` of the cancel question.
    CancelNo,
    /// `checkin now`.
    Checkin(String),
    /// Read the report.
    Report(String),
    /// Fold or unfold a running entry.
    Fold(String),
    /// The entry itself: gives it the keys.
    Entry(String),
    /// A row of the board.
    Rail(String),
}

/// The inline question under a card.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    /// The text of a `steer` answer is being typed.
    Steer { project: String, text: String },
    /// `cancel p_x? y / n`.
    ConfirmCancel { project: String },
}

/// The key each option of a card answers to: `a` for approve and `x` for cancel wherever they
/// appear, `r` is taken by a plan's revise, and every other option takes the first letter of its
/// label that no earlier option took.
pub fn option_keys(g: &Gate) -> Vec<char> {
    let mut used: Vec<char> = vec!['a', 'x'];
    if g.kind == GateKind::Plan {
        used.push('r');
    }
    let mut out = Vec::new();
    for o in &g.options {
        let k = match o.id.as_str() {
            "approve" => 'a',
            "cancel" => 'x',
            _ => {
                let pick = o.label.chars().chain(o.id.chars()).map(|c| c.to_ascii_lowercase()).chain('a'..='z').find(|c| c.is_ascii_lowercase() && !used.contains(c));
                let k = pick.unwrap_or('?');
                used.push(k);
                k
            }
        };
        out.push(k);
    }
    out
}

/// A live entry's rows between frames: laying one out walks every item.
#[derive(Debug, Clone)]
pub struct Memo {
    /// What the rows depend on.
    pub key: u64,
    /// The rows.
    pub rows: Vec<BLine>,
}

/// Everything the client holds about projects between frames.
#[derive(Debug, Clone, Default)]
pub struct ProjectsUi {
    /// The projects of the open conversation.
    pub loaded: Vec<Loaded>,
    /// Projects the state of which was asked for and has not come.
    asked: BTreeSet<String>,
    /// Projects of other conversations: their updates are ignored.
    foreign: BTreeSet<String>,
    /// Answers already seen (the daemon tells every client twice).
    answered: BTreeSet<(String, String)>,
    /// The project whose entry has the keys (while the Conversation box has focus).
    pub focus: Option<String>,
    /// The focused option of the open card.
    pub card_sel: usize,
    /// The gate `card_sel` belongs to.
    card_gate: Option<(String, String)>,
    /// The question under a card, if one is open.
    pub prompt: Option<Prompt>,
    /// Running entries folded to their header.
    pub folded: BTreeSet<String>,
    /// The clock the entries are drawn against, when it is not the client's (tests).
    pub clock: Option<u64>,
    /// The height of the conversation pane as last drawn.
    pub pane_rows: Cell<u16>,
    next_version: u64,
    /// The rows of each live entry, by project.
    pub memo: RefCell<BTreeMap<String, Memo>>,
}

impl ProjectsUi {
    /// Projects are on screen.
    pub fn active(&self) -> bool {
        !self.loaded.is_empty()
    }

    /// The loaded project with this id.
    pub fn get(&self, id: &str) -> Option<&Loaded> {
        self.loaded.iter().find(|l| l.state.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Loaded> {
        self.loaded.iter_mut().find(|l| l.state.id == id)
    }

    /// Projects that are not final.
    pub fn live(&self) -> impl Iterator<Item = &Loaded> {
        self.loaded.iter().filter(|l| !l.is_final())
    }

    /// Projects that wait on a card.
    pub fn needs_you(&self) -> usize {
        self.loaded.iter().filter(|l| l.needs_you()).count()
    }

    /// Projects running or waiting.
    pub fn running(&self) -> usize {
        self.live().count()
    }

    /// The clock the entries are drawn against.
    pub fn now(&self, client_ms: i64) -> u64 {
        self.clock.unwrap_or(client_ms.max(0) as u64)
    }

    /// The project `alt-p` goes to: the one that needs you, else the newest running one.
    pub fn attention(&self) -> Option<&str> {
        self.loaded
            .iter()
            .find(|l| l.needs_you())
            .or_else(|| self.live().max_by_key(|l| l.state.updated_ms))
            .map(|l| l.state.id.as_str())
    }

    /// The projects as the board lists them: waiting first, then running, then the newest finished.
    pub fn ordered(&self) -> Vec<&Loaded> {
        let mut v: Vec<&Loaded> = self.loaded.iter().collect();
        v.sort_by_key(|l| {
            let rank = if l.needs_you() {
                0
            } else if !l.is_final() {
                1
            } else {
                2
            };
            (rank, std::cmp::Reverse(l.state.updated_ms))
        });
        v
    }

    /// The version a transcript row of this project depends on.
    pub fn version_of(&self, id: &str) -> u64 {
        self.get(id).map_or(0, |l| l.version)
    }

    /// The option of the open card that a key letter answers.
    pub fn option_for_key(&self, project: &str, c: char) -> Option<String> {
        let g = self.get(project)?.open_gate()?;
        let keys = option_keys(g);
        g.options.iter().zip(keys).find(|(_, k)| *k == c).map(|(o, _)| o.id.clone())
    }

    /// The lead's recommendation on the open card, by option id.
    pub fn recommended(&self, project: &str) -> Option<String> {
        let g = self.get(project)?.open_gate()?;
        g.payload.get("recommended").and_then(|v| v.as_str()).map(str::to_string)
    }

    /// Keeps the card's focused option in step with the card that is open: a new card starts on the
    /// lead's recommendation.
    pub fn sync_card(&mut self, project: &str) {
        let Some(l) = self.get(project) else { return };
        let Some(g) = l.open_gate() else {
            self.card_gate = None;
            return;
        };
        let key = (project.to_string(), g.id.clone());
        if self.card_gate.as_ref() != Some(&key) {
            let rec = g.payload.get("recommended").and_then(|v| v.as_str());
            self.card_sel = rec.and_then(|r| g.options.iter().position(|o| o.id == r)).unwrap_or(0);
            self.card_gate = Some(key);
            if matches!(&self.prompt, Some(Prompt::Steer { project: p, .. } | Prompt::ConfirmCancel { project: p }) if p == project) {
                self.prompt = None;
            }
        }
    }

    /// Answers the project's open card with option `id`; `steer` first asks for its text and
    /// `cancel` asks to be sure. True when a request was sent.
    pub fn answer(&mut self, project: &str, id: &str, now_ms: i64, out: &mut Vec<Request>) -> bool {
        let Some(l) = self.get(project) else { return false };
        if l.answering(now_ms).is_some() {
            return false;
        }
        let Some(g) = l.open_gate() else { return false };
        let Some(opt) = g.options.iter().find(|o| o.id == id) else { return false };
        let steer_text = match &self.prompt {
            Some(Prompt::Steer { project: p, text }) if p == project => Some(text.clone()),
            _ => None,
        };
        if opt.needs_text && steer_text.is_none() {
            self.prompt = Some(Prompt::Steer { project: project.to_string(), text: String::new() });
            return false;
        }
        let confirmed = matches!(&self.prompt, Some(Prompt::ConfirmCancel { project: p }) if p == project);
        if id == "cancel" && !confirmed {
            self.prompt = Some(Prompt::ConfirmCancel { project: project.to_string() });
            return false;
        }
        let gate = g.id.clone();
        let text = steer_text.filter(|t| !t.trim().is_empty());
        self.prompt = None;
        if let Some(l) = self.get_mut(project) {
            l.pending = Some(Pending { gate: gate.clone(), choice: id.to_string(), at_ms: now_ms });
        }
        self.bump(project);
        out.push(Request::AnswerProjectGate { project: project.to_string(), gate, choice: id.to_string(), text });
        true
    }

    /// Cancels a running project after the inline question.
    pub fn cancel_run(&mut self, project: &str, out: &mut Vec<Request>) -> bool {
        let Some(l) = self.get(project) else { return false };
        if l.is_final() {
            return false;
        }
        let confirmed = matches!(&self.prompt, Some(Prompt::ConfirmCancel { project: p }) if p == project);
        if !confirmed {
            self.prompt = Some(Prompt::ConfirmCancel { project: project.to_string() });
            return false;
        }
        self.prompt = None;
        out.push(Request::CancelProject { project: project.to_string() });
        true
    }

    /// Forces a checkin at the next safe point.
    pub fn checkin_now(&mut self, project: &str, out: &mut Vec<Request>) -> bool {
        let Some(l) = self.get(project) else { return false };
        if l.is_final() || l.needs_you() {
            return false;
        }
        out.push(Request::CheckinProject { project: project.to_string() });
        true
    }

    /// The text `revise` puts in the composer.
    pub fn revise_text(&self, project: &str) -> Option<String> {
        self.get(project).map(|l| format!("revise the plan of {}: ", l.state.id))
    }

    fn bump(&mut self, project: &str) {
        self.next_version += 1;
        let v = self.next_version;
        if let Some(l) = self.get_mut(project) {
            l.version = v;
        }
    }

    /// Forgets everything: the conversation changed.
    pub fn reset(&mut self) {
        *self = ProjectsUi::default();
    }
}

impl State {
    /// A daemon event about projects. True when it was one.
    pub(crate) fn projects_event(&mut self, ev: &Event, out: &mut Vec<Request>) -> bool {
        if matches!(ev, Event::ProjectList { .. } | Event::ProjectState { .. } | Event::ProjectUpdated { .. }) {
            self.wake.projects_presence = crate::wake::Presence::Online;
        }
        match ev {
            Event::ProjectList { projects } => self.on_project_list(projects, out),
            Event::ProjectState { project, rev, state } => self.on_project_state(project, *rev, state, out),
            Event::ProjectUpdated { project, base, rev, patches } => self.on_project_updated(project, *base, *rev, patches, out),
            Event::ProjectGateRaised { project, .. } => {
                if self.projects.get(project).is_none() {
                    self.ask_project(project, out);
                }
            }
            Event::ProjectGateAnswered { project, gate, .. } => {
                if self.projects.answered.insert((project.clone(), gate.clone())) {
                    let now = self.now_ms;
                    if let Some(l) = self.projects.get_mut(project) {
                        // Answered here or somewhere else: the card is no longer ours to offer.
                        if l.pending.as_ref().is_none_or(|p| &p.gate != gate) {
                            l.pending = Some(Pending { gate: gate.clone(), choice: String::new(), at_ms: now });
                        }
                    }
                    self.projects.bump(project);
                    if matches!(&self.projects.prompt, Some(Prompt::Steer { project: p, .. } | Prompt::ConfirmCancel { project: p }) if p == project) {
                        self.projects.prompt = None;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn ask_project(&mut self, project: &str, out: &mut Vec<Request>) {
        if self.projects.foreign.contains(project) {
            return;
        }
        if self.projects.asked.insert(project.to_string()) {
            out.push(Request::GetProject { project: project.to_string() });
        }
    }

    fn mine_str(&self, session: Option<&str>) -> bool {
        match (self.session.as_ref(), session) {
            (Some(mine), Some(s)) => mine.as_str() == s,
            _ => false,
        }
    }

    fn on_project_list(&mut self, projects: &[ProjectSummary], out: &mut Vec<Request>) {
        if self.session.is_none() {
            return;
        }
        let mut taken = 0;
        for p in projects {
            if !self.mine_str(p.session.as_deref()) {
                self.projects.foreign.insert(p.id.clone());
                continue;
            }
            self.projects.foreign.remove(&p.id);
            let behind = self.projects.get(&p.id).is_none_or(|l| l.replica.rev < p.rev);
            if behind && taken < LOAD_AT_MOST {
                taken += 1;
                self.ask_project(&p.id, out);
            }
        }
    }

    fn on_project_state(&mut self, project: &str, rev: u64, state: &serde_json::Value, _out: &mut Vec<Request>) {
        self.projects.asked.remove(project);
        let Ok(st) = ProjectState::deserialize(state) else { return };
        if self.session.is_none() || !self.mine_str(st.session.as_deref()) {
            self.projects.foreign.insert(project.to_string());
            self.projects.loaded.retain(|l| l.state.id != project);
            return;
        }
        self.projects.foreign.remove(project);
        let now = self.now_ms;
        let mut replica = Replica::from_state(state.clone());
        replica.rev = rev;
        let (report, pending, seen_live, relays_seen) = match self.projects.get(project) {
            Some(old) => (old.report.clone(), old.pending.clone(), old.seen_live, old.relays_seen),
            None => (None, None, false, None),
        };
        let l = Loaded { replica, state: st, parsed_rev: rev, parsed_at: Some(now), version: 0, report, pending, seen_live, relays_seen };
        match self.projects.get_mut(project) {
            Some(slot) => *slot = l,
            None => self.projects.loaded.push(l),
        }
        self.projects.bump(project);
        self.projects_settle();
        self.projects.sync_card(project);
    }

    fn on_project_updated(&mut self, project: &str, base: u64, rev: u64, patches: &[marlowe_proto::ProjectPatch], out: &mut Vec<Request>) {
        if self.projects.foreign.contains(project) {
            return;
        }
        let now = self.now_ms;
        let applied = match self.projects.get_mut(project) {
            None => None,
            Some(l) => Some(l.replica.apply(base, rev, patches).is_ok()),
        };
        match applied {
            None | Some(false) => self.ask_project(project, out),
            Some(true) => {
                // A status or a card changing is read at once; a burst of item changes at most
                // four times a second.
                let urgent = patches.iter().any(|p| p.path.starts_with("/status") || p.path.starts_with("/gates"));
                let due = self.projects.get(project).is_some_and(|l| urgent || l.parsed_at.is_none_or(|t| now - t >= REPARSE_MS));
                if due {
                    self.projects_reparse(project);
                }
            }
        }
    }

    fn projects_reparse(&mut self, project: &str) {
        let now = self.now_ms;
        let ok = self.projects.get_mut(project).is_some_and(|l| l.reparse(now));
        if ok {
            self.projects.bump(project);
            self.projects_settle();
            self.projects.sync_card(project);
        }
    }

    /// Run-loop work: patches that arrived inside the reparse window are read once it has passed.
    pub(crate) fn projects_tick(&mut self) {
        if self.projects.loaded.is_empty() {
            return;
        }
        let now = self.now_ms;
        let due: Vec<String> =
            self.projects.loaded.iter().filter(|l| l.stale() && l.parsed_at.is_none_or(|t| now - t >= REPARSE_MS)).map(|l| l.state.id.clone()).collect();
        for id in due {
            self.projects_reparse(&id);
        }
    }

    /// Reads the reports of projects that ended and puts their entries in the transcript. A
    /// project that runs again (a reopen) takes its old entry out: its block is drawn after the
    /// conversation while it runs, and the result of the new run settles when it ends. Entries
    /// are not moved while a turn streams (the turn's blocks are found by position); the end of
    /// the turn settles again.
    pub(crate) fn projects_settle(&mut self) {
        let writing = self.live.is_some();
        let ids: Vec<String> = self.projects.loaded.iter().map(|l| l.state.id.clone()).collect();
        for id in ids {
            let held = self.entries.iter().filter(|e| matches!(e, Entry::News { label, .. } if label.contains(id.as_str()))).count();
            let Some(l) = self.projects.get_mut(&id) else { continue };
            if !l.is_final() {
                l.seen_live = true;
                l.relays_seen.get_or_insert(held);
                if !writing {
                    if let Some(i) = self.project_entry_at(&id) {
                        self.entries.remove(i);
                    }
                }
                continue;
            }
            if l.report.is_none() {
                if let Some(path) = l.report_path() {
                    let text = read_report(&path);
                    l.report = Some(ReportDoc { path, text });
                }
            }
            if !writing && self.project_entry_at(&id).is_none() {
                self.place_project_entry(&id);
            }
        }
    }

    fn project_entry_at(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| matches!(e, Entry::Project { id: p } if p == id))
    }

    /// Puts the result of an ended project in the transcript: where its relay already stands, else
    /// at the end when the client watched it end.
    fn place_project_entry(&mut self, id: &str) {
        let Some(l) = self.projects.get(id) else { return };
        let word = l.state.status.as_str();
        let (seen_live, skip) = (l.seen_live, l.relays_seen.unwrap_or(0));
        // The relay of this run: after the ones of the runs before it.
        let relay = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Entry::News { label, .. } if label.contains(id)))
            .skip(skip)
            .filter(|(_, e)| matches!(e, Entry::News { label, .. } if label.trim_end().ends_with(word)))
            .map(|(i, _)| i)
            .last();
        if let Some(l) = self.projects.get_mut(id) {
            l.relays_seen = None;
        }
        let entry = Entry::Project { id: id.to_string() };
        match relay {
            Some(i) => self.entries.insert(i + 1, entry),
            None if seen_live => self.entries.push(entry),
            None => {}
        }
    }

    /// A news entry about a project that already has its result in the transcript goes before
    /// it: the label first, then the card (the relay turn starts just after the project ends).
    pub(crate) fn projects_news_order(&mut self) {
        let n = self.entries.len();
        if n < 2 {
            return;
        }
        let swap = match (&self.entries[n - 2], &self.entries[n - 1]) {
            (Entry::Project { id }, Entry::News { label, .. }) => label.contains(id.as_str()),
            _ => false,
        };
        if swap {
            self.entries.swap(n - 2, n - 1);
        }
    }

    /// The transcript was rebuilt (a session opened or reloaded): the results go back in.
    pub(crate) fn projects_after_transcript(&mut self, out: &mut Vec<Request>) {
        let session = self.session.clone();
        self.projects.loaded.retain(|l| l.state.session.as_deref() == session.as_ref().map(|s| s.as_str()));
        self.projects_settle();
        if self.conn.is_online() && self.asks_projects() {
            out.push(Request::ListProjects);
        }
    }

    /// The header's count of projects: (running or waiting, waiting).
    pub fn project_counts(&self) -> (usize, usize) {
        (self.projects.running(), self.projects.needs_you())
    }
}

fn read_report(path: &str) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut s = String::new();
    f.take(REPORT_BYTES).read_to_string(&mut s).ok()?;
    Some(s)
}
