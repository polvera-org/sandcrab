# Changelog

All notable changes to Sandcrab. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Phase 1 (V1) and the libkrun backend flow (Phase 2, partial). Nothing has been released yet.

### Breaking changes (upgrade notes)

Stop every running `sandcrab`, `crabagent` and bench process before upgrading, then:

- **Protocol 0.2, connection auth.** Host and `crabd` must come from the same build: rebuild `crabd`
  (`scripts/build-crabd.sh`). The Docker backend starts `crabd` with `--auth-token-file`, which older builds do not
  have. See [docs/protocol.md](docs/protocol.md#authentication-auth).
- **libkrun helper protocol v2.** A `sandcrab-krun` helper and a backend from different builds refuse each other:
  re-run `sandcrab libkrun install --helper <path>` after every rebuild.
- **Stamped, read-only rootfs caches** (Firecracker, libkrun). Images are now `0444` with a stat stamp in their
  `manifest.json`. Older entries are converted on first use; a cache owned by another user fails that first use
  (rebuild it as its owner).
- **Host-local template pins.** Committed `sandcrab.toml` manifests no longer carry image IDs. Hosts with existing
  images run `sandcrab template pin --all` once (or rebuild with `templates/build.sh`).
- **No user namespaces in microVM sandboxes** (guest hardening, as on Docker): rootless container tools and `bwrap`
  do not work there, and Chromium-based browsers need `--no-sandbox`.
- **Unpinned template images are refused** on libkrun and Firecracker (Docker already refused them unless
  `--allow-unpinned`).
- **`DockerDev` is renamed `Docker`**; the old type names remain as deprecated aliases (see Changed).

### Added
- **libkrun backend** (`sandcrab-backend-libkrun`), hardware-isolated microVMs on Apple Silicon macOS
  (Hypervisor.framework) and Linux x86_64 KVM ([design](docs/design/libkrun-backend.md)). One signed `sandcrab-krun`
  helper process per sandbox becomes the VMM (JSON `HelperConfig` on stdin, stdin as the lifeline); libkrun and
  libkrunfw are `dlopen`ed at runtime from SHA-256-pinned paths (no link-time dependency; the crate builds and tests
  without libkrun). ext4 block rootfs from the shared pipeline plus a per-sandbox scratch disk; `crabd init` as
  PID 1; control and tool ports over vsock mapped to host Unix sockets. **Live virtio-fs workspaces** (`rw` / `ro`,
  read-only enforced by the host server, share roots pinned by fd), the sandbox user defaulting to the host uid:gid.
  Published ports over `crabd` tunnels on `127.0.0.1`. TSI is never enabled implicitly. Linux: per-helper cgroup v2
  leaf under a delegated parent (`SANDCRAB_LIBKRUN_CGROUP`; `memory.max` = guest RAM + 128 MiB, `pids.max` 64).
  Capabilities: `hardware_isolated: true`, `multi_tenant_safe: false`, `supports_host_dir_mounts: true`.
- libkrun `network.policy = unrestricted` without TSI (design §9.1). The helper attaches a virtio-net NIC
  (`krun_add_net_unixstream`, DHCP by `init.krun`) to a pinned userspace netstack (passt on Linux, gvproxy on macOS)
  through its own default-deny egress filter: host loopback (including gvproxy's `192.168.127.254` alias), the host's
  own addresses and on-link subnets, private / metadata / reserved ranges and SMTP are dropped; the gateway serves
  only DNS and DHCP. The netstack runs as the user with no listening socket and exits with the helper; the helper
  waits for it to answer before the VM boots and fails the spawn if it stays silent. Without it, `unrestricted` is
  `PolicyUnenforceable` (never a TSI fallback).
- `sandcrab libkrun install` (`--from-source`, `--fw-from-source`, `--force`, `--helper`, `--no-helper`,
  `--no-netstack`, `--dry-run`): pinned libkrun + libkrunfw (Homebrew bottles on macOS, built from the pinned tags
  with `BLK=1 NET=1` on Linux), the pinned netstack, and the signed helper with its install record.
  `scripts/fetch-libkrun.sh` (also `--netstack-only` / `--no-netstack`), `scripts/sign-krun-helper.sh` (ad hoc or
  `SANDCRAB_SIGN_IDENTITY`), `entitlements.plist`. `sandcrab doctor` gains the `lk:*` checks (hypervisor, helper,
  artifacts, crabd, mke2fs, case-insensitive workspace volumes, cgroup, netstack, orphans). CLI `--backend libkrun`
  (alias `krun`); SDK feature `libkrun` (opt-in, `.libkrun(LibkrunConfig)`); CLI default features are now `docker`,
  `firecracker` and `libkrun` (each compiles to nothing off its targets). Setup:
  [docs/hosting.md](docs/hosting.md#2-macos-apple-silicon).
- crabagent: `--backend libkrun` (macOS Apple Silicon, Linux) and `--backend firecracker` (Linux), through the same
  `sandcrab-sdk` cargo features and `cfg` gating as the CLI (a backend not compiled in is a clear error, never a
  fallback). libkrun uses the CLI's config: host-dir workspaces over virtio-fs, `--network unrestricted` through the
  pinned netstack (the deprecated TSI opt-in is not offered). The session's backend line reports the backend's
  declared capabilities. New ignored test `libkrun_e2e`.
- `sandcrab-rootfs` crate: the OCI → ext4 rootfs pipeline, extracted from the Firecracker backend and shared with
  libkrun, now arch-aware (`--arch linux/amd64|linux/arm64`, arch in the cache key). Multi-arch template
  Dockerfiles and `templates/build.sh --platform`; `scripts/build-crabd.sh` cross-compiles `crabd` on macOS with
  `rust-lld`.
- Published ports on the microVM backends: protocol verb `tunnel.open` and `crabd` tunnels to guest loopback ports,
  a core `127.0.0.1` port forwarder (`PortForwards`), wired into Firecracker and libkrun. See
  [docs/protocol.md](docs/protocol.md#tunnels-tunnelopen-port-forwarding).
- Docker backend on macOS (Docker Desktop): a `tcp` transport (`DockerTransport`, default on macOS, `unix`
  elsewhere, override with `SANDCRAB_DOCKER_TRANSPORT=unix|tcp`). `crabd` serves a Unix socket in a per-sandbox
  tmpfs volume and a hardened relay container (`crabd relay`) publishes it on `127.0.0.1` only, with an ephemeral
  port. The sandbox container's network is unchanged, so `disabled` is still network mode `none`. `sandcrab doctor`
  shows the transport; `reap_orphans` also removes relay containers and socket volumes. See
  [docs/hosting.md](docs/hosting.md#23-docker-on-macos-docker-desktop).
- Connection authentication (protocol **0.2**): `crabd serve --auth-token-file` requires an `auth` frame with a
  per-sandbox 256-bit token as the first frame of every connection (tools, reconnects, `tunnel.open`), compared in
  constant time; anything else gets `-32008 UNAUTHORIZED` and the connection is closed. The Docker backend always
  uses it (both transports); the token is passed as a 0400 file that `crabd` deletes, held in memory by
  `sandcrab_core::AuthConnector`, and redacted in `Debug`. The SDK maps `UNAUTHORIZED` to a fatal `unauthorized`
  tool error.
- microVM guest hardening (Firecracker, libkrun), to Docker parity: `crabd init` empties the capability bounding
  set, `crabd serve` sets `no_new_privs`, a cgroup v2 `/sandbox` with `pids.max` 1024, a
  `nosuid,nodev,noexec` `/tmp` tmpfs, core dumps off. Then hardening sysctls before any sandbox code runs
  (`user.max_user_namespaces=0`, `kernel.unprivileged_bpf_disabled=1`, `vm.unprivileged_userfaultfd=0`,
  `kernel.io_uring_disabled=2`, `kernel.perf_event_paranoid=3`, `kernel.kptr_restrict=2`, `kernel.dmesg_restrict=1`,
  `fs.protected_*`, `fs.suid_dumpable=0`), and read-only `/proc/sys` (and Docker's other `ReadonlyPaths`) and
  `/sys`. List in `sandcrab_protocol::init::HARDENING_SYSCTLS`; details in
  [docs/design/guest-seccomp.md](docs/design/guest-seccomp.md) (a guest seccomp profile was evaluated and deferred).
- `sandcrab template pin <names>|--all`: adopt images already present as host-local pins, with a warning.
  `sandcrab_core::pins` (`PinStore`, `TemplatePin`, `effective_pin`), `SandcrabBuilder::template_pins_dir`,
  `SandboxManager::pin_store`.
- Firecracker egress policy in `scripts/fc-net-setup.sh`: outbound SMTP (rejected with a reset) and reserved ranges
  blocked, the uplink's own subnets and gateway treated as private; `--allow-smtp` to opt out.
- Bench: libkrun backend (macOS helper RSS sampler, Linux cgroup/PSS), Firecracker VMM memory and CPU probes,
  `--host-label` (replaces the hostname in the file name and the JSON), smoke / standard / full tiers, and a
  per-host report (`scripts/bench-report-data.py`, `docs/site/bench-report.html`).
- Phase 1 (V1): `crabd` guest tools over length-prefixed JSON-RPC 2.0, persistent workspaces, the Docker backend,
  the Firecracker backend, SDK, CLI, `crabagent`, and the benchmark harness.

### Changed
- Template image pins are **host-local** ([docs/template-pins.md](docs/template-pins.md)). Image IDs do not reproduce
  across hosts, so committed `sandcrab.toml` manifests no longer carry `sha256` / `sha256_by_arch`; a fresh host's
  build used to be refused until someone re-pinned it by hand. `templates/build.sh` now records each built image ID
  under `$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins/<name>/<version>/<arch>.json`
  (pinning is the default; `--no-pin` skips it, `--pin` is a no-op), and spawns verify against that pin. This is local
  integrity ("unchanged since this host built it"), not provenance. Reproducible template content digests are a planned
  follow-up.
- `sandcrab_core::verify_template_image` / `verify_template_image_for` are replaced by
  `verify_image_pin(verifier, image, pin)`; the pin comes from `ResolvedSpec::image_sha256`, which the manager now
  resolves with `pins::effective_pin` (host-local pin, else legacy manifest pin). `ManagerConfig` gains
  `template_pins`.
- The libkrun and Firecracker backends refuse a template rootfs with no pin (they used to warn and continue).
- `sandcrab template list --json`: `sha256` → `pin { image_id, origin, path }`.
- The `DockerDev` backend is renamed to `Docker` and is now an officially supported backend (hardened, shared kernel;
  reports `hardware_isolated = false`, `multi_tenant_safe = false`). `BackendKind::DockerDev` → `BackendKind::Docker`,
  `DockerDevBackend` / `DockerDevConfig` → `DockerBackend` / `DockerConfig` (the old names remain as deprecated type
  aliases; `docker-dev` is still accepted in config, CLI and bench arguments).
- `SandboxError::InsecureBackendRejected` → `SandboxError::HardwareIsolationRequired`, raised when
  `require_hardware_isolation` is set and the selected backend cannot provide it.
- Container label `sandcrab.backend=docker-dev` → `sandcrab.backend=docker` (the reaper still cleans up both).
- libkrun helper protocol: `HELPER_PROTO_VERSION` 2. `PinnedFile` and `DiskSpec` gain `stamp`, `NetstackSpec` gains
  `avx2`. `KrunLibs` gains `netstack: Option<NetstackBin>`, `LibkrunConfig` gains `netstack_dir`, `HelperConfig`
  gains `netstack` (network `userspace`).
- Rootfs cache: images are `0444` and their `manifest.json` records `rootfs_stamp`; libkrun/libkrunfw and netstack
  manifests record `stamp` (see Security).
- Firecracker tap leases live in one host-wide, per-user lock dir (`/run/user/<uid>/sandcrab/taps`, else
  `$XDG_RUNTIME_DIR/sandcrab/taps`, else `$TMPDIR/sandcrab-<uid>/taps`; `TapPoolConfig::lock_dir`), so processes with
  different `SANDCRAB_HOME`s no longer lease the same tap.
- vsock tunnels: `crabd` closes a tunnel on target EOF, and a client's half-close is not forwarded (optional in
  `PortForwarder`), because a host `SHUT_WR` breaks the stream on both microVMs.
- `list_dir` no longer stats entries below the requested depth, and scratch images zero their inode tables at format
  time (faster large-tree workspaces and spawns).

### Deprecated
- libkrun TSI opt-in (`LibkrunConfig::allow_tsi_unrestricted`, CLI `--libkrun-tsi`): still explicit and still exposes
  host loopback; an installed netstack now wins over it. Use the userspace netstack instead.
- In-manifest `[template] sha256` / `sha256_by_arch`: still parsed (`TemplateManifest` stays backward compatible) and
  honored as a fallback when no host-local pin exists (a match passes, a mismatch is refused); `sandcrab doctor` warns.
- `--allow-insecure` and `SANDCRAB_ALLOW_INSECURE`: accepted, hidden from `--help`, print a warning, and do nothing.

### Removed
- `DockerConfig::allow_insecure_dev_backend` and `BackendConfig::Custom { allow_insecure }`. Docker no longer needs
  an opt-in; the backend is still selected explicitly and never chosen as a fallback.

### Security
- Pinned files are verified through the fd that is then used, per spawn, at the cost of an `fstat` (design
  [§12.4](docs/design/libkrun-backend.md)). The rootfs cache makes images `0444` and records a stat stamp (dev, inode,
  size, mtime, ctime) next to each SHA-256 (`rootfs_stamp`); libkrun/libkrunfw and the netstack get the same
  (`stamp`, recorded at install or on first use). A stamp mismatch re-hashes through the same fd, then re-stamps or
  refuses. libkrun hands the helper the stamped root disk, which `krun_add_disk`s it by fd; Firecracker gets
  `/proc/<pid>/fd/<n>` for the root drive. Before, the root disk was trusted unchecked.
- libkrun helper: loads libkrunfw/libkrun with `dlopen("/proc/self/fd/<n>")` (macOS `/dev/fd/<n>`) after
  checking that fd, and binds libkrun's leaf-name lookup of libkrunfw to that copy (`RTLD_NOLOAD`). Removes the
  ~190 ms per-spawn library hash and the check-then-load path race (review R4/R5).
- libkrun netstack: on Linux executed through its verified fd; passt's AVX2 build is installed and pinned
  (`avx2` in the netstack manifest) and executed by the helper itself on AVX2 CPUs; an unpinned `passt.avx2` next to
  the binary is refused.
- libkrun helper binary: `sandcrab libkrun install` records it (`<helper>.pin.json`, SHA-256 + stamp); the backend
  checks it before every launch and on Linux executes it through the verified fd (without a record: trusted on first
  use per process, with a warning) (review R18).
- State directories (`artifacts/`, `bin/`, the netstack tree, `scripts/fetch-libkrun.sh` output) are created `0700`
  whatever the umask (review R18).
- libkrun egress filter drops fragmented TCP/UDP and TCP/UDP packets shorter than an L4 header, so port checks
  (SMTP) cannot be slipped past with a tiny first fragment (review R3).
- An installed libkrun netstack wins over the deprecated TSI opt-in; the opt-in is then ignored with an error-level
  log (review R12).
- libkrun and Firecracker cgroup leaves set `memory.swap.max = 0` (review R7).
- Guest hardening fails closed for every uid: a failed sysctl, read-only seal or pids cap refuses `init.configure`
  before anything is mounted (review R6). `crabd serve` sets `no_new_privs` for every tool process (before, a setuid
  binary in a microVM rootfs could regain root), and `crabd init` reaps orphaned tool processes.
- `crabd` and `crabd relay`: unauthenticated connections no longer hold connection slots; they wait in a 16-entry
  pre-auth queue that evicts the oldest (review R2).
- Docker `tcp` relay: every relay joins a dedicated `sandcrab-relay` bridge with inter-container traffic off and no
  outbound NAT, not the default bridge; a same-named network without those options is refused (review R1).
- Docker backend: the `state_dir` must be a real directory owned by the user (not a symlink); a looser mode is
  tightened to 0700 (review R14).
- libkrun and Firecracker backends, and `sandcrab libkrun install`, secure the existing state tree at construction:
  the state root and every directory down to the helper, libraries, netstack, kernel, caches, run and recovery dirs.
  A symlinked root or a foreign owner is refused; a group/world-writable directory owned by the user (e.g. a `0775`
  `~/.sandcrab` made under umask `0002`) is tightened to `0700` with a warning (review R18).
- Stat stamps record the digest they vouch for; a stamp hit counts only for that digest, so a stamp copied from
  another file cannot stand in for a pin (review N2). This is **libkrun helper protocol v3**: re-run
  `sandcrab libkrun install --helper <path>`. Older stamps re-hash once and are re-recorded.
- macOS netstack quarantine is removed through the verified fd (never by path), at backend construction and now also
  by `sandcrab libkrun install`; `doctor` warns (`lk:netstack`) while it is present (review N3).

### Fixed
- libkrun helper: dropped `PR_SET_PDEATHSIG`, which fired when the *thread* that spawned the helper exited (tokio
  blocking threads retire after 10 s idle) and could kill healthy VMs; the stdin lifeline alone ties the helper to
  its owner (review R8).
- libkrun on macOS with the hardened runtime (Developer ID, review R10): signing with the runtime now uses
  `entitlements-hardened.plist` (adds `com.apple.security.cs.disable-library-validation`), and libkrunfw no longer
  depends on `DYLD_FALLBACK_LIBRARY_PATH`, which the hardened runtime strips. `SANDCRAB_SIGN_RUNTIME=1` tests it ad
  hoc. Signing with a real Developer ID and notarization are still untested.
- libkrun on Linux: libkrun is built with `BLK=1` (and `NET=1`) and its exported symbols are checked before install;
  share roots are passed as `/proc/self/fd/<n>/.` (libkrun opens its root `O_PATH | O_NOFOLLOW`, so `/dev/fd/<n>`
  failed with `EIO`); the cgroup leaf is removed when a VM is dropped without `destroy`; the helper raises its soft
  `RLIMIT_NOFILE` to the hard limit (large multi-mount trees hit `EMFILE`).
- libkrun on macOS: the netstack's `com.apple.quarantine` attribute is cleared after its pin check, so Gatekeeper
  no longer blocks a freshly downloaded gvproxy behind a dialog.
- Firecracker: request/response traffic over vsock tunnels; `fc-net-setup.sh` is idempotent when `DOCKER-USER` rules
  already exist; the rootfs pipeline clears a stale ustar prefix when re-pathing exported entries.
- Bench: `ws_sync_back` measured at teardown for sync-back backends, the Sandcrab home used for Firecracker artifacts,
  per-arch template pins, `sampler_cpu_pct` recorded as `null` where it cannot be measured, libkrun's rounded-up
  guest RAM region matched in `smaps` (K3), the inverted `mem.vmm_overhead` skip reason.
- `scripts/build-crabd.sh` honours `CARGO_TARGET_DIR`.
