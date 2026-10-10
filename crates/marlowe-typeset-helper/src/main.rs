//! `marlowe-typeset.exe`: one formula per JSON line on stdin, one reply per line on stdout, until
//! stdin closes (which it does when `marlowe.exe` exits, however it exits).
//!
//! `marlowe-typeset --png <dir> <markup>…` writes renders as PNG files instead, for looking at.

use std::io::{self, BufWriter};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--png") {
        return png(&args[1..]);
    }
    if !args.is_empty() {
        eprintln!("marlowe-typeset: started by marlowe.exe; it reads JSON lines on stdin");
        return ExitCode::from(2);
    }
    let stdin = io::stdin().lock();
    let stdout = BufWriter::new(io::stdout().lock());
    let mut typeset = marlowe_typeset_helper::typesetter();
    match marlowe_typeset::proto::serve(stdin, stdout, &mut typeset) {
        Ok(()) => ExitCode::SUCCESS,
        // The client went away mid-reply: nothing to report it to.
        Err(_) => ExitCode::from(1),
    }
}

fn png(args: &[String]) -> ExitCode {
    let Some((dir, sources)) = args.split_first() else {
        eprintln!("usage: marlowe-typeset --png <dir> <typst markup>…");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("marlowe-typeset: {dir}: {e}");
        return ExitCode::from(1);
    }
    for (k, m) in sources.iter().enumerate() {
        match marlowe_typeset_helper::typeset_png(m, 32.0, [0xcd, 0xc9, 0xd6]) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(format!("{dir}/{k:03}.png"), bytes) {
                    eprintln!("marlowe-typeset: {e}");
                    return ExitCode::from(1);
                }
            }
            Err(e) => eprintln!("{k}: {e}"),
        }
    }
    ExitCode::SUCCESS
}
