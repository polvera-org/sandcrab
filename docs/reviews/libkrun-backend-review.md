---
title: libkrun backend flow review (lk-review)
created: 2026-11-25
updated: 2026-11-25
tags: [review, security, libkrun, docker, guest, firecracker]
---

# libkrun backend flow review (`lk-review`)

Reviewer: Nebula (QA, security and spec compliance). Range: `09e566b..a1ce64f` (186 commits, 155 files).
Flow: [`docs/plans/libkrun-backend.flow.yaml`](../plans/libkrun-backend.flow.yaml).

Inputs: the code, [design/libkrun-backend.md](../design/libkrun-backend.md),
[design/guest-seccomp.md](../design/guest-seccomp.md), [design/firecracker-backend.md](../design/firecracker-backend.md),
[protocol.md](../protocol.md), [template-pins.md](../template-pins.md), [hosting.md](../hosting.md), and the
invariants in `AGENTS.md` §1. Host paths below use `kvm-dedicated` and `/home/dev`.

## Verdict

**Ship with follow-ups.**

No critical or high findings. The integrated gate on `kvm-dedicated` is green end to end: fmt, clippy, 728 unit
and integration tests, Firecracker 6/6, libkrun VM 11/11, and SDK e2e 18/18 on each of docker, firecracker,
libkrun and docker-over-TCP. The security core is sound:

- the unsafe island is small and checked;
- TSI is never implicit;
- the egress filter is default-deny and holds up live;
- share roots are fd-pinned;
- the auth token is strong, redacted and on every connection;
- the guest hardens itself before user code runs;
- the capability flags are honest.

Counts: **critical 0, high 0, medium 2, low 13, info 9** (R1–R19 plus the known items K1–K5).

Follow-ups, in priority order:

1. **R2 + R1:** pre-auth slot exhaustion on the Docker TCP transport, which on default Docker (ICC on) other
   bridge containers can also reach. Fix before the TCP transport is advertised beyond single-user dev boxes.
2. **Decisions (a) and (b):** they also close R4 and R5.
3. **R3:** drop IPv4 fragments in the egress filter.
4. **R6, R7, R18:** fail-closed hardening for uid 0, `memory.swap.max`, and a 0700 state tree.
5. **R10 before `lk-docs`:** that step plans to document Developer ID + hardened runtime signing, which is
   probably broken as written.
6. The rest as convenient.

## Findings

Severity: critical / high / medium / low / info. Line numbers are at `a1ce64f`.

| ID | Sev | Location | Scenario | Suggested fix |
| --- | --- | --- | --- | --- |
| R1 | medium | `crates/sandcrab-backend-docker/src/spec.rs:433` (`build_relay_spec`, `network_mode: "bridge"`), `crates/sandcrab-guest/src/main.rs` relay `--tcp 0.0.0.0:7070` | With the Docker `tcp` transport the relay container sits on the **default bridge** and listens on `0.0.0.0:7070` inside it. Every other container on that bridge can connect to `<relay ip>:7070` directly, bypassing the `127.0.0.1` host publish. That includes every `unrestricted` Docker sandbox (also on the default bridge, `spec.rs:121`) and any unrelated container on the host. So a sandbox whose own network is `disabled` still has its `crabd` reachable from its neighbours. The token stops a takeover (verified constant-time, 256-bit), but the pre-auth surface and R2's slot exhaustion are open to other tenants. **Live check:** on `kvm-dedicated` a probe from a throwaway bridge container to a live relay's `:7070` was dropped, but only because that daemon runs with `enable_icc=false`. Docker's default, and Docker Desktop (the main user of the `tcp` transport), is `icc=true`, so there the relay is reachable. | Put each relay on its own `--internal` user-defined network (or `network_mode: none` plus a host-side proxy), or bind the relay to the address Docker publishes and turn off ICC for that network. At minimum, document that the relay is reachable from the default bridge. |
| R2 | medium | `crates/sandcrab-guest/src/server.rs:334` (permit before auth), `crates/sandcrab-guest/src/auth.rs:59` (5 s), `crates/sandcrab-guest/src/relay.rs:70` (256 slots) | A connection takes one of `crabd serve`'s 64 `--max-connections` slots **before** it authenticates and keeps it for up to 5 s. Any local host user (TCP transport, `127.0.0.1:<port>`) or any bridge neighbour (R1) can open 64 idle connections every 5 s and starve the legitimate client: readiness fails, reconnects fail and tunnels fail. It costs the attacker a few sockets. Each refusal also logs a `warn!`, so the flood also floods logs. | Authenticate outside the connection semaphore: give pre-auth connections their own small bucket (say 8) with a shorter timeout (1 s; the host sends `auth` in its first write). Reserve at least one slot for authenticated connections. Rate-limit the refusal log. |
| R3 | low | `crates/sandcrab-backend-libkrun/src/netstack.rs:495` | The egress filter reads TCP/UDP ports only when `frag_offset == 0 && total >= ihl + 4`. A **tiny first fragment** (offset 0, MF set, fewer than 4 L4 bytes) has `port = None` and skips the SMTP check. The next fragment (offset ≥ 1) also has no port. The destination check still applies to every fragment, so this bypasses **only the SMTP block**, and only where the netstack reassembles. gvproxy (gVisor) does; passt does not reassemble, so in practice this is macOS. The gateway and broadcast paths are safe because they require `Some(port)`. | Drop every IPv4 fragment (`MF` set or offset ≠ 0) for TCP/UDP. With MTU 1500 and no offloads the guest has no reason to fragment, and the netstacks do PMTU. Or drop first fragments whose L4 header is shorter than 8 bytes. Add a test for both cases. |
| R4 | low | `crates/sandcrab-backend-libkrun/src/ffi.rs:304-307`; `src/bin/sandcrab-krun.rs:373` and `:398`; `src/netstack.rs:622` | Hash then load **by path**: libkrun and libkrunfw are hashed, then `dlopen`ed by path, and the netstack is hashed, then `exec`ed by path. A same-user writer can swap a file in the gap. Also, passt re-execs `<path>.avx2` if that file exists (design §12.3); that sibling is never hashed. The impact is bounded, because each of these needs write access to the user's own state dir, which already holds the helper binary. It is still a gap against the "pinned" promise and a cheap one to close. | Linux: open each file once, hash the fd, then `dlopen("/proc/self/fd/N")` and `fexecve` (or `execveat(fd, "", AT_EMPTY_PATH)`). Refuse to start passt when `<path>.avx2` exists, or install the pinned `.avx2` build too. macOS: hash, then `fcntl(F_GETPATH)` plus a dev/ino re-check after `dlopen` (dyld has no fd-based load). Together with decision (b). |
| R5 | low | `crates/sandcrab-backend-libkrun/src/backend.rs:358` (`root_disk`, `sha256: None`); `crates/sandcrab-rootfs/src/cache.rs:168` (`verify_on_hit: false`) | The libkrun (and Firecracker) root disk is trusted by path. A cache hit only checks key and size, and the helper does not hash the disk. Corruption (a partial write, a disk error, or a tampered `rootfs.ext4` of the same size) boots silently. On microVMs the image includes `/sbin/crabd`, the guest init. | Decision (a): a stat-stamp cache, verified once per process. |
| R6 | low | `crates/sandcrab-guest/src/init/pid1.rs:180`, `:189`, `:193`, `:323` | Guest hardening fails **open**. A sysctl write that fails (`Outcome::Failed`), and a `/proc/sys`, `/sys` or cgroupfs seal that fails, are only logged as `warn!` inside the guest, and the host never sees them. With the default non-root guest user that is acceptable, since DAC still protects those files. With a configured **uid-0** guest user (no capabilities, but the owner of every root-owned file) an unsealed `/proc/sys` or cgroupfs lets sandbox code undo sysctls that only check DAC, and raise `pids.max` / leave the pids cgroup. "Missing" sysctls (`Outcome::Missing`) are fine: the guest kernel is pinned, and a missing key means a missing feature. | Report the hardening outcome in `InitConfigureResult` (set / missing / failed lists, plus a `sealed` flag) and have the backends refuse, or at least `tracing::warn!` on the host. Fail `init.configure` closed when a seal failed and the guest user is uid 0. |
| R7 | low | `crates/sandcrab-backend-libkrun/src/cgroup.rs:40-57` (also `sandcrab-backend-firecracker/src/rundir.rs:170`) | The cgroup leaf sets `memory.max` but not `memory.swap.max`. On a host with swap (`kvm-dedicated` has 8 GiB) the guest RAM and VMM overhead above `memory.max` go to swap instead of being capped, so the per-VM memory cap is RAM + swap, not RAM. | Write `memory.swap.max = 0` (or a small value) next to `memory.max`, and add `memory.swap.max` to the `doctor` delegation check (the controller is part of `memory`). |
| R8 | low | `crates/sandcrab-backend-libkrun/src/bin/sandcrab-krun.rs:256` | `PR_SET_PDEATHSIG` fires when the **thread** that forked the helper exits, not the process. Today `tokio::process::Command::spawn` runs on a runtime worker, which lives as long as the runtime. A caller that spawns from a `spawn_blocking` or `block_in_place` thread (blocking-pool threads retire after 10 s idle) gets its VM SIGKILLed about 10 s later, with "lifeline closed" never logged. The stdin lifeline alone already covers owner death. | Drop `PR_SET_PDEATHSIG` (the pipe lifeline is enough, and it works on macOS too), or document that the spawn must come from a long-lived thread. |
| R9 | low | design §12.2 (`chmod u+s`), libkrun virtio-fs `LinuxComplete` | A guest `chmod u+s` sets the setuid bit on the **host** file, setuid to `<user>`. On a multi-user host with a workspace that other users can traverse (live host-dir mounts are arbitrary user paths), another local user who runs that file gets `<user>`'s uid. Docker bind mounts have the same property. Documented, but not called out as a risk. | Clear `S_ISUID`/`S_ISGID` on guest `setattr`/`create` in the share (libkrun `killpriv`-style option, or a patch), or refuse shares whose root is group- or world-accessible. Add the risk to `hosting.md`. |
| R10 | low | `scripts/sign-krun-helper.sh:66` | The Developer ID path adds `--options runtime` (hardened runtime) with only the `hypervisor` entitlement. Under the hardened runtime, library validation rejects the ad-hoc or Homebrew-signed `libkrun.dylib`, and dyld drops `DYLD_*` variables, which breaks the `DYLD_FALLBACK_LIBRARY_PATH` lookup of libkrunfw. Unverified, since only ad-hoc signing was exercised. | Test the Developer ID path. If it is kept, add `com.apple.security.cs.disable-library-validation` (and re-sign the pinned dylibs) or resolve libkrunfw some other way. Until then, document that the path is unsupported. |
| R11 | info | `crates/sandcrab-backend-libkrun/src/backend.rs:31-42` | Capability honesty is **confirmed**. libkrun reports `multi_tenant_safe: false`, which is correct: the virtio-fs passthrough, vsock and block device models run in-process as the host user with no seccomp, Landlock or Seatbelt, so an escape through device emulation gets every workspace of that user. `hardware_isolated: true` is correct (separate guest kernel on HVF/KVM). Firecracker (`multi_tenant_safe: false`, no jailer) and Docker (`false`/`false`) are also honest. | None. Revisit when Phase C confines the helper. |
| R12 | info | `crates/sandcrab-backend-libkrun/src/net.rs:46` | The deprecated `allow_tsi_unrestricted` **wins** over an installed netstack, so an operator who still passes `--libkrun-tsi` gets host-loopback-reachable networking even though the safe path exists. It is never implicit, and the CLI prints a deprecation warning (`sandcrab-cli/src/util.rs:108`). | When the netstack is installed, refuse the flag (or ignore it with an error-level log) and remove it in the next minor version. |
| R13 | info | `crates/sandcrab-backend-libkrun/src/netstack.rs:151` (`--dns-forward`) | The guest's DNS goes to the host's resolver (on Ubuntu, `systemd-resolved` at `127.0.0.53`). So the guest can resolve split-horizon or internal names (VPN, tailnet MagicDNS) that it cannot connect to. That is a small information leak, and DNS remains a covert channel, as on every backend with DNS. | Optional: forward to a fixed public resolver for `unrestricted`, or document it. |
| R14 | info | `crates/sandcrab-backend-docker/src/backend.rs:224`, `:231` | When the container uid differs from the host uid, the socket dir is `0777` and the token file `0444`. Other host users are kept out only by the `0700` `state_dir`, which is created with `0700` only when it does not exist yet. A pre-existing, looser `state_dir` is not checked. | Check that `state_dir` is owned by the user, `0700` and not a symlink before use, like the Firecracker tap lock dir (`net.rs:179`). |
| R15 | info | `scripts/fc-net-setup.sh` (uplink rules) | The uplink's on-link subnets and gateway are read once, when the script runs (each boot with `--persist`). A DHCP renumbering or a new uplink route between runs is not covered until the next run. | Document it, or re-run from a `networkd-dispatcher`/`NetworkManager` hook. |
| R16 | info | `crates/sandcrab-backend-firecracker/tests/fake_vmm.rs:523`, `src/config.rs:182` | `cargo test --workspace` fails 9/9 `fake_vmm` tests when `TMPDIR` is long. A 37-byte `TMPDIR` pushed the run-dir sockets past 107 bytes. Also, without `--no-fail-fast` this aborts the remaining test binaries. A gate artifact, not a product bug. | Have the tests create their run dir under a short base (`/tmp/scfc-XXXX`) and not under `TMPDIR`. |
| R17 | info | `crates/sandcrab-guest` (`crabd serve` runs as the guest user) | Tool processes run as the same uid as `crabd serve`. They can kill it and bind vsock port 1024 (not a privileged port), or ptrace it. On Docker they can replace the Unix socket in the rw run dir. The host then talks to a fake `crabd`, which can lie about tool results. The guest is untrusted anyway, and nothing on the host trusts tool results for host-side decisions (workspaces are host-verified). The `init.*` control port (1) is privileged and safe. | No action, but keep host-side logic from trusting `crabd` responses (e.g. future snapshot or commit paths). Optionally run `crabd serve` as a separate uid. |
| R18 | low | `crates/sandcrab-cli/src/libkrun.rs:744`, `crates/sandcrab-backend-libkrun/src/artifacts.rs:770`, `src/netstack_pins.rs:329`, `scripts/fetch-libkrun.sh:224` | The state tree is created with `create_dir_all` / `mkdir -p` under the caller's umask. On the gate host (umask `0002`) `<state>/bin`, `<state>/artifacts/libkrun/<ver>`, `<state>/cache` and `manifest.json` came out **group-writable** (`0775`/`0664`); the shared `~/.sandcrab` there is `0775` too. The helper binary in `bin/` is not hashed before it is executed, so a member of the user's group can replace `sandcrab-krun` and run code as the user on the next spawn. The libraries are protected by the sha, but `manifest.json`, which holds the Linux libkrun sha, is group-writable as well. This is harmless with per-user private groups (the Ubuntu default), and real on hosts with shared primary groups. | Create every state directory `0700` (`DirBuilder::mode`), and at backend construction refuse a state root, `bin/` or `artifacts/` directory that is group- or world-writable (as the Firecracker tap lock dir does). Optionally record the helper's sha in the install manifest and check it at backend construction. |
| R19 | low | `crates/sandcrab-bench/src/host.rs:280` (`hostname: hostname()`), `src/run.rs:1837` | `--host-label` changes only the result **file name**. `host.hostname` in the JSON is still the real hostname: the smoke run with `--host-label kvm-dedicated` recorded the machine's real name. In a public repo every committed result has to be scrubbed by hand (the committed results are clean today). | When `--host-label` is given, write it to `host.hostname` too (or add `--redact-host`), and add a test that the label is the only host name in the output. |

### Known items (confirmed or downgraded)

| ID | Sev | Item | Result |
| --- | --- | --- | --- |
| K1 | low | `sandcrab-guest` `pty_process_has_a_terminal` fails on macOS | **Confirmed by inspection:** `crates/sandcrab-guest/tests/exec_process.rs:319` asserts `starts_with("/dev/pts/")`, while macOS ptys are `/dev/ttysNNN`. A test-only portability bug (not run on the Mac, by instruction). Fix: accept `/dev/pts/` or `/dev/tty`, or `cfg(target_os = "linux")` the path assertion. |
| K2 | low | SDK tests do not compile with Docker disabled | **Confirmed on `kvm-dedicated`:** `cargo build -p sandcrab-sdk --no-default-features --features libkrun` builds, but `cargo test ... --no-run` fails in all 5 e2e test crates (`unresolved import sandcrab_sdk::DockerConfig`, `no method docker`). Fix: gate `common::docker_config()` and the Docker arm in `crates/sandcrab-sdk/tests/common/mod.rs` behind `feature = "docker"`. |
| K3 | low | Bench cannot find libkrun's guest RAM in smaps | **Confirmed, root cause found.** For a 512 MiB guest, libkrun on x86_64 maps guest memory as one **1 GiB** anonymous region (`/proc/<helper>/smaps`: a 1 048 576 kB mapping, no 524 288 kB one). `guest_split` (`crates/sandcrab-bench/src/sample.rs:149`) keeps only mappings `<= guest_bytes`, so it never matches, and `mem.vmm_guest_pss_*` / `mem.vmm_overhead*` are "not measured". The cgroup and PSS totals are fine. Fix: for libkrun, take the smallest anonymous mapping `>= guest_bytes` (and report its touched PSS), or read the KVM memslots. |
| K4 | info | Firecracker resource metrics missing from the Full run | **Not reproduced in the smoke run:** the FC resources cell has `mem.host_pss_*`, `mem.vmm_guest_pss_*`, `mem.vmm_overhead*` and `mem.vmm_rss_*` from VMM smaps. Only the `mem.cgroup_*` metrics are "not measured", by design, without `SANDCRAB_BENCH_FC_CGROUP`. Downgraded to a run-config issue: set `SANDCRAB_BENCH_FC_CGROUP` (a delegated parent, as for libkrun) for the Full run. |
| K5 | info | crabagent was docker-only | Out of scope (step `crabagent-backends` is in progress). |

Bench sanity (light): metric ids are consistent across backends (32 ids, `mem.helper_rss_*` only for libkrun,
`mem.shim_pss` only for Docker). `valid=false` was set correctly, with both reasons (`n < 30` under `--quick`, and
1-minute load 1.57 > 0.5 right after the e2e suites). The drift check (+9.3%) is reported, not flagged. Every
"not measured" entry has a reason. Docker `mem.host_pss_*` reads `Permission denied` on `containerd-shim` smaps for
an unprivileged user, which is expected and has a reason.


### Reviewed and found sound

- **ffi.rs:** a single `unsafe` island; every symbol is copied into a `Krun` that owns both `Library` handles, so the
  fn pointers cannot outlive the mapping. `krun_set_port_map` is always called with an empty array (never `NULL`), TSI
  is `0` unless the deprecated opt-in is set, and the call plan is unit-tested. Strings are validated twice (in
  `helper_proto` and again in `str_cstr`). `LD_LIBRARY_PATH` / `DYLD_FALLBACK_LIBRARY_PATH` must equal the pinned
  directory, and `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` must be unset (`check_libkrunfw_lookup`). The launcher
  `env_clear()`s, so `LD_AUDIT` and similar variables cannot reach the helper either.
- **HelperConfig:** `deny_unknown_fields`, a 64 KiB cap, version gate, normalized absolute paths, socket length
  limits, a read-only root and a writable scratch disk enforced, duplicate IDs, tags and paths refused, cmdline
  strings limited to printable ASCII. The lifeline moves stdin to a private fd, so libkrun's console cannot swallow
  it, and EOF SIGKILLs the helper. The `RLIMIT_NOFILE` raise is bounded by `nr_open` / `OPEN_MAX` and never lowers.
- **virtio-fs:** share roots are opened `O_DIRECTORY|O_NOFOLLOW`, dev/ino-checked against the lease, and passed as
  `/proc/self/fd/N/.` (Linux) / `/dev/fd/N` (macOS). A unit test covers the swap-after-verify case. Read-only
  shares are enforced by libkrun's server (`EROFS`, guest remount refused).
- **Netstack filter:** default deny; only IPv4 and ARP pass, so IPv6 and VLAN tags (0x8100/0x88a8) are dropped. IHL
  and total length are bounds-checked, IP options are skipped correctly, oversize frames kill the stream. The
  gateway allows only DNS 53 and DHCP 67, and broadcast only DHCP. `ALWAYS_DENIED` matches the Firecracker nftables
  set. Host `/32`s and on-link subnets are added. ARP is passed through unfiltered, but it never leaves the
  netstack, so spoofing it reaches nothing. The netstack runs with an empty env, no listening sockets
  (`-t none -u none`, `-ssh-port -1`), and exits on socketpair EOF.
- **Docker auth:** a 256-bit `getrandom` token, a constant-time compare (length leak only), `Debug` redacted. The
  token file is created `O_EXCL` at its final mode and deleted by `crabd` after reading. It never appears in env,
  cmd or labels (only the file path does). `AuthConnector` wraps every connection (readiness, reconnects, tunnels).
  The relay is `cap_drop ALL`, `no-new-privileges`, read-only rootfs, 64 pids / 64 MiB, and published on
  `127.0.0.1` only (`loopback_port` ignores other bindings). A `disabled` sandbox keeps `network_mode: none`
  (but see R1). microVM vsock transports are correctly unauthenticated: the run dir is `0700` and port 1 is
  privileged.
- **Guest hardening:** sysctls, read-only `/proc/{sys,sysrq-trigger,irq,bus,fs}` and `/sys`, and the pids cgroup
  are all set before the control port listens, so before any user code. `crabd serve` is forked from a thread with
  an empty bounding set (checked, fails closed) and born inside the pids cgroup. `no_new_privs` is set before the
  runtime starts (fails closed). cgroupfs is sealed after the spawn.
- **Template pins:** Firecracker and libkrun refuse an unpinned image (`unpinned_error`) before any rootfs build.
  Docker refuses unless `allow_unpinned_images` is explicitly set (warned). A mismatch is `DigestMismatch` with a
  re-pin hint on all three.
- **Firecracker egress / tap locks:** the reserved, private, uplink-subnet, gateway and SMTP drops run at priority
  `filter - 1`, ahead of Docker. Anti-spoofing pairs are per tap. The `--persist` copy is root-owned. The lock dir
  is ownership-, mode- and symlink-checked, and lock files are opened `O_NOFOLLOW`.
- **Port forwards:** `127.0.0.1` only (not configurable), 64 tunnels per forwarder, a 10 s handshake bound.

## Decisions

### (a) Per-spawn root-disk hash for libkrun: **cache by a stat stamp, verified once per process**

What the code does today: neither libkrun nor Firecracker hashes the root disk on spawn (`root_disk(..)` sends
`sha256: None`). The rootfs cache checks only key and size on a hit (`verify_on_hit` is off except in
`sandcrab rootfs ... --verify`), and the in-process cache skips even that after the first spawn.

Options:

- **Keep a full per-spawn hash** of 320 to 960 MiB: about 1.5 to 5 s per spawn on this Xeon without SHA-NI, more
  than the whole boot. Rejected.
- **Drop it** (the status quo): a corrupted or same-size tampered image boots silently, and that image carries
  `/sbin/crabd`, the guest's PID 1.
- **Recommended: a stat stamp.** At build time, `chmod 0444` the image and record
  `(dev, ino, size, mtime_ns, ctime_ns)` next to `rootfs_sha256` in `manifest.json`. On the first spawn of each
  process, `open` the image, `fstat` it, and compare the stamp. On a match, trust the recorded sha. On a mismatch,
  re-hash in full (on a blocking thread), then refuse or re-stamp. Hand the helper the **verified fd**
  (`/proc/self/fd/N` on Linux, as the shares already do), so the stamp check and the use refer to the same inode.

Security trade-off: an unprivileged user cannot set `ctime`, and any write, truncate, `chmod`, or rename-over
changes it (rename-over also changes the inode). So the stamp catches every same-user, non-root modification a
hash would catch, at the cost of one `fstat`. What it misses is root or raw block-device writes and
filesystem-level corruption that leaves metadata unchanged (bit rot). Root is out of the threat model, since root
owns the hypervisor anyway. For bit rot, `sandcrab rootfs verify` and `doctor` (periodic full hash) are enough.
The real gain over today is detecting a swapped image and closing the hash-to-use race. Apply the same scheme to
Firecracker.

### (b) Linux libkrun library hash cost (~187 ms per spawn without SHA-NI): **the same stat stamp plus fd-based `dlopen`; fs-verity as an optional fast path**

Options:

- **Accept:** about 45% of the ~407 ms warm spawn goes to re-proving something that has not changed since
  install. Not worth it.
- **fs-verity:** constant-time and kernel-enforced, but it needs `tune2fs -O verity` (root), ext4 or f2fs on the
  install fs, and it does not exist on APFS. Most hosts would still need a fallback. Keep it as an optional
  optimisation: if `FS_IOC_MEASURE_VERITY` succeeds and matches the recorded digest, skip hashing.
- **Recommended: hash once per install, then a stamp check.** `sandcrab libkrun install` already writes the
  installed sha into `manifest.json`, and the files are already `0444`. Also record each file's
  `(dev, ino, size, mtime_ns, ctime_ns)`. The helper then opens libkrunfw and libkrun `O_RDONLY|O_CLOEXEC`,
  `fstat`s them, compares the stamps (on a mismatch it hashes the fd in full and fails closed on a bad sha), and
  `dlopen`s `"/proc/self/fd/N"`. glibc then resolves libkrun's leaf-name `dlopen` of libkrunfw through the
  already-loaded `DT_SONAME`, so `LD_LIBRARY_PATH` no longer matters for correctness. Keep the check as defence in
  depth. That removes both the 187 ms and the R4 hash-to-load race. The netstack gets the same treatment
  (`fexecve`). macOS keeps hashing for now (about 20 ms with SHA extensions on M-series, and dyld has no fd load).

Security trade-off: the same as (a). The stamp detects any modification an unprivileged user can make. The
manifest holding the stamp is exactly as trusted as the manifest that holds the Linux sha today (both are
user-writable state; the Linux libkrun sha is not compiled in, because it is built from source). So nothing gets
weaker, and loading through the verified fd makes it strictly stronger.

## Integrated gate (`kvm-dedicated`, Ubuntu x86_64, KVM, unprivileged user)

All on a1ce64f, copied (no `.git`) to a private dir with its **own** `CARGO_TARGET_DIR` and a private
`SANDCRAB_HOME` (0700). The Docker template images were rebuilt from cache, so the image IDs were unchanged and
the shared host pins were untouched. libkrun ran under `systemd-run --user --scope` with `SANDCRAB_LIBKRUN_CGROUP`
set to the delegated `sandcrab-vms.service` parent. Everything was removed afterwards (14 GB of target and source,
1.8 GB of state); no containers, volumes, helpers, netstacks or cgroup leaves were left.

| # | Step | Result |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | pass |
| 1 | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| 1 | `cargo test --workspace --no-fail-fast` | **728 passed, 0 failed, 54 ignored** (58 test binaries, incl. doctests). The first attempt failed 9/9 `fake_vmm` tests on a 37-byte `TMPDIR` (R16) and, without `--no-fail-fast`, skipped the rest. Re-run with a short `TMPDIR`: green. |
| 2 | `scripts/build-crabd.sh`, helper (`--release`), `sandcrab-cli` | pass (crabd sha `add81a2c6c40…`) |
| 2 | `sandcrab libkrun install --helper …` | pass: libkrun 1.19.8 built from the pinned tag (`NET=1`), libkrunfw 5.6.4 release, passt `2026_10_02.cba3570` static, all hashed |
| 2 | FC artifacts, `templates/build.sh base python-dev ts-dev`, `sandcrab rootfs build …` | pass: kernel 6.18.62 + firecracker v1.18.0 pins verified; 3 pins written; rootfs 320 / 896 / 960 MiB |
| 2 | `sandcrab doctor` (all, `--backend libkrun`, `--backend firecracker`) | all `ok`. `lk:cgroup` warns outside the user scope, as documented. `render` is unpinned (not built), as intended. |
| 3 | Firecracker e2e (`firecracker_kvm`, `--ignored --test-threads=1`) | **6/6** (incl. `unrestricted_network_egress_policy`, `unrestricted_network_reaches_the_internet`) |
| 3 | libkrun VM tests (`libkrun_vm`, under the user scope + cgroup) | **11/11** (incl. `helper_runs_in_its_cgroup_leaf`, `share_root_is_pinned_by_fd`, `tsi_off_regressions`, `userspace_network_reaches_internet_not_the_host`, `userspace_disabled_has_no_nic_and_netstack_dies_with_helper`) |
| 3 | libkrun `fake_helper` | **14/14** |
| 4 | SDK e2e, `SANDCRAB_E2E_BACKEND=docker` | **18/18** (harness 2, lifecycle 1, security 7, tools 4, workspace 4) |
| 4 | SDK e2e, `firecracker` (`--features firecracker`) | **18/18**, no SKIP lines |
| 4 | SDK e2e, `libkrun` (`--features libkrun`, user scope + cgroup) | **18/18**, no SKIP lines |
| 5 | `sandcrab-backend-docker` e2e, `unix` transport | **6/6** |
| 5 | `sandcrab-backend-docker` e2e, `SANDCRAB_DOCKER_TRANSPORT=tcp` | **6/6** |
| 5 | SDK e2e Docker with `SANDCRAB_DOCKER_TRANSPORT=tcp` | **18/18** |
| 6 | Bench smoke `run --quick --backends docker,firecracker,libkrun --templates base` (output outside the repo) | completed, all 3 backends `available`, `valid=false` (expected: `--quick`, host load). Warm `spawn_ready` p50: Docker 2102 ms, Firecracker 750 ms, libkrun 428 ms. libkrun `call.grep` p50 93 ms vs FC 24 ms (virtio-fs lookup cost; worth a perf note). |
| extra | R1 probe, K2 and K3 checks | see R1, K2 and K3 |

Totals across the e2e suites: 6 + 11 + 14 + 18 × 4 + 6 × 2 = **115 passed, 0 failed**.

Not run (out of scope): the Mac gate (the Mac's disk is tight and another agent is building there).

## Re-review (after review-fixes + lk-verified-fd)

Reviewer: Nebula. Tip `093cb12` (`feat/phase-1`). Ranges: `98bb52f..b03da08` (lk-verified-fd) and
`b03da08..093cb12` (review-fixes, including the merge `054b9f9`). Locations below are at `093cb12`.

### Verdict

**Ship with follow-ups.** Both medium findings (R1, R2) are closed, and so are decisions (a) and (b). No new
finding is medium or above. What remains is low or info, and each of those needs either the user's own uid or a
documented platform limit (macOS exec by path, Docker Desktop routing). The deferred items are acceptable as
follow-ups. One of them should come first: wiring `check_private_dir` (R18).

### Findings: status

| ID | Status | Notes |
| --- | --- | --- |
| R1 | **closed** (Desktop residual documented) | Every relay joins one shared bridge network, `sandcrab-relay`, with `enable_icc=false` and no masquerade. An existing network with that name but without those options is refused, never used. Concurrent creators race benignly: a `409` is followed by a re-inspect and the same check. The network is shared (not one per sandbox) and never removed, which is fine because it holds no state. Docker Engine isolates bridges from each other, so no neighbour can reach a relay. Docker Desktop still routes containers to published ports (`host.docker.internal`). That residual is documented in `hosting.md`, and the token plus the R2 queue cover it. Checked live on the Mac: the network came up as `bridge` with `enable_icc=false` and `enable_ip_masquerade=false`, and SDK e2e passed over TCP. See I5 for old daemons. |
| R2 | **closed** (low residual, N1) | Unauthenticated connections no longer hold a slot. They wait in a separate 16-entry `PendingQueue`; when it is full, the oldest is evicted. Deadlines are 2 s (crabd) and 4 s (relay, which waits for crabd's verdict), and refusals are logged at most once per second. **Probe** on `kvm-dedicated`: a real `crabd serve --auth-token-file` behind `crabd relay`, with Python flooders on loopback and a legitimate client authenticating and sending `ping` 200 times. Results: no flood 200/200 (p50 1.3 ms); churn flood at about 10k, 13k and 24k connections/s 178, 186 and 165/200; hold flood at 13.5k and 19k connections/s 199 and 196/200; p50 of successful attempts below 1.3 ms throughout. So the flood can fail individual attempts but cannot keep the client out, and the host's readiness and reconnect retries absorb that. The old attack (64 idle sockets → total starvation) is gone. Eviction is the right trade-off: refusing new connections when full would let a flood that keeps the queue full lock the client out for good. |
| R3 | **closed** | Every TCP/UDP fragment (MF set or offset ≠ 0) and every TCP/UDP packet with fewer than 8 L4 bytes is dropped as `Denial::Fragment`. ICMP may still fragment, and its destination is still checked. Tests cover the tiny first fragment, the follow-up fragment, DF alone, and ICMP. |
| R4 | **closed** on Linux; **partially** on macOS (documented) | Linux: libkrun and libkrunfw are `dlopen`ed through `/proc/self/fd/N`, and the netstack and helper are executed through their verified CLOEXEC fd (the kernel resolves the path before closing the fd). libkrun's leaf-name lookup is bound to the loaded libkrunfw by `RTLD_NOLOAD` handle identity. passt's `.avx2` re-exec is closed: the variant is pinned and executed directly, and an unpinned sibling is refused (exit 71). macOS: libraries and disks go through `/dev/fd/N`, which works. The netstack and helper are still executed by path after the fd check, because macOS cannot exec an fdesc path. That needs a same-user writer in a 0700 tree and is documented in design §12.4. |
| R5 | **closed** | Decision (a) is implemented for libkrun and Firecracker. The image is `0444`, `rootfs_stamp` is stored in the cache manifest, and every spawn runs `open_image`: an `fstat` on a match, otherwise a re-hash through the same fd, then a re-stamp or a `DigestMismatch`. The libkrun helper re-checks on its own fd and hands libkrun that fd. Firecracker gets `/proc/<pid>/fd/N`, held until `configure_and_start` (see I1). `tampered_rootfs_is_refused` passed on both hosts. |
| R6 | **closed** | Guest hardening now fails closed for **every** uid, which is stricter than the fix suggested. A failed sysctl write (`Missing`/ENOENT is still accepted), a failed `/proc`/`/sys` seal, no pids cap at all, or uid 0 with only `RLIMIT_NPROC` makes `init.configure` refuse before anything is mounted. A failed cgroupfs seal kills `crabd serve` before the host connects. Brick risk is low: absent paths are skipped (`seal_readonly_path` returns `Ok(false)`), and both pinned guest kernels pass the microVM suites. A future kernel or template that breaks a hardening step now fails loudly instead of silently. That is the right default, but the e2e suites must run on every kernel pin bump. |
| R7 | **closed** | `memory.swap.max = 0` is set on the libkrun and Firecracker leaves, and skipped when absent (no swap accounting). The `doctor` check is deferred (see the backlog). |
| R8 | **closed** | `PR_SET_PDEATHSIG` was removed; the stdin lifeline is the only owner-death mechanism. `vm_survives_the_thread_that_spawned_it` passed on both hosts. |
| R9 | open (low) | Out of scope for these steps. Documented in design §8; still missing from `hosting.md`. |
| R10 | **partially closed** | Hardened runtime now gets `disable-library-validation` (`entitlements-hardened.plist`), and libkrunfw is bound by `RTLD_NOLOAD`, so the `DYLD_*` stripping no longer matters. **Verified on the Mac** with an ad-hoc signature plus hardened runtime (`SANDCRAB_SIGN_RUNTIME=1`; `codesign` flags `adhoc,runtime`): `base_boot_hello_bash_and_destroy`, `tampered_libraries_are_refused` and `userspace_network_reaches_internet_not_the_host` passed 3/3, and the normal helper was reinstalled afterwards. A real Developer ID with notarization is still untested. |
| R11 | info, unchanged | Capability flags are still honest. |
| R12 | **closed** | An installed netstack now beats the deprecated TSI opt-in; the opt-in is logged at error level and ignored. TSI applies only when no netstack is installed. Covered by `fake_helper::network_policies`. The `doctor` check is deferred. |
| R13 | open (info) | Documented in design §9 (DNS through the host resolver). |
| R14 | **closed** | `secure_state_dir` refuses a symlink, a non-directory, or another owner, and tightens a looser mode to 0700 with a warning. |
| R15 | open (info) | Unchanged. |
| R16 | **closed** | `fake_vmm` now uses `/tmp/scfcXXXX`. On the Mac, with its long `TMPDIR`, `fake_vmm` passed 7/7; 2 Linux-only cases print `skipped`. |
| R17 | info, unchanged | |
| R18 | **partially closed** | New state directories are created 0700 whatever the umask (Rust and shell installers), and the helper now has an install record (`<helper>.pin.json`, sha + stamp) that is checked before every launch. However, `check_private_dir` is implemented but **not wired**, and `create_private_dir_all` does not tighten existing directories. So a pre-existing group-writable tree (the shared `~/.sandcrab` on `kvm-dedicated` is `0775`, `bin/` included) still lets a group member rewrite **both** the helper and its `.pin.json`, which sit in the same directory. This is harmless with per-user groups (the Ubuntu default). It is the first follow-up. |
| R19 | **closed** | `--host-label` now also replaces `host.hostname` in the JSON. A test asserts that the real name is absent. |
| K1 | **closed** | The pty test accepts `/dev/tty*` (it passed in the Mac workspace run). |
| K2 | **closed** (by inspection) | `docker_config()` and the Docker arm are behind `feature = "docker"`; a Docker arm without the feature prints SKIP. The `--no-run` build was stopped by the scope change. |
| K3 | **closed** | `guest_split` falls back to the smallest anonymous mapping larger than the guest. Unit-tested; the bench smoke was not re-run here. |

### Focus areas checked

- **Stamp / verified-fd TOCTOU.** Each consumer opens once, `fstat`s that fd, hashes with `pread` on that fd if
  needed, and then uses the fd itself or `/proc/self/fd/N` (`/dev/fd/N`). Nothing is re-resolved by path, except on
  macOS as noted under R4. The ctime argument holds for a non-root attacker on a local filesystem (no syscall sets
  ctime, and rename-over changes the inode). The 100 ms racy margin, plus a second `fstat` after hashing, prevents
  same-tick stamps. Limits (root, NFS/FUSE/FAT, bit rot) are documented in `stamp.rs` and design §12.4. A `dev`
  change after a reboot only costs one re-hash.
- **`/proc/<pid>/fd/N` handed to Firecracker.** Opening it requires PTRACE_MODE_READ on the backend process: the
  same uid and a dumpable process. Yama `ptrace_scope` only restricts ATTACH, and `/proc/<pid>/fd` is 0500 for that
  uid, so other users cannot open it. See I1.
- **dlopen by fd and the libkrunfw leaf.** `check_leaf_is_loaded` requires the `RTLD_NOLOAD` handle to equal the
  handle of the verified image. glibc matches it by `DT_SONAME`, dyld by the rewritten install name. It passed live on
  Linux, on macOS, and on macOS with the hardened runtime, where `DYLD_*` is stripped. The handle reference counting is
  correct (the extra reference is released).
- **`.pin.json`.** It is written by `sandcrab libkrun install` after the sign and the rename. A same-user attacker can
  rewrite both the binary and the record. That is accepted: the same user already owns the user's environment, and
  the record defends against accidental and group writes, which depends on R18. Without a record, the helper is
  trusted on first use once per process, with a warning.
- **Relay network.** Covered under R1.
- **Pre-auth eviction.** Covered under R2 and N1.
- **Fail closed.** Covered under R6.
- **Quarantine clearing.** It runs only when `verify_pins` is on, right after the pin check of that same path, and
  only on the main netstack binary, in `KrunLibs::check` (once, at backend construction). The removal goes by path
  (`removexattr` follows symlinks) rather than through the verified fd (N3).
- **Netstack readiness probe.** It is bounded at 10 s, with a 250 ms re-probe, and fails at once if the child exits.
  The probe is a single ARP request for the gateway from the guest MAC, on the helper's own socketpair, so it adds no
  listener and no new peer. Any frame back counts as ready. A late reply queues for the guest NIC, which ignores an
  unsolicited ARP reply. The gvproxy tests passed on the Mac.

### Merge conflict resolutions (`054b9f9`)

`git show --remerge-diff 054b9f9`: both conflicts are adjacency and doc-comment only.

- **`backend.rs`:** `check_quarantine` was kept next to `verify_pinned`/`HelperPin`.
- **`netstack.rs`:** the `start()` doc now describes the verified-fd/`argv0` exec and the readiness wait.

No logic was lost. `start()` takes `binary` (the fd path) plus `argv0`, then calls `wait_ready`. In the helper,
`start_netstack` keeps the verified fd open until `netstack::start` has returned, so the fd outlives the readiness
wait. `check_quarantine` runs after the stamp verification in `KrunLibs::check`. `093cb12` differs from `054b9f9`
only in the flow yaml.

### New findings

| ID | Sev | Location | Scenario | Suggested fix |
| --- | --- | --- | --- | --- |
| N1 | low | `crates/sandcrab-guest/src/preauth.rs` (`PendingQueue::admit`) | Residual of R2. A local peer that sustains a connect flood of more than about 16 connections per auth round-trip (≈10k–24k/s on loopback in the probe) evicts a fraction of the legitimate attempts: 0.5–18% in the probe. Each failed attempt is retried, so the result is extra latency, not starvation. It costs the attacker a full CPU. | Accept as is. Optional: give each pending connection a short minimum grace (~5 ms) before it can be evicted, and/or count evictions in `doctor`/metrics. |
| N2 | low | `crates/sandcrab-rootfs/src/stamp.rs` (`verify_open`) | A stamp hit returns `Stamp` without comparing `expected_sha256` to anything (by design; see the `record_then_stamp_hit_reads_nothing` test). So the stamp is not bound to the digest it was recorded with. Where the expected digest is compiled in (unmodified release assets: gvproxy, libkrun/libkrunfw without fixups), anyone who can write the manifest can point its stamp at a different file, and the compiled-in pin is never consulted. That needs write access to the 0700 artifacts tree, which already holds the helper, so it adds nothing for a same-user attacker. But it is a silent downgrade from "compiled pin" to "manifest trust". | Record the digest with the stamp (`stamp: {…, sha256}`) and accept a stamp hit only when that digest equals `expected_sha256`. That is one string compare. |
| N3 | low | `crates/sandcrab-backend-libkrun/src/netstack.rs` (`clear_quarantine`), `backend.rs` (`check_quarantine`) | The quarantine flag is removed by **path** (`removexattr`, which follows symlinks), after the verifying fd has been closed. A writer in the artifacts dir who swaps the path in that gap gets `com.apple.quarantine` cleared on an unverified file, which bypasses the Gatekeeper prompt for it. That again needs a same-user writer in a 0700 tree. Also, both the error text and the `wait_ready` hint say that `sandcrab libkrun install` clears the attribute, which it does not yet (that is deferred). | Use `fremovexattr` on the verified fd. Fix the hint text until install-time clearing lands. |
| I1 | info | `crates/sandcrab-backend-firecracker/src/backend.rs` (`verified_root`) | Firecracker opens the root drive through `/proc/<backend pid>/fd/N`. That breaks (fails closed) if the embedding process is non-dumpable (`PR_SET_DUMPABLE 0`, or setuid/fscaps) and with the planned jailer (different uid plus chroot). | When the jailer lands, pass the fd into the Firecracker child (no CLOEXEC on that fd only) and give it `/proc/self/fd/N`, or hard-link the verified inode into the jail. Note it in the Firecracker design. |
| I2 | info | `crates/sandcrab-backend-libkrun/src/backend.rs` (`HelperPin`) | The helper pin is cached per process. Reinstalling the helper while a long-lived process runs makes every later spawn there fail with `DigestMismatch` until restart, and the message suggests re-running install, which is what caused the mismatch. | On a mismatch, reload `.pin.json` once before refusing, or say "restart the process" in the message. |
| I3 | info | `crates/sandcrab-rootfs/src/cache.rs` (`open_image`) | For a legacy manifest without a stamp, the first use `chmod`s the image to 0444. On a cache owned by another user (a shared read-only cache) that fails, and the spawn fails with it, instead of falling back to a hash without a stamp. | Ignore `EPERM` from the chmod and just hash (no stamp is recorded). |
| I4 | info | design §12.4 | The macOS exec-by-path windows (netstack, helper) are documented. Nothing to do until macOS offers fd-based spawn. | None. |
| I5 | info | `crates/sandcrab-backend-docker/src/backend.rs` (`ensure_relay_network`) | Docker daemons older than 25 allow duplicate network names. Two concurrent first spawns can then create two `sandcrab-relay` networks; later name inspects are ambiguous, and spawns fail closed. | Look the network up by label plus name, and use the oldest, or document Docker ≥ 25. |

### Deferred items: assessment

| Item | Verdict |
| --- | --- |
| `doctor` check for `memory.swap.max` and for TSI precedence | Fine as follow-ups. The runtime behaviour is already correct (swap is capped at leaf creation; TSI is ignored, with an error log, when a netstack exists). |
| Quarantine clearing at install time | Fine as a follow-up. The backend clears it at construction after the pin check, and `curl`/`reqwest` downloads are not quarantined anyway. Fix the misleading hints (N3). |
| `check_private_dir` not wired for artifact dirs | **Follow-up, first priority**, not blocking. Without it, R18 stays open for existing group-writable trees (see R18). It is cheap: call it at backend construction for the state root, `bin/` and `artifacts/` (and refuse, or tighten, like `secure_state_dir`). |
| fs-verity | Fine as a follow-up (an optional fast path; stamps already remove the cost). |

### Gate

Scope changed mid-review (coordinator instruction): the full integrated gate was dropped, because `093cb12` has the
same code as `054b9f9`, on which the review-fixes step already ran the full gate. **Relied on (reported by the
review-fixes step, tree `054b9f9`):**

- **Linux:** 765 unit tests, SDK e2e 18/18 on each of docker, firecracker, libkrun and docker-tcp, Docker e2e 7/7 on
  unix and tcp, Firecracker 6/6, libkrun_vm 16/16, fake_helper 14/14, bench smoke.
- **Mac:** 737 unit tests, libkrun_vm 15/15.

**Run by this review on `093cb12`** (some of it before the scope change):

| Host | Run | Result |
| --- | --- | --- |
| `kvm-dedicated` | `cargo fmt --check`, `clippy -D warnings` | pass, pass |
| `kvm-dedicated` | `cargo test --workspace --no-fail-fast` (short `TMPDIR`) | **765 passed, 0 failed, 61 ignored** (59 binaries) |
| `kvm-dedicated` | crabd, helper v2 and CLI release builds; `sandcrab libkrun install` (libkrun from source with NET=1, libkrunfw release, passt and `passt.avx2` pinned and stamped) into a private 0700 state; `doctor` (all, libkrun, firecracker) | all ok (`lk:cgroup` warns outside the user scope, as documented); `lk:helper` reports "install record ok" |
| `kvm-dedicated` | Firecracker e2e `firecracker_kvm --ignored --test-threads=1` | **6/6** |
| `kvm-dedicated` | `libkrun_vm --ignored --test-threads=1` under the user scope plus delegated cgroup | **16/16**, including `tampered_{rootfs,libraries,netstack}_is_refused`, `replaced_helper_is_refused` and `vm_survives_the_thread_that_spawned_it` |
| `kvm-dedicated` | `fake_helper` | **14/14** |
| `kvm-dedicated` | Pre-auth eviction probe (R2/N1) | see R2 |
| Mac (M2) | `cargo test --workspace --no-fail-fast` | **744 passed, 0 failed, 60 ignored** (59 binaries; `fake_vmm` 7/7 with 2 Linux-only skips) |
| Mac | `libkrun_vm --ignored --test-threads=1`, helper reinstalled from this tip (release, signed) | **15/15** |
| Mac | `libkrun_vm` subset with the helper signed ad hoc plus hardened runtime (R10) | **3/3** |
| Mac | SDK e2e libkrun | **18/18** (17/18 at first: `multi_turn_workspace_mutation_cross_template` needs `python-dev`, which was not built on the Mac; pinned, re-run, passed) |
| Mac | SDK e2e docker, `SANDCRAB_DOCKER_TRANSPORT=tcp` (the gap) | **18/18** (the same `python-dev` gap, closed the same way). Pruned afterwards: the `python-dev` image, pin and rootfs, stale rootfs cache entries, and the build cache |

Stopped by the scope change: the SDK e2e and Docker e2e reruns on Linux, `fake_vmm` with a long `TMPDIR` on Linux,
the K2 `--no-run` build, and the bench. Everything on `kvm-dedicated` was removed afterwards (source copy, target,
state, scratch). No containers, VMs, netstacks, cgroup leaves or images were left on either host.

**Spawn p50s.** Not re-measured, since the bench was dropped by instruction. The latest figures are from step
`lk-verified-fd` (design §12.4: bench smoke on `kvm-dedicated`, warm `spawn_ready` p50): libkrun 427.9 → **238.0 ms**
(the per-spawn library hash is gone), Firecracker 714.9 → **726.9 ms** (unchanged within noise). The Docker p50 was
not reported there. The review-fixes step's bench smoke is reported but its figures were not handed over.

### Consolidated follow-up backlog

1. **R18:** wire `check_private_dir` (or tighten like `secure_state_dir`) for the state root, `bin/` and `artifacts/`
   at backend construction.
2. **N2:** bind each stat stamp to its digest, so a stamp hit cannot sidestep a compiled-in pin.
3. **N3:** clear quarantine with `fremovexattr` on the verified fd; fix the hints; add install-time clearing (deferred).
4. **R10:** test a real Developer ID plus notarization (ad-hoc hardened runtime is verified).
5. **`doctor`:** checks for `memory.swap.max` (R7) and TSI-vs-netstack precedence (R12).
6. **I1:** a fd-passing scheme for Firecracker drives before the jailer lands.
7. **R9:** clear setuid/setgid on virtio-fs shares, or refuse group-/world-accessible share roots; add the risk to
   `hosting.md`.
8. **N1, optional:** a minimum grace period before eviction, and eviction counters.
9. **I2, I3, I5:** helper-pin reload on mismatch, the legacy-manifest chmod fallback, duplicate relay networks on old
   daemons.
10. **fs-verity** fast path (optional).
11. **R13, R15, R17:** docs only, unchanged.
