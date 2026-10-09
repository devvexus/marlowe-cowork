#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use marlowe_agent::{ToolCtx, ToolHost, ToolOutcome};
use marlowe_exec::BuiltinTools;
use marlowe_model::{CancelToken, ToolCall};
use marlowe_proto::SessionId;
use serde_json::Value;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A throwaway workspace directory.
pub struct Ws {
    pub root: PathBuf,
}

impl Ws {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "marlowe-exec-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Canonical, so comparisons against paths a child reports are stable.
        Self { root: root.canonicalize().unwrap() }
    }

    pub fn put(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn get(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn tools(&self) -> BuiltinTools {
        BuiltinTools::new(Some(self.root.clone()))
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn ctx(root: Option<&Path>) -> ToolCtx {
    ToolCtx {
        session: SessionId::from("test"),
        turn: 1,
        workspace: root.map(Path::to_path_buf),
        cancel: CancelToken::new(),
    }
}

pub fn req(name: &str, args: Value) -> ToolCall {
    ToolCall { id: "c1".into(), name: name.into(), args }
}

pub fn call(tools: &BuiltinTools, name: &str, args: Value) -> ToolOutcome {
    tools.call(&req(name, args), &ctx(tools.workspace()))
}
