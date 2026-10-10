//! Which language a file is, by extension: the tags grammar and the LSP `languageId`.

use std::path::Path;

/// A language the index tags or a language server serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
}

impl Lang {
    pub const ALL: [Lang; 8] =
        [Lang::Rust, Lang::Python, Lang::JavaScript, Lang::TypeScript, Lang::Tsx, Lang::Go, Lang::C, Lang::Cpp];

    /// The language of `path`, by extension (case-insensitive).
    pub fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" => Lang::Cpp,
            _ => return None,
        })
    }

    /// The short name stored in the index.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
            Lang::C => "c",
            Lang::Cpp => "cpp",
        }
    }

    pub fn from_name(name: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|l| l.name() == name)
    }

    /// The LSP `languageId` for a file of this language at `path` (`.jsx` is `javascriptreact`).
    pub fn language_id(self, path: &Path) -> &'static str {
        let jsx = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("jsx"));
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript if jsx => "javascriptreact",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "typescriptreact",
            Lang::Go => "go",
            Lang::C => "c",
            Lang::Cpp => "cpp",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_languages_and_ids() {
        assert_eq!(Lang::of(Path::new("a/b.RS")), Some(Lang::Rust));
        assert_eq!(Lang::of(Path::new("x.pyi")), Some(Lang::Python));
        assert_eq!(Lang::of(Path::new("x.tsx")), Some(Lang::Tsx));
        assert_eq!(Lang::of(Path::new("x.hpp")), Some(Lang::Cpp));
        assert_eq!(Lang::of(Path::new("Makefile")), None);
        assert_eq!(Lang::JavaScript.language_id(Path::new("a.jsx")), "javascriptreact");
        for l in Lang::ALL {
            assert_eq!(Lang::from_name(l.name()), Some(l));
        }
    }
}
