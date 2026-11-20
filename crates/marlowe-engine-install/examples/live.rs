//! Installs the real backends from GitHub into a throwaway folder, for a human to watch:
//! `cargo run -p marlowe-engine-install --example live -- <engines root> <downloads folder>`.
//! Nothing here runs in the tests.

use marlowe_engine_install::discover::{check, Depth, Platform};
use marlowe_engine_install::http::Http;
use marlowe_engine_install::install::{install, Context, Target};
use marlowe_engine_install::smoke::SystemRunner;
use marlowe_engines::{status, Backend};
use marlowe_net::download::Net;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(downloads)) = (args.next(), args.next()) else {
        eprintln!("usage: live <engines root> <downloads folder>");
        std::process::exit(2);
    };
    let (root, downloads) = (std::path::PathBuf::from(root), std::path::PathBuf::from(downloads));
    let platform = Platform::detect().expect("platform");
    println!("platform: {:?} {:?} (driver CUDA {:?})", platform.arch, platform.cuda, marlowe_engine_install::driver::cuda_driver());
    let http = Http::from_env(&Net);
    let runner = SystemRunner::new(Vec::new());
    for backend in Backend::ALL {
        let ctx = Context { root: &root, downloads: &downloads, http: &http, runner: &runner, platform, keep_downloads: true };
        let mut last = String::new();
        let rec = install(&ctx, backend, Target::Release, &mut |p| {
            if p.message != last {
                println!("[{backend}] {} ({}/{})", p.message, p.done, p.total);
                last = p.message;
            }
        });
        match rec {
            Ok(r) => println!("[{backend}] installed {} device={:?} version={:?} note={:?}", r.label(), r.device, r.version, r.note),
            Err(e) => println!("[{backend}] FAILED: {e}"),
        }
        match check(&root, backend, &http, platform, Depth::Build, marlowe_engines::now_secs()) {
            Ok(c) => println!("[{backend}] newest build: {:?} note={:?}", c.remote.newest_build.map(|r| r.tag), c.note),
            Err(e) => println!("[{backend}] check failed: {e}"),
        }
        let s = status(&root, backend);
        println!("[{backend}] status: update={:?} builds_newer={:?} latest={:?}", s.update, s.builds_newer, s.latest_release);
    }
}
