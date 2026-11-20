//! The two backends. They expose the identical `llama-server` HTTP API, so the engine runs on
//! either; they differ in where they come from and what GGUF tensor types they understand.

/// A llama.cpp build Marlowe can serve models with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    /// Upstream `ggml-org/llama.cpp`.
    LlamaCpp,
    /// Matthew's fork, `devvexus/ms-llama.cpp`: adds the MS ("Mad Shift") quant types and their
    /// kernels.
    MsLlamaCpp,
}

/// The backend a fresh install selects.
pub const DEFAULT: Backend = Backend::LlamaCpp;

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::LlamaCpp, Backend::MsLlamaCpp];

    /// The id on the wire, in settings and as the folder name.
    pub fn id(self) -> &'static str {
        match self {
            Backend::LlamaCpp => "llama.cpp",
            Backend::MsLlamaCpp => "ms-llama.cpp",
        }
    }

    /// `llama.cpp`, `ms-llama.cpp`, and the spellings without the dot.
    pub fn parse(s: &str) -> Option<Backend> {
        match s.trim().to_ascii_lowercase().as_str() {
            "llama.cpp" | "llamacpp" | "llama" => Some(Backend::LlamaCpp),
            "ms-llama.cpp" | "ms-llamacpp" | "ms-llama" | "ms" => Some(Backend::MsLlamaCpp),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        self.id()
    }

    pub fn description(self) -> &'static str {
        match self {
            Backend::LlamaCpp => "The standard llama.cpp backend.",
            Backend::MsLlamaCpp => "A much more accurate MS-capable model backend.",
        }
    }

    /// About how much a first install downloads (the Windows x64 CUDA 12 archives), for the offer
    /// that asks before anything is fetched; the install reports the real total as it goes.
    pub fn approx_download_bytes(self) -> u64 {
        match self {
            Backend::LlamaCpp => 710_000_000,
            Backend::MsLlamaCpp => 751_000_000,
        }
    }

    /// The GitHub repository, `owner/name`.
    pub fn repo(self) -> &'static str {
        match self {
            Backend::LlamaCpp => "ggml-org/llama.cpp",
            Backend::MsLlamaCpp => "devvexus/ms-llama.cpp",
        }
    }

    /// Whether a release carries no binaries of its own, only `nightly-tag.txt` naming the build
    /// whose binaries it stands for.
    pub fn releases_point_at_builds(self) -> bool {
        self == Backend::LlamaCpp
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_the_default_is_stock_llama_cpp() {
        for b in Backend::ALL {
            assert_eq!(Backend::parse(b.id()), Some(b));
        }
        assert_eq!(DEFAULT, Backend::LlamaCpp);
        assert_eq!(Backend::parse("MS-Llama.cpp"), Some(Backend::MsLlamaCpp));
        assert_eq!(Backend::parse("ollama"), None);
        assert_eq!(Backend::MsLlamaCpp.repo(), "devvexus/ms-llama.cpp");
    }
}
