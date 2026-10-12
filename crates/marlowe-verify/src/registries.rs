//! Where a detected install really goes.
//!
//! An install command reaches whatever its project says: a mirror in `.npmrc`, an index in
//! `pip.conf` or `[tool.uv]`, a source replacement in `.cargo/config.toml`, a git dependency, the
//! hosts a lockfile resolved to. The hosts recorded on the command come from those files, with the
//! public registry as the default only when nothing replaces it, so the approval names, and the
//! crew's network rule checks, the host the install will reach. Go is left at its proxy default.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::profile::{Found, Fs, Kind, Source, Stack};

/// What the project's files say about where its packages come from.
#[derive(Default)]
struct Hosts {
    /// Replaces the stack's default registry.
    replace: Vec<String>,
    /// Reached in addition.
    extra: Vec<String>,
}

impl Hosts {
    fn replace(&mut self, url: &str) {
        if let Some(h) = host_of(url) {
            push_unique(&mut self.replace, h);
        }
    }

    fn extra(&mut self, url: &str) {
        if let Some(h) = host_of(url) {
            push_unique(&mut self.extra, h);
        }
    }
}

fn push_unique(v: &mut Vec<String>, h: String) {
    if !v.contains(&h) {
        v.push(h);
    }
}

/// The host of a URL, a `git+https://` / `sparse+https://` URL or an scp-style `git@host:path`;
/// `None` for a placeholder, a local address or something that is not a remote URL.
pub(crate) fn host_of(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_matches(['"', '\'', ',']);
    for p in ["sparse+", "registry+", "git+"] {
        s = s.strip_prefix(p).unwrap_or(s);
    }
    if s.contains("${") || s.contains("{{") || s.starts_with('$') {
        return None;
    }
    let rest = match s.split_once("://") {
        Some((_, r)) => r,
        None => s.strip_prefix("git@")?,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if authority.starts_with('[') {
        return None;
    }
    let host = authority.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() || host.contains(' ') || crate::http::is_loopback_host(&host) {
        return None;
    }
    Some(host)
}

/// The directory a scan of `rel` under `dir` started from.
fn root_of(dir: &Path, rel: &str) -> PathBuf {
    let mut root = dir.to_path_buf();
    if rel != "." {
        for _ in rel.split('/').filter(|p| !p.is_empty()) {
            root.pop();
        }
    }
    root
}

/// Sets `registry_host` / `extra_hosts` on the detected install commands of `rel`.
pub(crate) fn refine(fs: &Fs<'_>, dir: &Path, rel: &str, f: &mut Found) {
    let root = root_of(dir, rel);
    let here: Vec<Stack> = f.stacks.iter().filter(|s| s.dir == rel).map(|s| s.stack).collect();
    for c in f.commands.iter_mut().filter(|c| c.kind == Kind::Install && c.cwd == rel && c.source == Source::Detected) {
        let Some(stack) = c.stack.filter(|s| here.contains(s)) else { continue };
        let default = c.registry_host.clone();
        let mut hosts = Hosts::default();
        match stack {
            Stack::Cargo => cargo(fs, dir, &root, &mut hosts),
            Stack::Uv | Stack::Poetry | Stack::Pip => python(fs, dir, &mut hosts),
            Stack::Npm | Stack::Pnpm | Stack::Yarn | Stack::Bun => node(fs, dir, &root, stack, default.as_deref(), &mut hosts),
            _ => continue,
        }
        let mut replace = hosts.replace.into_iter();
        if let Some(first) = replace.next() {
            c.registry_host = Some(first);
        }
        let mut extra: Vec<String> = replace.chain(hosts.extra).collect();
        extra.retain(|h| Some(h) != c.registry_host.as_ref());
        extra.sort();
        extra.dedup();
        c.extra_hosts = extra;
    }
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

// ---- node ------------------------------------------------------------------------------------

fn node(fs: &Fs<'_>, dir: &Path, root: &Path, stack: Stack, default: Option<&str>, h: &mut Hosts) {
    static YARNRC: OnceLock<Regex> = OnceLock::new();
    static YARN1: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let mut dirs = vec![dir.to_path_buf()];
    if root != dir {
        dirs.push(root.to_path_buf());
    }
    for d in &dirs {
        if let Some(t) = fs.text(&d.join(".npmrc")) {
            for line in t.lines().map(str::trim).filter(|l| !l.starts_with(['#', ';'])) {
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    if k == "registry" {
                        h.replace(v);
                    } else if k.ends_with(":registry") {
                        h.extra(v);
                    }
                }
            }
        }
        if let Some(t) = fs.text(&d.join(".yarnrc.yml")) {
            for c in re(&YARNRC, r#"(?m)^(\s*)npmRegistryServer\s*:\s*["']?([^\s"']+)"#).captures_iter(&t) {
                if c[1].is_empty() { h.replace(&c[2]) } else { h.extra(&c[2]) }
            }
        }
        if let Some(t) = fs.text(&d.join(".yarnrc")) {
            for c in re(&YARN1, r#"(?m)^registry\s+"?([^\s"]+)"#).captures_iter(&t) {
                h.replace(&c[1]);
            }
        }
    }
    // Dependencies that are not from the registry: git, tarball URLs, `github:user/repo`.
    if let Some(pkg) = fs.text(&dir.join("package.json")).and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
        for key in ["dependencies", "devDependencies", "optionalDependencies"] {
            for spec in pkg.get(key).and_then(|d| d.as_object()).into_iter().flat_map(|d| d.values()).filter_map(|v| v.as_str()) {
                if let Some(host) = spec_host(spec) {
                    h.extra(&format!("https://{host}/"));
                }
            }
        }
    }
    // What the lockfile resolved to is where `npm ci` / `yarn install --immutable` fetch from.
    let lock = match stack {
        Stack::Npm => ["package-lock.json", "npm-shrinkwrap.json"].as_slice(),
        Stack::Pnpm => ["pnpm-lock.yaml"].as_slice(),
        Stack::Yarn => ["yarn.lock"].as_slice(),
        _ => ["bun.lock"].as_slice(),
    };
    let mut from_lock = Vec::new();
    for name in lock {
        if let Some(t) = fs.text(&dir.join(name)) {
            for line in t.lines().filter(|l| l.contains("resolved") || l.contains("tarball")) {
                for m in re(&URL, r#"https?://[^\s"',)]+"#).find_iter(line) {
                    if let Some(host) = host_of(m.as_str()) {
                        push_unique(&mut from_lock, host);
                    }
                }
            }
        }
    }
    from_lock.truncate(8);
    // A lockfile that never mentions the default registry says the packages come from elsewhere.
    let default_used = default.is_some_and(|d| from_lock.iter().any(|x| x == d));
    for host in from_lock {
        if !default_used && h.replace.is_empty() {
            h.replace.push(host);
        } else {
            push_unique(&mut h.extra, host);
        }
    }
}

/// The host a package.json dependency spec names, if it is not a registry version range.
fn spec_host(spec: &str) -> Option<String> {
    let s = spec.trim();
    if let Some(h) = host_of(s).filter(|_| s.contains("://") || s.starts_with("git@") || s.starts_with("git+")) {
        return Some(h);
    }
    for (prefix, host) in [("github:", "github.com"), ("gitlab:", "gitlab.com"), ("bitbucket:", "bitbucket.org")] {
        if s.starts_with(prefix) {
            return Some(host.into());
        }
    }
    // `user/repo` and `user/repo#ref` are GitHub shorthand.
    static SHORT: OnceLock<Regex> = OnceLock::new();
    re(&SHORT, r"^[A-Za-z0-9_.\-]+/[A-Za-z0-9_.\-]+(#.*)?$").is_match(s).then(|| "github.com".to_string())
}

// ---- python ----------------------------------------------------------------------------------

fn python(fs: &Fs<'_>, dir: &Path, h: &mut Hosts) {
    static GIT: OnceLock<Regex> = OnceLock::new();
    static LOCK: OnceLock<Regex> = OnceLock::new();
    for name in fs.file_names(dir) {
        let lower = name.to_ascii_lowercase();
        let Some(text) = fs.text(&dir.join(&name)) else { continue };
        if lower.starts_with("requirements") && lower.ends_with(".txt") {
            requirements(&text, h);
        } else if lower == "pip.conf" || lower == "pip.ini" {
            for line in text.lines().map(str::trim).filter(|l| !l.starts_with(['#', ';'])) {
                if let Some((k, v)) = line.split_once('=') {
                    match k.trim().replace('_', "-").as_str() {
                        "index-url" => h.replace(v),
                        "extra-index-url" | "find-links" => v.split_whitespace().for_each(|u| h.extra(u)),
                        _ => {}
                    }
                }
            }
        } else if lower == "pyproject.toml" || lower == "uv.toml" {
            if let Ok(value) = text.parse::<toml::Value>() {
                walk_toml(&value, "", h);
            }
            for c in re(&GIT, r"git\+[^\s'\x22,]+").find_iter(&text) {
                h.extra(c.as_str());
            }
        } else if lower == "uv.lock" {
            for c in re(&LOCK, r#"(?:registry|git)\s*=\s*"([^"]+)""#).captures_iter(&text) {
                h.extra(&c[1]);
            }
        }
    }
}

fn requirements(text: &str, h: &mut Hosts) {
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("-i" | "--index-url") => words.next().into_iter().for_each(|u| h.replace(u)),
            Some("--extra-index-url" | "-f" | "--find-links") => words.next().into_iter().for_each(|u| h.extra(u)),
            _ => {
                for w in line.split_whitespace().chain(line.split('@').map(str::trim)) {
                    if w.starts_with("git+") || w.starts_with("http://") || w.starts_with("https://") {
                        h.extra(w);
                    }
                }
            }
        }
    }
    for line in text.lines() {
        for opt in ["--index-url=", "--extra-index-url="] {
            if let Some(u) = line.trim().strip_prefix(opt) {
                if opt == "--index-url=" { h.replace(u) } else { h.extra(u) }
            }
        }
    }
}

/// Every index, source and git/url dependency in a pyproject or uv.toml.
fn walk_toml(v: &toml::Value, path: &str, h: &mut Hosts) {
    match v {
        toml::Value::Table(t) => {
            // `[[tool.uv.index]]` / `[[tool.poetry.source]]` entries say whether they are the default.
            let primary = t.get("default").and_then(toml::Value::as_bool).unwrap_or(false)
                || matches!(t.get("priority").and_then(toml::Value::as_str), Some("primary" | "default"));
            for (k, val) in t {
                let sub = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match (k.as_str(), val) {
                    ("index-url" | "index_url", toml::Value::String(s)) => h.replace(s),
                    ("extra-index-url" | "extra_index_url" | "find-links" | "find_links", toml::Value::String(s)) => h.extra(s),
                    ("extra-index-url" | "extra_index_url" | "find-links" | "find_links", toml::Value::Array(a)) => a.iter().filter_map(toml::Value::as_str).for_each(|s| h.extra(s)),
                    ("url", toml::Value::String(s)) if !path.starts_with("project.urls") => {
                        if primary { h.replace(s) } else { h.extra(s) }
                    }
                    ("git", toml::Value::String(s)) => h.extra(s),
                    _ => walk_toml(val, &sub, h),
                }
            }
        }
        toml::Value::Array(a) => a.iter().for_each(|x| walk_toml(x, path, h)),
        // `name @ https://...` in a dependency list.
        toml::Value::String(s) if s.contains(" @ ") => {
            if let Some((_, url)) = s.split_once(" @ ") {
                h.extra(url.trim());
            }
        }
        _ => {}
    }
}

// ---- cargo -----------------------------------------------------------------------------------

fn cargo(fs: &Fs<'_>, dir: &Path, root: &Path, h: &mut Hosts) {
    static GIT: OnceLock<Regex> = OnceLock::new();
    static SOURCE: OnceLock<Regex> = OnceLock::new();
    let mut dirs = vec![dir.to_path_buf()];
    if root != dir {
        dirs.push(root.to_path_buf());
    }
    for d in &dirs {
        for name in ["config.toml", "config"] {
            let Some(text) = fs.text(&d.join(".cargo").join(name)) else { continue };
            let Ok(cfg) = text.parse::<toml::Value>() else { continue };
            let sources = cfg.get("source").and_then(toml::Value::as_table);
            let url_of = |t: &toml::Value| ["registry", "git"].iter().find_map(|k| t.get(*k).and_then(toml::Value::as_str)).map(String::from);
            let replaced_with = sources.and_then(|s| s.get("crates-io")).and_then(|t| t.get("replace-with")).and_then(toml::Value::as_str);
            for (name, t) in sources.into_iter().flatten().filter(|(n, _)| n.as_str() != "crates-io") {
                let Some(url) = url_of(t) else { continue };
                if replaced_with == Some(name.as_str()) { h.replace(&url) } else { h.extra(&url) }
            }
            for t in cfg.get("registries").and_then(toml::Value::as_table).into_iter().flat_map(|r| r.values()) {
                if let Some(index) = t.get("index").and_then(toml::Value::as_str) {
                    h.extra(index);
                }
            }
        }
    }
    if let Some(text) = fs.text(&dir.join("Cargo.toml")) {
        for c in re(&GIT, r#"\bgit\s*=\s*"([^"]+)""#).captures_iter(&text) {
            h.extra(&c[1]);
        }
    }
    // The lockfile lists where each crate comes from; crates.io itself is the default.
    if let Some(text) = fs.text(&dir.join("Cargo.lock")) {
        for c in re(&SOURCE, r#"(?m)^source\s*=\s*"((?:git|registry|sparse)\+[^"]+)""#).captures_iter(&text) {
            if !c[1].contains("crates.io-index") && !c[1].contains("index.crates.io") {
                h.extra(&c[1]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_read_from_urls_of_every_shape() {
        for (raw, want) in [
            ("https://registry.npmjs.org/", Some("registry.npmjs.org")),
            ("git+https://user:tok@github.com/o/r.git#main", Some("github.com")),
            ("sparse+https://my.mirror.example/index/", Some("my.mirror.example")),
            ("git@gitlab.com:o/r.git", Some("gitlab.com")),
            ("http://localhost:4873/", None),
            ("${REGISTRY}/x", None),
            ("^1.2.3", None),
        ] {
            assert_eq!(host_of(raw).as_deref(), want, "{raw}");
        }
        assert_eq!(spec_host("github:o/r").as_deref(), Some("github.com"));
        assert_eq!(spec_host("o/r#v1").as_deref(), Some("github.com"));
        assert_eq!(spec_host("^1.2.3"), None);
        assert_eq!(spec_host("https://cdn.example/pkg.tgz").as_deref(), Some("cdn.example"));
    }

    #[test]
    fn requirements_and_pyproject_name_their_indexes() {
        let mut h = Hosts::default();
        requirements("-i https://pypi.internal.example/simple\n--extra-index-url https://extra.example/simple\nrequests==2\nfoo @ git+https://github.com/o/foo.git\n", &mut h);
        assert_eq!(h.replace, ["pypi.internal.example"]);
        assert!(h.extra.contains(&"extra.example".to_string()) && h.extra.contains(&"github.com".to_string()), "{:?}", h.extra);
        let mut h = Hosts::default();
        let t: toml::Value = "[[tool.uv.index]]\nname = 'corp'\nurl = 'https://corp.example/simple'\ndefault = true\n[tool.uv.sources]\nfoo = { git = 'https://git.example/foo' }\n[project.urls]\nHomepage = 'https://home.example'\n".parse().unwrap();
        walk_toml(&t, "", &mut h);
        assert_eq!(h.replace, ["corp.example"]);
        assert_eq!(h.extra, ["git.example"]);
    }
}
