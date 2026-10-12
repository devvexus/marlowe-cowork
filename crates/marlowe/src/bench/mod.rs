//! `marlowe bench <model> [--suite tools|edits|memory|research|coding|all] [--n N] [--seed S]
//! [--vs MODEL] [--apply] [--json FILE]` (track L, design 3.10): the report card for a model.
//!
//! | Suite | Measures | Gate | Runs |
//! |---|---|---|---|
//! | `tools` | 200 prompts: right tool, right values, schema-valid, share repaired | A4 | straight at the provider with the real tool specs ([`tools`]) |
//! | `edits` | 40 small edits per format (`edit`, `edit_lines`, `write`): first-attempt apply, intended text within 2 retries | A5 | straight at the provider ([`edits`]) |
//! | `memory` | facts told in one process, asked in another under a private home | - | `marlowe -p --standalone` children ([`e2e`]) |
//! | `research` | answers and quoted citations over local notes, citations checked against the files | - | children |
//! | `coding` | the micro coding tasks through the harness | - | `marlowe eval run --suite micro` |
//!
//! `--apply` records the formats that did best for the model (`Library::set_edit_formats`); without
//! it nothing is written but the report (`<home>\bench\`). The model valiance is refused until
//! Matthew enables it by creating `<home>\bench\allow-valiance` (design 9.2-i).
//!
//! Live runs load the model: they belong to a GPU window Matthew opens. A debug build with
//! `MARLOWE_SCRIPTED_PROVIDER` answers from a script instead and touches no model.

mod e2e;
mod edits;
mod prompts;
mod tools;

#[cfg(test)]
mod fake;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use marlowe_brains::{CredentialStore, KeyringStore, Library, MemoryStore};
use marlowe_code::edit::bench::Format;
use marlowe_model::{CancelToken, ModelInfo, ModelRef, Provider, ProviderKind, ToolSpec};
use marlowe_provider::ollama::repair::RepairStats;
use serde::Serialize;

use e2e::{CodingReport, MemoryReport, RealSpawn, ResearchReport, Setup, Spawn};
use edits::EditsReport;
use tools::ToolsReport;

const USAGE: &str = "\
marlowe bench <model> [flags]

  --suite LIST     tools, edits, memory, research, coding, or all (default all); comma list
  --n N            cap each suite at N items (tools 200, edits 40 per format, memory 6, research 5)
  --seed S         seed for the edit tasks (default 1)
  --vs MODEL       also bench MODEL and print the two side by side
  --apply          record the best edit formats for the model in the library
  --json FILE      write the report here (default <home>\\bench\\<model>-<time>.json)
  --timeout SECS   per child run for memory, research and coding (default 900)

<model> is provider/model (ollama/marlowe-dusk:27b-super) or a bare id from the library.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    Tools,
    Edits,
    Memory,
    Research,
    Coding,
}

impl Suite {
    const ALL: [Suite; 5] = [Suite::Tools, Suite::Edits, Suite::Memory, Suite::Research, Suite::Coding];

    fn parse(s: &str) -> Result<Vec<Suite>, String> {
        let mut out = Vec::new();
        for w in s.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match w {
                "all" => out.extend(Suite::ALL),
                "tools" => out.push(Suite::Tools),
                "edits" => out.push(Suite::Edits),
                "memory" => out.push(Suite::Memory),
                "research" => out.push(Suite::Research),
                "coding" => out.push(Suite::Coding),
                other => return Err(format!("unknown suite {other:?} (tools, edits, memory, research, coding, all)")),
            }
        }
        if out.is_empty() {
            return Err("--suite needs at least one suite".into());
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct Opts {
    pub model: String,
    pub suites: BTreeSet<Suite>,
    pub n: Option<usize>,
    pub seed: u64,
    pub vs: Option<String>,
    pub apply: bool,
    pub json: Option<PathBuf>,
    pub timeout: Duration,
}

pub fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        model: String::new(),
        suites: BTreeSet::new(),
        n: None,
        seed: 1,
        vs: None,
        apply: false,
        json: None,
        timeout: Duration::from_secs(900),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--suite" => o.suites.extend(Suite::parse(&value("--suite")?)?),
            "--n" => o.n = Some(value("--n")?.parse().map_err(|_| "--n needs a number")?),
            "--seed" => o.seed = value("--seed")?.parse().map_err(|_| "--seed needs a number")?,
            "--vs" => o.vs = Some(value("--vs")?),
            "--json" => o.json = Some(PathBuf::from(value("--json")?)),
            "--timeout" => o.timeout = Duration::from_secs(value("--timeout")?.parse().map_err(|_| "--timeout needs seconds")?),
            "--apply" => o.apply = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            model if o.model.is_empty() => o.model = model.to_string(),
            extra => return Err(format!("unexpected argument {extra:?}")),
        }
    }
    if o.model.is_empty() {
        return Err("a model is needed".into());
    }
    if o.suites.is_empty() {
        o.suites.extend(Suite::ALL);
    }
    if o.apply && !o.suites.contains(&Suite::Edits) {
        return Err("--apply records edit formats, so it needs the edits suite".into());
    }
    Ok(o)
}

// ---- the card ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GateRow {
    pub id: &'static str,
    pub pass: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Card {
    pub model: String,
    pub local: bool,
    pub started_unix: u64,
    pub tools: Option<ToolsReport>,
    pub edits: Option<EditsReport>,
    pub memory: Option<MemoryReport>,
    pub research: Option<ResearchReport>,
    pub coding: Option<CodingReport>,
    /// Suites that did not run, and why.
    pub skipped: Vec<String>,
    pub gates: Vec<GateRow>,
}

fn pct(x: f64) -> String {
    format!("{:.1}%", 100.0 * x)
}

fn verdict(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "FAIL"
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

impl Card {
    fn compute_gates(&self) -> Vec<GateRow> {
        let mut g = Vec::new();
        if let Some(t) = &self.tools {
            let (valid, repair) = if self.local { (0.98, 0.05) } else { (0.995, 1.0) };
            g.push(GateRow {
                id: "A4",
                pass: t.meets_a4(self.local),
                detail: format!(
                    "valid after repair {} of all prompts (need {}), needing repair {} (need <= {}), {} prompts, {} got a tool call",
                    pct(t.valid_rate),
                    pct(valid),
                    pct(t.repair_rate),
                    if self.local { pct(repair) } else { "any".into() },
                    t.prompts,
                    pct(t.call_rate())
                ),
            });
        }
        if let Some(e) = &self.edits {
            let best = e.recommended.first().and_then(|f| e.formats.get(f).map(|r| (f.clone(), r.clone())));
            g.push(GateRow {
                id: "A5",
                pass: e.meets_a5(),
                detail: match best {
                    Some((f, r)) => format!(
                        "best format {f}: first attempt {} (need {}), within {} retries {} (need {})",
                        pct(r.first_rate),
                        if self.local { "92%" } else { "98%" },
                        e.retries,
                        pct(r.success_rate),
                        if self.local { "98%" } else { "99.5%" }
                    ),
                    None => "no format reached 90% within two retries".into(),
                },
            });
        }
        g
    }

    pub fn render(&self) -> String {
        let mut s = format!("Marlowe bench: {} ({})\n", self.model, if self.local { "local" } else { "frontier" });
        if let Some(t) = &self.tools {
            s += &format!(
                "tools    {} prompts{}: right tool {}, right values {}, schema-valid {}; valid after repair {}, needing repair {}; {:.0} ms/call\n",
                t.prompts,
                if t.aborted { " (stopped early)" } else { "" },
                pct(t.selection_rate()),
                pct(t.args_rate()),
                pct(ratio(t.schema_valid, t.prompts)),
                pct(t.valid_rate),
                pct(t.repair_rate),
                t.mean_ms
            );
            for m in t.misses.iter().take(3) {
                s += &format!("           miss: {}\n", m.chars().take(160).collect::<String>());
            }
        }
        if let Some(e) = &self.edits {
            for (f, r) in &e.formats {
                s += &format!(
                    "edits    {f:<10} first attempt {} ({}/{}), within {} retries {} ({}/{}), {:.2} calls/edit\n",
                    pct(r.first_rate),
                    r.first_applied,
                    r.cases,
                    e.retries,
                    pct(r.success_rate),
                    r.succeeded,
                    r.cases,
                    r.mean_calls
                );
            }
            if !e.skipped.is_empty() {
                s += &format!("           not offered: {}\n", e.skipped.join(", "));
            }
            s += &format!("           recommended: {}\n", if e.recommended.is_empty() { "none".into() } else { e.recommended.join(" > ") });
        }
        if let Some(m) = &self.memory {
            s += &format!("memory   recalled {}/{} after a restart ({}), stored {}/{}\n", m.recalled, m.facts, pct(m.recall_rate), m.stored, m.facts);
            for e in m.errors.iter().take(2) {
                s += &format!("           {e}\n");
            }
        }
        if let Some(r) = &self.research {
            s += &format!(
                "research answers {}/{} ({}), citations verified {}/{} ({})\n",
                r.answers_correct,
                r.questions,
                pct(r.answer_rate),
                r.citations_verified,
                r.citations,
                pct(r.citation_accuracy)
            );
            for e in r.errors.iter().take(2) {
                s += &format!("           {e}\n");
            }
        }
        if let Some(c) = &self.coding {
            match &c.error {
                Some(e) => s += &format!("coding   not run: {e}\n"),
                None => s += &format!("coding   micro tasks passed {}/{} ({})\n", c.passed, c.runs, pct(c.pass_rate)),
            }
        }
        for k in &self.skipped {
            s += &format!("skipped  {k}\n");
        }
        for g in &self.gates {
            s += &format!("{:<4} {}  {}\n", g.id, verdict(g.pass), g.detail);
        }
        s
    }
}

/// Two cards side by side: the headline numbers and their difference (b - a, in points).
pub fn render_vs(a: &Card, b: &Card) -> String {
    type Pick = fn(&edits::FormatRow) -> f64;
    let best = |c: &Card, pick: Pick| c.edits.as_ref().and_then(|e| e.recommended.first().and_then(|f| e.formats.get(f))).map(pick);
    let coding = |c: &Card| c.coding.as_ref().filter(|c| c.error.is_none()).map(|c| c.pass_rate);
    let rows: Vec<(&str, Option<f64>, Option<f64>)> = vec![
        ("tools: valid after repair", a.tools.as_ref().map(|t| t.valid_rate), b.tools.as_ref().map(|t| t.valid_rate)),
        ("tools: right tool", a.tools.as_ref().map(ToolsReport::selection_rate), b.tools.as_ref().map(ToolsReport::selection_rate)),
        ("tools: right values", a.tools.as_ref().map(ToolsReport::args_rate), b.tools.as_ref().map(ToolsReport::args_rate)),
        ("edits: first attempt (best format)", best(a, |r| r.first_rate), best(b, |r| r.first_rate)),
        ("edits: within 2 retries (best format)", best(a, |r| r.success_rate), best(b, |r| r.success_rate)),
        ("memory: recall after restart", a.memory.as_ref().map(|m| m.recall_rate), b.memory.as_ref().map(|m| m.recall_rate)),
        ("research: citation accuracy", a.research.as_ref().map(|r| r.citation_accuracy), b.research.as_ref().map(|r| r.citation_accuracy)),
        ("coding: micro pass rate", coding(a), coding(b)),
    ];
    let cell = |v: Option<f64>| v.map(pct).unwrap_or_else(|| "-".into());
    let mut s = format!("{:<40} {:>14} {:>14} {:>9}\n", "", short(&a.model), short(&b.model), "delta");
    for (name, x, y) in rows {
        if x.is_none() && y.is_none() {
            continue;
        }
        let d = match (x, y) {
            (Some(x), Some(y)) => format!("{:+.1}", 100.0 * (y - x)),
            _ => "-".into(),
        };
        s += &format!("{name:<40} {:>14} {:>14} {d:>9}\n", cell(x), cell(y));
    }
    s
}

fn short(model: &str) -> String {
    let id = model.rsplit('/').next().unwrap_or(model);
    id.chars().take(14).collect()
}

// ---- running a card ---------------------------------------------------------------------

pub struct Ctx<'a> {
    pub provider: &'a dyn Provider,
    pub stats: Option<&'a RepairStats>,
    pub spawn: &'a dyn Spawn,
    pub exe: &'a Path,
    /// Scratch space for children: a private `MARLOWE_HOME`, workspaces, summaries.
    pub scratch: &'a Path,
    /// Where the harness is run from (the checkout), for the coding suite.
    pub checkout: &'a Path,
    /// The tools a session of this model class offers (the tools suite).
    pub session_specs: Vec<ToolSpec>,
    /// Every edit tool (the edits suite measures all formats whatever the session offers).
    pub edit_specs: Vec<ToolSpec>,
}

pub fn measure(ctx: &Ctx<'_>, model: &ModelRef, local: bool, o: &Opts, cancel: &CancelToken, say: &mut dyn FnMut(&str)) -> Card {
    let mut card = Card {
        model: model.to_string(),
        local,
        started_unix: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        ..Card::default()
    };
    let model_id = model.model_id.as_str();
    for suite in Suite::ALL.iter().filter(|s| o.suites.contains(*s)) {
        if cancel.is_cancelled() {
            card.skipped.push(format!("{suite:?}: cancelled"));
            continue;
        }
        match suite {
            Suite::Tools => {
                let available: Vec<String> = ctx.session_specs.iter().map(|s| s.name.clone()).collect();
                let prompts = prompts::tool_prompts(&available, o.n.unwrap_or(200).min(200));
                if prompts.is_empty() {
                    card.skipped.push("tools: the session offers none of the tools the prompts use".into());
                    continue;
                }
                say(&format!("tools: {} prompts", prompts.len()));
                let report = tools::run(ctx.provider, model_id, &ctx.session_specs, &prompts, ctx.stats, cancel, |i, n| {
                    if i % 20 == 0 {
                        say(&format!("tools {i}/{n}"));
                    }
                });
                card.tools = Some(report);
            }
            Suite::Edits => {
                let tasks = edits::tasks(o.n.unwrap_or(40), o.seed);
                say(&format!("edits: {} tasks x 3 formats", tasks.len()));
                let formats = [Format::Edit, Format::EditLines, Format::Write];
                let report = edits::run(ctx.provider, model_id, &ctx.edit_specs, &tasks, &formats, local, cancel, |f, i, n| {
                    if i % 10 == 0 {
                        say(&format!("edits {f} {i}/{n}"));
                    }
                });
                card.edits = Some(report);
            }
            Suite::Memory | Suite::Research => {
                let (home, ws) = (ctx.scratch.join("home"), ctx.scratch.join(format!("ws-{suite:?}").to_lowercase()));
                if let Err(e) = std::fs::create_dir_all(&home).and_then(|_| std::fs::create_dir_all(&ws)) {
                    card.skipped.push(format!("{suite:?}: cannot make a scratch home: {e}"));
                    continue;
                }
                let model_ref = model.to_string();
                let setup = Setup { exe: ctx.exe, home: &home, workspace: &ws, model: &model_ref, timeout: o.timeout };
                if *suite == Suite::Memory {
                    say("memory: facts told, process restarted, facts asked");
                    card.memory = Some(e2e::memory(ctx.spawn, &setup, o.n.unwrap_or(e2e::FACTS.len())));
                } else {
                    say("research: questions over local notes");
                    card.research = Some(e2e::research(ctx.spawn, &setup, o.n.unwrap_or(e2e::QUESTIONS.len())));
                }
            }
            Suite::Coding => {
                say("coding: micro tasks through marlowe eval run");
                card.coding = Some(e2e::coding(ctx.spawn, ctx.exe, ctx.checkout, &model.to_string(), o.n, o.timeout * 20, ctx.scratch));
            }
        }
    }
    card.gates = card.compute_gates();
    card
}

// ---- the real thing ---------------------------------------------------------------------

/// Whether a debug build is running from a script (no model, no key).
fn scripted() -> bool {
    #[cfg(debug_assertions)]
    {
        crate::scripted::active()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

fn open_library(home: &Path) -> Option<Library> {
    let db = marlowe_proto::paths::cowork_db(home);
    db.is_file().then(|| Library::open(&db).ok()).flatten()
}

/// The model the argument names: `provider/id`, or a bare id looked up in the library (an
/// unknown bare id is taken as an Ollama tag). Local means the model runs on this machine.
fn resolve(home: &Path, raw: &str) -> (ModelRef, bool) {
    let lib = open_library(home);
    let model = match raw.parse::<ModelRef>() {
        Ok(r) => r,
        Err(_) => lib
            .as_ref()
            .and_then(|l| l.models().ok())
            .and_then(|ms| ms.into_iter().find(|m| m.model.model_id == raw).map(|m| m.model))
            .unwrap_or_else(|| ModelRef::new(ProviderKind::Ollama, raw)),
    };
    let flagged = lib.as_ref().and_then(|l| l.get(&model).ok().flatten()).is_some_and(|m| m.info.local);
    let local = flagged || matches!(model.provider, ProviderKind::Ollama | ProviderKind::LocalGguf);
    (model, local)
}

/// Valiance stays blocked until Matthew enables it (design 9.2-i).
fn check_allowed(home: &Path, model: &ModelRef) -> Result<(), String> {
    let marker = home.join("bench").join("allow-valiance");
    if model.model_id.to_lowercase().contains("valiance") && !marker.is_file() {
        return Err(format!("{model} is blocked in bench until Matthew enables it: create {} to allow it", marker.display()));
    }
    Ok(())
}

fn session_specs(local: bool) -> Vec<ToolSpec> {
    use marlowe_agent::{ExposureProfile, ToolHost};
    let profile = if local { ExposureProfile::Local } else { ExposureProfile::Frontier };
    let groups = marlowe_exec::specs::exposed_groups(profile, None);
    marlowe_exec::BuiltinTools::new(None).with_exposure(profile, groups).specs()
}

fn edit_specs() -> Vec<ToolSpec> {
    use marlowe_agent::{ExposureProfile, ToolGroup, ToolHost};
    marlowe_exec::BuiltinTools::new(None).with_exposure(ExposureProfile::Frontier, ToolGroup::ALL.to_vec()).specs()
}

/// The provider for `model`, with the library's context allocation for it applied.
fn provider_for(home: &Path, model: &ModelRef) -> Result<Arc<dyn Provider>, String> {
    let credentials: Arc<dyn CredentialStore> = if scripted() { Arc::new(MemoryStore::new()) } else { Arc::new(KeyringStore::new()) };
    let factory = crate::wire::provider_factory(credentials, home);
    if let Some(policy) = open_library(home).and_then(|l| l.policy(model).ok().flatten()) {
        factory.allocations().set(&model.model_id, policy.max_ctx);
    }
    factory.provider(model.provider).map_err(|e| format!("{}: {e}", model.provider))
}

/// A scratch profile home that knows `model`, for the children (the real home is never written).
fn prepare_home(real: &Path, scratch: &Path, model: &ModelRef) -> Result<(), String> {
    let home = scratch.join("home");
    std::fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    let dst = Library::open(&marlowe_proto::paths::cowork_db(&home)).map_err(|e| e.to_string())?;
    let src = open_library(real);
    let info = src.as_ref().and_then(|l| l.get(model).ok().flatten()).map(|m| m.info).unwrap_or_else(|| ModelInfo {
        supports_tools: true,
        local: matches!(model.provider, ProviderKind::Ollama | ProviderKind::LocalGguf),
        ..ModelInfo::bare(model.provider, model.model_id.clone())
    });
    dst.add(&info).map_err(|e| e.to_string())?;
    if let Some(p) = src.as_ref().and_then(|l| l.policy(model).ok().flatten()) {
        dst.set_policy(model, &p).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn find_checkout(exe: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    [exe.parent().map(Path::to_path_buf), Some(cwd.clone())]
        .into_iter()
        .flatten()
        .flat_map(|d| d.ancestors().map(Path::to_path_buf).collect::<Vec<_>>())
        .find(|d| d.join("eval").join("coding").join("run.py").is_file())
        .unwrap_or(cwd)
}

fn safe_name(model: &str) -> String {
    model.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

/// Stands in for the provider when only child-process suites run.
struct Idle;

impl Provider for Idle {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, marlowe_model::ProviderError> {
        Ok(Vec::new())
    }

    fn stream(
        &self,
        _req: &marlowe_model::ChatRequest,
        _cancel: &CancelToken,
        _on: &mut dyn FnMut(marlowe_model::StreamEvent),
    ) -> Result<marlowe_model::Completion, marlowe_model::ProviderError> {
        Err(marlowe_model::ProviderError::bad_request("no provider in this bench run"))
    }
}

fn bench_one(home: &Path, raw: &str, o: &Opts, cancel: &CancelToken) -> Result<Card, String> {
    let (model, local) = resolve(home, raw);
    check_allowed(home, &model)?;
    let live = o.suites.iter().any(|s| matches!(s, Suite::Tools | Suite::Edits));
    let provider = if live { Some(provider_for(home, &model)?) } else { None };
    let scratch = std::env::temp_dir().join(format!("marlowe-bench-{}-{}", std::process::id(), safe_name(&model.to_string())));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
    prepare_home(home, &scratch, &model)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let checkout = find_checkout(&exe);
    let stats = crate::wire::repair_stats();
    let idle = Idle;
    let ctx = Ctx {
        provider: provider.as_deref().unwrap_or(&idle),
        stats: Some(stats.as_ref()),
        spawn: &RealSpawn,
        exe: &exe,
        scratch: &scratch,
        checkout: &checkout,
        session_specs: session_specs(local),
        edit_specs: edit_specs(),
    };
    let card = measure(&ctx, &model, local, o, cancel, &mut |m| eprintln!("bench: {m}"));
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(card)
}

/// Writes the card as JSON under `<home>\bench\` (or where `--json` says); returns the path.
fn save(home: &Path, card: &Card, to: Option<&Path>) -> Result<PathBuf, String> {
    let path = match to {
        Some(p) => p.to_path_buf(),
        None => home.join("bench").join(format!("{}-{}.json", safe_name(&card.model), card.started_unix)),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(card).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// `--apply`: records the recommended formats for `model` in the library.
fn apply(home: &Path, model: &str, card: &Card) -> Result<String, String> {
    let Some(e) = &card.edits else { return Err("no edits suite ran".into()) };
    if e.aborted {
        return Err("the edits suite stopped early, so nothing was recorded".into());
    }
    if e.recommended.is_empty() {
        return Err("no format reached 90% within two retries, so nothing was recorded".into());
    }
    let (m, _) = resolve(home, model);
    let lib = open_library(home).ok_or("there is no model library yet (marlowe models add)")?;
    lib.set_edit_formats(&m, &e.recommended).map_err(|e| format!("{e} (add the model to the library first: marlowe models add)"))?;
    Ok(format!("recorded edit formats for {m}: {}", e.recommended.join(" > ")))
}

pub fn run(home: &Path, args: &[String]) -> Result<i32, String> {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(if args.is_empty() { 1 } else { 0 });
    }
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("marlowe bench: {e}\n\n{USAGE}");
            return Ok(1);
        }
    };
    let cancel = CancelToken::new();
    let card = bench_one(home, &o.model, &o, &cancel)?;
    print!("{}", card.render());
    let saved = save(home, &card, o.json.as_deref())?;
    eprintln!("bench: report written to {}", saved.display());
    if let Some(vs) = &o.vs {
        let other = bench_one(home, vs, &o, &cancel)?;
        print!("\n{}", other.render());
        save(home, &other, None)?;
        print!("\n{}", render_vs(&card, &other));
    }
    if o.apply {
        match apply(home, &o.model, &card) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("marlowe bench: --apply: {e}");
                return Ok(1);
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::e2e::Out;
    use crate::bench::fake::{first_user_text, last_user_text, Fake, Reply};
    use serde_json::json;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-bench-mod-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn flags_parse_and_a_model_is_required() {
        let o = parse(&args("ollama/m --suite tools,edits --n 20 --seed 3 --apply --json out.json --timeout 60")).unwrap();
        assert_eq!(o.model, "ollama/m");
        assert_eq!(o.suites, BTreeSet::from([Suite::Tools, Suite::Edits]));
        assert_eq!((o.n, o.seed, o.apply, o.timeout), (Some(20), 3, true, Duration::from_secs(60)));
        assert_eq!(parse(&args("m")).unwrap().suites.len(), 5, "all suites by default");
        assert!(parse(&args("--suite tools")).unwrap_err().contains("model"));
        assert!(parse(&args("m --suite nonsense")).is_err());
        assert!(parse(&args("m --wat")).is_err());
        assert!(parse(&args("m --n x")).is_err());
        assert!(parse(&args("m --suite tools --apply")).unwrap_err().contains("edits"));
        assert!(parse(&args("a b")).is_err());
    }

    #[test]
    fn valiance_is_refused_until_the_marker_exists() {
        let home = scratch("valiance");
        let v = ModelRef::new(ProviderKind::Ollama, "marlowe-valiance:22b");
        let d = ModelRef::new(ProviderKind::Ollama, "marlowe-dusk:27b-super");
        assert!(check_allowed(&home, &v).unwrap_err().contains("blocked"));
        assert!(check_allowed(&home, &d).is_ok());
        std::fs::create_dir_all(home.join("bench")).unwrap();
        std::fs::write(home.join("bench").join("allow-valiance"), "").unwrap();
        assert!(check_allowed(&home, &v).is_ok());
    }

    #[test]
    fn a_bare_id_is_found_in_the_library_or_taken_as_an_ollama_tag() {
        let home = scratch("resolve");
        let (m, local) = resolve(&home, "marlowe-dusk:27b-super");
        assert_eq!(m.to_string(), "ollama/marlowe-dusk:27b-super");
        assert!(local);
        let lib = Library::open(&marlowe_proto::paths::cowork_db(&home)).unwrap();
        lib.add(&ModelInfo::bare(ProviderKind::OpenRouter, "vendor/big")).unwrap();
        drop(lib);
        let (m, local) = resolve(&home, "vendor/big");
        assert_eq!(m.provider, ProviderKind::OpenRouter);
        assert!(!local, "an API model is frontier-class");
        let (m, _) = resolve(&home, "openrouter/vendor/big");
        assert_eq!(m.model_id, "vendor/big");
    }

    /// A model that does everything right.
    fn oracle() -> Fake {
        let all_edit = edits::tasks(10_000, 1);
        let tools: Vec<String> = ["read", "bash", "grep", "edit", "glob", "write"].iter().map(|s| s.to_string()).collect();
        Fake::new(move |req| {
            let first = first_user_text(req);
            let found = [Format::Edit, Format::EditLines, Format::Write]
                .iter()
                .find_map(|f| all_edit.iter().find(|t| edits::prompt(t, *f) == first).map(|t| (t, *f)));
            if let Some((t, f)) = found {
                let name = match f {
                    Format::Edit => "edit",
                    Format::EditLines => "edit_lines",
                    Format::Write => "write",
                };
                return Reply::call(name, t.oracle[&f].clone());
            }
            let text = last_user_text(req);
            match prompts::tool_prompts(&tools, 200).into_iter().find(|p| p.text == text) {
                Some(p) => {
                    let args = match p.tool {
                        "read" => json!({"path": p.expect[0]}),
                        "bash" => json!({"command": p.expect[0]}),
                        "grep" | "glob" => json!({"pattern": p.expect[0]}),
                        "write" => json!({"path": p.expect[0], "content": p.expect[1]}),
                        _ => json!({"path": p.expect[0], "old_string": p.expect[1], "new_string": p.expect[2]}),
                    };
                    Reply::call(p.tool, args)
                }
                None => Reply::text("?"),
            }
        })
    }

    struct Children;

    impl Spawn for Children {
        fn run(&self, argv: &[String], env: &[(String, String)], _cwd: &Path, _t: Duration) -> Result<Out, String> {
            if argv.get(1).map(String::as_str) == Some("eval") {
                let path = argv.iter().position(|a| a == "--summary-json").map(|i| argv[i + 1].clone()).unwrap();
                std::fs::write(path, r#"{"runs": 4, "passed": 3}"#).unwrap();
                return Ok(Out::default());
            }
            let home = PathBuf::from(&env.iter().find(|(k, _)| k == "MARLOWE_HOME").unwrap().1);
            let prompt = argv.last().unwrap();
            let store = home.join("told");
            let text = if let Some(f) = prompt.strip_prefix("Please remember this about me: ") {
                let mut t = std::fs::read_to_string(&store).unwrap_or_default();
                t.push_str(f);
                std::fs::write(&store, t).unwrap();
                "ok".to_string()
            } else if prompt.contains("notes/") {
                "3e-4 [notes/optimizers.md:3] \"The Lantern-350M run used a peak learning rate of 3e-4.\"".into()
            } else {
                std::fs::read_to_string(&store).unwrap_or_default()
            };
            Ok(Out { code: 0, stdout: json!({"type": "result", "is_error": false, "result": text}).to_string(), ..Out::default() })
        }
    }

    fn specs() -> Vec<ToolSpec> {
        ["read", "bash", "grep", "edit", "glob", "write", "edit_lines"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: json!({"type": "object", "additionalProperties": true}) })
            .collect()
    }

    #[test]
    fn a_full_card_runs_every_suite_scores_the_gates_and_renders() {
        let dir = scratch("card");
        let provider = oracle();
        let ctx = Ctx {
            provider: &provider,
            stats: None,
            spawn: &Children,
            exe: Path::new("marlowe.exe"),
            scratch: &dir,
            checkout: &dir,
            session_specs: specs(),
            edit_specs: specs(),
        };
        let o = parse(&args("ollama/oracle --n 12")).unwrap();
        let model = ModelRef::new(ProviderKind::Ollama, "oracle");
        let mut said = Vec::new();
        let card = measure(&ctx, &model, true, &o, &CancelToken::new(), &mut |m| said.push(m.to_string()));
        let t = card.tools.as_ref().unwrap();
        assert_eq!((t.prompts, t.args_ok), (12, 12));
        let e = card.edits.as_ref().unwrap();
        assert_eq!(e.formats.len(), 3);
        assert!(e.meets_a5());
        assert_eq!(card.memory.as_ref().unwrap().recalled, 6);
        assert_eq!(card.research.as_ref().unwrap().citations_verified, 5);
        assert_eq!(card.coding.as_ref().unwrap().pass_rate, 0.75);
        assert_eq!(card.gates.iter().map(|g| g.id).collect::<Vec<_>>(), ["A4", "A5"]);
        assert!(card.gates.iter().all(|g| g.pass), "{:?}", card.gates);
        let text = card.render();
        for want in ["Marlowe bench: ollama/oracle (local)", "tools", "edits", "memory", "research", "coding", "A4", "A5", "PASS"] {
            assert!(text.contains(want), "{want} missing from:\n{text}");
        }
        assert!(!said.is_empty());
        // The report survives a round trip through JSON, and a second card renders alongside.
        let path = save(&dir, &card, None).unwrap();
        let back: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(back["tools"]["prompts"], 12);
        assert!(render_vs(&card, &card).contains("+0.0"));
    }

    #[test]
    fn only_the_chosen_suites_run_and_a_failing_gate_says_so() {
        let dir = scratch("some");
        let blind = Fake::new(|_| Reply::text("I do not know."));
        let ctx = Ctx { provider: &blind, stats: None, spawn: &Children, exe: Path::new("marlowe.exe"), scratch: &dir, checkout: &dir, session_specs: specs(), edit_specs: specs() };
        let o = parse(&args("ollama/blind --suite tools --n 10")).unwrap();
        let card = measure(&ctx, &ModelRef::new(ProviderKind::Ollama, "blind"), true, &o, &CancelToken::new(), &mut |_| {});
        assert!(card.edits.is_none() && card.memory.is_none() && card.coding.is_none());
        assert_eq!(card.gates.len(), 1);
        assert!(!card.gates[0].pass);
        assert!(card.render().contains("FAIL"));
    }

    #[test]
    fn apply_records_the_recommendation_and_refuses_what_it_cannot_trust() {
        let home = scratch("apply");
        let lib = Library::open(&marlowe_proto::paths::cowork_db(&home)).unwrap();
        let model = lib.add(&ModelInfo::bare(ProviderKind::Ollama, "dusk")).unwrap();
        drop(lib);
        let row = |ok| edits::FormatRow { cases: 40, first_applied: 39, succeeded: ok, first_rate: 0.975, success_rate: ok as f64 / 40.0, ..Default::default() };
        let mut card = Card::default();
        let mut e = EditsReport::default();
        e.formats.insert("edit".into(), row(40));
        e.formats.insert("edit_lines".into(), row(30));
        e.recommended = edits::recommend(&e.formats);
        card.edits = Some(e);
        let msg = apply(&home, "ollama/dusk", &card).unwrap();
        assert!(msg.contains("edit"), "{msg}");
        let lib = Library::open(&marlowe_proto::paths::cowork_db(&home)).unwrap();
        assert_eq!(lib.edit_formats(&model).unwrap(), Some(vec!["edit".to_string()]));
        // An aborted run, an empty recommendation and an unknown model change nothing.
        card.edits.as_mut().unwrap().aborted = true;
        assert!(apply(&home, "ollama/dusk", &card).unwrap_err().contains("early"));
        card.edits.as_mut().unwrap().aborted = false;
        card.edits.as_mut().unwrap().recommended.clear();
        assert!(apply(&home, "ollama/dusk", &card).unwrap_err().contains("90%"));
        card.edits.as_mut().unwrap().recommended = vec!["edit".into()];
        assert!(apply(&home, "ollama/ghost", &card).is_err());
    }

    #[test]
    fn the_scratch_home_knows_the_model_and_leaves_the_real_one_alone() {
        let (real, scr) = (scratch("real"), scratch("scr"));
        let model = ModelRef::new(ProviderKind::Ollama, "dusk");
        prepare_home(&real, &scr, &model).unwrap();
        let lib = Library::open(&marlowe_proto::paths::cowork_db(&scr.join("home"))).unwrap();
        let m = lib.get(&model).unwrap().expect("the model is in the scratch library");
        assert!(m.info.supports_tools && m.info.local);
        assert!(!marlowe_proto::paths::cowork_db(&real).exists(), "the real home gets no database from a bench");
    }
}
