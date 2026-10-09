//! The real catalog over HTTPS (GET /api/v1/models; free, no key, no inference).

use marlowe_model::Provider;
use marlowe_openrouter::OpenRouterProvider;

#[test]
#[ignore = "network: fetches openrouter.ai/api/v1/models"]
fn live_catalog_parses() {
    let p = OpenRouterProvider::new(None);
    let models = p.list_models().expect("catalog");
    assert!(models.len() > 50, "only {} models", models.len());
    let tools = models.iter().filter(|m| m.supports_tools).count();
    let reasoning = models.iter().filter(|m| m.supports_reasoning).count();
    let priced = models.iter().filter(|m| m.pricing.is_some()).count();
    println!("{} models, {tools} with tools, {reasoning} with reasoning, {priced} priced", models.len());
    for m in models.iter().filter(|m| m.id.starts_with("anthropic/") || m.id.starts_with("openai/gpt-5")).take(8) {
        println!(
            "{} | {} | ctx={:?} max_out={:?} efforts={:?} price={:?}",
            m.id, m.display, m.context_window, m.max_output, m.effort_levels, m.pricing
        );
    }
    assert!(models.iter().all(|m| m.context_window.is_some()));
}
