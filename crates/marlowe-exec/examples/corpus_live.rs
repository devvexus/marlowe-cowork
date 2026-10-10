//! A live corpus pass over a few public pages (robots.txt and host spacing apply).
//!
//! `cargo run --release -p marlowe-exec --example corpus_live [-- url ...]`

use std::time::Instant;

use marlowe_exec::corpus::{Corpus, Outcome};
use marlowe_model::CancelToken;

const DEFAULT: &[&str] = &[
    "https://en.wikipedia.org/wiki/Transformer_(deep_learning_architecture)",
    "https://doc.rust-lang.org/std/sync/struct.Mutex.html",
    "https://blog.rust-lang.org/",
    "https://datatracker.ietf.org/doc/html/rfc9309",
    "https://arxiv.org/abs/2106.09685",
    "https://arxiv.org/pdf/2106.09685",
    "https://stackoverflow.com/questions/26946646/rust-package-with-both-a-library-and-a-binary",
    "https://en.wikipedia.org/wiki/Robots.txt",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let urls: Vec<String> = if args.is_empty() { DEFAULT.iter().map(|s| s.to_string()).collect() } else { args };
    let corpus = Corpus::product();
    println!("edge renderer: {}", if corpus.config.renderer.is_some() { "available" } else { "absent" });
    let t = Instant::now();
    let out = corpus.fetch(&urls, 8, &CancelToken::new());
    println!("{} urls in {:.1} s\n", urls.len(), t.elapsed().as_secs_f64());
    for o in &out {
        match o {
            Outcome::Read(p) => {
                let r = p.reading.as_ref();
                println!(
                    "READ {} -> {} | {} | {} | md {} chars, {} links{}",
                    p.url,
                    p.status,
                    p.document.format.as_str(),
                    r.map_or("-".into(), |r| format!(
                        "{} ({:?}, {}, conf {:?}, cov {:.2})",
                        r.candidate.as_str(),
                        r.reason,
                        r.page_type.as_str(),
                        r.confidence,
                        r.coverage
                    )),
                    p.markdown.chars().count(),
                    p.links().len(),
                    if p.rendered { " | rendered" } else { "" },
                );
                let head: String = p.markdown.chars().take(160).collect();
                println!("     {}", head.replace('\n', " / "));
            }
            other => println!("MISS {} | {}", other.url(), other.why_not().unwrap_or_default()),
        }
    }
}
