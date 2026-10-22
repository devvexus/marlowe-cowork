//! Seam tests: the facade's shape and the trivial helpers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use marlowe_contract::TrustClass;

use crate::paths::{install_models, remember_models_dir_in};
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
fn models_dir_candidates_are_env_pointer_exe_dev_fixed_then_home() {
    let home = Path::new("H");
    let fixed = std::env::temp_dir().join(format!("mind-fixed-none-{}", std::process::id()));
    let exe = Path::new("R").join("target").join("debug").join("marlowe.exe");
    let c = models_dir_candidates(Some(OsString::from("E")), Some(&exe), Some(&fixed), home);
    assert_eq!(
        c,
        vec![
            PathBuf::from("E"),
            exe.parent().unwrap().join("models"),
            PathBuf::from("R").join("models"),
            fixed.join("models"),
            home.join("models"),
        ]
    );
    let c = models_dir_candidates(Some(OsString::new()), None, None, home);
    assert_eq!(c, vec![home.join("models")], "an empty env var is ignored");
}

#[test]
fn a_remembered_models_dir_is_found_without_the_env_var() {
    let root = std::env::temp_dir().join(format!("mind-remember-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let models = root.join("elsewhere");
    for sub in [EMBEDDER_DIR, RERANKER_DIR] {
        std::fs::create_dir_all(models.join(sub)).unwrap();
    }
    let fixed = root.join("fixed");
    assert!(!remember_models_dir_in(&root.join("not-models"), &fixed), "a dir without the models is not remembered");
    assert!(remember_models_dir_in(&models, &fixed));
    assert!(!remember_models_dir_in(&models, &fixed), "an unchanged pointer is not rewritten");
    let c = models_dir_candidates(None, None, Some(&fixed), Path::new("H"));
    assert_eq!(c[0], models, "the pointer leads when the env var is absent");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn install_copies_the_model_dirs_to_the_fixed_location() {
    let root = std::env::temp_dir().join(format!("mind-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    for sub in [EMBEDDER_DIR, RERANKER_DIR] {
        std::fs::create_dir_all(src.join(sub)).unwrap();
        std::fs::write(src.join(sub).join("model.onnx"), b"x").unwrap();
    }
    let fixed = root.join("fixed");
    let dest = install_models(&src, &fixed).unwrap();
    assert!(has_models(&dest) && dest.join(EMBEDDER_DIR).join("model.onnx").is_file());
    assert!(install_models(&root.join("nope"), &fixed).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn defaults_match_the_design() {
    let cfg = MindConfig::default();
    assert_eq!(cfg.model_idle.as_secs(), 600);
    assert!(cfg.models_dir.is_none());
    assert_eq!(MAX_INJECT_TOKENS, 7_000);
    assert_eq!(profile_dir(Path::new("H")), Path::new("H").join("profile"));
}
