//! Per-turn limits.

use marlowe_model::Usage;

/// Limits for one turn. `None` means unlimited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub max_steps: u32,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_steps: crate::agent::MAX_STEPS, max_tokens: None, max_cost_usd: None }
    }
}

/// What a turn has used so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spent {
    pub steps: u32,
    pub usage: Usage,
}

impl Spent {
    /// Records one finished model call.
    pub fn record(&mut self, usage: &Usage) {
        self.steps += 1;
        self.usage.add(usage);
    }
}

impl Budget {
    /// The first exhausted dimension, if any: `"steps"`, `"tokens"` or `"cost"`.
    pub fn exhausted(&self, spent: &Spent) -> Option<&'static str> {
        if spent.steps >= self.max_steps {
            return Some("steps");
        }
        if self.max_tokens.is_some_and(|m| spent.usage.total() >= m) {
            return Some("tokens");
        }
        if let (Some(m), Some(c)) = (self.max_cost_usd, spent.usage.cost_usd) {
            if c >= m {
                return Some("cost");
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_dimension_exhausts() {
        let b = Budget { max_steps: 2, max_tokens: Some(100), max_cost_usd: Some(1.0) };
        let mut s = Spent::default();
        assert_eq!(b.exhausted(&s), None);
        s.record(&Usage { input: 60, output: 50, ..Default::default() });
        assert_eq!(b.exhausted(&s), Some("tokens"));
        let mut s = Spent::default();
        s.record(&Usage { cost_usd: Some(2.0), ..Default::default() });
        assert_eq!(b.exhausted(&s), Some("cost"));
        s.record(&Usage::default());
        assert_eq!(b.exhausted(&s), Some("steps"));
    }
}
