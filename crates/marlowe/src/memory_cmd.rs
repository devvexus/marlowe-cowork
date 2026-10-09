//! `marlowe memory ...`: search, stats, forget, correct, import and a latency bench, all through
//! the daemon (it alone opens the memory profile).

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use marlowe_proto::{Event, ImportReport, MemoryChange, MemoryHit, MemoryScope, MemoryStatsReport, Request};

use crate::client::{Client, ClientError};
use crate::daemon_ctl;

const USAGE: &str = "usage: marlowe memory [search <query> [--limit N] [--project DIR | --global] | stats \
| probe <query> [--workspace DIR] | forget <id> | correct <id> <text> | import [--from DIR] | bench [--n N]]";

const OP_TIMEOUT: Duration = Duration::from_secs(60);

/// An import of a large old profile can take a while.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Queries the bench cycles through.
const BENCH_QUERIES: [&str; 8] = [
    "what did we decide about the experiment",
    "my preferences",
    "how do I run the tests",
    "the deadline",
    "which model did we pick and why",
    "results from the last run",
    "where is the dataset",
    "what was left open",
];

type CmdResult = Result<i32, String>;

pub fn run(home: &Path, args: &[String]) -> CmdResult {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str).unwrap_or("stats") {
        "search" => search(home, rest),
        "stats" => stats(home),
        "probe" => probe_cmd(home, rest),
        "forget" => forget(home, rest),
        "correct" => correct(home, rest),
        "import" => import(home, rest),
        "bench" => bench(home, rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown memory command {other:?}\n{USAGE}")),
    }
}

fn connect(home: &Path) -> Result<Client, String> {
    let info = daemon_ctl::ensure(home)?;
    Client::connect(&info, "marlowe-memory").map_err(|e| e.to_string())
}

fn send(c: &mut Client, req: &Request) -> Result<(), String> {
    c.send(req).map_err(|e| e.to_string())
}

fn wait<T>(c: &mut Client, timeout: Duration, pick: impl FnMut(&Event) -> Option<T>) -> Result<T, String> {
    c.wait_for(timeout, pick).map_err(|e| e.to_string())?
}

fn results(e: &Event) -> Option<Vec<MemoryHit>> {
    match e {
        Event::MemoryResults { hits } => Some(hits.clone()),
        _ => None,
    }
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a String, String> {
    it.next().ok_or(format!("{flag} needs a value"))
}

fn search(home: &Path, args: &[String]) -> CmdResult {
    let (mut words, mut limit, mut scope) = (Vec::new(), 20u32, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--limit" => limit = value(&mut it, "--limit")?.parse().map_err(|_| "--limit needs a number")?,
            "--project" => scope = Some(MemoryScope::Project(value(&mut it, "--project")?.clone())),
            "--global" => scope = Some(MemoryScope::Global),
            _ => words.push(a.as_str()),
        }
    }
    if words.is_empty() {
        return Err(USAGE.into());
    }
    let mut c = connect(home)?;
    send(&mut c, &Request::MemorySearch { query: words.join(" "), scope, limit })?;
    let hits = wait(&mut c, OP_TIMEOUT, results)?;
    if hits.is_empty() {
        println!("no memories match");
    }
    for h in &hits {
        print_hit(h);
    }
    Ok(0)
}

/// What auto-injection would recall for a query, or why it abstains.
fn probe_cmd(home: &Path, args: &[String]) -> CmdResult {
    let (mut words, mut workspace) = (Vec::new(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" => workspace = Some(value(&mut it, "--workspace")?.clone()),
            _ => words.push(a.as_str()),
        }
    }
    if words.is_empty() {
        return Err(USAGE.into());
    }
    let mut c = connect(home)?;
    send(&mut c, &Request::MemoryProbe { query: words.join(" "), workspace })?;
    let (recalled, abstained, ms) = wait(&mut c, OP_TIMEOUT, |e| match e {
        Event::MemoryProbed { recalled, abstained, latency_ms } => {
            Some((recalled.clone(), abstained.clone(), *latency_ms))
        }
        _ => None,
    })?;
    match abstained {
        Some(reason) => println!("abstained: {reason} ({ms} ms)"),
        None => println!("would inject {} ({ms} ms)", recalled.len()),
    }
    for h in &recalled {
        print_hit(h);
    }
    Ok(0)
}

fn print_hit(h: &MemoryHit) {
    let scope = match &h.scope {
        MemoryScope::Global => String::new(),
        MemoryScope::Project(p) => format!(" · {p}"),
    };
    println!(
        "{}  {} · {} · {}{} · {}  [{:.3}]",
        h.id,
        h.kind.label(),
        h.trust.label(),
        date(h.occurred_at_ms),
        scope,
        h.provenance,
        h.score
    );
    let text = if h.text.is_empty() { "(forgotten)".to_string() } else { h.text.split_whitespace().collect::<Vec<_>>().join(" ") };
    // Memory text is data: no control characters (escape sequences) reach the terminal.
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    println!("    {text}");
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

fn stats(home: &Path) -> CmdResult {
    let mut c = connect(home)?;
    print_stats(&fetch_stats(&mut c)?);
    Ok(0)
}

fn fetch_stats(c: &mut Client) -> Result<MemoryStatsReport, String> {
    send(c, &Request::MemoryStats)?;
    wait(c, OP_TIMEOUT, |e| match e {
        Event::MemoryStats(s) => Some(s.clone()),
        _ => None,
    })
}

fn print_stats(s: &MemoryStatsReport) {
    println!("beliefs {} · episodes {} · vectors {} · journal seq {}", s.beliefs, s.episodes, s.vectors, s.journal_seq);
    let models = if s.models_loaded.is_empty() { "none resident".to_string() } else { s.models_loaded.join(", ") };
    println!("models: {models}");
    match &s.degraded {
        Some(reason) => println!("degraded: {reason} (lexical recall only; nothing is auto-injected)"),
        None => println!("retrieval: dense + rerank + gate"),
    }
}

fn forget(home: &Path, args: &[String]) -> CmdResult {
    let id = args.first().ok_or(USAGE)?.clone();
    let mut c = connect(home)?;
    send(&mut c, &Request::MemoryForget { id: id.clone() })?;
    wait(&mut c, OP_TIMEOUT, |e| match e {
        Event::MemoryChanged { id: x, change: MemoryChange::Forgotten } if *x == id => Some(()),
        _ => None,
    })?;
    println!("forgot {id}");
    Ok(0)
}

fn correct(home: &Path, args: &[String]) -> CmdResult {
    let id = args.first().ok_or(USAGE)?.clone();
    let text = args.get(1..).unwrap_or_default().join(" ");
    if text.trim().is_empty() {
        return Err(USAGE.into());
    }
    let mut c = connect(home)?;
    send(&mut c, &Request::MemoryCorrect { id: id.clone(), text })?;
    let by = wait(&mut c, OP_TIMEOUT, |e| match e {
        Event::MemoryChanged { id: x, change: MemoryChange::Corrected { by } } if *x == id => Some(by.clone()),
        _ => None,
    })?;
    println!("corrected {id}; the new memory is {by}");
    Ok(0)
}

fn import(home: &Path, args: &[String]) -> CmdResult {
    let path = match args {
        [] => None,
        [flag, dir] if flag == "--from" => Some(dir.clone()),
        _ => return Err(USAGE.into()),
    };
    let mut c = connect(home)?;
    send(&mut c, &Request::ImportLegacy { path })?;
    let end = Instant::now() + IMPORT_TIMEOUT;
    let report = loop {
        if Instant::now() >= end {
            return Err(ClientError::Timeout.to_string());
        }
        let Some(env) = c.next_event(Duration::from_secs(5)).map_err(|e| e.to_string())? else { continue };
        match env.event {
            Event::ImportProgress { read, imported } => {
                eprint!("\r\x1b[2mread {read} · imported {imported}\x1b[0m");
                let _ = std::io::stderr().flush();
            }
            Event::ImportDone { report } => break report,
            // Only replies (seq 0) are ours; a broadcast error belongs to a session.
            Event::Error { message, .. } if env.seq == 0 => {
                eprintln!();
                return Err(message);
            }
            _ => {}
        }
    };
    eprintln!();
    print_report(&report);
    Ok(if report.errors.is_empty() { 0 } else { 1 })
}

fn print_report(r: &ImportReport) {
    println!(
        "read {} · imported {} · already present {} · forgotten {} · superseded {}",
        r.read, r.imported, r.skipped_duplicates, r.tombstoned, r.superseded
    );
    for e in &r.errors {
        println!("error: {e}");
    }
}

/// Inject probes (the auto-injection path, models warmed first) and searches, `n` of each.
fn bench(home: &Path, args: &[String]) -> CmdResult {
    let n: usize = match args {
        [] => 50,
        [flag, n] if flag == "--n" => n.parse().map_err(|_| "--n needs a number")?,
        _ => return Err(USAGE.into()),
    };
    let n = n.max(1);
    let mut c = connect(home)?;
    let before = fetch_stats(&mut c)?;
    print_stats(&before);

    // Warm-up: the first probe loads the embedder and reranker; it is reported, not counted.
    let t0 = Instant::now();
    let (_, abstained, cold) = probe(&mut c, BENCH_QUERIES[0])?;
    println!(
        "warm-up inject: {cold} ms in the mind, {:.1} ms round trip{}",
        t0.elapsed().as_secs_f64() * 1e3,
        abstained.map(|a| format!(" (abstained: {a})")).unwrap_or_default()
    );

    let (mut mind_ms, mut trip_ms, mut recalled, mut abstentions) = (Vec::new(), Vec::new(), 0usize, 0usize);
    for i in 0..n {
        let t0 = Instant::now();
        let (count, abstained, ms) = probe(&mut c, BENCH_QUERIES[i % BENCH_QUERIES.len()])?;
        trip_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        mind_ms.push(ms as f64);
        recalled += count;
        abstentions += usize::from(abstained.is_some());
    }
    println!(
        "{n} injects · mind {} · round trip {} · {:.1} recalled/inject · {abstentions} abstained",
        percentiles(&mut mind_ms),
        percentiles(&mut trip_ms),
        recalled as f64 / n as f64
    );

    let (mut ms, mut hits) = (Vec::with_capacity(n), 0usize);
    for i in 0..n {
        let query = BENCH_QUERIES[i % BENCH_QUERIES.len()].to_string();
        let t0 = Instant::now();
        send(&mut c, &Request::MemorySearch { query, scope: None, limit: 10 })?;
        hits += wait(&mut c, OP_TIMEOUT, results)?.len();
        ms.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    println!("{n} searches · round trip {} · {:.1} hits/search", percentiles(&mut ms), hits as f64 / n as f64);

    let after = fetch_stats(&mut c)?;
    if after.models_loaded != before.models_loaded {
        println!("models now resident: {}", after.models_loaded.join(", "));
    }
    Ok(0)
}

/// One `MemoryProbe`: (recalled count, abstention, latency inside the mind in ms).
fn probe(c: &mut Client, query: &str) -> Result<(usize, Option<String>, u64), String> {
    send(c, &Request::MemoryProbe { query: query.to_string(), workspace: None })?;
    wait(c, OP_TIMEOUT, |e| match e {
        Event::MemoryProbed { recalled, abstained, latency_ms } => {
            Some((recalled.len(), abstained.clone(), *latency_ms))
        }
        _ => None,
    })
}

/// "p50 X ms · p95 Y ms · max Z ms" (nearest rank).
fn percentiles(ms: &mut [f64]) -> String {
    ms.sort_by(f64::total_cmp);
    let pct = |p: f64| ms[((ms.len() - 1) as f64 * p).round() as usize];
    format!("p50 {:.1} ms · p95 {:.1} ms · max {:.1} ms", pct(0.50), pct(0.95), ms[ms.len() - 1])
}
