//! **Two `edit`s to one file in one batch used to race, and the loser was silent.**
//!
//! `execute_batch` runs a turn's tool calls concurrently, and `write`/`edit` each do their own
//! read-modify-write through their own cloned handle. Both read the original; the second's
//! `set_len(0)` + `write_all` overwrote the first's result. **Both reported success**, with
//! truthful-looking `+n −m` lines, and no consumer could tell — the same shape as the empty
//! `replacing` prepend, which this executor already calls the worst outcome available to it.
//!
//! It has been reachable since batching landed. What made it urgent is the non-unique `replacing`
//! refusal: the remedy that refusal names is *"edit each site in a separate call"*, and separate
//! calls in one turn are exactly one batch. Closing one hazard by routing the model into another
//! is not a fix.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{BatchItem, ToolHost};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{
    Adjudication, Adjudicator, Args, EgressPolicy, Request, TaintSet, Tier,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, ToolRegistry, BUILTIN_TOOLS};

/// Enough calls that a lost write is a certainty rather than a coin flip. A two-item batch races
/// only when the interleaving happens to be adversarial; at this width, on this machine's worker
/// count, the unguarded version loses writes on every run.
const SITES: usize = 32;

struct Fixture {
    root: PathBuf,
    registry: ToolRegistry,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir()
            .join(format!("marlowe-concurrent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root, registry: builtin_registry().unwrap() }
    }

    fn adjudicate(&self, tool: &str, args: &Args) -> Adjudication {
        let exposed =
            ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()).unwrap();
        let mut taint = TaintSet::new();
        for (name, _) in args.iter() {
            taint.insert(name.clone(), TrustClass::UserAsserted);
        }
        let mut adj = Adjudicator::new(WorkspaceScope::new().expect("verified platform"));
        adj.adjudicate(Request {
            manifest: self.registry.manifest(&ToolId::new(tool)).unwrap(),
            args,
            taint: &taint,
            exposed: &exposed,
            egress: &EgressPolicy::DenyAll,
            workspace: &self.root,
            tier: Tier::Silent,
            novelty: None,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// **The property: every edit in the batch lands.** Asserted on the FILE, not on the outcomes —
/// the outcomes were all successes on the broken build, which is exactly why the defect was
/// invisible.
#[test]
fn every_edit_in_one_batch_reaches_the_file() {
    let fx = Fixture::new("edits");
    let seed: String = (0..SITES).map(|i| format!("SLOT{i:02}\n")).collect();
    fs::write(fx.root.join("many.txt"), &seed).unwrap();

    let tool = ToolId::new("edit");
    let args: Vec<Args> = (0..SITES)
        .map(|i| {
            Args::new()
                .text("path", "many.txt")
                .text("replacing", &format!("SLOT{i:02}\n"))
                .text("content", &format!("DONE{i:02}\n"))
        })
        .collect();
    let adjudications: Vec<Adjudication> =
        args.iter().map(|a| fx.adjudicate("edit", a)).collect();
    let items: Vec<BatchItem<'_>> = args
        .iter()
        .zip(adjudications.iter())
        .map(|(a, adj)| BatchItem { tool: &tool, args: a, adjudication: adj })
        .collect();

    let mut host =
        FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), &fx.root);
    let outcomes = host.execute_batch(&items);

    assert_eq!(outcomes.len(), SITES);
    for (i, o) in outcomes.iter().enumerate() {
        assert!(!o.failed, "call {i} failed: {:?}", o.summary);
    }

    // The file is the only witness that can tell a landed edit from a lost one.
    let after = fs::read_to_string(fx.root.join("many.txt")).unwrap();
    let lost: Vec<usize> = (0..SITES).filter(|i| !after.contains(&format!("DONE{i:02}"))).collect();
    assert!(
        lost.is_empty(),
        "{} of {SITES} edits reported success and are not in the file: {lost:?}",
        lost.len()
    );
    assert!(!after.contains("SLOT"), "and nothing was left unedited: {after}");
}

/// The control for the control: a batch of ONE still works, so a lock that deadlocked or a batch
/// path that stopped executing would not pass the test above by doing nothing.
#[test]
fn a_batch_of_one_edit_still_edits() {
    let fx = Fixture::new("one");
    fs::write(fx.root.join("f.txt"), "before\n").unwrap();
    let tool = ToolId::new("edit");
    let a = Args::new().text("path", "f.txt").text("replacing", "before").text("content", "after");
    let adj = fx.adjudicate("edit", &a);
    let mut host =
        FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), &fx.root);
    let out = host.execute_batch(&[BatchItem { tool: &tool, args: &a, adjudication: &adj }]);
    assert!(!out[0].failed, "{:?}", out[0].summary);
    assert_eq!(fs::read_to_string(fx.root.join("f.txt")).unwrap(), "after\n");
}
