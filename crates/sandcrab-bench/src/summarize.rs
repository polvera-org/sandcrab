//! Plain-text summary table of a results file (`sandcrab-bench summarize`).

use std::fmt::Write as _;

use crate::results::{Availability, CellStatus, RunResult, Unit};

fn fmt_val(unit: Unit, v: f64) -> String {
    match unit {
        Unit::Ns => format!("{:.2}ms", v / 1e6),
        Unit::Bytes => format!("{:.1}MiB", v / (1024.0 * 1024.0)),
        Unit::CpuS => format!("{v:.3}s"),
    }
}

#[must_use]
pub fn render(r: &RunResult) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "run {} on {} ({}, {}) commit {}{}  seed {}",
        r.run_id,
        r.host.hostname,
        r.host.kind,
        r.host.cpu_model,
        r.git.commit.chars().take(10).collect::<String>(),
        if r.git.dirty { "-dirty" } else { "" },
        r.seed
    );
    let _ = writeln!(
        s,
        "valid: {}{}",
        r.valid,
        if r.invalid_reasons.is_empty() {
            String::new()
        } else {
            format!(" — {}", r.invalid_reasons.join("; "))
        }
    );
    for b in &r.backends {
        if b.status == Availability::Unavailable {
            let _ = writeln!(
                s,
                "backend {}: UNAVAILABLE{} — {}",
                b.backend,
                if b.todo { " (TODO)" } else { "" },
                b.reason.as_deref().unwrap_or("")
            );
        }
    }
    let _ = writeln!(
        s,
        "harness: noop {:.0} ns, sampler {}",
        r.harness.noop_overhead_ns,
        r.harness.sampler_cpu_pct.map_or_else(
            || "n/a (no cgroup sampler)".into(),
            |p| format!("{p:.3}% CPU")
        )
    );
    let _ = writeln!(
        s,
        "\n{:<44} {:<24} {:>4} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "cell", "metric", "n", "p50", "p95", "p99", "min", "max", "stddev"
    );
    let mut skipped = Vec::new();
    for c in &r.cells {
        if c.status != CellStatus::Ok {
            skipped.push(format!(
                "{} [{}]: {}",
                c.key(),
                match c.status {
                    CellStatus::Unavailable => "unavailable",
                    _ => "not measured",
                },
                c.reason.as_deref().unwrap_or("")
            ));
            continue;
        }
        let key = format!(
            "{}{}",
            c.key(),
            if c.failures.is_empty() {
                String::new()
            } else {
                format!(" ({} fail)", c.failures.len())
            }
        );
        for (id, m) in &c.metrics {
            match &m.stats {
                Some(st) => {
                    let _ = writeln!(
                        s,
                        "{:<44} {:<24} {:>4} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
                        key,
                        id,
                        st.n,
                        fmt_val(m.unit, st.p50),
                        fmt_val(m.unit, st.p95),
                        fmt_val(m.unit, st.p99),
                        fmt_val(m.unit, st.min),
                        fmt_val(m.unit, st.max),
                        fmt_val(m.unit, st.stddev),
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "{:<44} {:<24}    - not measured: {}",
                        key,
                        id,
                        m.not_measured.as_deref().unwrap_or("no samples")
                    );
                }
            }
        }
    }
    if !r.density.is_empty() {
        let _ = writeln!(s, "\ndensity:");
        for d in &r.density {
            let first = d.steps.first().map_or(0, |x| x.spawn_ready_ns);
            let last = d.steps.last().map_or(0, |x| x.spawn_ready_ns);
            let _ = writeln!(
                s,
                "  {} {} {} rep {}: max_n {} (stop: {}), spawn_ready first {:.1}ms → last {:.1}ms, budget {} MiB{}",
                d.backend,
                d.template,
                d.variant,
                d.rep,
                d.max_n,
                d.stop_reason,
                first as f64 / 1e6,
                last as f64 / 1e6,
                d.budget_bytes >> 20,
                d.error
                    .as_deref()
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default()
            );
        }
    }
    if let Some(d) = &r.drift_check {
        let _ = writeln!(
            s,
            "\ndrift: spawn_ready p50 {:.2}ms → {:.2}ms ({:+.1}%){}",
            d.start_p50_ns / 1e6,
            d.end_p50_ns / 1e6,
            d.drift_pct,
            if d.flagged { " FLAGGED" } else { "" }
        );
    }
    if !skipped.is_empty() {
        let _ = writeln!(s, "\nnot measured / unavailable:");
        for k in skipped {
            let _ = writeln!(s, "  {k}");
        }
    }
    if !r.notes.is_empty() {
        let _ = writeln!(s, "\nnotes:");
        for n in &r.notes {
            let _ = writeln!(s, "  - {n}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(fmt_val(Unit::Ns, 1_500_000.0), "1.50ms");
        assert_eq!(fmt_val(Unit::Bytes, 1_048_576.0), "1.0MiB");
        assert_eq!(fmt_val(Unit::CpuS, 0.25), "0.250s");
    }
}
