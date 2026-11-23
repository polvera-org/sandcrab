//! Security-invariant and concurrency e2e scenarios (AGENTS.md §1.1): explicit backend
//! selection, no silent downgrade, unenforceable policies refused, sandbox hardening, and
//! concurrent sandboxes without leaks. See `tests/common/mod.rs` for how to run the suite.

mod common;

use std::sync::Arc;

use common::{docker_config, docker_ps, host_uid, run, run_exclusive, sh};
use sandcrab_sdk::{
    BackendKind, MountMode, NetworkConfig, NetworkPolicy, SandboxConfig, SandboxError,
};
use tokio::task::JoinSet;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn backend_selection_is_explicit_and_never_downgraded() {
    run("backend-selection", |h| async move {
        let caps = h.sc.backend_capabilities();

        // No backend → error (never a default / fallback).
        let err = h.builder().build().unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");

        if h.is_docker() {
            assert_eq!(h.sc.backend_kind(), BackendKind::DockerDev);
            assert!(
                !caps.hardware_isolated && !caps.multi_tenant_safe,
                "{caps:?}"
            );
            // DockerDev without the explicit insecure opt-in → error.
            let err = h
                .builder()
                .docker(docker_config(false))
                .build()
                .unwrap_err();
            assert!(
                matches!(err, SandboxError::InsecureBackendRejected(_)),
                "{err}"
            );
            // Hardware isolation required → DockerDev refused, even with the opt-in.
            let err = h
                .builder()
                .docker(docker_config(true))
                .require_hardware_isolation(true)
                .build()
                .unwrap_err();
            assert!(
                matches!(err, SandboxError::InsecureBackendRejected(_)),
                "{err}"
            );
        }

        // Per-spawn requirement: refused unless the backend really is hardware isolated.
        let res =
            h.sc.spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
                .await;
        if caps.hardware_isolated {
            res.unwrap().destroy().await.unwrap();
        } else {
            let err = res.unwrap_err();
            assert!(
                matches!(err, SandboxError::InsecureBackendRejected(_)),
                "{err}"
            );
        }
        assert!(h.sc.list_sandboxes().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn unenforceable_egress_policy_is_refused() {
    // Exclusive: compares host-wide managed-container counts.
    run_exclusive("egress-restricted", |h| async move {
        let before = h
            .is_docker()
            .then(|| docker_ps("sandcrab.managed=true").len());
        let err =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .network(NetworkConfig::egress_restricted(["example.com"])),
            )
            .await
            .unwrap_err();
        // V1 backends cannot enforce a domain allow-list → refuse, never widen/narrow.
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
        assert!(h.sc.list_sandboxes().is_empty());
        if let Some(before) = before {
            assert_eq!(
                docker_ps("sandcrab.managed=true").len(),
                before,
                "refused spawn left a container"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn sandbox_hardening_network_user_capabilities() {
    run("hardening", |h| async move {
        let ws = h.name("ws");
        h.ws().create(&ws).unwrap();
        let sb =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .mount(ws.as_str(), "/workspace", MountMode::ReadWrite)
                    .network(NetworkPolicy::Disabled),
            )
            .await
            .unwrap();

        // Network disabled → no egress (and no DNS).
        let curl = sb
            .bash("curl -sS -m 3 -o /dev/null https://example.com")
            .await
            .unwrap();
        assert_ne!(curl.exit_code, Some(0), "egress succeeded: {curl:?}");
        let tcp = sb
            .bash("timeout 3 bash -c 'echo > /dev/tcp/1.1.1.1/443'")
            .await
            .unwrap();
        assert_ne!(tcp.exit_code, Some(0), "raw TCP egress succeeded: {tcp:?}");
        let ifaces = sh(&sb, "ls /sys/class/net").await;
        assert!(
            ifaces.split_whitespace().all(|i| i == "lo"),
            "only loopback expected: {ifaces}"
        );

        // No capabilities, no privilege escalation.
        let status = sh(&sb, "cat /proc/self/status").await;
        let field = |k: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .map(str::trim)
                .unwrap_or_else(|| panic!("{k} missing"))
                .to_owned()
        };
        for cap in ["CapEff:", "CapPrm:", "CapAmb:"] {
            assert_eq!(u64::from_str_radix(&field(cap), 16).unwrap(), 0, "{cap}");
        }
        assert_eq!(field("NoNewPrivs:"), "1");
        let su = sb.bash("touch /etc/sandcrab-e2e").await.unwrap();
        assert_ne!(su.exit_code, Some(0), "rootfs writable as root? {su:?}");

        // DockerDev runs as the host user, so workspace files stay host-owned.
        if h.is_docker() {
            let uid: u32 = sh(&sb, "id -u").await.trim().parse().unwrap();
            assert_eq!(uid, host_uid());
            assert_ne!(uid, 0);
            sb.file_write("/workspace/owned.txt", "x").await.unwrap();
        }
        sb.destroy().await.unwrap();
        if h.is_docker() {
            use std::os::unix::fs::MetadataExt;
            let root = h
                .ws()
                .get(ws.as_str())
                .unwrap()
                .backing
                .path()
                .to_path_buf();
            let meta = std::fs::metadata(root.join("owned.txt")).unwrap();
            assert_eq!(meta.uid(), host_uid());
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn concurrent_sandboxes_isolated_and_leak_free() {
    const N: usize = 4;
    run_exclusive("concurrency", |h| async move {
        let names: Vec<String> = (0..N).map(|i| h.name(&format!("c{i}"))).collect();
        for n in &names {
            h.ws().create(n).unwrap();
        }
        let mut set = JoinSet::new();
        for (i, name) in names.iter().cloned().enumerate() {
            let h = Arc::clone(&h);
            set.spawn(async move {
                let sb =
                    h.sc.spawn(
                        SandboxConfig::from_template("base")
                            .mount(name.as_str(), "/workspace", MountMode::ReadWrite)
                            .default_cwd("/workspace"),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("spawn {i}: {e}"));
                let id = sb.id();
                sb.file_write("mine.txt", format!("sandbox {i}\n"))
                    .await
                    .unwrap();
                sh(&sb, &format!("echo {i} >> mine.txt && ls /workspace")).await;
                sb.destroy().await.unwrap();
                id
            });
        }
        let mut ids = Vec::new();
        while let Some(res) = set.join_next().await {
            ids.push(res.expect("sandbox task panicked"));
        }
        assert_eq!(ids.len(), N);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), N, "sandbox ids must be unique");

        h.assert_none_running(&ids);
        for (i, name) in names.iter().enumerate() {
            let files = h.ws().list_files(name.as_str(), "").unwrap();
            assert_eq!(files.len(), 1, "{name}: {files:?}");
            assert_eq!(
                h.ws().read_file(name.as_str(), "mine.txt").unwrap(),
                format!("sandbox {i}\n{i}\n").as_bytes()
            );
        }
        if h.is_docker() {
            let left = docker_ps("sandcrab.managed=true");
            assert!(left.is_empty(), "managed containers leaked: {left:?}");
        }
    })
    .await;
}
