//! Security-invariant and concurrency e2e scenarios (AGENTS.md §1.1): explicit backend
//! selection, no silent downgrade, unenforceable policies refused, sandbox hardening, and
//! concurrent sandboxes without leaks. See `tests/common/mod.rs` for how to run the suite.

mod common;

use std::{sync::Arc, time::Duration};

use common::{
    E2eBackend, bash_t, docker_config, docker_ps, host_uid, run, run_exclusive, sh, wait_for,
};
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
            assert_eq!(h.sc.backend_kind(), BackendKind::Docker);
            assert!(
                !caps.hardware_isolated && !caps.multi_tenant_safe,
                "{caps:?}"
            );
            // Hardware isolation required → Docker refused (never a silent downgrade).
            let err = h
                .builder()
                .docker(docker_config())
                .require_hardware_isolation(true)
                .build()
                .unwrap_err();
            assert!(
                matches!(err, SandboxError::HardwareIsolationRequired(_)),
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
                matches!(err, SandboxError::HardwareIsolationRequired(_)),
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
        // Only loopback. libkrun: libkrunfw's kernel always has `dummy0` (TSI's interface);
        // with TSI off it is down, unaddressed and the sandbox cannot bring it up.
        let links = sh(
            &sb,
            "for i in /sys/class/net/*; do echo \"$(basename $i) $(cat $i/flags)\"; done",
        )
        .await;
        for line in links.lines() {
            let (name, flags) = line.split_once(' ').unwrap();
            let up = u32::from_str_radix(flags.trim_start_matches("0x"), 16).unwrap() & 1 != 0;
            let tolerated = h.backend == E2eBackend::Libkrun && name.starts_with("dummy") && !up;
            assert!(name == "lo" || tolerated, "only loopback expected: {links}");
        }
        assert_eq!(
            sh(&sb, "tail -n +2 /proc/net/route").await,
            "",
            "no IPv4 routes"
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

        // Docker and libkrun run as the host user, so workspace files stay host-owned.
        if h.live_workspaces() {
            let uid: u32 = sh(&sb, "id -u").await.trim().parse().unwrap();
            assert_eq!(uid, host_uid());
            assert_ne!(uid, 0);
            sb.file_write("/workspace/owned.txt", "x").await.unwrap();
        }
        sb.destroy().await.unwrap();
        if h.live_workspaces() {
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

/// Docker's `cap_drop: ALL`, `pids_limit` and `/tmp` tmpfs, reproduced by `crabd init` in
/// the microVM guests: every backend must show the same baseline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn hardening_parity_capbnd_pids_tmp() {
    run("hardening-parity", |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();

        // Capability bounding set empty (Docker: cap_drop ALL; microVMs: dropped by PID 1
        // before it forks `crabd serve`), so not even an exec as uid 0 could regain one.
        let status = sh(&sb, "cat /proc/self/status").await;
        for cap in ["CapBnd:", "CapEff:", "CapPrm:", "CapInh:", "CapAmb:"] {
            let v = status
                .lines()
                .find_map(|l| l.strip_prefix(cap))
                .unwrap_or_else(|| panic!("{cap} missing"));
            assert_eq!(u64::from_str_radix(v.trim(), 16).unwrap(), 0, "{cap}");
        }

        // pids cap: the tool process sits in a cgroup with `pids.max` = Docker's
        // `pids_limit`. Docker shows the container cgroup as the namespace root `/`; the
        // microVM guests put `crabd serve` and its children in `/sandbox` (PID 1 outside).
        let cgroup = sh(&sb, "cut -d: -f3- /proc/self/cgroup").await;
        let expected_cgroup = if h.is_docker() { "/" } else { "/sandbox" };
        assert_eq!(cgroup.trim(), expected_cgroup);
        let max = sh(
            &sb,
            "cg=$(cut -d: -f3- /proc/self/cgroup); cat /sys/fs/cgroup${cg%/}/pids.max",
        )
        .await;
        assert_eq!(max.trim(), "1024");
        // Read-only cgroupfs: the sandbox cannot raise its own cap.
        let raise = sb
            .bash("cg=$(cut -d: -f3- /proc/self/cgroup); echo max > /sys/fs/cgroup${cg%/}/pids.max")
            .await
            .unwrap();
        assert_ne!(raise.exit_code, Some(0), "pids.max writable: {raise:?}");

        // ...and it bites: background `sleep`s until fork fails (dash does not retry
        // EAGAIN), then kill them with builtins only.
        let bomb = bash_t(
            &sb,
            r#"P=$(mktemp)
sh -c 'n=0; while [ $n -lt 1100 ]; do sleep 120 & echo $! >> "$1"; n=$((n+1)); done' sh "$P"
rc=$?
mapfile -t pids < "$P"
kill -9 "${pids[@]}" 2>/dev/null
echo "rc=$rc spawned=${#pids[@]}""#,
            120_000,
        )
        .await;
        assert_eq!(bomb.exit_code, Some(0), "{bomb:?}");
        let out = bomb.stdout.trim();
        let spawned: usize = out
            .rsplit_once("spawned=")
            .and_then(|(_, n)| n.parse().ok())
            .unwrap_or_else(|| panic!("{bomb:?}"));
        assert!(out.starts_with("rc=2 "), "fork never failed: {bomb:?}");
        assert!(bomb.stderr.contains("fork"), "{bomb:?}");
        // The cap counts the tool's own processes (and crabd's threads) too.
        assert!(
            (900..1024).contains(&spawned),
            "spawned {spawned}: {bomb:?}"
        );
        // The sandbox recovers once the processes are gone.
        wait_for("fork recovery", Duration::from_secs(30), || async {
            let r = sb.bash("true").await.ok()?;
            (r.exit_code == Some(0)).then_some(())
        })
        .await;

        // `/tmp`: Docker's size-limited `nosuid,nodev,noexec` tmpfs on every backend.
        let mi = sh(&sb, "grep ' /tmp ' /proc/self/mountinfo | tail -n 1").await;
        let (mount_part, super_part) = mi
            .trim()
            .split_once(" - ")
            .unwrap_or_else(|| panic!("{mi}"));
        let opts: Vec<&str> = mount_part
            .split_whitespace()
            .nth(5)
            .unwrap()
            .split(',')
            .collect();
        for o in ["rw", "nosuid", "nodev", "noexec"] {
            assert!(opts.contains(&o), "/tmp lacks {o}: {mi}");
        }
        let mut sup = super_part.split_whitespace();
        assert_eq!(sup.next(), Some("tmpfs"), "{mi}");
        let super_opts = sup.nth(1).unwrap();
        assert!(
            super_opts.split(',').any(|o| o == "size=262144k"),
            "/tmp not 256 MiB: {mi}"
        );
        assert_eq!(sh(&sb, "stat -c %a /tmp").await.trim(), "1777");
        let exec = sb
            .bash("printf '#!/bin/sh\\necho ran\\n' > /tmp/x.sh && chmod +x /tmp/x.sh && /tmp/x.sh")
            .await
            .unwrap();
        assert_eq!(exec.exit_code, Some(126), "exec from /tmp: {exec:?}");
        assert!(!exec.stdout.contains("ran"), "{exec:?}");

        sb.destroy().await.unwrap();
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
