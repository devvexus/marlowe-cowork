//! The CUDA version the installed NVIDIA driver supports, which picks CUDA 13.5 or 12.4.

/// `(13, 4)` for a driver that runs CUDA 13.4 builds; `None` when there is no NVIDIA driver or it
/// could not be read (the caller then takes 12.4).
pub fn cuda_driver() -> Option<(u32, u32)> {
    nvml().or_else(nvidia_smi)
}

fn nvml() -> Option<(u32, u32)> {
    let v = nvml_wrapper::Nvml::init().ok()?.sys_cuda_driver_version().ok()?;
    u32::try_from(v).ok().map(from_nvml)
}

/// NVML packs the version as `major * 1000 + minor * 10` (13040 is 13.4).
pub fn from_nvml(v: u32) -> (u32, u32) {
    (v / 1000, (v % 1000) / 10)
}

/// The `CUDA Version: 12.9` (or `CUDA UMD Version: 13.4`) in `nvidia-smi`'s header.
fn nvidia_smi() -> Option<(u32, u32)> {
    let limits = marlowe_os::child::RunLimits { timeout: std::time::Duration::from_secs(10), max_output_bytes: 64 * 1024 };
    let out = marlowe_os::child::run(std::process::Command::new("nvidia-smi"), &marlowe_os::child::ChildSpec::new(), limits, &|| false).ok()?;
    parse_smi(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_smi(text: &str) -> Option<(u32, u32)> {
    let at = text.find("CUDA Version:").map(|i| i + "CUDA Version:".len()).or_else(|| text.find("CUDA UMD Version:").map(|i| i + "CUDA UMD Version:".len()))?;
    let word = text[at..].split_whitespace().next()?;
    let (major, minor) = word.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvml_and_nvidia_smi_spellings_both_read() {
        assert_eq!(from_nvml(13040), (13, 4));
        assert_eq!(from_nvml(12090), (12, 9));
        assert_eq!(from_nvml(13050), (13, 5));
        assert_eq!(parse_smi("| NVIDIA-SMI 617.14   KMD Version: 617.14   CUDA UMD Version: 13.4 |"), Some((13, 4)));
        assert_eq!(parse_smi("| Driver Version: 555.1    CUDA Version: 12.5     |"), Some((12, 5)));
        assert_eq!(parse_smi("command not found"), None);
    }
}
