//! Ollama's `server.log`: each `starting llama-server cmd=` launch and the load block that
//! follows it. The runner prints every buffer it allocates, per device, so this is the ground
//! truth the planner calibrates against (`/api/ps` sizes are not; see the crate docs).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One runner launch: the argv Ollama passed to `llama-server`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    /// 1-based line of the `starting llama-server` line.
    pub line: usize,
    pub time: Option<String>,
    pub argv: Vec<String>,
    pub model: Option<String>,
    pub draft: Option<String>,
    pub mmproj: Option<String>,
    pub port: Option<u16>,
    pub ctx: Option<u32>,
    pub parallel: Option<u32>,
    pub cache_k: Option<String>,
    pub cache_v: Option<String>,
    pub cache_k_draft: Option<String>,
    pub cache_v_draft: Option<String>,
    pub flash_attn: Option<String>,
    pub spec_n: Option<u32>,
    pub batch: Option<u32>,
    pub ubatch: Option<u32>,
    pub n_gpu_layers: Option<String>,
    pub fit: Option<String>,
    pub kv_unified: bool,
    pub context_shift: bool,
}

/// The KV-cache summary line: `size = 1088.00 MiB ( 32768 cells,  16 layers,  1/1 seqs), K (q8_0): …`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KvLine {
    pub mib: f64,
    pub cells: u64,
    pub layers: u32,
    pub seqs: u32,
    pub k_type: String,
    pub k_mib: f64,
    pub v_type: String,
    pub v_mib: f64,
}

/// The recurrent-state summary line: `size = 598.50 MiB ( 1 cells, 64 layers, 1 seqs 3 rs_seq), R …`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RsLine {
    pub mib: f64,
    pub layers: u32,
    pub seqs: u32,
    pub rs_seq: u32,
    pub r_mib: f64,
    pub s_mib: f64,
}

/// One `llama_context` (the main model, then the draft) and its buffers, MiB per device
/// (`CUDA0`, `CUDA_Host`, `CPU`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextLoad {
    /// `offloaded N/M layers to GPU`.
    pub offloaded: Option<(u32, u32)>,
    pub model: BTreeMap<String, f64>,
    pub kv: BTreeMap<String, f64>,
    pub rs: BTreeMap<String, f64>,
    /// The last reservation per device (the runner reserves again; later ones replace).
    pub compute: BTreeMap<String, f64>,
    pub output: BTreeMap<String, f64>,
    pub kv_lines: Vec<KvLine>,
    pub rs_line: Option<RsLine>,
    pub n_ctx: Option<u32>,
    pub n_ctx_seq: Option<u32>,
    pub n_seq_max: Option<u32>,
    pub n_ubatch: Option<u32>,
    pub flash_attn: Option<String>,
}

impl ContextLoad {
    /// Everything this context put on `device`.
    pub fn on(&self, device: &str) -> f64 {
        [&self.model, &self.kv, &self.rs, &self.compute, &self.output].iter().filter_map(|m| m.get(device)).sum()
    }

    /// Everything this context put on any GPU device (`CUDA*`, `ROCm*`, `Vulkan*`, `Metal`).
    pub fn on_gpu(&self) -> f64 {
        let mut devices: Vec<&String> =
            [&self.model, &self.kv, &self.rs, &self.compute, &self.output].iter().flat_map(|m| m.keys()).collect();
        devices.sort();
        devices.dedup();
        devices.into_iter().filter(|d| is_gpu_device(d)).map(|d| self.on(d)).sum()
    }

    /// Everything in host memory (pinned `CUDA_Host` and plain `CPU`).
    pub fn on_host(&self) -> f64 {
        let mut devices: Vec<&String> =
            [&self.model, &self.kv, &self.rs, &self.compute, &self.output].iter().flat_map(|m| m.keys()).collect();
        devices.sort();
        devices.dedup();
        devices.into_iter().filter(|d| !is_gpu_device(d)).map(|d| self.on(d)).sum()
    }

    pub fn kv_total(&self) -> f64 {
        self.kv_lines.iter().map(|k| k.mib).sum()
    }
}

/// `CUDA0`, `ROCm1`, `Vulkan0`, `Metal`; not `CUDA_Host` or `CPU`.
pub fn is_gpu_device(d: &str) -> bool {
    !d.ends_with("_Host") && !d.eq_ignore_ascii_case("CPU") && !d.starts_with("CPU")
}

/// A launch and what loaded after it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadBlock {
    pub launch: Launch,
    /// The main context first, then the draft's.
    pub contexts: Vec<ContextLoad>,
    /// `system memory total="31.2 GiB" free="12.3 GiB"` at launch, GiB.
    pub system_total_gib: Option<f64>,
    pub system_free_gib: Option<f64>,
    /// The device's free memory as the runner saw it before loading, MiB.
    pub device_free_mib: Option<f64>,
    /// `llama-server started in 6.78 seconds`.
    pub started_secs: Option<f64>,
    /// The fit step kept the user's context and reduced layers instead.
    pub fit_reduced_layers: bool,
    /// `forcing full prompt re-processing` lines seen after the load (hybrid/SWA cache misses).
    pub reprocess_events: u32,
    /// The last decode speed the runner printed (`eval time = … tokens per second`).
    pub decode_tok_s: Option<f64>,
    /// The last prompt speed (`prompt eval time = …`).
    pub prompt_tok_s: Option<f64>,
}

impl LoadBlock {
    pub fn main(&self) -> Option<&ContextLoad> {
        self.contexts.first()
    }

    pub fn draft(&self) -> Option<&ContextLoad> {
        self.contexts.get(1)
    }

    /// GPU bytes over every context, MiB.
    pub fn gpu_mib(&self) -> f64 {
        self.contexts.iter().map(ContextLoad::on_gpu).sum()
    }

    pub fn host_mib(&self) -> f64 {
        self.contexts.iter().map(ContextLoad::on_host).sum()
    }
}

/// Parses every launch and its load block, oldest first.
pub fn parse(text: &str) -> Vec<LoadBlock> {
    let mut blocks: Vec<LoadBlock> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.contains("msg=\"starting llama-server\"") {
            if let Some(cmd) = quoted_field(line, "cmd") {
                let mut launch = parse_argv(&split_argv(&cmd));
                launch.line = i + 1;
                launch.time = quoted_or_bare(line, "time");
                blocks.push(LoadBlock { launch, ..LoadBlock::default() });
            }
            continue;
        }
        let Some(b) = blocks.last_mut() else { continue };
        if line.contains("msg=\"system memory\"") {
            b.system_total_gib = quoted_field(line, "total").and_then(|v| gib(&v));
            b.system_free_gib = quoted_field(line, "free").and_then(|v| gib(&v));
        } else if line.contains("msg=\"gpu memory\"") {
            b.device_free_mib = quoted_field(line, "free").and_then(|v| gib(&v)).map(|g| g * 1024.0);
        } else if let Some(rest) = line.split("llama-server started in ").nth(1) {
            b.started_secs = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if line.contains("context size set by user") && line.contains("no change") {
            b.fit_reduced_layers = true;
        } else if line.contains("forcing full prompt re-processing") {
            b.reprocess_events += 1;
        } else if line.contains(" eval time =") && line.contains("tokens per second") {
            let speed = line.split(',').next_back().and_then(first_num);
            if line.contains("prompt eval time") {
                b.prompt_tok_s = speed.or(b.prompt_tok_s);
            } else {
                b.decode_tok_s = speed.or(b.decode_tok_s);
            }
        } else if let Some(rest) = line.strip_prefix("load_tensors: offloaded ") {
            let (n, m) = rest.split_whitespace().next().and_then(|f| f.split_once('/')).unwrap_or(("", ""));
            b.contexts.push(ContextLoad {
                offloaded: n.parse().ok().zip(m.parse().ok()),
                ..ContextLoad::default()
            });
        } else if let Some(c) = b.contexts.last_mut() {
            context_line(c, line);
        }
    }
    blocks
}

/// The newest block whose launch loaded `blob` (matched on the file name, any separators).
pub fn last_for_blob<'a>(blocks: &'a [LoadBlock], blob: &str) -> Option<&'a LoadBlock> {
    let want = file_name(blob);
    blocks.iter().rev().find(|b| b.launch.model.as_deref().map(file_name) == Some(want))
}

fn file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

fn context_line(c: &mut ContextLoad, line: &str) {
    let Some((prefix, rest)) = line.split_once(':') else { return };
    let rest = rest.trim();
    let device_and_mib = |marker: &str| -> Option<(String, f64)> {
        let (dev, tail) = rest.split_once(marker)?;
        let mib = tail.trim_start_matches([' ', '=']).split_whitespace().next()?.parse().ok()?;
        Some((dev.trim().to_string(), mib))
    };
    match prefix {
        "load_tensors" => {
            if let Some((d, m)) = device_and_mib(" model buffer size") {
                *c.model.entry(d).or_default() += m;
            }
        }
        "llama_context" => {
            if let Some((d, m)) = device_and_mib(" output buffer size") {
                c.output.insert(d, m);
            } else if let Some((key, val)) = rest.split_once('=') {
                let val = val.trim();
                match key.trim() {
                    "n_ctx" => c.n_ctx = val.parse().ok(),
                    "n_ctx_seq" => c.n_ctx_seq = val.parse().ok(),
                    "n_seq_max" => c.n_seq_max = val.parse().ok(),
                    "n_ubatch" => c.n_ubatch = val.parse().ok(),
                    "flash_attn" => c.flash_attn = Some(val.to_string()),
                    _ => {}
                }
            }
        }
        p if p.contains("kv_cache") => {
            if let Some((d, m)) = device_and_mib(" KV buffer size") {
                *c.kv.entry(d).or_default() += m;
            } else if rest.starts_with("size =") {
                if let Some(k) = kv_line(rest) {
                    c.kv_lines.push(k);
                }
            }
        }
        p if p.contains("memory_recurrent") => {
            if let Some((d, m)) = device_and_mib(" RS buffer size") {
                *c.rs.entry(d).or_default() += m;
            } else if rest.starts_with("size =") {
                c.rs_line = rs_line(rest);
            }
        }
        "sched_reserve" => {
            if let Some((d, m)) = device_and_mib(" compute buffer size") {
                c.compute.insert(d, m);
            }
        }
        _ => {}
    }
}

/// The first number after `=` in `size = 1088.00 MiB`.
fn first_num(s: &str) -> Option<f64> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.')).find(|t| !t.is_empty() && t != &".")?.parse().ok()
}

/// Number before `word` in `( 32768 cells,  16 layers,  1/1 seqs)`.
fn num_before(s: &str, word: &str) -> Option<String> {
    let i = s.find(word)?;
    s[..i].split([' ', '(', ',']).rfind(|t| !t.is_empty()).map(str::to_string)
}

fn kv_line(rest: &str) -> Option<KvLine> {
    let mib = first_num(rest.strip_prefix("size =")?)?;
    let cells = num_before(rest, " cells")?.parse().ok()?;
    let layers = num_before(rest, " layers")?.parse().ok()?;
    let seqs = num_before(rest, " seqs").and_then(|s| s.split('/').next_back().map(str::to_string))?.parse().ok()?;
    let typed = |tag: &str| -> Option<(String, f64)> {
        let after = rest.split(tag).nth(1)?;
        let ty = after.split(')').next()?.to_string();
        let mib = first_num(after.split_once(':')?.1)?;
        Some((ty, mib))
    };
    let (k_type, k_mib) = typed("K (")?;
    let (v_type, v_mib) = typed("V (").unwrap_or_default();
    Some(KvLine { mib, cells, layers, seqs, k_type, k_mib, v_type, v_mib })
}

fn rs_line(rest: &str) -> Option<RsLine> {
    let mib = first_num(rest.strip_prefix("size =")?)?;
    let layers = num_before(rest, " layers")?.parse().ok()?;
    let seqs = num_before(rest, " seqs")?.parse().ok()?;
    let rs_seq = num_before(rest, " rs_seq").and_then(|s| s.parse().ok()).unwrap_or(0);
    let tagged = |tag: &str| rest.split(tag).nth(1).and_then(|a| a.split_once(':')).and_then(|(_, v)| first_num(v));
    Some(RsLine { mib, layers, seqs, rs_seq, r_mib: tagged("R (").unwrap_or(0.0), s_mib: tagged("S (").unwrap_or(0.0) })
}

/// `"31.2 GiB"` → 31.2; MiB and B are converted.
fn gib(s: &str) -> Option<f64> {
    let mut it = s.split_whitespace();
    let n: f64 = it.next()?.parse().ok()?;
    Some(match it.next().unwrap_or("GiB") {
        "MiB" => n / 1024.0,
        "KiB" => n / 1024.0 / 1024.0,
        "B" => n / 1024.0 / 1024.0 / 1024.0,
        _ => n,
    })
}

/// `key="value"` from a logfmt line, with Go's `\\` and `\"` escapes undone.
fn quoted_field(line: &str, key: &str) -> Option<String> {
    let pat = format!(" {key}=\"");
    let start = line.find(&pat).map(|i| i + pat.len()).or_else(|| line.strip_prefix(&pat[1..]).map(|_| pat.len() - 1))?;
    let mut out = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => out.push(n),
                None => break,
            },
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    Some(out)
}

fn quoted_or_bare(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=");
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    if rest.starts_with('"') {
        return quoted_field(line, key);
    }
    rest.split_whitespace().next().map(str::to_string)
}

/// Splits the command line. Ollama prints it unquoted, so a path with a space would split; the
/// paths it passes (its own install and the blob store) have none on a default install.
fn split_argv(cmd: &str) -> Vec<String> {
    cmd.split_whitespace().map(str::to_string).collect()
}

fn parse_argv(argv: &[String]) -> Launch {
    let mut l = Launch { argv: argv.to_vec(), ..Launch::default() };
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        let next = argv.get(i + 1).cloned();
        let mut takes = true;
        match a {
            "-m" | "--model" => l.model = next,
            "--spec-draft-model" | "-md" | "--model-draft" => l.draft = next,
            "--mmproj" | "-mm" => l.mmproj = next,
            "--port" => l.port = next.and_then(|s| s.parse().ok()),
            "-c" | "--ctx-size" => l.ctx = next.and_then(|s| s.parse().ok()),
            "-np" | "--parallel" => l.parallel = next.and_then(|s| s.parse().ok()),
            "-ctk" | "--cache-type-k" => l.cache_k = next,
            "-ctv" | "--cache-type-v" => l.cache_v = next,
            "-ctkd" | "--cache-type-k-draft" => l.cache_k_draft = next,
            "-ctvd" | "--cache-type-v-draft" => l.cache_v_draft = next,
            "-fa" | "--flash-attn" => l.flash_attn = next,
            "--spec-draft-n-max" | "--draft-max" | "--draft" => l.spec_n = next.and_then(|s| s.parse().ok()),
            "-b" | "--batch-size" => l.batch = next.and_then(|s| s.parse().ok()),
            "-ub" | "--ubatch-size" => l.ubatch = next.and_then(|s| s.parse().ok()),
            "-ngl" | "--n-gpu-layers" | "--gpu-layers" => l.n_gpu_layers = next,
            "--fit" | "-fit" => l.fit = next,
            "--kv-unified" | "-kvu" => {
                l.kv_unified = true;
                takes = false;
            }
            "--context-shift" => {
                l.context_shift = true;
                takes = false;
            }
            _ => takes = false,
        }
        i += if takes { 2 } else { 1 };
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCH: &str = r#"time=2026-10-09T12:50:51.885-05:00 level=INFO source=llama_server.go:442 msg="starting llama-server" cmd="C:\\Ollama\\llama-server.exe --model C:\\m\\blobs\\sha256-17c3 --port 61916 --host 127.0.0.1 -c 32768 -np 1 --spec-type draft-mtp --spec-draft-n-max 3 --spec-draft-model C:\\m\\blobs\\sha256-50d9 --no-mmap --cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on -b 512 -ub 512 --context-shift --keep 4""#;

    #[test]
    fn launch_argv_is_read_with_go_escapes_undone() {
        let b = parse(LAUNCH);
        let l = &b[0].launch;
        assert_eq!(l.model.as_deref(), Some(r"C:\m\blobs\sha256-17c3"));
        assert_eq!(l.draft.as_deref(), Some(r"C:\m\blobs\sha256-50d9"));
        assert_eq!((l.port, l.ctx, l.parallel, l.spec_n, l.ubatch), (Some(61916), Some(32768), Some(1), Some(3), Some(512)));
        assert_eq!((l.cache_k.as_deref(), l.flash_attn.as_deref()), (Some("q8_0"), Some("on")));
        assert!(l.context_shift && !l.kv_unified);
        assert_eq!(l.time.as_deref(), Some("2026-10-09T12:50:51.885-05:00"));
    }

    #[test]
    fn buffer_lines_parse() {
        let text = format!(
            "{LAUNCH}\nload_tensors: offloaded 57/65 layers to GPU\nload_tensors:        CUDA0 model buffer size = 12433.29 MiB\n\
             llama_kv_cache:        CPU KV buffer size =   136.00 MiB\nllama_kv_cache:      CUDA0 KV buffer size =   952.00 MiB\n\
             llama_kv_cache: size = 1088.00 MiB ( 32768 cells,  16 layers,  1/1 seqs), K (q8_0):  544.00 MiB, V (q8_0):  544.00 MiB\n\
             llama_memory_recurrent: size =  598.50 MiB (     1 cells,  64 layers,  1 seqs  3 rs_seq), R (f32):   22.50 MiB, S (f32):  576.00 MiB\n\
             sched_reserve:      CUDA0 compute buffer size =   352.44 MiB\nsched_reserve:      CUDA0 compute buffer size =   350.00 MiB\n"
        );
        let b = parse(&text);
        let c = b[0].main().unwrap();
        assert_eq!(c.offloaded, Some((57, 65)));
        assert_eq!(c.kv.get("CPU"), Some(&136.0));
        assert_eq!(c.kv_lines[0].layers, 16);
        assert_eq!(c.kv_lines[0].k_type, "q8_0");
        assert_eq!(c.kv_lines[0].v_mib, 544.0);
        let rs = c.rs_line.as_ref().unwrap();
        assert_eq!((rs.layers, rs.seqs, rs.rs_seq, rs.s_mib), (64, 1, 3, 576.0));
        assert_eq!(c.compute.get("CUDA0"), Some(&350.0), "a later reservation replaces");
        assert!((c.on_gpu() - (12433.29 + 952.0 + 350.0)).abs() < 1e-6);
    }

    #[test]
    fn speeds_come_from_the_timing_lines() {
        let text = format!(
            "{LAUNCH}
slot print_timing: id  0 | task 1 | prompt eval time =     946.15 ms /  1433 tokens (    0.66 ms per token,  1514.57 tokens per second)
             slot print_timing: id  0 | task 1 |        eval time =  101303.42 ms /  8192 tokens (   12.37 ms per token,    80.87 tokens per second)
"
        );
        let b = &parse(&text)[0];
        assert_eq!((b.prompt_tok_s, b.decode_tok_s), (Some(1514.57), Some(80.87)));
    }
}
