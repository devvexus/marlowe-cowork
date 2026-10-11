//! Which network hosts a command contacts: URL arguments of fetchers, `ssh`/`scp` targets,
//! `git clone` URLs, and package installers mapped to their registry (design 3.5 Egress).

/// The host of a URL or `user@host:path` (scp-style), lower-case; `None` for local paths.
pub fn host_of_url(arg: &str) -> Option<String> {
    let arg = arg.trim().trim_matches(|c| c == '"' || c == '\'');
    if arg.is_empty() || arg.contains(char::is_whitespace) {
        return None;
    }
    let rest = if let Some((scheme, rest)) = arg.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") || scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) {
            return None;
        }
        rest
    } else if let Some((user_host, _path)) = arg.split_once(':') {
        // `git@github.com:owner/repo.git`. Not a drive letter (`C:\x`) or a port-only value.
        let host = user_host.rsplit('@').next().unwrap_or(user_host);
        if host.len() <= 1 || !user_host.contains('@') && !host.contains('.') || host.contains(['/', '\\']) {
            return None;
        }
        return clean_host(host);
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    };
    clean_host(host)
}

/// A bare `host[/path]` as curl accepts it (`example.com/x`), when it looks like a domain.
pub(crate) fn host_of_bare(arg: &str) -> Option<String> {
    if let Some(h) = host_of_url(arg) {
        return Some(h);
    }
    let first = arg.split(['/', '?', '#']).next().unwrap_or("");
    let host = first.split(':').next().unwrap_or("");
    if host.eq_ignore_ascii_case("localhost") {
        return Some("localhost".into());
    }
    let labels: Vec<&str> = host.split('.').collect();
    let tld_ok = labels.last().is_some_and(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphabetic()));
    if labels.len() >= 2 && tld_ok && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
        return clean_host(host);
    }
    // A dotted IPv4 address.
    if labels.len() == 4 && labels.iter().all(|l| l.parse::<u8>().is_ok()) {
        return clean_host(host);
    }
    None
}

fn clean_host(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains(['$', '%', '{', '}', '(', ')', '*']) {
        return None;
    }
    Some(host)
}

/// Loopback names, always free.
pub(crate) fn is_local(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.") || host.ends_with(".localhost") || host == "0.0.0.0"
}

/// The registry hosts a package-manager invocation contacts (`npm install` ->
/// `registry.npmjs.org`), from its program and arguments. Empty when it fetches nothing.
pub fn registry_hosts(program: &str, args: &[String]) -> Vec<&'static str> {
    let sub = args.iter().find(|a| !a.starts_with('-')).map(|s| s.to_ascii_lowercase());
    let sub = sub.as_deref();
    const NPM: &[&str] = &["registry.npmjs.org"];
    const PYPI: &[&str] = &["pypi.org", "files.pythonhosted.org"];
    const CRATES: &[&str] = &["index.crates.io", "static.crates.io"];
    const GO: &[&str] = &["proxy.golang.org"];
    match program {
        "npm" | "pnpm" | "bun" => match sub {
            Some("install" | "i" | "ci" | "add" | "update" | "up" | "upgrade" | "dlx" | "x" | "exec" | "create" | "init") => NPM.to_vec(),
            None if program == "bun" => Vec::new(),
            _ => Vec::new(),
        },
        "yarn" => match sub {
            None | Some("install" | "add" | "upgrade" | "up" | "dlx" | "create") => NPM.to_vec(),
            _ => Vec::new(),
        },
        "npx" | "bunx" | "pnpx" => NPM.to_vec(),
        "pip" | "pip3" => match sub {
            Some("install" | "download" | "wheel" | "index" | "search") => PYPI.to_vec(),
            _ => Vec::new(),
        },
        "uv" => match sub {
            Some("add" | "sync" | "lock" | "tool" | "venv" | "python" | "export" | "run") => PYPI.to_vec(),
            Some("pip") if args.iter().any(|a| a == "install" || a == "compile" || a == "sync") => PYPI.to_vec(),
            _ => Vec::new(),
        },
        "uvx" | "pipx" => PYPI.to_vec(),
        "poetry" | "pdm" | "hatch" => match sub {
            Some("add" | "install" | "update" | "lock" | "sync") => PYPI.to_vec(),
            _ => Vec::new(),
        },
        "cargo" => match sub {
            Some("install" | "add" | "fetch" | "update" | "search" | "generate-lockfile") => CRATES.to_vec(),
            _ => Vec::new(),
        },
        "go" => match sub {
            Some("get" | "install") => GO.to_vec(),
            Some("mod") if args.iter().any(|a| a == "download" || a == "tidy") => GO.to_vec(),
            _ => Vec::new(),
        },
        "gem" | "bundle" => match sub {
            Some("install" | "update" | "add") => vec!["rubygems.org"],
            _ => Vec::new(),
        },
        "dotnet" => match sub {
            Some("add" | "restore" | "tool") => vec!["api.nuget.org"],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Flags of fetchers that take a value (so the value is not read as a URL).
pub(crate) fn fetch_value_flags(program: &str) -> &'static [&'static str] {
    match program {
        "curl" => &[
            "-o", "--output", "-H", "--header", "-d", "--data", "--data-raw", "--data-binary", "--data-urlencode", "-X",
            "--request", "-u", "--user", "-A", "--user-agent", "-e", "--referer", "-T", "--upload-file", "-F", "--form",
            "-b", "--cookie", "-c", "--cookie-jar", "-m", "--max-time", "--connect-timeout", "-w", "--write-out", "-x",
            "--proxy", "--retry", "-K", "--config", "--cacert", "--cert", "--key", "-r", "--range", "-Y", "-y", "-z",
            "--output-dir", "--resolve", "--interface", "--limit-rate",
        ],
        "wget" => &["-O", "--output-document", "-o", "--output-file", "-P", "--directory-prefix", "-U", "--user-agent", "--header", "-t", "--tries", "-T", "--timeout", "-e", "--post-data", "-a", "-i", "--input-file"],
        _ => &[
            "-outfile", "-method", "-headers", "-body", "-contenttype", "-credential", "-useragent", "-timeoutsec",
            "-proxy", "-websession", "-sessionvariable", "-infile", "-maximumredirection", "-destination", "-source",
        ],
    }
}
