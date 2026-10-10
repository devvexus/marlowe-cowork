//! A minimal ustar/GNU tar reader for arXiv e-prints: regular files only, bounded.

/// Files kept from one archive, and total bytes, before the rest is ignored.
const MAX_FILES: usize = 2_000;
const MAX_TOTAL: usize = crate::MAX_INPUT_BYTES;

/// Whether `bytes` looks like a tar archive (ustar magic, or a plausible first header).
pub fn is_tar(bytes: &[u8]) -> bool {
    bytes.len() >= 512 && (&bytes[257..262] == b"ustar" || header_checksum_ok(&bytes[..512]))
}

fn header_checksum_ok(h: &[u8]) -> bool {
    let Some(stored) = octal(&h[148..156]) else { return false };
    let sum: u64 = h.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { 32 } else { u64::from(*b) }).sum();
    stored == sum && h[0] != 0
}

fn octal(field: &[u8]) -> Option<u64> {
    let s: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect::<String>().trim().to_string();
    if s.is_empty() {
        return None;
    }
    u64::from_str_radix(&s, 8).ok()
}

fn name(h: &[u8]) -> String {
    let field = |r: std::ops::Range<usize>| -> String {
        let f = &h[r];
        let end = f.iter().position(|b| *b == 0).unwrap_or(f.len());
        String::from_utf8_lossy(&f[..end]).into_owned()
    };
    let base = field(0..100);
    let prefix = if &h[257..262] == b"ustar" { field(345..500) } else { String::new() };
    if prefix.is_empty() {
        base
    } else {
        format!("{prefix}/{base}")
    }
}

/// `(path, contents)` of every regular file, in archive order. Truncated archives yield what was
/// complete; absolute or `..` paths are kept as names only (nothing is written to disk).
pub fn entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut total = 0usize;
    let mut long_name: Option<String> = None;
    while at + 512 <= bytes.len() && out.len() < MAX_FILES {
        let h = &bytes[at..at + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let Some(size) = octal(&h[124..136]).map(|s| s as usize) else { break };
        let body_start = at + 512;
        let body_end = body_start.saturating_add(size);
        if body_end > bytes.len() {
            break;
        }
        let kind = h[156];
        let body = &bytes[body_start..body_end];
        match kind {
            // GNU long name: the next header's name is this body.
            b'L' => long_name = Some(String::from_utf8_lossy(body).trim_end_matches('\0').to_string()),
            b'0' | 0 | b'7' => {
                total += size;
                if total > MAX_TOTAL {
                    break;
                }
                let n = long_name.take().unwrap_or_else(|| name(h));
                out.push((n, body.to_vec()));
            }
            _ => long_name = None,
        }
        at = body_start + size.div_ceil(512) * 512;
    }
    out
}

#[cfg(test)]
pub(crate) fn build(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (n, data) in files {
        let mut h = [0u8; 512];
        h[..n.len()].copy_from_slice(n.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        let size = format!("{:011o}", data.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}
