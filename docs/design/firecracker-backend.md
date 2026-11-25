---
title: "Design — sandcrab-backend-firecracker (V1)"
status: proposed
date: 2026-11-23
author: Turing (architect)
flow_step: fc-design (docs/plans/phase-1-mvp.flow.yaml)
related: [docs/decisions/0001-phase-1-decisions.md, docs/plans/phase-1-mvp.bench-spec.md]
---

# Firecracker backend design (V1)

The goal is a hardware-isolated `RuntimeBackend` that runs the **same `crabd` and the same protocol** as
the Docker backend. It must be honest about its capabilities and cheap enough to benchmark fairly against Docker on
`kvm-homelab`. Upstream references are paths in `firecracker-microvm/firecracker` (`main`, release
**v1.18.0**, 2026-11-09).

## 0. Decisions at a glance

| # | Topic | V1 decision |
|---|---|---|
| 1 | Kernel | Firecracker **CI guest kernel 6.18** (`microvm-kernel-ci-x86_64-6.18.config`), downloaded once and pinned by SHA-256. No own build |
| 2 | Rootfs | `docker export` tar → `mke2fs -d <tar>` (rootless) → **shared read-only base** + per-sandbox **sparse scratch drive** + **overlayfs in the guest**. Cached by `sha256(image digest ‖ crabd sha ‖ pipeline ver)` |
| 3 | Init | `crabd` is PID 1 (`init` mode, root) and runs a separate `crabd serve` child (sandbox uid). Config arrives over a vsock **control port**, not the cmdline or MMDS. Syscalls go through `rustix` |
| 4 | Transport | vsock through FC's UDS `CONNECT <port>\n` / `OK <n>\n`. Then the generic `CrabdClient<S: AsyncRead+AsyncWrite>`. Ready = first `hello` OK on the tools port |
| 5 | Workspaces | **The host dir stays the source of truth.** Each spawn builds an ext4 image (`mke2fs -d` from a tar we generate). At teardown the guest exports a tar over vsock, the host unpacks it safely into staging and commits it. Read-only images are cached by generation |
| 6 | Network | `disabled` → no NIC. `unrestricted` → a **pre-created tap pool owned by the user** plus a one-time root `nftables` NAT setup. `egress_restricted` → spawn error. Port previews go **over vsock** and never need iptables |
| 7 | Process | One `firecracker` per sandbox under `~/.sandcrab/run/<id>/`. Graceful `init.shutdown` → guest `reboot` → VMM exits. Fallback is `SendCtrlAltDel`, then SIGKILL. Each sandbox gets its own cgroup v2 leaf. Orphans are reaped at startup |
| 8 | Jailer | **Out of V1.** FC's built-in seccomp stays on. `multi_tenant_safe=false` |
| 9 | Bench | The backend emits phase timestamps and cgroup + `smaps_rollup` memory samples, matching the bench-spec metric IDs |
| 10 | API client | A **hand-rolled HTTP/1.1 over a tokio `UnixStream`** client (~200 LOC). No hyper |

Capabilities: `hardware_isolated: true, multi_tenant_safe: false, supports_snapshots: false, supports_host_dir_mounts: false`.
`supports_host_dir_mounts=false` means there is **no live** host↔guest sharing. Host dirs are copied in at spawn and
synced back at teardown.

## 1. Guest kernel

**Decision:** use the Firecracker CI kernel **6.18.x** (the current config header reads `6.18.39-79.141.amzn2023…microvm`) from
the CI artifact bucket (`s3://spec.ccfc.min/firecracker-ci/v1.18/<arch>/vmlinux-6.18.*`). It is fetched by
`scripts/fetch-kernel.sh` and pinned in a repo-tracked `artifacts.lock.toml`:

```toml
# Pinned 2026-11-23 (rootfs-pipeline step). Source of truth: crates/sandcrab-backend-firecracker/src/artifacts.rs
# NOTE: the firecracker-ci/v1.18/ prefix is empty; CI artifacts now live under dated prefixes
# (firecracker-ci/<YYYYMMDD>-<sha>-0/), so the kernel is pinned from the newest one at pin time.
[kernel.x86_64]
version = "6.18.62"
url     = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20261118-6d7cfa9c6b53-0/x86_64/vmlinux-6.18.62"
sha256  = "6b2871bb1e035259a58d1b920fd8951e580125fda17825347bd475911fd2d069"
[firecracker.x86_64]
version = "1.18.0"
url     = "https://github.com/firecracker-microvm/firecracker/releases/download/v1.18.0/firecracker-v1.18.0-x86_64.tgz"
sha256  = "1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd"  # == upstream .tgz.sha256.txt
firecracker_sha256 = "b33f46cd6d88834cf6a4f8d184f08f219809c636236bffd286d233a60602a2ba"  # release-v1.18.0-x86_64/firecracker-v1.18.0-x86_64
jailer_sha256      = "ddadbaa15b5c942271a83b7040ca7259c8ccf92a9e08d44cce0b86fa407ba460"  # release-v1.18.0-x86_64/jailer-v1.18.0-x86_64
```

Rationale:
- `docs/kernel-policy.md` lists guest **6.1** support as ending **2026-09-02**, which has already passed. **6.18** is supported until 2028-06-01 (min FC v1.16.1).
- The CI config `resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config` plus `ci.config` already has everything we need:
  `VIRTIO_VSOCKETS`, `VIRTIO_BLK`, `VIRTIO_NET`, `EXT4_FS`, `OVERLAY_FS`, `TMPFS`, `DEVTMPFS(_MOUNT)`, `UNIX98_PTYS`,
  `IP_PNP` (the kernel `ip=` param handles NIC setup with no netlink code), `SERIO_I8042` (`SendCtrlAltDel`), `CGROUPS`, `VIRTIO_BALLOON`.
- The `doctor` and `fc-backend` startup verify the kernel and `firecracker` SHA-256 before every first use per process.
  A mismatch is a hard error.

Boot args (fixed, not user-extensible):
`console=ttyS0 8250.nr_uarts=0 quiet reboot=k panic=1 pci=off ro root=/dev/vda rootfstype=ext4 init=/sbin/crabd [ip=<guest>::<gw>:255.255.255.252::eth0:off]`.
In bench and default mode the serial console is off (`8250.nr_uarts=0` is listed in `docs/kernel-policy.md` boot-time tips). The `--debug-console` flag
re-enables it.

**Rejected:** building our own kernel in V1 means maintenance plus reproducibility work for no V1 gain. Later we may want a trimmed
config for boot time, or `CONFIG_BPF`/netfilter extras and cgroup controllers for nested Docker. Nested Docker in the guest is
**not a V1 goal**. Also rejected: **bzImage** (v1.17+ supports it on x86). The CI ships vmlinux, so we keep it.

## 2. Rootfs pipeline

```
sandcrab/<tpl>:0.1.0 ──docker create+export──▶ rootfs.tar ──augment(tar)──▶ rootfs+crabd.tar ──mke2fs -d──▶ rootfs.ext4 (ro, shared)
```

1. `docker create` + `docker export` (or `docker save` → flatten) produces a tar. We record the **image digest** (`RepoDigests` / `Id`).
2. A Rust tar rewriter (`tar` crate) appends `/sbin/crabd` (musl static, sha-pinned) plus the required empty dirs
   (`/proc /sys /dev /run /tmp /mnt/scratch /mnt/newroot`) and a stub `/etc/resolv.conf`. Ownership stays root (0:0).
3. `mke2fs -t ext4 -d rootfs.tar -L sc-root -U <uuid5(key)> -E hash_seed=<uuid5(key)> -O ^has_journal -m 0 -b 4096 img <size>`.
   - The **tarball input** to `-d` (e2fsprogs ≥ 1.47.1, `misc/create_inode_libarchive.c`) keeps tar uid/gid/modes **without root**
     and without fakeroot. Debian 13 and Proxmox VE 9 ship 1.47.2. `doctor` checks the version. Fallback: run mke2fs inside a
     throwaway `debian:trixie-slim` container.
   - Size = `ceil(tar_bytes × 1.15) + 64 MiB`, rounded up to 64 MiB. The image is read-only, so no free-space headroom is needed. No journal, because the image is read-only.
4. Cache: `~/.sandcrab/cache/rootfs/<key>/{rootfs.ext4, manifest.json}` with `key = sha256(image_digest ‖ crabd_sha256 ‖ PIPELINE_VERSION)`.
   `manifest.json` records `rootfs_sha256`, which is checked on load. A fixed UUID and hash seed make the build *mostly*
   reproducible. We pin **integrity** (the sha recorded at build). Bit-for-bit rebuilds are best-effort (set `E2FSPROGS_FAKE_TIME`).

**Writable layer, decided: shared ro base plus overlayfs on a per-sandbox scratch drive.**

| Option | Per-spawn cost | Notes |
|---|---|---|
| Per-sandbox full copy | ~1–3 s for 1–2 GiB on ext4 (no reflink) | Rejected. Proxmox root is often ext4 or LVM. Reflink exists only on xfs/btrfs/zfs≥2.2 |
| Reflink copy | ~ms | Rejected as the default because it is FS-dependent. It can come back as an optimisation |
| **ro base + scratch + overlayfs** | **~2–5 ms** (sparse copy of a cached, pre-formatted blank ext4) | The base page cache is shared across sandboxes on the host, which helps density. Chosen |

The scratch drive is created by a sparse copy (`SEEK_DATA`/`SEEK_HOLE` via `rustix`) of `cache/blank/<disk_mb>-v<format>.ext4`.
That image is made once with `mke2fs -O ^has_journal -E lazy_itable_init=0,lazy_journal_init=1`. Its logical size is
`ResourceLimits.disk_mb` and its physical size is a few MiB (mke2fs zeroes the inode tables by punching holes). Zeroing
them at format time keeps the guest's `ext4lazyinit` thread idle: with lazy init, it zeroed them in the first ~5 s after
every boot, which cost ~0.04 CPU-s/s on the Firecracker main (device-emulation) thread. The per-sandbox copy is deleted
at teardown.

## 3. `crabd` as PID 1

The same binary has two roles. Role selection: `getpid()==1` → **init**. Otherwise subcommands run (`serve`, …).

**init (root, PID 1)**, in order:
1. Mount `proc`, `sysfs`, and `devtmpfs` (already auto-mounted with `DEVTMPFS_MOUNT`, so mounting is idempotent).
2. Find drives by **virtio-blk ID** (`/sys/block/vd*/serial` = FC `drive_id`). The fallback is the ext4 superblock label at offset 1024+120.
   Mount the scratch drive at `/mnt/scratch`, then overlay (`lowerdir=/`, `upperdir=/mnt/scratch/upper`, `workdir=/mnt/scratch/work`) at `/mnt/newroot`.
   Move `/proc /sys /dev` into it, `pivot_root`, and lazy-unmount the old root.
3. Mount `devpts` (`/dev/pts`, `ptmxmode=0666`), `tmpfs` at `/dev/shm` and `/run` (small, `size=` 5% of RAM), and at `/tmp`
   the Docker backend's tmpfs: `nosuid,nodev,noexec`, `size=256m`, `mode=1777` (`SANDBOX_TMP_SIZE_MB`). Then the fork-bomb cap:
   mount cgroup v2 at `/sys/fs/cgroup` (libkrun's `init.krun` already did), enable `pids`, and create `/sandbox` with
   `pids.max` = 1024 (`SANDBOX_PIDS_MAX`, Docker's `pids_limit`). Without a `pids` controller the fallback is `RLIMIT_NPROC`,
   which the kernel never applies to uid 0.
   `crabd serve` is forked on a thread whose capability bounding set was emptied first (as Docker's `cap_drop: ALL`), while
   PID 1 is briefly in `/sandbox`, so the child is born capped. Then the cgroupfs is remounted read-only.
   Before any of that can happen (at boot, right after the pids cgroup), PID 1 writes the hardening sysctls
   (`HARDENING_SYSCTLS`: no user namespaces, unprivileged `bpf`/`userfaultfd`/`io_uring`/`perf` off, `kptr_restrict`,
   `fs.protected_*`, ...) and makes `/proc/sys` (plus Docker's other `ReadonlyPaths`) and `/sys` read-only. See
   [guest-seccomp.md](guest-seccomp.md).
4. Bring `lo` up with a single `RTM_NEWLINK` via `netlink-packet-route` + `netlink-sys` (safe APIs, guest-only deps). `eth0` was already configured by the kernel `ip=`.
5. Listen on **vsock port 1 (control)**. This takes framed JSON-RPC, as below.
6. Install `SIGCHLD` handling. A reaper loop runs `waitpid(-1, WNOHANG)`. Zombie handling is **split**: `crabd serve` reaps its own
   direct children (tokio `Child::wait` keeps working), and init only reaps orphans that are reparented to PID 1. This avoids the classic
   "PID 1 steals the exit status" bug.
7. Call `reboot(RB_DISABLE_CAD)`. Ctrl-Alt-Del then becomes `SIGINT` to PID 1, which is treated as `init.shutdown{grace_ms:500}`.

Control methods (init-only extension. Same framing and envelope as `sandcrab-protocol`, under an `init.` prefix, never exposed to agents):

```rust
#[derive(Serialize, Deserialize, JsonSchema)] #[serde(deny_unknown_fields)]
pub struct InitConfigure {
    pub hostname: String,
    pub user: UserSpec,                      // uid, gid, home, from template manifest `[template] user`
    pub env: BTreeMap<String, String>,       // never on the kernel cmdline: may carry secrets later
    pub default_cwd: PathBuf,
    pub mounts: Vec<GuestMount>,             // { drive_id, guest_path, read_only }
    pub dns: Vec<IpAddr>,                    // written to /etc/resolv.conf (overlay)
    pub tools_port: u32,                     // 1024
}
// init.configure → mounts ws drives (ext4, rw: "noatime"; ro: MS_RDONLY + "noload"), mkdir -p guest paths,
//                  spawns `crabd serve --vsock 1024` with Command::uid/gid/env/current_dir (safe std APIs),
//                  returns { serve_pid, boot_ms (from /proc/uptime) }.
// init.export { drive_id } → JSON ack, then connection switches to raw tar bytes until EOF.
// init.shutdown { grace_ms } → SIGTERM all, wait, SIGKILL, sync, umount -R, reboot(RB_AUTOBOOT) (exits the VMM).
```

`guest_path` is validated **again** in the guest: absolute, normalised, no `..`, and not under `/proc /sys /dev /run /mnt/scratch`.

**Config channel choice:** the vsock control RPC. **Rejected:** kernel cmdline (4 KiB limit, visible in `/proc/cmdline`,
and fixed before boot). **Rejected:** MMDS (needs a NIC and the MMDS netstack config even when the policy is `disabled`, plus an
HTTP client in the guest). **Rejected:** config drive (an extra block device and its own format). The cost is one vsock round-trip (<1 ms).

**`unsafe_code = "forbid"`:** all syscalls go through **`rustix`** (features `mount`, `system`, `process`, `fs`, `net`), using
`mount`/`move_mount`, `pivot_root`, `sethostname`, `reboot(RebootCommand::{Restart, CadOff})`, `waitpid`, and `openat2`.
The vsock listener is `tokio-vsock` (safe API, the dep already planned in `guest-vsock-init`). **Rejected:** `nix`. It is equally safe but has a wider surface and
duplicates `rustix`, which the tokio/`tokio-vsock` tree already pulls in. One syscall crate is enough.

## 4. Transport and readiness

Host side, per `docs/vsock.md` §Host-Initiated Connections: `connect(AF_UNIX, ~/.sandcrab/run/<id>/v.sock)`, send `CONNECT 1024\n`,
read one line, and expect `OK <hostport>\n`. If nothing listens in the guest, Firecracker closes the stream.

```rust
pub async fn vsock_connect(uds: &Path, port: u32) -> io::Result<UnixStream>;  // sandcrab-backend-firecracker::vsock
// handshake: bounded read (≤32 B) of one line; anything ≠ "OK <u32>\n" → io::ErrorKind::ConnectionRefused
let client = CrabdClient::new(vsock_connect(&uds, 1024).await?);              // core: generic over AsyncRead+AsyncWrite+Unpin+Send
```

Readiness loop: (a) the control port loops on `CONNECT 1` with backoff starting at 2 ms, ×1.5, capped at 20 ms, and a total deadline of
`boot_timeout` (default 10 s). Then (b) `init.configure`, then (c) `CONNECT 1024` + `hello` with the same backoff.
**`t_ready` is the moment the first successful `hello` response is parsed.** This is exactly bench-spec `spawn_ready` t1. Docker
uses the same definition, so the comparison is apples-to-apples.

## 5. Workspaces: the key decision

Requirements: host mutation between turns, multi-mount rw/ro, `default_cwd`, traversal safety, and the **same workspace
usable by Docker and FC** (cross-backend handoff, plus the shared e2e suite).

| | (a) ext4 image is the truth | (b) host dir is the truth, image per spawn | (c) hybrid |
|---|---|---|---|
| Host mutation | needs `debugfs`/`fuse2fs`/loop-mount | plain FS ops | plain FS ops |
| Host parses a guest-written fs | **yes** (untrusted ext4 → e2fsprogs or kernel = attack surface) | **no** (guest tar → our Rust unpacker) | no |
| Docker interop | needs a conversion | native | native |
| Spawn cost | ~0 | O(size): `mke2fs -d` | ~0 when unchanged |
| Teardown cost | ~0 | O(size): export + commit | O(size) |
| Crash semantics | guest writes land live | the turn's writes are lost (image kept for recovery) | same as (b) |

**V1: (b), with (c)'s read-only cache.** Rationale: (a) puts a hostile filesystem parser on the host, and it forks the workspace
model between backends. (b) keeps `WorkspaceBacking::{HostDir, ManagedDir}` as the one truth. Spawn and teardown costs become
explicit, measurable phases (`ws_attach`, `ws_sync_back` in bench-spec) and are not hidden in boot.

Mechanics:
- **Spawn, rw mount:** the registry takes an **exclusive rw lease**. `archive::pack_tree(root)` walks the tree with `openat2(RESOLVE_BENEATH|NO_SYMLINKS)`. It never follows
  symlinks (they are stored as symlinks), skips special files, and normalises uid/gid to the template user. The result is `ws.tar`, then
  `mke2fs -d ws.tar` into `run/<id>/ws-<n>.ext4`. Size = `max(2 × content, content + 1 GiB, workspace.quota)`, sparse, journaled.
  Attached with `is_read_only=false`.
- **Spawn, ro mount:** a **shared lease**. The image is cached at `workspaces/<id>/ro-<generation>.ext4` and **reused across sandboxes** while the generation
  is unchanged. Attached with FC `is_read_only=true`, which the VMM enforces, plus a guest `ro,noload` mount.
- **Teardown, rw:** `init.export{drive_id}` streams a tar over vsock. `archive::unpack_safe` writes into `workspaces/<id>/.staging-<gen>`
  via `openat2(RESOLVE_BENEATH)`. It rejects absolute paths, `..`, hardlinks outside the staging dir, devices, and setuid bits, and enforces size and entry caps.
  `commit` then runs: **ManagedDir** → atomic `rename` swap (old dir removed in the background). **HostDir** → in-place mirror-apply
  (non-atomic, documented). Then `generation += 1`. If the export fails (guest crash or timeout), nothing is committed,
  the workspace is flagged `dirty_session`, and `ws-<n>.ext4` is kept under `workspaces/<id>/recovery/` for a manual
  `sandcrab workspace recover` (which extracts it in a throwaway FC VM, never on the host).
- **Host mutation API** (all backends): writes are refused with `WorkspaceBusy` while an rw lease is held. Every successful
  write bumps `generation`. Out-of-band edits to a `HostDir` are detected by a cheap stat-walk fingerprint (max mtime/ctime,
  entry count) taken when the ro cache is consulted.

Expected costs (bare metal, NVMe, warm page cache; to be confirmed by `bench-run`):

| Workspace | pack+mke2fs (`ws_attach`) | export+unpack+commit (`ws_sync_back`) |
|---|---|---|
| W1 small repo, ~5k files / 50 MiB | ~80–200 ms | ~100–250 ms |
| W2 `node_modules`-class, ~50k files / 500 MiB | ~1.5–4 s | ~2–5 s |
| ro, cached generation | ~0 (attach only) | — |

A V1.1 lever, if W2 numbers hurt: incremental sync-back. The host sends a `(path,size,mtime)` manifest in `init.configure`, and the guest
exports only changed entries plus a deletion list. A further lever is a cached rw image per generation (option (c) proper).

Core API needed (in `sandcrab-core::workspace`):

```rust
pub struct SafeRelPath(PathBuf);                // validated: relative, normal components only, no `..`
impl TryFrom<&Path> for SafeRelPath { … }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace { pub id: WorkspaceId, pub name: String, pub backing: WorkspaceBacking,
                       pub generation: u64, pub quota_mb: Option<u64>, pub state: WorkspaceState,
                       pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc> }
pub enum WorkspaceState { Idle, LeasedRw { sandbox: SandboxId }, LeasedRo { holders: u32 }, DirtySession }

#[async_trait]
pub trait WorkspaceRegistry: Send + Sync {
    async fn create(&self, name: &str, backing: BackingRequest) -> Result<Workspace>;
    async fn get(&self, r: &WorkspaceRef) -> Result<Workspace>;
    async fn list(&self) -> Result<Vec<Workspace>>;
    async fn delete(&self, r: &WorkspaceRef) -> Result<()>;
    // host-side mutation between turns (bumps generation; WorkspaceBusy under an rw lease)
    async fn write_file(&self, r: &WorkspaceRef, p: &SafeRelPath, data: &[u8]) -> Result<()>;
    async fn read_file(&self, r: &WorkspaceRef, p: &SafeRelPath) -> Result<Vec<u8>>;
    async fn remove(&self, r: &WorkspaceRef, p: &SafeRelPath) -> Result<()>;
    async fn import_dir(&self, r: &WorkspaceRef, src: &Path, dst: &SafeRelPath) -> Result<()>;
    // lifecycle for backends
    async fn lease(&self, r: &WorkspaceRef, mode: MountMode, by: SandboxId) -> Result<WorkspaceLease>;
}
pub struct WorkspaceLease { pub workspace: Workspace, pub mode: MountMode, pub root: PathBuf,
                            pub generation: u64, /* Drop releases; commit consumes */ }
impl WorkspaceLease {
    pub async fn commit_from_staging(self, staging: StagingDir) -> Result<u64>; // rw only, → new generation
}
pub mod archive {                                     // shared by FC now, Cloud Hypervisor/cloud later
    pub fn pack_tree(root: &Path, owner: (u32, u32), w: impl Write) -> Result<PackStats>;
    pub fn unpack_safe(r: impl Read, staging: &StagingDir, limits: UnpackLimits) -> Result<UnpackStats>;
}
```

`WorkspaceBacking::BlockVolume(path)` stays reserved. V1 FC can **attach it directly** (zero-cost lower bound for the benchmark).
Host-mutation methods on it return `SandboxError::Unimplemented("block volume host mutation")`.

## 6. Networking

The policy mapping is strict and has no silent fallback:
- `disabled` → **no NIC**. Only vsock exists (`lo` is up). This is the **V1 default for FC** in the CLI and SDK.
- `unrestricted` → tap + NAT.
- `egress_restricted` → `SandboxError::PolicyUnenforceable` at spawn (V1). Note that `NetworkPolicy::default()` is currently
  `EgressRestricted`. **Core must change the default to `Disabled`**, or every template-default spawn fails on both backends.

**Least privilege, decided: a pre-created, user-owned tap pool.** A one-time root script (`scripts/host-setup.sh`, run as root
and idempotent, plus a systemd oneshot so it survives reboot) does the following:
- `ip tuntap add dev sctap<N> mode tap user nicolas group nicolas` for N in 0..63. Each tap gets `172.30.<N>.1/30`, guest `.2`.
- An `nftables` **dedicated table** `inet sandcrab` handles masquerade from `172.30.0.0/16` out the uplink, drops guest→host
  (`input iifname "sctap*" drop` except established) and guest→guest, and sets `net.ipv4.ip_forward=1`. Using a separate table keeps Proxmox's `pve-firewall` rules untouched.
- At runtime the backend needs **no privileges**. It takes `flock(<tap lock dir>/sctap<N>.lock)` in a host-wide per-user dir
  (`/run/user/<uid>/sandcrab/taps` or `$XDG_RUNTIME_DIR/...`, else `/tmp/sandcrab-<uid>/taps`; 0700, owner-checked) so every sandcrab home shares one lease set, passes `host_dev_name` to
  FC (the tap is user-owned, so `open(/dev/net/tun)`+`TUNSETIFF` succeeds unprivileged), and sets `ip=` boot args plus DNS in `init.configure`. MAC = `06:00:AC:1E:<N>:02`.
- **Rejected:** a setuid helper (new privileged attack surface for us to own). **Rejected:** `CAP_NET_ADMIN` on the sandcrab binary (too broad).
  **Rejected:** a per-VM netns (needs root per spawn, so later with the jailer).

**Port previews (`SandboxConfig.ports`):** a host tokio listener on `127.0.0.1:<host_port>`. Each accepted conn → `CONNECT 1024`
→ `tunnel.open{port}` → raw byte splice to the guest's `127.0.0.1:<port>`. It works with **no NIC**, needs no DNAT, and needs no privileges.
The same tunnel primitive is reusable by other microVM backends.

## 7. Process model, cleanup, limits

```
~/.sandcrab/run/<sandbox-uuid>/      # 0700; UDS paths ≤ 108 B (asserted at spawn)
  api.sock  v.sock  firecracker.log  metrics.fifo|metrics.json
  scratch.ext4  ws-0.ext4 …  meta.json   # meta: fc pid + /proc starttime, owner pid+starttime, tap, timestamps
```

- Spawn: `firecracker --api-sock api.sock --id <uuid> --level Warn` (tokio `Command`, `kill_on_drop(true)`, stdio → log).
  The PID is written into a cgroup v2 leaf **before any API call**. Guest memory is only touched after `InstanceStart`, so accounting
  is complete. The leaf is `<user delegated cgroup>/sandcrab.slice/<uuid>` (systemd user delegation of `memory pids cpu`).
  Limits: `memory.max = mem_mb + 64 MiB` (VMM headroom), `memory.swap.max = 0` (no swapping past the cap; skipped when
  the kernel has no swap accounting), `pids.max = 64`. vCPU/RAM come from `ResourceLimits` via `/machine-config`
  (`smt=false`). `disk_mb` sets the scratch size.
- Teardown, by budget: ① rw exports (§5). ② `init.shutdown{grace_ms:500}` → guest `reboot` → with `reboot=k` FC exits
  (`docs/api_requests/actions.md`; guest reboot ends the process). Expected **~30–80 ms**. ③ If not exited at +1.5 s:
  `PUT /actions SendCtrlAltDel` (x86_64 only). ④ At +3 s: SIGKILL. ⑤ Release tap lock, rm the run dir, rmdir the cgroup.
- Orphan reaping at backend construction: for each `run/*/meta.json` whose owner is dead (pid plus starttime check, which avoids PID reuse)
  → SIGKILL FC (pid plus starttime match), move any `ws-*.ext4` to the workspace's `recovery/`, mark it `DirtySession`, and clean up.
  (`PR_SET_PDEATHSIG` would need `pre_exec`, which is `unsafe`, so it is rejected. `kill_on_drop` plus the reaper covers it.)

## 8. Jailer

**Out of V1.** `docs/jailer.md` requires starting as root (chroot, `mknod`, cgroup and netns setup, then the uid drop). That is the
same privilege class as networking and belongs with the netns/multi-tenant work. In V1, FC runs as the unprivileged user in group `kvm`
with its **default seccomp filters on** (`docs/seccomp.md`; never pass `--no-seccomp`). `multi_tenant_safe=false` stays until all of these hold:
jailer + per-VM netns + cgroup limits + a `docs/prod-host-setup.md` checklist (SMT off, KSM off, swap considerations, mitigations).

## 9. Benchmark hooks

`SandboxHandle` gains `fn timings(&self) -> &LifecycleTimings`. All values are `Instant`-based ns offsets from `spawn_requested`:

| Field | Point |
|---|---|
| `spawn_requested` | entry of `spawn()` (bench `spawn_ready` t0) |
| `rootfs_ready` | cache hit or build done; scratch copied |
| `ws_attach_done` | all workspace images built (bench `ws_attach`) |
| `vmm_started` | `api.sock` accepts `GET /` |
| `configured` | last pre-boot PUT returned |
| `instance_start` | `PUT /actions InstanceStart` returned 204 |
| `control_connected` | first `OK` on port 1 |
| `ready` | first `hello` parsed (bench `spawn_ready` t1) |
| `guest_boot_ms` | from the `init.configure` reply (`/proc/uptime`), as a cross-check |
| `teardown_start` / `ws_sync_done` / `vmm_exited` / `teardown_end` | bench `ws_sync_back`, `teardown` |

`first_call` is measured by the bench harness, not the backend. Memory sampler: `memory.current` and `memory.stat`
(`anon`, `file`, `kernel`) of the FC leaf cgroup, plus `/proc/<fc>/smaps_rollup` PSS. The guest-RAM mapping is split from VMM
overhead by summing `/proc/<fc>/smaps` entries for the single large anonymous guest mapping (size = `mem_size_mib`) against the rest.
Caveats to report: the shared base-rootfs **host page cache** is charged to the cgroup that faulted it first. The bench therefore
headlines PSS (per bench-spec) and drops caches only for "cold" cells. Guest page cache shows up as host **anon** RSS and never
shrinks without a balloon (V1: none. Later: balloon `free_page_reporting`).

## 10. Owned Firecracker API client

**Hand-rolled HTTP/1.1** over `tokio::net::UnixStream`, one keep-alive connection, used sequentially. It writes
`PUT <path> HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: n\r\n\r\n<body>` and parses the status line,
headers (only `Content-Length`; FC never chunks), and body. A non-2xx response maps to `SandboxError::Backend` carrying FC's `fault_message`.
The request types are `serde` structs mirroring `src/firecracker/swagger/firecracker.yaml`. **Rejected:** `hyper`+`hyperlocal`
(about 15 transitive crates for 8 fixed requests, plus a hyper 1.x / hyperlocal version coupling).

Endpoints used in V1:
- `GET /`: liveness, which also gives `vmm_started`
- `PUT /logger`, `PUT /metrics`
- `PUT /machine-config` `{vcpu_count, mem_size_mib, smt:false}`
- `PUT /boot-source` `{kernel_image_path, boot_args}`
- `PUT /drives/{rootfs|scratch|ws-N}` `{drive_id, path_on_host, is_root_device, is_read_only, io_engine:"Sync"}`
- `PUT /network-interfaces/eth0` `{iface_id, host_dev_name, guest_mac}` (only when `unrestricted`)
- `PUT /vsock` `{guest_cid:3, uds_path}`
- `PUT /actions` `{InstanceStart | SendCtrlAltDel | FlushMetrics}`

Not used in V1: MMDS, balloon, rate limiters, `/snapshot/*`. Those are Phase 2.

## 11. Risks and open questions

1. **Host type (needs approval).** On **bare Proxmox** the setup needs `nicolas` in group `kvm`, a root run of `host-setup.sh`
   (64 taps plus the nft table), and `firecracker` in `~/.local/bin`. On a **nested VM** we need `cpu: host` plus nested enabled on the hypervisor, and FC
   pays nested-exit overhead while Docker does not. The bench must record which one was used. Bare metal is recommended for credible numbers.
2. **`/sys/block/vdX/serial` = drive_id** needs verifying on FC 1.18 (VIRTIO_BLK_T_GET_ID). The superblock-label fallback is specified.
3. **mke2fs tar input** needs e2fsprogs ≥ 1.47.1 on the host. Otherwise use the container fallback.
4. **The W2 sync cost** may dominate turn latency. Mitigation: the V1.1 incremental export.
5. **The `NetworkPolicy` default** (§6) must flip to `Disabled`. Confirm with the user.
6. **aarch64** is untested. `SendCtrlAltDel` uses a GPIO path there, and kvm-homelab is presumably x86_64.
7. **Proxmox KSM/ballooning** would distort PSS. This is covered by bench-spec, and `host-setup.sh --bench` prints their status.

## 12. Implementation breakdown → flow steps

- **guest-vsock-init:** `vsock.rs` (tokio-vsock listener). `init.rs` covers PID 1 detection, mounts, overlay plus pivot_root, `lo` up,
  the reaper, CAD→shutdown, and the `init.configure/export/shutdown` handlers. `tunnel.open` goes in serve. Unit-test path validation and
  config parsing on macOS. Integration happens via fc-e2e.
- **core:** `SafeRelPath`, the `Workspace` additions (generation, state, quota), the `WorkspaceRegistry` + `WorkspaceLease` + `archive::{pack_tree, unpack_safe}`
  (with traversal/symlink/limit tests), `CrabdClient<S>`, `LifecycleTimings`, `PolicyUnenforceable`, the `NetworkPolicy::default()=Disabled` change,
  and the template `user`. Protocol additions: `init.*` + `tunnel.open` param types.
- **rootfs-pipeline:** `scripts/build-rootfs.sh` (thin) plus `core::rootfs` (export → augment tar → mke2fs → cache/manifest).
  Also `scripts/fetch-kernel.sh`, `artifacts.lock.toml`, and the blank scratch images.
- **fc-backend:** the `api` client, the `vsock` handshake, `vm.rs` (run dir, cgroup leaf, boot sequence, timings), `net.rs` (tap pool and locks,
  plus `scripts/host-setup.sh`), `workspace.rs` (image build/attach, export/commit), `teardown.rs`, `reaper.rs`, and the capabilities.
- **fc-wire:** the SDK `firecracker` feature and `--backend firecracker`. `doctor` checks `/dev/kvm` rw, group `kvm`, the FC/kernel/rootfs sha pins,
  e2fsprogs ≥ 1.47.1, the tap pool plus the nft table, cgroup delegation, and nested-vs-bare detection.
