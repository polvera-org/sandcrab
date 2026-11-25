//! Token usage totals and a rough USD cost estimate (Anthropic list prices).

use std::collections::BTreeMap;

use crate::llm::Usage;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// 5-minute cache writes.
    pub cache_write: f64,
    pub cache_read: f64,
}

/// List prices (first-party API, as of late 2026). `None` for unknown models.
#[must_use]
pub fn price_for(model: &str) -> Option<Price> {
    let p = |input: f64, output: f64, cache_read: f64| Price {
        input,
        output,
        cache_write: input * 1.25,
        cache_read,
    };
    Some(match model {
        m if m.starts_with("claude-sonnet-5-5") => p(2.0, 10.0, 0.10),
        m if m.starts_with("claude-sonnet-5") => p(2.0, 10.0, 0.20),
        m if m.starts_with("claude-sonnet-4") => p(3.0, 15.0, 0.30),
        m if m.starts_with("claude-opus-5-5") => p(4.0, 20.0, 0.20),
        m if m.starts_with("claude-opus-5") || m.starts_with("claude-opus-4") => p(5.0, 25.0, 0.50),
        m if m.starts_with("claude-haiku-5-5") => p(0.10, 0.50, 0.01),
        m if m.starts_with("claude-haiku-4-5") => p(1.0, 5.0, 0.10),
        m if m.starts_with("claude-fable-5-2") || m.starts_with("claude-mythos-5-2") => {
            p(8.0, 40.0, 0.20)
        }
        m if m.starts_with("claude-fable-5") || m.starts_with("claude-mythos-5") => {
            p(10.0, 50.0, 1.0)
        }
        _ => return None,
    })
}

#[must_use]
pub fn cost(model: &str, u: &Usage) -> Option<f64> {
    let p = price_for(model)?;
    let m = 1_000_000.0;
    #[allow(clippy::cast_precision_loss)]
    Some(
        (u.input_tokens as f64 * p.input
            + u.output_tokens as f64 * p.output
            + u.cache_creation_input_tokens as f64 * p.cache_write
            + u.cache_read_input_tokens as f64 * p.cache_read)
            / m,
    )
}

/// Usage per served model (a refusal fallback can switch models mid-session).
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub by_model: BTreeMap<String, Usage>,
    pub requests: u64,
}

impl UsageTotals {
    pub fn add(&mut self, model: &str, u: &Usage) {
        self.requests += 1;
        self.by_model.entry(model.to_owned()).or_default().add(u);
    }

    #[must_use]
    pub fn total(&self) -> Usage {
        let mut t = Usage::default();
        for u in self.by_model.values() {
            t.add(u);
        }
        t
    }

    /// `(usd, every model priced)`.
    #[must_use]
    pub fn cost(&self) -> (f64, bool) {
        let mut usd = 0.0;
        let mut known = true;
        for (m, u) in &self.by_model {
            match cost(m, u) {
                Some(c) => usd += c,
                None => known = false,
            }
        }
        (usd, known)
    }

    /// `12 requests · in 1.2k (+340k cached, 20k written) · out 8.1k · ≈ $0.42`.
    #[must_use]
    pub fn summary(&self) -> String {
        let t = self.total();
        let (usd, known) = self.cost();
        let cost = if self.by_model.is_empty() {
            "$0.00".to_owned()
        } else if known {
            format!("≈ ${usd:.2}")
        } else {
            format!("≈ ${usd:.2} (some models unpriced)")
        };
        format!(
            "{} requests · in {} (+{} cache read, {} cache write) · out {} · {cost}",
            self.requests,
            human(t.input_tokens),
            human(t.cache_read_input_tokens),
            human(t.cache_creation_input_tokens),
            human(t.output_tokens),
        )
    }
}

#[must_use]
pub fn human(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", f / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonnet_cost() {
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 10_000_000,
        };
        // 2 + 1 + 2.5 + 1.0
        let c = cost("claude-sonnet-5-5", &u).unwrap();
        assert!((c - 6.5).abs() < 1e-9, "{c}");
        assert!(cost("fake-model", &u).is_none());
    }

    #[test]
    fn totals() {
        let mut t = UsageTotals::default();
        let u = Usage {
            input_tokens: 1500,
            output_tokens: 10,
            ..Usage::default()
        };
        t.add("claude-sonnet-5-5", &u);
        t.add("claude-sonnet-5-5", &u);
        assert_eq!(t.total().input_tokens, 3000);
        assert_eq!(t.requests, 2);
        assert!(t.cost().1);
        assert!(t.summary().contains("in 3.0k"), "{}", t.summary());
        t.add("mystery", &u);
        assert!(!t.cost().1);
        assert!(t.summary().contains("unpriced"));
        assert_eq!(human(999), "999");
        assert_eq!(human(2_500_000), "2.5M");
    }
}
