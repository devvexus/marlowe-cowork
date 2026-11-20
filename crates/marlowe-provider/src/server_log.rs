//! The engine's `llama-server` output: the log every server Marlowe starts appends to (kept in the
//! Marlowe home, size-capped), its tail for the planner, and a feed of new lines for the live meter.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use marlowe_hw::LogFeed;

/// The most the engine's log may grow to before it is rotated at a server's start.
pub const OWN_LOG_CAP: u64 = 8 << 20;
/// The most one poll reads; a burst beyond it is read by the next.
const POLL_BYTES: u64 = 4 << 20;

/// Opens the log a server Marlowe starts writes to (appending), after making room: a log over
/// `cap` is moved to `<name>.1` (replacing the older one).
pub fn open_own_log(path: &Path, cap: u64) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > cap) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, &old);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// New whole lines of one file. A file that shrinks (rotated, emptied) is read again from its start.
pub struct FileTail {
    path: PathBuf,
    offset: u64,
    /// The unfinished last line, until its newline arrives.
    partial: Vec<u8>,
    /// Marlowe's own log: emptied once everything in it is read and it is over the cap.
    trim_over: Option<u64>,
}

impl FileTail {
    /// Starts at the end of the file as it is now; a file not there yet is read from its start.
    pub fn at_end(path: &Path) -> Self {
        let offset = std::fs::metadata(path).map_or(0, |m| m.len());
        Self { path: path.to_path_buf(), offset, partial: Vec::new(), trim_over: None }
    }

    /// Empties the file when everything in it has been read and it is larger than `cap`.
    pub fn trimmed_over(mut self, cap: u64) -> Self {
        self.trim_over = Some(cap);
        self
    }

    pub fn poll(&mut self) -> Vec<String> {
        let Ok(mut f) = File::open(&self.path) else { return Vec::new() };
        let Ok(len) = f.metadata().map(|m| m.len()) else { return Vec::new() };
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if len > self.offset {
            let take = (len - self.offset).min(POLL_BYTES);
            let mut buf = Vec::with_capacity(take as usize);
            if f.seek(SeekFrom::Start(self.offset)).is_err() || f.take(take).read_to_end(&mut buf).is_err() {
                return Vec::new();
            }
            self.offset += buf.len() as u64;
            self.partial.extend_from_slice(&buf);
        } else if self.trim_over.is_some_and(|cap| len > cap) && self.partial.is_empty() {
            // The writer appends, so emptying it from here never leaves a gap.
            if OpenOptions::new().write(true).open(&self.path).and_then(|w| w.set_len(0)).is_ok() {
                self.offset = 0;
            }
        }
        let mut lines = Vec::new();
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string());
        }
        lines
    }
}

/// The last `max` bytes of a file, from the first whole line.
pub fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 { text.split_once('\n').map(|(_, t)| t.to_string()).unwrap_or(text) } else { text })
}

/// The engine's output from the moment this was made. Never emptied: the planner reads the loads
/// in it, and a server's start rotates it.
pub struct EngineLogFeed {
    tail: FileTail,
}

impl EngineLogFeed {
    pub fn new(path: &Path) -> Self {
        Self { tail: FileTail::at_end(path) }
    }
}

impl LogFeed for EngineLogFeed {
    fn poll(&mut self) -> Vec<String> {
        self.tail.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marlowe-ollama-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append(p: &Path, s: &str) {
        OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn a_tail_reads_only_what_is_written_after_it_started_and_waits_for_whole_lines() {
        let dir = scratch("tail");
        let p = dir.join("server.log");
        append(&p, "old line\n");
        let mut t = FileTail::at_end(&p);
        assert!(t.poll().is_empty());
        append(&p, "slot a | task 0 | new\r\nhalf");
        assert_eq!(t.poll(), vec!["slot a | task 0 | new".to_string()]);
        append(&p, " done\n");
        assert_eq!(t.poll(), vec!["half done".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_shrinks_is_read_again_and_one_that_is_missing_is_quiet() {
        let dir = scratch("shrink");
        let p = dir.join("server.log");
        let mut t = FileTail::at_end(&p);
        assert!(t.poll().is_empty(), "not there yet");
        append(&p, "first line of a new file\n");
        assert_eq!(t.poll().len(), 1, "a file that appears is read from its start");
        std::fs::write(&p, "x\n").unwrap();
        assert_eq!(t.poll(), vec!["x".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marlowes_own_log_is_rotated_at_start_and_emptied_once_read_and_over_the_cap() {
        let dir = scratch("cap");
        let p = dir.join("sub").join("ollama-serve.log");
        // Start: a log over the cap is moved aside, the new one starts empty.
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x".repeat(100)).unwrap();
        drop(open_own_log(&p, 50).unwrap());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(p.with_file_name("ollama-serve.log.1")).unwrap().len(), 100);
        // Running: the feed reads what is written, then empties the file over the cap.
        let mut w = open_own_log(&p, 50).unwrap();
        let mut t = FileTail::at_end(&p).trimmed_over(10);
        w.write_all(b"slot b | task 1 | line one\nsecond\n").unwrap();
        assert_eq!(t.poll().len(), 2);
        assert!(t.poll().is_empty());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0, "emptied");
        w.write_all(b"after\n").unwrap();
        assert_eq!(t.poll(), vec!["after".to_string()], "the append handle writes at the new end, no gap");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_feed_reads_the_engines_new_lines_and_the_tail_starts_on_a_whole_line() {
        let dir = scratch("feed");
        let log = dir.join("llama-server.log");
        append(&log, "before\n");
        let mut f = EngineLogFeed::new(&log);
        append(&log, "slot a | task 0 | new\n");
        assert_eq!(f.poll(), vec!["slot a | task 0 | new".to_string()]);
        assert_eq!(read_tail(&log, 12).unwrap(), "", "the cut line is dropped");
        assert_eq!(read_tail(&log, 1 << 20).unwrap(), "before\nslot a | task 0 | new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
