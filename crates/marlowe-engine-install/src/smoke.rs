//! The smoke test of a fresh install: `llama-server --version` must run, and `--list-devices`
//! should name a GPU. A server that runs but sees no GPU is still installed, listed as CPU with a
//! note: models would load on the CPU at a tenth of the speed.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marlowe_engines::Device;

use crate::EngineError;

/// How a program ended and what it printed (stdout and stderr together).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub text: String,
}

/// Runs a program from a fresh install. The real one is [`SystemRunner`].
pub trait Runner: Send + Sync {
    fn run(&self, exe: &Path, args: &[&str]) -> Result<RunOutput, String>;
}

/// Runs the program hidden, from its own folder with that folder first on `PATH`, without the
/// credential variables, and kills it after the timeout.
pub struct SystemRunner {
    /// Environment variables removed from the child (API keys).
    pub scrub: Vec<String>,
    pub timeout: Duration,
}

impl SystemRunner {
    pub fn new(scrub: Vec<String>) -> Self {
        Self { scrub, timeout: Duration::from_secs(60) }
    }
}

impl Runner for SystemRunner {
    fn run(&self, exe: &Path, args: &[&str]) -> Result<RunOutput, String> {
        let dir = exe.parent().ok_or("the binary has no folder")?;
        let mut cmd = Command::new(exe);
        cmd.args(args).current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for k in &self.scrub {
            cmd.env_remove(k);
        }
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&inherited))).unwrap_or(inherited);
        cmd.env("PATH", path);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
        // Each pipe is read on its own thread so a full pipe never stalls the child.
        let mut readers = Vec::new();
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)].into_iter().flatten() {
            readers.push(std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.take(1 << 20).read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).into_owned()
            }));
        }
        let began = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break s;
            }
            if began.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not finish within {} s", exe.display(), self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let text = readers.into_iter().filter_map(|r| r.join().ok()).collect::<Vec<_>>().join("\n");
        Ok(RunOutput { success: status.success(), text })
    }
}

/// What the smoke test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Smoke {
    /// First line of `--version`.
    pub version: String,
    pub device: Device,
    /// The GPUs `--list-devices` named.
    pub gpus: Vec<String>,
    /// Why it is [`Device::Cpu`].
    pub note: Option<String>,
}

/// The device lines of `--list-devices`, `CUDA0: NVIDIA GeForce ... (16375 MiB, 15061 MiB free)`,
/// that are not the CPU.
pub fn gpu_devices(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (name, rest) = l.split_once(':')?;
            let letters = name.trim_end_matches(|c: char| c.is_ascii_digit());
            let digits = &name[letters.len()..];
            let ok = !letters.is_empty() && !digits.is_empty() && letters.chars().all(|c| c.is_ascii_alphabetic()) && !letters.eq_ignore_ascii_case("cpu");
            ok.then(|| format!("{name}:{rest}"))
        })
        .collect()
}

/// Runs the two checks on `exe`. A server that will not run, or prints no version, fails; one that
/// runs without a GPU passes as CPU.
pub fn smoke(runner: &dyn Runner, exe: &Path) -> Result<Smoke, EngineError> {
    let v = runner.run(exe, &["--version"]).map_err(EngineError::Smoke)?;
    let version = v.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    if !v.success || !version.to_ascii_lowercase().contains("version") {
        return Err(EngineError::Smoke(format!("`llama-server --version` did not report a version{}", if version.is_empty() { String::new() } else { format!(": {version}") })));
    }
    let devices = runner.run(exe, &["--list-devices"]);
    let gpus = devices.as_ref().map(|d| gpu_devices(&d.text)).unwrap_or_default();
    if !gpus.is_empty() {
        return Ok(Smoke { version, device: Device::Gpu, gpus, note: None });
    }
    let why = match devices {
        Ok(d) if !d.success => "`--list-devices` failed".to_string(),
        Err(e) => e,
        Ok(_) => "`--list-devices` listed no GPU".to_string(),
    };
    Ok(Smoke { version, device: Device::Cpu, gpus, note: Some(format!("{why}, so models will load on the CPU, far slower; check the NVIDIA driver")) })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers `--version` and `--list-devices` from fixed text.
    pub(crate) struct Fake {
        pub version: Result<RunOutput, String>,
        pub devices: Result<RunOutput, String>,
        pub asked: Mutex<Vec<String>>,
    }

    pub(crate) fn ok(text: &str) -> Result<RunOutput, String> {
        Ok(RunOutput { success: true, text: text.into() })
    }

    pub(crate) const VERSION: &str = "version: 1.0.0-dev (build 12359, commit b1b770d90)\nbuilt with Clang 20.1.8 for Windows x86_64";
    pub(crate) const GPU: &str = "Available devices:\n  CUDA0: NVIDIA GeForce RTX 4080 SUPER (16375 MiB, 15061 MiB free)";

    impl Fake {
        pub(crate) fn gpu() -> Self {
            Self { version: ok(VERSION), devices: ok(GPU), asked: Mutex::new(Vec::new()) }
        }
    }

    impl Runner for Fake {
        fn run(&self, _exe: &Path, args: &[&str]) -> Result<RunOutput, String> {
            self.asked.lock().unwrap().push(args.join(" "));
            if args == ["--version"] { self.version.clone() } else { self.devices.clone() }
        }
    }

    #[test]
    fn a_listed_cuda_device_is_a_gpu_and_the_cpu_is_not() {
        assert_eq!(gpu_devices(GPU), ["CUDA0: NVIDIA GeForce RTX 4080 SUPER (16375 MiB, 15061 MiB free)"]);
        assert!(gpu_devices("Available devices:\n  CPU: AMD Ryzen (65000 MiB)").is_empty());
        assert!(gpu_devices("Available devices:\n").is_empty());
        assert_eq!(gpu_devices("  Vulkan0: AMD Radeon (8192 MiB)").len(), 1);
    }

    #[test]
    fn a_working_install_with_a_gpu_passes() {
        let s = smoke(&Fake::gpu(), Path::new("x/llama-server.exe")).unwrap();
        assert_eq!(s.device, Device::Gpu);
        assert!(s.version.starts_with("version: 1.0.0-dev"), "{}", s.version);
        assert_eq!(s.note, None);
    }

    #[test]
    fn no_gpu_falls_back_to_cpu_with_a_clear_note_and_a_broken_binary_fails() {
        let cpu = Fake { devices: ok("Available devices:\n"), ..Fake::gpu() };
        let s = smoke(&cpu, Path::new("x")).unwrap();
        assert_eq!(s.device, Device::Cpu);
        assert!(s.note.unwrap().contains("listed no GPU"));
        let dead = Fake { devices: Err("could not start".into()), ..Fake::gpu() };
        assert_eq!(smoke(&dead, Path::new("x")).unwrap().device, Device::Cpu);
        let broken = Fake { version: Ok(RunOutput { success: false, text: "missing cudart64_12.dll".into() }), ..Fake::gpu() };
        let e = smoke(&broken, Path::new("x")).unwrap_err().to_string();
        assert!(e.contains("smoke test") && e.contains("cudart64_12.dll"), "{e}");
        let silent = Fake { version: ok(""), ..Fake::gpu() };
        assert!(smoke(&silent, Path::new("x")).is_err());
    }
}
