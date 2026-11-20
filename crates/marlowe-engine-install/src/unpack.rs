//! Unzipping a release into a folder. An entry whose name would land outside the folder, or that
//! is a symbolic link, stops the whole unpack; the total size is capped against a zip bomb.

use std::io::{Read, Write};
use std::path::Path;

use marlowe_engines::SERVER_EXE;

use crate::EngineError;

/// Most bytes one release may unpack to. The biggest real one is about 1.5 GB.
const MAX_UNPACKED: u64 = 8 << 30;

/// Extracts `zip` into `dest`, calling `on(done, total)` in unpacked bytes.
pub fn unzip(zip: &Path, dest: &Path, on: &mut dyn FnMut(u64, u64)) -> Result<(), EngineError> {
    let fail = |m: String| EngineError::Unpack(format!("{}: {m}", zip.display()));
    let file = std::fs::File::open(zip).map_err(|e| fail(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| fail(e.to_string()))?;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let e = archive.by_index_raw(i).map_err(|e| fail(e.to_string()))?;
        total = total.saturating_add(e.size());
    }
    if total > MAX_UNPACKED {
        return Err(fail(format!("it unpacks to {total} bytes, more than the {MAX_UNPACKED} allowed")));
    }
    std::fs::create_dir_all(dest).map_err(|e| fail(e.to_string()))?;
    let mut done = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| fail(e.to_string()))?;
        let name = entry.name().to_string();
        let rel = entry.enclosed_name().ok_or_else(|| fail(format!("{name:?} would unpack outside the folder")))?;
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(fail(format!("{name:?} is a symbolic link")));
        }
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| fail(e.to_string()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
        }
        let mut f = std::fs::File::create(&out).map_err(|e| fail(format!("{}: {e}", out.display())))?;
        loop {
            let n = entry.read(&mut buf).map_err(|e| fail(format!("{name}: {e}")))?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).map_err(|e| fail(format!("{}: {e}", out.display())))?;
            done += n as u64;
            on(done, total);
        }
    }
    Ok(())
}

/// A release zip that wraps everything in one folder is moved up a level, so the server binary
/// is directly in `dir`. Errors when there is no server binary at either depth.
pub fn flatten(dir: &Path) -> Result<(), EngineError> {
    if dir.join(SERVER_EXE).is_file() {
        return Ok(());
    }
    let subs: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).filter(|e| e.path().is_dir()).collect();
    let [only] = subs.as_slice() else {
        return Err(EngineError::Unpack(format!("the archive has no {SERVER_EXE}")));
    };
    let inner = only.path();
    if !inner.join(SERVER_EXE).is_file() {
        return Err(EngineError::Unpack(format!("the archive has no {SERVER_EXE}")));
    }
    for e in std::fs::read_dir(&inner)?.filter_map(Result::ok) {
        std::fs::rename(e.path(), dir.join(e.file_name()))?;
    }
    std::fs::remove_dir(&inner)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// A zip in memory: `(name, bytes)`; a name ending in `/` is a folder.
    pub(crate) fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-unpack-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_zip_unpacks_with_its_folders_and_reports_progress() {
        let d = scratch("ok");
        std::fs::write(d.join("a.zip"), make_zip(&[("llama-server.exe", b"MZ"), ("sub/", b""), ("sub/x.dll", b"dll")])).unwrap();
        let mut last = (0, 0);
        unzip(&d.join("a.zip"), &d.join("out"), &mut |done, total| last = (done, total)).unwrap();
        assert_eq!(std::fs::read(d.join("out/sub/x.dll")).unwrap(), b"dll");
        assert_eq!(last, (5, 5));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_entry_that_climbs_out_of_the_folder_stops_the_unpack() {
        let d = scratch("evil");
        std::fs::write(d.join("a.zip"), make_zip(&[("../escaped.txt", b"x")])).unwrap();
        let e = unzip(&d.join("a.zip"), &d.join("out"), &mut |_, _| {}).unwrap_err().to_string();
        assert!(e.contains("outside the folder"), "{e}");
        assert!(!d.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wrapping_folder_is_flattened_and_a_missing_server_is_refused() {
        let d = scratch("flat");
        std::fs::create_dir_all(d.join("wrapped/bin")).unwrap();
        std::fs::write(d.join("wrapped").join(SERVER_EXE), b"MZ").unwrap();
        std::fs::write(d.join("wrapped/lib.dll"), b"d").unwrap();
        flatten(&d.join("wrapped")).unwrap();
        let inner = d.join("only");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::write(inner.join("deep").join(SERVER_EXE), b"MZ").unwrap();
        flatten(&inner).unwrap();
        assert!(inner.join(SERVER_EXE).is_file() && !inner.join("deep").exists());
        let empty = d.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(flatten(&empty).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
