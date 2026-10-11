//! What every harness call needs of its host: the workspace port, the roots, the extra scrub
//! and environment of a crew sandbox, and the cancel token. Children are only ever built from
//! here, so `marlowe_os::child` (credential scrub, Job Object) is the single door.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use marlowe_model::CancelToken;
use marlowe_os::workspace::{ExecRequest, Workspace};
use marlowe_os::ChildSpec;

use crate::cmd::{resolve_program, valid_env_name, Cmd};

/// Output bytes kept per stream of a project command.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// The host's side of a call.
pub struct Env<'a> {
    pub ws: &'a dyn Workspace,
    /// The workspace root (project root).
    pub root: &'a Path,
    /// A crew sandbox: every `cwd` must stay inside it.
    pub pin: Option<&'a Path>,
    /// Further variables removed from every child (beyond `marlowe_os::SCRUBBED_ENV`).
    pub scrub: &'a [String],
    /// Variables set on every child (the crew's no-push `GIT_CONFIG_*`).
    pub child_env: &'a [(String, String)],
    pub cancel: &'a CancelToken,
}

impl Env<'_> {
    /// `rel` (a directory, relative to the root or absolute) as an absolute path inside the
    /// root (and the sandbox pin).
    pub fn cwd(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim();
        let abs = if rel.is_empty() || rel == "." { self.root.to_path_buf() } else { resolve(self.root, rel) };
        let boundary = self.pin.unwrap_or(self.root);
        if !inside(boundary, &abs) {
            return Err(format!("cwd {} is outside the workspace {}", abs.display(), boundary.display()));
        }
        if !self.ws.metadata(&abs).map(|m| m.is_dir).unwrap_or(false) {
            return Err(format!("cwd {} is not a directory", abs.display()));
        }
        Ok(abs)
    }

    /// An exec request for `cmd` in `cwd`: the program resolved on `PATH`, the scrubbed
    /// environment plus this launch's `env_allow` and `env`.
    pub fn request(
        &self,
        cmd: &Cmd,
        cwd: &Path,
        timeout: Duration,
        env_allow: &[String],
        env: &[(String, String)],
    ) -> Result<ExecRequest, String> {
        if let Some(bad) = env_allow.iter().find(|n| !valid_env_name(n)) {
            return Err(format!("env_allow `{bad}` is not a variable name"));
        }
        let program = resolve_program(cmd.program(), Some(cwd))
            .ok_or_else(|| format!("`{}` is not on PATH (install it, or override the command in .marlowe/project.toml)", cmd.program()))?;
        let mut spec = ChildSpec::new().with_cwd(cwd).with_env_allow(env_allow.iter().cloned());
        spec = spec.with_env_remove(self.scrub.iter().filter(|n| !env_allow.iter().any(|a| a.eq_ignore_ascii_case(n))).cloned());
        spec = spec.with_env(self.child_env.iter().cloned()).with_env(env.iter().cloned());
        let mut req = ExecRequest::new(program.to_string_lossy().into_owned(), cmd.args().iter().cloned());
        req.spec = spec;
        req.timeout = timeout;
        req.max_output_bytes = MAX_OUTPUT_BYTES;
        Ok(req)
    }
}

/// Lexically normalised `raw` below `base` (absolute `raw` is taken as is).
pub fn resolve(base: &Path, raw: &str) -> PathBuf {
    let raw = Path::new(raw.trim());
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { base.join(raw) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The deepest existing ancestor canonicalised, so a link or `..` cannot hide an escape.
fn real_path(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            out.extend(tail.iter().rev());
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Whether `abs` is `root` or lies beneath it, after resolving links.
pub fn inside(root: &Path, abs: &Path) -> bool {
    real_path(abs).starts_with(real_path(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_stays_inside_the_root_and_the_pin() {
        let root = std::env::temp_dir().join(format!("marlowe-verify-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let cancel = CancelToken::new();
        let ws = marlowe_os::workspace::LocalFs;
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        assert_eq!(env.cwd(".").unwrap(), root);
        assert_eq!(env.cwd("sub").unwrap(), root.join("sub"));
        assert!(env.cwd("..").unwrap_err().contains("outside the workspace"));
        assert!(env.cwd("sub/../../x").is_err());
        assert!(env.cwd("missing").unwrap_err().contains("not a directory"));
        let pin = root.join("sub");
        let env = Env { pin: Some(&pin), ..env };
        assert!(env.cwd(".").is_err(), "the root is outside a sandbox pinned to sub");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_request_scrubs_keys_and_refuses_an_unknown_program() {
        let cancel = CancelToken::new();
        let ws = marlowe_os::workspace::LocalFs;
        let extra = vec!["MY_SECRET".to_string(), "HF_TOKEN".to_string()];
        let env = Env { ws: &ws, root: Path::new("."), pin: None, scrub: &extra, child_env: &[], cancel: &cancel };
        let err = env.request(&Cmd::new(&["definitely-not-a-program-xyz"]), Path::new("."), Duration::from_secs(1), &[], &[]).unwrap_err();
        assert!(err.contains("not on PATH"), "{err}");
        let me = std::env::current_exe().unwrap();
        let cmd = Cmd { argv: vec![me.to_string_lossy().into_owned(), "x".into()] };
        let req = env.request(&cmd, Path::new("."), Duration::from_secs(1), &["HF_TOKEN".into()], &[]).unwrap();
        assert!(req.spec.env_allow.contains(&"HF_TOKEN".to_string()));
        assert!(req.spec.env_remove.contains(&"MY_SECRET".to_string()));
        assert!(!req.spec.env_remove.contains(&"HF_TOKEN".to_string()), "an allowed name is not removed");
        assert!(env.request(&cmd, Path::new("."), Duration::from_secs(1), &["bad name".into()], &[]).is_err());
    }
}
