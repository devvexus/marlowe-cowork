//! `/cost`: tokens and money for a session, from the usage its assistant rows kept.
//!
//! A call's cost is what the provider reported (`Usage::cost_usd`, OpenRouter does), else its
//! usage times the model's price in the library (`Pricing::cost`: input tokens times the input
//! price plus output tokens times the output price, per million), else zero for a local model,
//! else unknown. Calls with an unknown cost leave the total's cost unknown only when none of the
//! others has one.

use std::collections::BTreeMap;

use marlowe_model::{ModelRef, Pricing, ProviderKind, Usage};
use marlowe_proto::{CostReport, ModelCost};

use crate::store::RowOut;

/// What the library knows about a model's price.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Price {
    pub pricing: Option<Pricing>,
    pub local: bool,
}

/// The cost of one call's `usage` under `price`.
pub(crate) fn call_cost(usage: &Usage, price: Price) -> Option<f64> {
    usage
        .cost_usd
        .or_else(|| price.pricing.map(|p| p.cost(usage.input, usage.output)))
        .or_else(|| price.local.then_some(0.0))
}

/// Totals per model and overall for `rows`; `price_of` looks a model up in the library.
pub(crate) fn report(rows: &[RowOut], price_of: &dyn Fn(&ModelRef) -> Price) -> CostReport {
    let mut by_model: BTreeMap<ModelRef, (u32, Usage)> = BTreeMap::new();
    let mut prices: BTreeMap<ModelRef, Price> = BTreeMap::new();
    for meta in rows.iter().filter_map(|r| r.meta.as_ref()) {
        let provider = meta.provider.parse::<ProviderKind>().unwrap_or(ProviderKind::Ollama);
        let model = ModelRef::new(provider, meta.model.clone());
        let price = *prices.entry(model.clone()).or_insert_with(|| price_of(&model));
        let mut usage = meta.usage;
        usage.cost_usd = call_cost(&usage, price);
        let entry = by_model.entry(model).or_insert((0, Usage::default()));
        entry.0 += 1;
        entry.1.add(&usage);
    }
    let mut total = Usage::default();
    let mut calls = 0;
    let by_model: Vec<ModelCost> = by_model
        .into_iter()
        .map(|(model, (n, usage))| {
            total.add(&usage);
            calls += n;
            ModelCost { model, calls: n, usage }
        })
        .collect();
    CostReport { calls, usage: total, by_model }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CallMeta;
    use marlowe_model::{Part, Role};

    fn call(provider: &str, model: &str, input: u64, output: u64, cost: Option<f64>) -> RowOut {
        RowOut {
            idx: 0,
            role: Role::Assistant,
            content: vec![Part::Text { text: "x".into() }],
            at_ms: 0,
            turn: 0,
            step: Some(1),
            meta: Some(CallMeta {
                provider: provider.into(),
                model: model.into(),
                usage: Usage { input, output, cost_usd: cost, ..Usage::default() },
                latency_ms: 1,
            }),
            mem_seq: None,
        }
    }

    #[test]
    fn cost_is_usage_times_the_price_table() {
        let rows = vec![
            call("openrouter", "vendor/big", 1_000_000, 500_000, None),
            call("openrouter", "vendor/big", 2_000_000, 0, None),
            call("ollama", "dusk", 5_000, 700, None),
        ];
        let price = |m: &ModelRef| match m.model_id.as_str() {
            "vendor/big" => Price { pricing: Some(Pricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }), local: false },
            _ => Price { pricing: None, local: true },
        };
        let r = report(&rows, &price);
        assert_eq!(r.calls, 3);
        assert_eq!((r.usage.input, r.usage.output), (3_005_000, 500_700));
        let big = r.by_model.iter().find(|m| m.model.model_id == "vendor/big").unwrap();
        assert_eq!(big.calls, 2);
        // 3M input at $3/M + 0.5M output at $15/M.
        assert!((big.usage.cost_usd.unwrap() - 16.5).abs() < 1e-9);
        let local = r.by_model.iter().find(|m| m.model.model_id == "dusk").unwrap();
        assert_eq!(local.usage.cost_usd, Some(0.0), "a local model is free");
        assert!((r.usage.cost_usd.unwrap() - 16.5).abs() < 1e-9);
    }

    #[test]
    fn a_provider_reported_cost_beats_the_table_and_an_unpriced_remote_model_is_unknown() {
        let rows = vec![
            call("openrouter", "vendor/big", 1_000_000, 0, Some(0.42)),
            call("openrouter", "mystery", 10, 10, None),
        ];
        let price = |m: &ModelRef| {
            if m.model_id == "vendor/big" {
                Price { pricing: Some(Pricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }), local: false }
            } else {
                Price::default()
            }
        };
        let r = report(&rows, &price);
        let big = r.by_model.iter().find(|m| m.model.model_id == "vendor/big").unwrap();
        assert_eq!(big.usage.cost_usd, Some(0.42));
        let mystery = r.by_model.iter().find(|m| m.model.model_id == "mystery").unwrap();
        assert_eq!(mystery.usage.cost_usd, None);
        assert_eq!(r.usage.cost_usd, Some(0.42));
        assert_eq!(report(&[], &price).calls, 0);
    }
}
