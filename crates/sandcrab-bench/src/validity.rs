//! Run validity, per benchmark tier (`docs/bench/README.md`, "Benchmark tiers").
//!
//! Everything here is derived from what a results file records (config, start load, drift
//! check, cells), so `sandcrab-bench recheck` can recompute it for any committed file without
//! touching its measured data.
//!
//! - `valid` / `invalid_reasons`: the spec rules, i.e. valid for **Full** (publishable). The
//!   meaning is unchanged from the first results files.
//! - `tier` / `valid_for_tier` / `tier_invalid_reasons`: the same run judged against the rules
//!   of the tier it was run at. Standard differs from Full only in its N floor
//!   (`resource_runs` ≥ 10 instead of 30); the load and drift rules are the Full ones, because
//!   they protect the latency cells, which Standard measures at the full N=30. Smoke is never
//!   valid: it checks correctness only.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::results::{Cell, DriftCheck, RunResult};

/// Spec minimum for measured iterations per cell (latency cells, and resource cells in Full).
pub const MIN_N: usize = 30;
/// Standard tier: minimum iterations per resource cell.
pub const STANDARD_RESOURCE_MIN_N: usize = 10;

/// Benchmark tier, derived from the run's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// `--quick`, or anything below Standard's N: correctness only, never published.
    Smoke,
    /// N ≥ 30 per latency cell, 10 ≤ `resource_runs` < 30.
    Standard,
    /// N ≥ 30 everywhere: the canonical, publishable run.
    Full,
}

impl Tier {
    /// The tier a run with these arguments is at.
    #[must_use]
    pub fn derive(quick: bool, runs: usize, resource_runs: usize) -> Self {
        if quick || runs < MIN_N || resource_runs < STANDARD_RESOURCE_MIN_N {
            Self::Smoke
        } else if resource_runs < MIN_N {
            Self::Standard
        } else {
            Self::Full
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "smoke" => Ok(Self::Smoke),
            "standard" => Ok(Self::Standard),
            "full" => Ok(Self::Full),
            o => Err(format!("unknown tier `{o}` (smoke | standard | full)")),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }
}

/// What validity is computed from: exactly what a results file records.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub quick: bool,
    pub runs: usize,
    pub resource_runs: usize,
    /// 1-minute load at the start of the run.
    pub load_start_1m: f64,
    pub max_load: f64,
    pub drift: Option<&'a DriftCheck>,
    pub cells: &'a [Cell],
}

impl<'a> Inputs<'a> {
    #[must_use]
    pub fn of(r: &'a RunResult) -> Self {
        Self {
            quick: r.config.quick,
            runs: r.config.runs,
            resource_runs: r.config.resource_runs,
            load_start_1m: r.noise.load_start[0],
            max_load: r.noise.max_load,
            drift: r.drift_check.as_ref(),
            cells: &r.cells,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub tier: Tier,
    /// Valid for Full (the spec rules): the `valid` field.
    pub valid: bool,
    pub invalid_reasons: Vec<String>,
    pub valid_for_tier: bool,
    pub tier_invalid_reasons: Vec<String>,
}

/// Load, drift and failure-rate reasons: shared by every tier.
fn common_reasons(i: &Inputs<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if i.load_start_1m > i.max_load {
        out.push(format!(
            "host busy: 1-min load {:.2} > {} at start",
            i.load_start_1m, i.max_load
        ));
    }
    if let Some(d) = i.drift
        && d.flagged
    {
        out.push(format!(
            "spawn_ready p50 drifted {:.1}% start→end",
            d.drift_pct
        ));
    }
    for c in i.cells {
        if c.unreliable {
            out.push(format!(
                "cell {} unreliable: {}/{} iterations failed",
                c.key(),
                c.failures.len(),
                c.n
            ));
        }
    }
    out
}

#[must_use]
pub fn assess(i: &Inputs<'_>) -> Assessment {
    let tier = Tier::derive(i.quick, i.runs, i.resource_runs);
    let common = common_reasons(i);

    // Full: the spec rules (wording kept from the first results files).
    let mut full = Vec::new();
    if i.runs < MIN_N || i.resource_runs < MIN_N {
        full.push(format!(
            "n below the spec minimum of {MIN_N} (runs={}, resource_runs={}; --allow-small-n/--quick)",
            i.runs, i.resource_runs
        ));
    }
    full.extend(common.iter().cloned());

    let tier_reasons = match tier {
        Tier::Full => full.clone(),
        Tier::Standard => common,
        Tier::Smoke => {
            let mut v = vec![format!(
                "smoke tier (runs={}, resource_runs={}{}): correctness only, never valid for publishing",
                i.runs,
                i.resource_runs,
                if i.quick { ", --quick" } else { "" }
            )];
            v.extend(common);
            v
        }
    };
    Assessment {
        tier,
        valid: full.is_empty(),
        invalid_reasons: full,
        valid_for_tier: tier_reasons.is_empty(),
        tier_invalid_reasons: tier_reasons,
    }
}

/// Write the assessment into a result (`valid`, `invalid_reasons`, `tier`, `valid_for_tier`,
/// `tier_invalid_reasons`).
pub fn apply(r: &mut RunResult) {
    let a = assess(&Inputs::of(r));
    r.valid = a.valid;
    r.invalid_reasons = a.invalid_reasons;
    r.tier = Some(a.tier);
    r.valid_for_tier = Some(a.valid_for_tier);
    r.tier_invalid_reasons = a.tier_invalid_reasons;
}

/// `text` (a results file as written by the bench) with `tier`, `valid_for_tier` and
/// `tier_invalid_reasons` set from `a`, every other byte unchanged: the fields are spliced in
/// before the top-level `"notes"` (replacing earlier copies), never re-serialized, so measured
/// floats cannot change. Returns the new text and checks it parses back to the same result
/// plus the three fields.
pub fn with_tier_fields(text: &str, a: &Assessment) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let notes = text
        .find("\n  \"notes\": ")
        .context("no top-level \"notes\" field: not a results file written by sandcrab-bench")?;
    let start = text
        .find("\n  \"tier\": ")
        .filter(|&i| i < notes)
        .unwrap_or(notes);
    let reasons = serde_json::to_string_pretty(&a.tier_invalid_reasons)?.replace('\n', "\n  ");
    let fields = format!(
        "\n  \"tier\": \"{}\",\n  \"valid_for_tier\": {},\n  \"tier_invalid_reasons\": {reasons},",
        a.tier.label(),
        a.valid_for_tier
    );
    let out = format!("{}{fields}{}", &text[..start], &text[notes..]);

    let mut want: RunResult = serde_json::from_str(text)?;
    want.tier = Some(a.tier);
    want.valid_for_tier = Some(a.valid_for_tier);
    want.tier_invalid_reasons = a.tier_invalid_reasons.clone();
    let got: RunResult = serde_json::from_str(&out).context("rewritten file does not parse")?;
    anyhow::ensure!(got == want, "rewrite changed more than the tier fields");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(quick: bool, runs: usize, resource_runs: usize, load: f64) -> Inputs<'static> {
        Inputs {
            quick,
            runs,
            resource_runs,
            load_start_1m: load,
            max_load: 0.5,
            drift: None,
            cells: &[],
        }
    }

    #[test]
    fn tiers_from_args() {
        assert_eq!(Tier::derive(true, 30, 30), Tier::Smoke);
        assert_eq!(Tier::derive(false, 3, 1), Tier::Smoke);
        assert_eq!(Tier::derive(false, 30, 9), Tier::Smoke);
        assert_eq!(Tier::derive(false, 30, 10), Tier::Standard);
        assert_eq!(Tier::derive(false, 30, 29), Tier::Standard);
        assert_eq!(Tier::derive(false, 30, 30), Tier::Full);
        assert_eq!(Tier::derive(false, 50, 30), Tier::Full);
        assert_eq!(Tier::parse("standard"), Ok(Tier::Standard));
        assert!(Tier::parse("huge").is_err());
    }

    #[test]
    fn standard_quiet_run_is_valid_for_standard_not_full() {
        let a = assess(&inputs(false, 30, 10, 0.38));
        assert_eq!(a.tier, Tier::Standard);
        assert!(a.valid_for_tier, "{:?}", a.tier_invalid_reasons);
        assert!(!a.valid);
        assert_eq!(a.invalid_reasons.len(), 1);
        assert!(a.invalid_reasons[0].starts_with("n below the spec minimum"));
    }

    #[test]
    fn standard_keeps_full_load_and_drift_rules() {
        let a = assess(&inputs(false, 30, 10, 0.9));
        assert!(!a.valid_for_tier);
        assert!(a.tier_invalid_reasons[0].starts_with("host busy"));
        let d = DriftCheck {
            backend: "docker".into(),
            n: 10,
            start_p50_ns: 100.0,
            end_p50_ns: 120.0,
            drift_pct: 20.0,
            flagged: true,
        };
        let mut i = inputs(false, 30, 10, 0.1);
        i.drift = Some(&d);
        let a = assess(&i);
        assert!(!a.valid_for_tier);
        assert!(a.tier_invalid_reasons[0].contains("drifted 20.0%"));
    }

    #[test]
    fn full_matches_valid() {
        let a = assess(&inputs(false, 30, 30, 0.1));
        assert_eq!(a.tier, Tier::Full);
        assert!(a.valid && a.valid_for_tier);
        let a = assess(&inputs(false, 30, 30, 4.37));
        assert!(!a.valid && !a.valid_for_tier);
        assert_eq!(a.invalid_reasons, a.tier_invalid_reasons);
    }

    #[test]
    fn committed_standard_run_rechecks_and_splices() {
        // kvm-dedicated libkrun, Standard tier: load 0.38 at start, drift +0.6%, 0 failures.
        let text = include_str!("../../../docs/bench/results/20261125T014108Z-kvm-dedicated.json");
        let r: RunResult = serde_json::from_str(text).unwrap();
        let a = assess(&Inputs::of(&r));
        assert_eq!(a.tier, Tier::Standard);
        assert!(a.valid_for_tier, "{:?}", a.tier_invalid_reasons);
        // The recorded Full verdict is reproduced exactly.
        assert_eq!((a.valid, &a.invalid_reasons), (r.valid, &r.invalid_reasons));
        let out = with_tier_fields(text, &a).unwrap();
        assert!(out.contains("\n  \"tier\": \"standard\",\n  \"valid_for_tier\": true,\n"));
        // Idempotent, and only lines were added.
        assert_eq!(with_tier_fields(&out, &a).unwrap(), out);
        let stripped: String = out
            .lines()
            .filter(|l| {
                !(l.starts_with("  \"tier\"")
                    || l.starts_with("  \"valid_for_tier\"")
                    || l.starts_with("  \"tier_invalid_reasons\""))
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let orig: String = text
            .lines()
            .filter(|l| {
                !(l.starts_with("  \"tier\"")
                    || l.starts_with("  \"valid_for_tier\"")
                    || l.starts_with("  \"tier_invalid_reasons\""))
            })
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(stripped, orig);
        assert!(with_tier_fields("{}", &a).is_err());
    }

    #[test]
    fn smoke_is_never_valid() {
        let a = assess(&inputs(true, 3, 1, 0.0));
        assert_eq!(a.tier, Tier::Smoke);
        assert!(!a.valid_for_tier);
        assert!(a.tier_invalid_reasons[0].contains("never valid for publishing"));
    }
}
