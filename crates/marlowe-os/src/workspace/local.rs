//! [`LocalFs`]: the [`Workspace`] port on this machine.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{ExecHandle, ExecOutput, ExecRequest, Meta, WalkEntry, WalkOpts, Workspace};
use crate::child::{self, ChildProc, RunLimits};
use crate::OsError;

/// Makes each write's temp name unique within the process: two hosts (the conductor's and a
/// crew run's) writing the same path at once must not share a temp file.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp name beside `path`, unique per call in this process.
fn temp_path(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.marlowe-{}-{seq}.tmp", std::process::id()))
}

fn io_err(e: OsError) -> std::io::Error {
    match e {
        OsError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// This machine's filesystem, and children through [`crate::child`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFs;

impl Workspace for LocalFs {
    fn kind(&self) -> String {
        "local".into()
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = temp_path(path);
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn walk(&self, dir: &Path, opts: &WalkOpts) -> std::io::Result<Vec<WalkEntry>> {
        let mut out = Vec::new();
        let mut stack = vec![(dir.to_path_buf(), 0usize)];
        let cap = opts.max_entries.unwrap_or(usize::MAX);
        while let Some((d, depth)) = stack.pop() {
            let mut entries: Vec<_> = match std::fs::read_dir(&d) {
                Ok(rd) => rd.filter_map(Result::ok).collect(),
                Err(e) if depth == 0 => return Err(e),
                Err(_) => continue,
            };
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                if out.len() >= cap {
                    return Ok(out);
                }
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                let path = e.path();
                let child_depth = depth + 1;
                if is_dir {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if opts.skip_dirs.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                        continue;
                    }
                    if opts.max_depth.map_or(true, |m| child_depth < m) {
                        stack.push((path.clone(), child_depth));
                    }
                }
                out.push(WalkEntry { path, is_dir, depth: child_depth });
            }
        }
        Ok(out)
    }

    fn metadata(&self, path: &Path) -> std::io::Result<Meta> {
        let m = std::fs::metadata(path)?;
        Ok(Meta {
            is_dir: m.is_dir(),
            is_file: m.is_file(),
            len: m.len(),
            modified: m.modified().ok(),
            readonly: m.permissions().readonly(),
        })
    }

    fn exec(&self, req: &ExecRequest, cancelled: &dyn Fn() -> bool) -> std::io::Result<ExecOutput> {
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args);
        let limits = RunLimits { timeout: req.timeout, max_output_bytes: req.max_output_bytes };
        let out = child::run(cmd, &req.spec, limits, cancelled).map_err(io_err)?;
        Ok(ExecOutput {
            code: out.code(),
            stdout: out.stdout,
            stderr: out.stderr,
            timed_out: out.timed_out,
            flooded: out.flooded,
            cancelled: out.cancelled,
            leftover: out.leftover,
        })
    }

    fn spawn(&self, req: &ExecRequest) -> std::io::Result<Box<dyn ExecHandle>> {
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut proc = child::spawn(&mut cmd, &req.spec).map_err(io_err)?;
        let output = Arc::new(Mutex::new(Ring { start: 0, buf: Vec::new(), cap: req.max_output_bytes.max(1) }));
        if let Some(p) = proc.child_mut().stdout.take() {
            pump(p, &output);
        }
        if let Some(p) = proc.child_mut().stderr.take() {
            pump(p, &output);
        }
        Ok(Box::new(LocalJob { proc, output, exit: None }))
    }
}

/// The last `cap` bytes of a background process's output; `start` is the offset of `buf[0]`.
struct Ring {
    start: u64,
    buf: Vec<u8>,
    cap: usize,
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let drop = self.buf.len() - self.cap;
            self.buf.drain(..drop);
            self.start += drop as u64;
        }
    }
}

fn pump(mut pipe: impl Read + Send + 'static, out: &Arc<Mutex<Ring>>) {
    let out = Arc::clone(out);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            out.lock().unwrap_or_else(|p| p.into_inner()).push(&chunk[..n]);
        }
    });
}

/// A background process on this machine, in its own kill-on-close job.
struct LocalJob {
    proc: ChildProc,
    output: Arc<Mutex<Ring>>,
    exit: Option<i32>,
}

impl ExecHandle for LocalJob {
    fn pid(&self) -> Option<u32> {
        Some(self.proc.id())
    }

    fn read_since(&self, since: u64) -> (Vec<u8>, u64) {
        let ring = self.output.lock().unwrap_or_else(|p| p.into_inner());
        let from = since.max(ring.start);
        let skip = usize::try_from(from - ring.start).unwrap_or(usize::MAX).min(ring.buf.len());
        (ring.buf[skip..].to_vec(), ring.start + ring.buf.len() as u64)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        if self.exit.is_none() {
            self.exit = self.proc.child_mut().try_wait()?.map(|s| s.code().unwrap_or(-1));
        }
        Ok(self.exit)
    }

    fn kill_tree(&mut self) -> std::io::Result<()> {
        if self.exit.is_none() {
            self.proc.kill_tree();
            let status = self.proc.child_mut().wait()?;
            self.exit = Some(status.code().unwrap_or(-1));
        }
        Ok(())
    }
}

impl Drop for LocalJob {
    fn drop(&mut self) {
        let _ = self.kill_tree();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-os-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_read_rename_walk_and_metadata_round_trip() {
        let d = temp("rt");
        let fs = LocalFs;
        let f = d.join("a").join("b.txt");
        fs.write(&f, b"hello").unwrap();
        assert_eq!(fs.read(&f).unwrap(), b"hello");
        assert_eq!(fs.metadata(&f).unwrap().len, 5);
        let g = d.join("a").join("c.txt");
        fs.rename(&f, &g).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("target").join("x"), b"").unwrap();
        let opts = WalkOpts { skip_dirs: vec!["target".into()], ..WalkOpts::default() };
        let names: Vec<String> =
            fs.walk(&d, &opts).unwrap().iter().map(|e| e.path.strip_prefix(&d).unwrap().display().to_string()).collect();
        assert!(names.iter().any(|n| n.ends_with("c.txt")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("target")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_write_gets_its_own_temp_file() {
        let p = Path::new("C:/ws/same.txt");
        let (a, b) = (temp_path(p), temp_path(p));
        assert_ne!(a, b, "two hosts writing one path at once must not share a temp file");
        assert_eq!(a.parent(), p.parent());
        assert!(a.file_name().unwrap().to_string_lossy().starts_with(".same.txt.marlowe-"));
    }

    #[cfg(windows)]
    #[test]
    fn spawn_keeps_output_and_kills_the_tree() {
        let req = ExecRequest::new("cmd.exe", ["/d", "/c", "echo started & ping -n 30 127.0.0.1 >nul"]);
        let mut job = LocalFs.spawn(&req).unwrap();
        assert!(job.pid().is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut at = 0;
        while !String::from_utf8_lossy(&seen).contains("started") && std::time::Instant::now() < deadline {
            let (bytes, next) = job.read_since(at);
            seen.extend(bytes);
            at = next;
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(String::from_utf8_lossy(&seen).contains("started"));
        assert_eq!(job.try_wait().unwrap(), None, "still running");
        job.kill_tree().unwrap();
        assert!(job.try_wait().unwrap().is_some());
    }

    #[test]
    fn the_ring_keeps_the_last_bytes_and_offsets_stay_absolute() {
        let mut r = Ring { start: 0, buf: Vec::new(), cap: 4 };
        r.push(b"abcdef");
        assert_eq!((r.start, r.buf.as_slice()), (2, &b"cdef"[..]));
        r.push(b"gh");
        assert_eq!((r.start, r.buf.as_slice()), (4, &b"efgh"[..]));
    }

    #[cfg(windows)]
    #[test]
    fn exec_runs_through_the_child_spawner() {
        let req = ExecRequest::new("cmd.exe", ["/d", "/c", "echo hi"]);
        let out = LocalFs.exec(&req, &|| false).unwrap();
        assert_eq!(out.code, 0);
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    }
}
