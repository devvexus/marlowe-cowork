//! Seam tests: the facade's shape and the trivial helpers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use marlowe_contract::TrustClass;

use crate::*;

#[test]
fn the_handle_is_cloneable_and_crosses_threads() {
    fn assert_handle<T: Clone + Send + Sync + 'static>() {}
    assert_handle::<Mind>();
}

#[test]
fn trust_is_derived_from_provenance() {
    let user = Provenance::User { session: Some("s1".into()) };
    assert_eq!(user.trust_class(), TrustClass::UserAsserted);
    assert_eq!(user.session(), Some("s1"));
    let agent = Provenance::Agent { session: "s1".into(), turn: 2, tainted: false };
    assert_eq!(agent.trust_class(), TrustClass::AgentInferred);
    let tainted = Provenance::Agent { session: "s1".into(), turn: 2, tainted: true };
    assert_eq!(tainted.trust_class(), TrustClass::UntrustedContent, "a tainted run writes untrusted");
    assert_eq!(Provenance::Harness { what: "exit code".into() }.trust_class(), TrustClass::AgentObserved);
    assert_eq!(Provenance::External { source: "web".into() }.trust_class(), TrustClass::UntrustedContent);
    for t in [TrustClass::UntrustedContent, TrustClass::AgentInferred, TrustClass::AgentObserved, TrustClass::UserAsserted] {
        assert_eq!(wire_trust(t) as u8, t as u8, "wire trust keeps the contract order");
    }
}

#[test]
fn kinds_map_to_their_wire_tags() {
    assert_eq!(Kind::Decision { reason: "cheaper".into() }.tag(), MemoryKind::Decision);
    assert_eq!(Kind::Episode.tag(), MemoryKind::Episode);
    assert_eq!(Kind::Procedure.tag(), MemoryKind::Procedure);
}

#[test]
fn models_dir_candidates_are_env_then_dev_tree_then_home() {
    let home = Path::new("H");
    let exe = Path::new("R").join("target").join("debug").join("marlowe.exe");
    let c = models_dir_candidates(Some(OsString::from("E")), Some(&exe), home);
    assert_eq!(c, vec![PathBuf::from("E"), PathBuf::from("R").join("models"), home.join("models")]);
    let c = models_dir_candidates(Some(OsString::new()), None, home);
    assert_eq!(c, vec![home.join("models")], "an empty env var is ignored");
}

#[test]
fn defaults_match_the_design() {
    let cfg = MindConfig::default();
    assert_eq!(cfg.model_idle.as_secs(), 600);
    assert!(cfg.models_dir.is_none());
    assert_eq!(MAX_INJECT_TOKENS, 7_000);
    assert_eq!(profile_dir(Path::new("H")), Path::new("H").join("profile"));
}
