//! `DockerExec`: the `Workspace` port inside a container (`docker exec`). Track K.
//!
//! Deferred: Matthew approved no Docker or WSL for M4.6 (design 9.0), so the in-container tb/vmini
//! arms are unscored. This is the port's shape with every call refused as unsupported, so the eval
//! harness can name it today and gain the real implementation without an API change. When it
//! lands, the container runtime is launched through `crate::child` like every other child.

use std::io;
use std::path::Path;

use super::{ExecHandle, ExecOutput, ExecRequest, Meta, WalkEntry, WalkOpts, Workspace};

/// The workspace inside container `container`. Nothing here talks to Docker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerExec {
    pub container: String,
}

impl DockerExec {
    pub fn new(container: impl Into<String>) -> Self {
        Self { container: container.into() }
    }

    fn unsupported<T>(&self, what: &str) -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("docker workspaces are not supported ({what} in container {}); Docker is not part of this build", self.container),
        ))
    }
}

impl Workspace for DockerExec {
    fn kind(&self) -> String {
        format!("docker:{}", self.container)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.unsupported(&format!("read {}", path.display()))
    }

    fn write(&self, path: &Path, _bytes: &[u8]) -> io::Result<()> {
        self.unsupported(&format!("write {}", path.display()))
    }

    fn rename(&self, from: &Path, _to: &Path) -> io::Result<()> {
        self.unsupported(&format!("rename {}", from.display()))
    }

    fn walk(&self, dir: &Path, _opts: &WalkOpts) -> io::Result<Vec<WalkEntry>> {
        self.unsupported(&format!("walk {}", dir.display()))
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.unsupported(&format!("stat {}", path.display()))
    }

    fn exec(&self, req: &ExecRequest, _cancelled: &dyn Fn() -> bool) -> io::Result<ExecOutput> {
        self.unsupported(&format!("run {}", req.program))
    }

    fn spawn(&self, req: &ExecRequest) -> io::Result<Box<dyn ExecHandle>> {
        self.unsupported(&format!("spawn {}", req.program))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_call_says_unsupported_and_names_the_container() {
        let d = DockerExec::new("c1");
        assert_eq!(d.kind(), "docker:c1");
        let e = d.read(Path::new("/w/a.txt")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(e.to_string().contains("container c1"), "{e}");
        assert_eq!(d.write(Path::new("/w/a"), b"x").unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(d.rename(Path::new("/a"), Path::new("/b")).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(d.walk(Path::new("/"), &WalkOpts::default()).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(d.metadata(Path::new("/")).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(d.exec(&ExecRequest::new("ls", ["/"]), &|| false).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(d.spawn(&ExecRequest::new("ls", ["/"])).err().unwrap().kind(), io::ErrorKind::Unsupported);
    }
}
