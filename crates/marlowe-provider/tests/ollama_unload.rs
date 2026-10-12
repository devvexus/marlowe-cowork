//! Closing the window frees the GPU first (track N): the engine is told to unload a model with
//! `keep_alive: 0`, against a scripted loopback Ollama. Nothing is loaded for real.

use marlowe_hw::ModelSource;
use marlowe_provider::OllamaHw;
use serde_json::json;

mod common;
use common::{serve, Reply};

#[test]
fn unloading_a_model_posts_keep_alive_zero_and_a_loaded_one_is_listed() {
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/ps" => Reply::Json(200, json!({"models": [{"name": "marlowe-dusk:27b-super", "size": 1, "size_vram": 1}, {"name": "someone-elses:7b"}]}).to_string()),
        "/api/generate" => Reply::Json(200, json!({"model": "marlowe-dusk:27b-super", "done": true, "done_reason": "unload"}).to_string()),
        _ => Reply::Json(404, "{}".into()),
    }));
    let hw = OllamaHw::with_paths(endpoint, std::env::temp_dir().join("marlowe-no-store"), None);
    assert_eq!(hw.loaded_models(), ["marlowe-dusk:27b-super", "someone-elses:7b"]);
    hw.unload("marlowe-dusk:27b-super").expect("unloaded");
    let seen = seen.lock().unwrap().clone();
    let post = seen.iter().find(|s| s.path == "/api/generate").expect("a generate request");
    assert_eq!(post.body, json!({"model": "marlowe-dusk:27b-super", "keep_alive": 0}));
}

#[test]
fn an_engine_that_refuses_is_an_error_the_close_can_report() {
    let (endpoint, _) = serve(Box::new(|_| Reply::Json(500, json!({"error": "boom"}).to_string())));
    let hw = OllamaHw::with_paths(endpoint, std::env::temp_dir().join("marlowe-no-store"), None);
    let err = hw.unload("m:1").unwrap_err();
    assert!(err.contains("could not unload m:1"), "{err}");
}
