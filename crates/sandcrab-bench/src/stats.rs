//! Descriptive statistics per bench-spec §4: min, p50/p95/p99 (linear interpolation, R-7),
//! max, mean, sample stddev, and a seeded bootstrap 95% CI for the median.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::rng::Rng;

/// Bootstrap resamples for the p50 CI (spec: 10k).
pub const BOOTSTRAP_RESAMPLES: usize = 10_000;

/// p99 is only indicative below this many samples (spec §4).
pub const P99_MIN_N: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
    /// Sample standard deviation (n − 1); 0 for a single sample.
    pub stddev: f64,
    /// Bootstrap 95% CI of the median.
    pub p50_ci95: [f64; 2],
    /// `true` when n < 100: p99 is indicative only.
    pub p99_indicative: bool,
}

/// Quantile `q ∈ [0, 1]` of an ascending-sorted, non-empty slice (R-7 / numpy default).
#[must_use]
pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile of empty slice");
    let q = q.clamp(0.0, 1.0);
    let h = (sorted.len() - 1) as f64 * q;
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    sorted[lo] + (h - lo as f64) * (sorted[hi] - sorted[lo])
}

#[must_use]
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample standard deviation (Bessel-corrected).
#[must_use]
pub fn stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    var.sqrt()
}

fn sorted(xs: &[f64]) -> Vec<f64> {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v
}

/// Percentile-bootstrap 95% CI of the median.
#[must_use]
pub fn bootstrap_median_ci(xs: &[f64], resamples: usize, seed: u64) -> [f64; 2] {
    if xs.is_empty() {
        return [0.0, 0.0];
    }
    if xs.len() == 1 || resamples == 0 {
        return [xs[0], xs[0]];
    }
    let mut rng = Rng::new(seed);
    let mut buf = vec![0.0; xs.len()];
    let mut medians = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        for slot in &mut buf {
            *slot = xs[rng.below(xs.len())];
        }
        buf.sort_by(f64::total_cmp);
        medians.push(percentile(&buf, 0.5));
    }
    medians.sort_by(f64::total_cmp);
    [percentile(&medians, 0.025), percentile(&medians, 0.975)]
}

/// Full stats block, or `None` for an empty sample set.
#[must_use]
pub fn compute(xs: &[f64], seed: u64) -> Option<Stats> {
    if xs.is_empty() {
        return None;
    }
    let s = sorted(xs);
    Some(Stats {
        n: s.len(),
        min: s[0],
        p50: percentile(&s, 0.50),
        p95: percentile(&s, 0.95),
        p99: percentile(&s, 0.99),
        max: s[s.len() - 1],
        mean: mean(&s),
        stddev: stddev(&s),
        p50_ci95: bootstrap_median_ci(&s, BOOTSTRAP_RESAMPLES, seed),
        p99_indicative: s.len() < P99_MIN_N,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn percentiles_r7() {
        let xs: Vec<f64> = (1..=10).map(f64::from).collect();
        assert!(close(percentile(&xs, 0.0), 1.0));
        assert!(close(percentile(&xs, 1.0), 10.0));
        assert!(close(percentile(&xs, 0.5), 5.5));
        // numpy.percentile([1..10], 95) == 9.55, 99 → 9.91
        assert!(close(percentile(&xs, 0.95), 9.55));
        assert!(close(percentile(&xs, 0.99), 9.91));
        assert!(close(percentile(&[7.0], 0.99), 7.0));
    }

    #[test]
    fn mean_and_stddev() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        assert!(close(mean(&xs), 5.0));
        // sample stddev = sqrt(32/7)
        assert!(close(stddev(&xs), (32.0f64 / 7.0).sqrt()));
        assert!(close(stddev(&[3.0]), 0.0));
        assert!(close(mean(&[]), 0.0));
    }

    #[test]
    fn compute_unsorted_input() {
        let st = compute(&[5.0, 1.0, 3.0, 2.0, 4.0], 1).unwrap();
        assert_eq!(st.n, 5);
        assert!(close(st.min, 1.0) && close(st.max, 5.0) && close(st.p50, 3.0));
        assert!(st.p99_indicative);
        assert!(st.p50_ci95[0] <= st.p50 && st.p50 <= st.p50_ci95[1]);
        assert!(compute(&[], 1).is_none());
    }

    #[test]
    fn bootstrap_is_seeded_and_brackets_median() {
        let xs: Vec<f64> = (0..200).map(|i| f64::from(i % 37) * 1.5).collect();
        let a = bootstrap_median_ci(&xs, 2_000, 9);
        let b = bootstrap_median_ci(&xs, 2_000, 9);
        assert_eq!(a, b);
        let med = percentile(&sorted(&xs), 0.5);
        assert!(a[0] <= med && med <= a[1], "{a:?} vs {med}");
        assert!(a[0] < a[1]);
        assert_eq!(bootstrap_median_ci(&[4.0], 100, 1), [4.0, 4.0]);
    }
}
