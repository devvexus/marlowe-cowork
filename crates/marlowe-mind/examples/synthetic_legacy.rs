//! Writes an old-harness-format profile for import and latency tests, using the old write path
//! (`marlowe_memory::ingest`, terminal user turns, so every belief is user-asserted).
//!
//!   cargo run --release -p marlowe-mind --example synthetic_legacy -- <out_dir> <turns.jsonl>
//!
//! `turns.jsonl`: one `{"session": "...", "text": "..."}` per line; consecutive lines of one
//! session become one ingest. `out_dir` must be absent or empty.

use std::io::BufRead;
use std::path::PathBuf;

use marlowe_contract::{Channel, Clock, IngestRequest, Origin, Speaker, Turn};
use marlowe_journal::{Journal, Profile};
use marlowe_memory::BeliefStore;

/// 2026-08-01T00:00:00Z; turns are a minute apart from here.
const T0: i64 = 1_785_542_400_000;

#[derive(serde::Deserialize)]
struct Line {
    session: String,
    text: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, turns] = args.as_slice() else {
        return Err("usage: synthetic_legacy <out_dir> <turns.jsonl>".into());
    };
    let profile = Profile::init(&PathBuf::from(out))?;
    let mut journal = Journal::open(&profile)?;
    let mut beliefs = BeliefStore::default();

    let mut sessions: Vec<(String, Vec<String>)> = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(turns)?).lines() {
        let l: Line = serde_json::from_str(&line?)?;
        match sessions.last_mut() {
            Some((s, texts)) if *s == l.session => texts.push(l.text),
            _ => sessions.push((l.session, vec![l.text])),
        }
    }
    let (mut n, mut written) = (0i64, 0usize);
    for (session, texts) in sessions {
        let turns = texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                n += 1;
                Turn {
                    turn_id: format!("t{i}"),
                    speaker: Speaker::User,
                    text,
                    occurred_at_ms: T0 + n * 60_000,
                    origin: Origin { channel: Channel::Terminal, actor: "user:primary".into(), r#ref: None },
                }
            })
            .collect();
        let request = IngestRequest {
            contract_version: "1.0".into(),
            clock: Clock::new(T0 + n * 60_000 + 1),
            session_id: session,
            turns,
        };
        let outcome = marlowe_memory::ingest(&mut journal, &mut beliefs, &request)?;
        written += outcome.written.iter().map(|w| w.memory_ids.len()).sum::<usize>();
    }
    println!("{written} memories, journal seq {}", journal.last_seq());
    Ok(())
}
