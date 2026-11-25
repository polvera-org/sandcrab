//! Security-invariant and concurrency e2e scenarios (AGENTS.md §1.1): explicit backend
//! selection, no silent downgrade, unenforceable policies refused, sandbox hardening, and
//! concurrent sandboxes without leaks. See `tests/common/mod.rs` for how to run the suite.

mod common;

use std::{sync::Arc, time::Duration};

#[cfg(feature = "docker")]
use common::docker_config;
use common::{E2eBackend, bash_t, docker_ps, host_uid, run, run_exclusive, sh, wait_for};
use sandcrab_sdk::{
    BackendKind, MountMode, NetworkConfig, NetworkPolicy, SandboxConfig, SandboxError, UserSpec,
    protocol::init::HARDENING_SYSCTLS,
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
            #[cfg(feature = "docker")]
            {
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
        // with TSI off it is down, unaddressed and the sandbox cannot bring it up. Docker
        // Desktop: its kernel creates the tunnel modules' fallback devices (`sit0`, `gre0`,
        // ...) in every network namespace, also down and unaddressed (no CAP_NET_ADMIN).
        let links = sh(
            &sb,
            "for i in /sys/class/net/*; do [ -f $i/flags ] && echo \"$(basename $i) $(cat $i/flags)\"; done; true",
        )
        .await;
        for line in links.lines() {
            let (name, flags) = line.split_once(' ').unwrap();
            let up = u32::from_str_radix(flags.trim_start_matches("0x"), 16).unwrap() & 1 != 0;
            let tunnel_fallback = [
                "gre0", "gretap0", "erspan0", "ip_vti0", "ip6_vti0", "sit0", "ip6tnl0", "ip6gre0",
                "tunl0",
            ]
            .contains(&name);
            let tolerated = !up
                && match h.backend {
                    E2eBackend::Libkrun => name.starts_with("dummy"),
                    E2eBackend::Docker => tunnel_fallback,
                    _ => false,
                };
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

/// Network `unrestricted` reaches the internet but never the host's loopback: a listener bound
/// only to 127.0.0.1 (and one on 0.0.0.0) is probed at every address a backend maps to the
/// host (libkrun userspace: gvproxy's NAT alias 192.168.127.254 and the gateway; Docker / FC:
/// the default gateway) and at the host's LAN address. Cloud metadata stays unreachable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker) and internet"]
async fn unrestricted_network_never_reaches_host_loopback() {
    run("net-unrestricted", |h| async move {
        if h.is_docker() && cfg!(target_os = "macos") {
            // Docker Desktop maps `host.docker.internal` to the host by design.
            println!("SKIP Docker Desktop: host-gateway access is a Docker Desktop feature");
            return;
        }
        let listen = |ip: &str| {
            let l = std::net::TcpListener::bind((ip, 0)).unwrap();
            l.set_nonblocking(true).unwrap();
            l
        };
        let (lo, any) = (listen("127.0.0.1"), listen("0.0.0.0"));
        // Docker / FC: a 0.0.0.0 listener on the gateway is the host itself (not part of
        // their contract); only the loopback-only one is probed there. libkrun denies both.
        let mut ports = vec![lo.local_addr().unwrap().port()];
        if h.backend == E2eBackend::Libkrun {
            ports.push(any.local_addr().unwrap().port());
        }
        let lan = {
            let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            s.connect("192.0.2.1:9")
                .ok()
                .and_then(|()| s.local_addr().ok())
                .map(|a| a.ip())
        };

        let sb = h
            .sc
            .spawn(SandboxConfig::from_template("base").network(NetworkPolicy::Unrestricted))
            .await
            .unwrap_or_else(|e| panic!("`unrestricted` spawn (libkrun: netstack installed?): {e}"));

        let r = bash_t(
            &sb,
            "curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://example.com/",
            40_000,
        )
        .await;
        assert_eq!(r.stdout.trim(), "200", "no internet: {r:?}");

        let gw = sh(
            &sb,
            "awk '$2 == \"00000000\" {print $3}' /proc/net/route | head -1",
        )
        .await;
        let gw = u32::from_str_radix(gw.trim(), 16)
            .map(|g| std::net::Ipv4Addr::from(g.swap_bytes()).to_string())
            .unwrap_or_default();
        let mut targets: Vec<String> = vec![gw.clone()];
        if h.backend == E2eBackend::Libkrun {
            targets.extend(["192.168.127.254".into(), "192.168.127.1".into()]);
        }
        targets.extend(lan.map(|ip| ip.to_string()));
        targets.retain(|t| !t.is_empty());
        let mut script = String::new();
        for t in &targets {
            for p in &ports {
                script.push_str(&format!(
                    "(timeout 3 bash -c 'echo x > /dev/tcp/{t}/{p}' 2>/dev/null \
                     && echo OPEN {t}:{p} || echo closed {t}:{p}) & "
                ));
            }
        }
        script.push_str(
            "(timeout 3 bash -c 'echo x > /dev/tcp/169.254.169.254/80' 2>/dev/null \
                         && echo OPEN metadata || echo closed metadata) & wait",
        );
        let r = bash_t(&sb, &script, 30_000).await;
        println!("PROBE {:?} (gw {gw}):\n{}", h.backend, r.stdout.trim());
        assert!(
            !r.stdout.contains("OPEN"),
            "guest reached the host:\n{}",
            r.stdout
        );
        let checked = if h.backend == E2eBackend::Libkrun {
            vec![&lo, &any]
        } else {
            vec![&lo]
        };
        for l in checked {
            if let Ok((_, peer)) = l.accept() {
                panic!(
                    "host listener {} reached from {peer}",
                    l.local_addr().unwrap()
                );
            }
        }
        sb.destroy().await.unwrap();
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

/// Per-mount options (mountinfo field 6) of the topmost mount exactly at `target`.
async fn mount_opts(sb: &sandcrab_sdk::Sandbox, target: &str) -> Vec<String> {
    let line = sh(
        sb,
        &format!("awk '$5 == \"{target}\"' /proc/self/mountinfo | tail -n 1"),
    )
    .await;
    let opts = line
        .split_whitespace()
        .nth(5)
        .unwrap_or_else(|| panic!("{target} is not a mount point"));
    opts.split(',').map(str::to_owned).collect()
}

/// Kernel interfaces closed to the sandbox, as root (uid 0 without capabilities, the
/// strongest sandbox user; microVMs only) and as the default user: read-only `/sys` and
/// `/proc/sys` (Docker's defaults, reproduced by `crabd init`), no user namespaces (Docker:
/// seccomp; microVMs: `user.max_user_namespaces=0`), and on the microVMs every hardening
/// sysctl the guest kernel has reads back as hardened. On Docker those sysctls are host-level, so only
/// the read-only mounts and the namespace denial are asserted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn kernel_interfaces_sysfs_sysctls_userns() {
    run("kernel-interfaces", |h| async move {
        // Docker runs crabd as the sandbox user, and a uid-0 crabd's Unix socket in the
        // host-owned run dir is not connectable by the host user, so root is microVM-only.
        let users = if h.is_docker() {
            vec![None]
        } else {
            vec![None, Some(UserSpec::new(0, 0))]
        };
        for user in users {
            let mut cfg = SandboxConfig::from_template("base");
            cfg.user = user;
            let sb = h.sc.spawn(cfg).await.unwrap();
            let who = sh(&sb, "id -u").await;
            let who = who.trim();

            for target in ["/sys", "/proc/sys"] {
                let opts = mount_opts(&sb, target).await;
                assert!(opts.iter().any(|o| o == "ro"), "{target} as {who}: {opts:?}");
            }
            // A write to an existing, owner-writable sysfs attribute fails.
            let w = sb
                .bash(
                    "f=$(find /sys/kernel /sys/bus -maxdepth 2 -type f -perm -u+w 2>/dev/null \
                     | head -n 1); test -n \"$f\" || exit 99; echo \"$f\" >&2; \
                     printf 1 > \"$f\"",
                )
                .await
                .unwrap();
            assert!(
                !matches!(w.exit_code, Some(0 | 99)),
                "sysfs write as {who}: {w:?}"
            );
            // ...and so does writing a hardening sysctl back (this one has no capability check
            // in the kernel: only the read-only `/proc/sys` stops a uid-0 sandbox user).
            let w = sb
                .bash("echo -1 > /proc/sys/kernel/perf_event_paranoid")
                .await
                .unwrap();
            assert_ne!(w.exit_code, Some(0), "sysctl write as {who}: {w:?}");

            // No user namespaces, so no "unprivileged" root inside one either.
            let u = sb.bash("command -v unshare && unshare -U true").await.unwrap();
            assert!(u.stdout.contains("unshare"), "no unshare in base: {u:?}");
            assert_ne!(u.exit_code, Some(0), "unshare -U as {who}: {u:?}");

            if !h.is_docker() {
                let script: String = HARDENING_SYSCTLS
                    .iter()
                    .map(|s| {
                        let path = s.key.replace('.', "/");
                        format!(
                            "printf '%s=' {key}; cat /proc/sys/{path} 2>/dev/null || echo MISSING\n",
                            key = s.key
                        )
                    })
                    .collect();
                let out = sh(&sb, &script).await;
                let mut missing = Vec::new();
                for (s, line) in HARDENING_SYSCTLS.iter().zip(out.lines()) {
                    let v = line
                        .strip_prefix(&format!("{}=", s.key))
                        .unwrap_or_else(|| panic!("{out}"))
                        .trim();
                    if v == "MISSING" {
                        missing.push(s.key);
                    } else {
                        assert!(s.values.contains(&v), "{} = {v} as {who}", s.key);
                    }
                }
                eprintln!("kernel-interfaces as {who}:\n{out}");
                // The mainline knobs every pinned guest kernel has.
                for key in [
                    "user.max_user_namespaces",
                    "kernel.perf_event_paranoid",
                    "kernel.kptr_restrict",
                    "kernel.dmesg_restrict",
                    "fs.protected_symlinks",
                    "fs.protected_regular",
                    "fs.suid_dumpable",
                ] {
                    assert!(!missing.contains(&key), "{key} missing");
                }
            }
            sb.destroy().await.unwrap();
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
