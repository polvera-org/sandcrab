//! Self-test e2e: every line passes on the base template, and a weakened sandbox (network
//! `unrestricted`) fails the matching lines, so the test can fail. Docker only for now (the
//! other backends build their `Sandcrab` in the harness without a mount policy). See
//! `tests/common/mod.rs` for how to run.

#![cfg(feature = "docker")]

mod common;

use common::run;
use sandcrab_sdk::protocol::api::{SelftestReport, SelftestStatus};
use sandcrab_sdk::selftest::{self, LINES, SelftestConfig};
use sandcrab_sdk::{MountMode, MountPolicy, MountRoot, NetworkPolicy, Sandcrab};

fn status(r: &SelftestReport, id: &str) -> SelftestStatus {
    r.lines.iter().find(|l| l.id == id).unwrap().status
}

fn sandcrab(h: &common::Harness) -> (Sandcrab, std::path::PathBuf) {
    let root = h.tmp().join("mounts");
    std::fs::create_dir_all(&root).unwrap();
    let policy = MountPolicy::new(
        vec![MountRoot {
            path: root.clone(),
            max_mode: MountMode::ReadWrite,
        }],
        Vec::new(),
    )
    .unwrap();
    let sc = h
        .builder()
        .docker(common::docker_config())
        .mount_policy(policy)
        .build()
        .unwrap();
    (sc, root)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs Docker"]
async fn selftest_passes_on_base_and_fails_when_weakened() {
    run("selftest", |h| async move {
        if !h.is_docker() {
            eprintln!("SKIP selftest: Docker only");
            return;
        }
        let (sc, root) = sandcrab(&h);

        let report = selftest::run(&sc, SelftestConfig::new("base"))
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        let ids: Vec<&str> = report.lines.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, LINES);
        assert!(report.ok, "{:?}", report.reason);
        assert_eq!(
            (report.backend.as_str(), report.template.as_str()),
            ("docker", "base")
        );
        assert!(
            sc.list_sandboxes().is_empty(),
            "the self-test destroys what it made"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "scratch left");

        let mut weak = SelftestConfig::new("base");
        weak.network = NetworkPolicy::Unrestricted;
        let report = selftest::run(&sc, weak).await.unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert!(!report.ok);
        assert_eq!(status(&report, "net-loopback-only"), SelftestStatus::Fail);
        assert!(
            report
                .reason
                .as_deref()
                .unwrap()
                .starts_with("net-loopback-only is fail"),
            "{:?}",
            report.reason
        );
        assert_ne!(status(&report, "gateway-refused"), SelftestStatus::Pass);
        assert_ne!(
            status(&report, "net-external-refused"),
            SelftestStatus::Pass
        );
        // The walls that do not depend on the network still stand.
        for id in [
            "caps-empty",
            "no-new-privs",
            "userns-refused",
            "fs-outside-mounts",
            "tree-kill",
        ] {
            assert_eq!(status(&report, id), SelftestStatus::Pass, "{id}");
        }
        assert!(sc.list_sandboxes().is_empty());
        sc.shutdown().await.unwrap();
    })
    .await;
}
