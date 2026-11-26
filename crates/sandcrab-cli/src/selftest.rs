//! `sandcrab selftest`: prove a template's isolation on this host (design §8).

use std::path::{Path, PathBuf};

use anyhow::Context;
use sandcrab_sdk::protocol::api::SelftestReport;
use sandcrab_sdk::selftest::{self, SelftestConfig, status_name};
use sandcrab_sdk::{MountMode, MountPolicy, MountRoot};

use crate::Global;
use crate::util::{self, print_json};

/// Run the self-test and print it. Exit status 1 unless it is `ok`.
pub async fn selftest(
    g: &Global,
    template: &str,
    scratch_root: Option<&Path>,
) -> anyhow::Result<u8> {
    let root = scratch_root.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let root: PathBuf = std::fs::canonicalize(&root)
        .with_context(|| format!("scratch root `{}`", root.display()))?;
    let state = util::state_root(g)?;
    let policy = MountPolicy::new(
        vec![MountRoot {
            path: root.clone(),
            max_mode: MountMode::ReadWrite,
        }],
        vec![state],
    )
    .context("scratch root")?;
    let sc = util::builder(g)?
        .mount_policy(policy)
        .build()
        .context("cannot initialise the backend")?;
    let mut config = SelftestConfig::new(template);
    config.scratch_root = Some(root);
    let report = selftest::run(&sc, config)
        .await
        .with_context(|| format!("self-test of template `{template}`"))?;
    if g.json {
        print_json(&report)?;
    } else {
        print!("{}", table(&report));
    }
    Ok(u8::from(!report.ok))
}

/// One row per line, then the verdict.
fn table(r: &SelftestReport) -> String {
    let width = r.lines.iter().map(|l| l.id.len()).max().unwrap_or(0);
    let mut out = format!(
        "self-test of `{}` on {} ({} ms)\n",
        r.template, r.backend, r.duration_ms
    );
    for l in &r.lines {
        out.push_str(&format!(
            "  {:<12} {:<width$}  {}\n",
            status_name(l.status),
            l.id,
            l.detail
        ));
    }
    match &r.reason {
        None => out.push_str("ok: isolation proven\n"),
        Some(reason) => out.push_str(&format!("NOT ok: {reason}\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use sandcrab_sdk::protocol::api::{SelftestLine, SelftestStatus};

    use super::*;

    #[test]
    fn table_shows_every_line_and_the_verdict() {
        let line = |id: &str, status| SelftestLine {
            id: id.into(),
            status,
            required: true,
            detail: "d".into(),
            evidence: serde_json::Value::Null,
        };
        let r = SelftestReport {
            ok: false,
            reason: Some("tree-kill is inconclusive: d".into()),
            backend: "docker".into(),
            template: "base".into(),
            started_at: String::new(),
            duration_ms: 7,
            lines: vec![
                line("caps-empty", SelftestStatus::Pass),
                line("tree-kill", SelftestStatus::Inconclusive),
            ],
        };
        let t = table(&r);
        assert!(t.contains("pass         caps-empty  d"), "{t}");
        assert!(t.contains("inconclusive tree-kill   d"), "{t}");
        assert!(t.ends_with("NOT ok: tree-kill is inconclusive: d\n"), "{t}");
    }
}
