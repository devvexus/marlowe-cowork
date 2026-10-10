//! tree-sitter tags: definitions and references per file (design 3.3 L1).
//!
//! The grammars' own `tags.scm` serve every language but Rust, whose stock query misses consts,
//! statics, scoped calls and type uses and names an `impl Trait for T` after the trait; ours
//! names it after the type, so methods get the type as their container. TypeScript and TSX use
//! the JavaScript query plus their own, as the tree-sitter CLI does.
//!
//! tree-sitter is a C compile, so it is absent on aarch64 until approval 9.1-i; there [`supported`]
//! is false and [`tag`] returns `None` (the tools fall back to the language server).

use super::lang::Lang;

/// One tag as the tagger found it. Rows and columns are 0-based; columns are bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTag {
    pub name: String,
    /// `function`, `method`, `class`, `interface`, `module`, `macro`, `constant`, `type`,
    /// `variant`, `field` for definitions; `call`, `type`, `implementation`, `class` for references.
    pub kind: String,
    pub is_def: bool,
    pub line: u32,
    pub col: u32,
    /// The last line of a definition's item (its body included); a reference's name's line.
    pub end_line: u32,
    pub end_col: u32,
    /// The innermost enclosing type, trait, impl, class or module.
    pub container: Option<String>,
    /// The definition's first line, trimmed (empty for references).
    pub sig: String,
    /// Doc comment, where the grammar's query captures one.
    pub docs: Option<String>,
}

/// Whether this build can tag files.
pub fn supported() -> bool {
    imp::SUPPORTED
}

/// The tags of `src` in `lang`, or `None` when this build has no tagger or the grammar failed.
pub fn tag(lang: Lang, src: &[u8]) -> Option<Vec<RawTag>> {
    imp::tag(lang, src)
}

/// Kinds that contain others (their names become a member's container).
fn is_container(kind: &str, is_def: bool) -> bool {
    if is_def {
        matches!(kind, "class" | "interface" | "module" | "type")
    } else {
        kind == "implementation"
    }
}

/// Fills in each definition's container: the smallest container tag whose node encloses it.
/// `ranges[i]` is tag `i`'s node byte range.
fn assign_containers(tags: &mut [RawTag], ranges: &[std::ops::Range<usize>]) {
    let containers: Vec<usize> = (0..tags.len()).filter(|&i| is_container(&tags[i].kind, tags[i].is_def)).collect();
    if containers.is_empty() {
        return;
    }
    for i in 0..tags.len() {
        if !tags[i].is_def {
            continue;
        }
        let r = &ranges[i];
        let best = containers
            .iter()
            .copied()
            .filter(|&c| c != i)
            .filter(|&c| {
                let cr = &ranges[c];
                cr.start <= r.start && r.end <= cr.end && (cr.start, cr.end) != (r.start, r.end)
            })
            .min_by_key(|&c| ranges[c].end - ranges[c].start);
        if let Some(c) = best {
            let parent_kind = tags[c].kind.clone();
            tags[i].container = Some(tags[c].name.clone());
            // Python and JS methods are functions inside a class.
            if tags[i].kind == "function" && matches!(parent_kind.as_str(), "class" | "implementation" | "interface") {
                tags[i].kind = "method".into();
            }
            // A Rust `mod` body is a declaration list too; its functions are not methods.
            if tags[i].kind == "method" && parent_kind == "module" {
                tags[i].kind = "function".into();
            }
        }
    }
}

/// Our Rust query (see the module doc). Definitions first: one tag per name node, and the lowest
/// pattern index wins, so a definition's name is never also a `type` reference.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
const RUST_TAGS: &str = r#"
(struct_item name: (type_identifier) @name) @definition.class
(enum_item name: (type_identifier) @name) @definition.class
(union_item name: (type_identifier) @name) @definition.class
(type_item name: (type_identifier) @name) @definition.type
(trait_item name: (type_identifier) @name) @definition.interface
(mod_item name: (identifier) @name) @definition.module
(macro_definition name: (identifier) @name) @definition.macro
(declaration_list (function_item name: (identifier) @name) @definition.method)
(declaration_list (function_signature_item name: (identifier) @name) @definition.method)
(function_item name: (identifier) @name) @definition.function
(function_signature_item name: (identifier) @name) @definition.function
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(enum_variant name: (identifier) @name) @definition.variant

(impl_item type: (type_identifier) @name) @reference.implementation
(impl_item type: (generic_type type: (type_identifier) @name)) @reference.implementation
(impl_item type: (scoped_type_identifier name: (type_identifier) @name)) @reference.implementation

(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (scoped_identifier name: (identifier) @name))) @reference.call
(call_expression function: (generic_function function: (field_expression field: (field_identifier) @name))) @reference.call
(macro_invocation macro: (identifier) @name) @reference.call
(macro_invocation macro: (scoped_identifier name: (identifier) @name)) @reference.call
(scoped_identifier path: (identifier) @name) @reference.type
(type_identifier) @name @reference.type
"#;

#[cfg(not(target_arch = "aarch64"))]
mod imp {
    use std::cell::RefCell;
    use std::sync::OnceLock;

    use tree_sitter_tags::{TagsConfiguration, TagsContext};

    use super::{assign_containers, Lang, RawTag, RUST_TAGS};

    pub const SUPPORTED: bool = true;

    /// Largest definition line kept as a signature.
    const SIG_MAX: usize = 200;

    static CONFIGS: [OnceLock<Option<TagsConfiguration>>; 8] = [const { OnceLock::new() }; 8];

    fn slot(lang: Lang) -> usize {
        Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0)
    }

    fn build(lang: Lang) -> Option<TagsConfiguration> {
        let js_ts = || format!("{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY);
        let (language, query, locals): (tree_sitter::Language, String, &str) = match lang {
            Lang::Rust => (tree_sitter_rust::LANGUAGE.into(), RUST_TAGS.to_string(), ""),
            Lang::Python => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::TAGS_QUERY.to_string(), ""),
            Lang::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::TAGS_QUERY.to_string(),
                tree_sitter_javascript::LOCALS_QUERY,
            ),
            Lang::TypeScript => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), js_ts(), ""),
            Lang::Tsx => (tree_sitter_typescript::LANGUAGE_TSX.into(), js_ts(), ""),
            Lang::Go => (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY.to_string(), ""),
            Lang::C => (tree_sitter_c::LANGUAGE.into(), tree_sitter_c::TAGS_QUERY.to_string(), ""),
            Lang::Cpp => (tree_sitter_cpp::LANGUAGE.into(), tree_sitter_cpp::TAGS_QUERY.to_string(), ""),
        };
        TagsConfiguration::new(language, &query, locals).ok()
    }

    fn config(lang: Lang) -> Option<&'static TagsConfiguration> {
        CONFIGS[slot(lang)].get_or_init(|| build(lang)).as_ref()
    }

    thread_local! {
        static CTX: RefCell<TagsContext> = RefCell::new(TagsContext::new());
    }

    pub fn tag(lang: Lang, src: &[u8]) -> Option<Vec<RawTag>> {
        let cfg = config(lang)?;
        CTX.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            let (iter, _has_error) = ctx.generate_tags(cfg, src, None).ok()?;
            let mut tags = Vec::new();
            let mut ranges = Vec::new();
            // Byte offset of each line start, for a definition's last line.
            let starts: Vec<usize> = std::iter::once(0)
                .chain(src.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1))
                .collect();
            let row_of = |off: usize| starts.partition_point(|&s| s <= off).saturating_sub(1) as u32;
            for t in iter {
                let Ok(t) = t else { continue };
                let name = String::from_utf8_lossy(src.get(t.name_range.clone())?).into_owned();
                if name.is_empty() {
                    continue;
                }
                let kind = cfg.syntax_type_name(t.syntax_type_id).to_string();
                let sig = if t.is_definition {
                    let line = String::from_utf8_lossy(src.get(t.line_range.clone()).unwrap_or_default());
                    let line = line.trim();
                    let mut cut = line.len().min(SIG_MAX);
                    while !line.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    line[..cut].to_string()
                } else {
                    String::new()
                };
                tags.push(RawTag {
                    name,
                    kind,
                    is_def: t.is_definition,
                    line: t.span.start.row as u32,
                    col: t.span.start.column as u32,
                    // A definition ends where its item does; a reference where its name does.
                    end_line: if t.is_definition { row_of(t.range.end.saturating_sub(1).max(t.range.start)) } else { t.span.end.row as u32 },
                    end_col: t.span.end.column as u32,
                    container: None,
                    sig,
                    docs: t.docs.filter(|d| !d.trim().is_empty()),
                });
                ranges.push(t.range);
            }
            assign_containers(&mut tags, &ranges);
            Some(tags)
        })
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use super::{Lang, RawTag};

    pub const SUPPORTED: bool = false;

    pub fn tag(_lang: Lang, _src: &[u8]) -> Option<Vec<RawTag>> {
        None
    }
}

#[cfg(all(test, not(target_arch = "aarch64")))]
mod tests {
    use super::*;

    fn defs(tags: &[RawTag]) -> Vec<(String, String, Option<String>)> {
        tags.iter().filter(|t| t.is_def).map(|t| (t.name.clone(), t.kind.clone(), t.container.clone())).collect()
    }

    #[test]
    fn rust_definitions_carry_kinds_and_containers() {
        let src = br#"
pub struct Parser { pos: usize }
const MAX: usize = 3;
pub trait Visit { fn visit(&self); }
impl Visit for Parser {
    fn visit(&self) { helper(); self.step(); Parser::new(); }
}
impl Parser {
    pub fn new() -> Self { Parser { pos: 0 } }
    fn step(&mut self) {}
}
fn helper() { println!("x"); }
mod inner { pub fn deep() {} }
"#;
        let tags = tag(Lang::Rust, src).unwrap();
        let d = defs(&tags);
        assert!(d.contains(&("Parser".into(), "class".into(), None)), "{d:?}");
        assert!(d.contains(&("MAX".into(), "constant".into(), None)));
        assert!(d.contains(&("Visit".into(), "interface".into(), None)));
        assert!(d.contains(&("visit".into(), "method".into(), Some("Parser".into()))), "impl Trait for T names T: {d:?}");
        assert!(d.contains(&("new".into(), "method".into(), Some("Parser".into()))));
        assert!(d.contains(&("helper".into(), "function".into(), None)));
        assert!(d.contains(&("deep".into(), "function".into(), Some("inner".into()))));
        let sig = tags.iter().find(|t| t.is_def && t.name == "new").unwrap();
        assert_eq!(sig.sig, "pub fn new() -> Self { Parser { pos: 0 } }");
        assert_eq!((sig.line, sig.col), (8, 11));
        let parser = tags.iter().find(|t| t.is_def && t.name == "Parser").unwrap();
        assert_eq!(parser.end_line, 1, "a one-line struct ends where it starts");
        let visit = tags.iter().find(|t| t.is_def && t.name == "visit" && t.line == 5).unwrap();
        assert_eq!(visit.end_line, 5);
        let imp = tags.iter().filter(|t| t.is_def && t.name == "step").map(|t| t.end_line).next();
        assert_eq!(imp, Some(9));
        let refs: Vec<&str> = tags.iter().filter(|t| !t.is_def && t.kind == "call").map(|t| t.name.as_str()).collect();
        for r in ["helper", "step", "new", "println"] {
            assert!(refs.contains(&r), "{r} in {refs:?}");
        }
        // The struct's own name is a definition, not also a type reference.
        let parser_line1 = tags.iter().filter(|t| t.name == "Parser" && t.line == 1).count();
        assert_eq!(parser_line1, 1);
    }

    #[test]
    fn python_methods_are_functions_inside_a_class() {
        let src = b"class Box:\n    def open(self):\n        return helper()\n\ndef helper():\n    pass\n";
        let d = defs(&tag(Lang::Python, src).unwrap());
        assert!(d.contains(&("Box".into(), "class".into(), None)));
        assert!(d.contains(&("open".into(), "method".into(), Some("Box".into()))), "{d:?}");
        assert!(d.contains(&("helper".into(), "function".into(), None)));
    }

    #[test]
    fn every_grammar_compiles_its_query() {
        let samples: [(Lang, &[u8]); 8] = [
            (Lang::Rust, b"fn a() {}"),
            (Lang::Python, b"def a():\n  pass\n"),
            (Lang::JavaScript, b"function a() {}\n"),
            (Lang::TypeScript, b"interface I { m(): void }\nfunction a(): number { return 1 }\n"),
            (Lang::Tsx, b"function A() { return <div/> }\n"),
            (Lang::Go, b"package p\nfunc a() {}\n"),
            (Lang::C, b"int a(void) { return 0; }\n"),
            (Lang::Cpp, b"class K { void m(); };\nint a() { return 0; }\n"),
        ];
        for (lang, src) in samples {
            let tags = tag(lang, src).unwrap_or_else(|| panic!("{lang:?} query failed to compile"));
            assert!(tags.iter().any(|t| t.is_def), "{lang:?}: {tags:?}");
        }
    }
}
