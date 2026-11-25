---
title: "Design — sandcrab-backend-libkrun"
status: proposed
date: 2026-11-24
author: Turing (architect)
flow_step: lk-design (docs/plans/libkrun-backend.flow.yaml, to be authored by Nova)
related: [docs/design/firecracker-backend.md, docs/decisions/0001-phase-1-decisions.md, docs/ROADMAP.md]
---

# libkrun backend design

The goal is a hardware-isolated `RuntimeBackend` that runs natively on **Apple Silicon macOS
(Hypervisor.framework)** and on **Linux KVM**, with the **same `crabd`, the same protocol and the same
guest init** as the Firecracker backend. It must be honest about its capabilities, never fall back, and be
testable on a developer Mac and on a Linux KVM host (`kvm-dedicated`).

Upstream references: `containers/libkrun` **v1.19.8** (`include/libkrun.h`), `containers/libkrunfw`
**v5.6.4** (guest kernel **6.12.115**). Every claim marked **[spike]** was observed on an Apple M2
(16 GB, macOS 26.2) on 2026-11-24 (§1). Everything else is design or upstream documentation, and the
Linux KVM half is **unverified** until `kvm-dedicated` is up.

## 0. Decisions at a glance

| # | Topic | Decision |
|---|---|---|
| 1 | Process model | **One helper process per sandbox** (`sandcrab-krun`, a `[[bin]]` of the backend crate). It reads a JSON `HelperConfig` from stdin, then calls `krun_start_enter` and *becomes* the VMM. The backend never calls libkrun in-process. stdin stays open as a **lifeline**: EOF kills the VM |
| 2 | FFI | **Hand-written** `extern "C"` fn-pointer table for the ~20 functions we use, loaded at runtime with **`libloading` from pinned absolute paths** (sha-256 checked before `dlopen`). No bindgen, no `krun-sys`, no link-time dependency. Only the helper contains `unsafe` |
| 3 | Signing | Only the helper needs `com.apple.security.hypervisor`. `scripts/sign-krun-helper.sh` ad-hoc signs it (`codesign -s - --entitlements`). Test binaries and the SDK/CLI need no entitlement. The e2e harness signs `CARGO_BIN_EXE_sandcrab-krun` before use |
| 4 | Kernel | The libkrunfw bundled kernel (6.12.115), pinned by **libkrunfw version + dylib sha-256** per OS/arch in `artifacts.rs` |
| 5 | Rootfs | **ext4 block device** from the existing OCI → ext4 pipeline (extracted to a shared `sandcrab-rootfs` crate, made arch-aware), attached **read-only** plus a per-sandbox **scratch** disk with the guest overlay, exactly as on Firecracker. Booted with `krun_set_root_disk_remount`. **Rejected:** virtio-fs directory root (case-insensitive APFS clobbers names, ownership lives in libkrun-private xattrs) |
| 6 | Init | `KRUN_INIT_PID1=1`: libkrun's `init.krun` mounts the block root, then **execs `crabd init` as PID 1**. The FC init path (drive serials, overlay + `pivot_root`, control port, reaper, reboot) is reused. Config goes over the vsock control port, never argv or env, because those land on the kernel cmdline |
| 7 | Transport | libkrun maps guest vsock listeners to host UNIX sockets (`krun_add_vsock_port2(listen=true)`): port 1 → `ctl.sock`, port 1024 → `v.sock`. The host uses a plain `UnixStream` with no `CONNECT` handshake, so it shares the Docker `Endpoint::Unix` connector |
| 8 | Workspaces | **Live virtio-fs** per mount (`krun_add_virtiofs3`, `read_only` = `MountMode::ReadOnly`, which the VMM enforces). No copy-in or sync-back. The sandbox user defaults to the **host uid:gid**, as on Docker |
| 9 | Network | **TSI is never enabled implicitly**: `krun_disable_implicit_vsock` + `krun_add_vsock(0)`, and always `krun_set_port_map([])`. `disabled` → no NIC. `unrestricted` → Phase B userspace netstack (passt on Linux, gvproxy on macOS) behind the helper's egress filter, host loopback unreachable (§9.1). Without the pinned netstack it is `PolicyUnenforceable`; the deprecated explicit TSI flag (states that host loopback is reachable) still exists and is never a fallback. `egress_restricted` → `PolicyUnenforceable` |
| 10 | Ports | Over a **vsock tunnel through `crabd`** (`tunnel.open`), bound to `127.0.0.1` by the backend. This is the primitive the FC design already specifies. Never TSI port mapping, which binds the wildcard address |
| 11 | Capabilities | `hardware_isolated: true, multi_tenant_safe: false, supports_snapshots: false, supports_host_dir_mounts: true` |
| 12 | Gating | SDK/CLI feature `libkrun`, **off by default**, compiled for `cfg(any(all(target_os="macos", target_arch="aarch64"), target_os="linux"))`. Elsewhere `--backend libkrun` is a clear "not compiled in" error |
| 13 | crabd | Cross-compiled **natively on macOS** for `aarch64-unknown-linux-musl` with `rust-lld` (no Docker, no musl-gcc) **[spike]** |
| 14 | Alternatives | Apple Containerization / `container` CLI and krunkit are **not adopted** as the backend (§13). Containerization stays on the watch list as a possible later macOS-only backend |

## 1. Feasibility spike (2026-11-24, M2, macOS 26.2)

Installed `brew install libkrun libkrunfw` → **libkrun 1.19.8, libkrunfw 5.6.4** (pulls in `libepoxy` and `molten-vk`
for the GPU path, which we do not use). Docker Desktop's CLI is present but the daemon was not running, so
the spike used no Docker. The spike was a ~60-line C program (`krun_create_ctx` → `set_vm_config` → `set_root`
or `add_disk`+`set_root_disk_remount` → optional `add_virtiofs3`, `add_vsock_port2`, `disable_implicit_vsock`
+`add_vsock(0)` → `set_exec` → `start_enter`), built with `clang $(pkg-config --cflags --libs libkrun)`
and signed with:

```sh
codesign --force -s - --entitlements entitlements.plist spike   # plist: com.apple.security.hypervisor = true
DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib ./spike [-n] [-d root.ext4] [-v 1024:v.sock] [-w tag:/path:ro] <root> <cmd…>
```

The guest was Alpine `alpine-minirootfs-3.22.8-aarch64` (sha-256 checked against the release index). `crabd` was
built natively:

```sh
rustup target add aarch64-unknown-linux-musl
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --locked -p sandcrab-guest --bin crabd --target aarch64-unknown-linux-musl
# → ELF 64-bit aarch64, statically linked, 4.1 MB, 55 s cold build
```

| Check | Result |
|---|---|
| Boot + run a command (virtio-fs dir root) | ✅ kernel `6.12.115 aarch64`, `init.krun` is PID 1, the workload's exit code becomes the process exit code |
| **Cold boot**, whole process (exec → boot → `cat /proc/uptime` → VM exit), 1 vCPU / 512 MiB, n=10 | **p50 127 ms** (min 124, max 153). Guest uptime at workload start is 60–70 ms. 2 vCPU / 2 GiB gives the same. The first run after install paid ~800 ms once (dyld/page-in) |
| Same with an **ext4 block root** (`krun_add_disk` + `krun_set_root_disk_remount`), n=10 | **p50 125 ms** (min 115, max 126) |
| **Real `crabd serve --vsock 1024`** ↔ host UDS (`krun_add_vsock_port2(listen=true)`), framed JSON-RPC `hello` | ✅ `protocol_version 0.1`, full tool list. **Spawn → first `hello` OK: 100–108 ms** (n=5, ext4 root, `crabd` as PID 1, TSI off) |
| `KRUN_INIT_PID1=1` | ✅ the workload runs as PID 1 after `init.krun` has mounted the block root |
| `krun_add_disk(block_id)` → guest `/sys/block/vdX/serial` | ✅ `root`, `scratch`, so FC `drives.rs` resolution works unchanged |
| Guest `reboot -f` | ✅ the VMM exits with status 0 |
| `SIGTERM` to the VMM process | Dies immediately (143). libkrun installs no handler, so there is no guest flush |
| Idle RSS, 512 MiB guest | ~105 MB |
| virtio-fs `read_only=true` | ✅ `EROFS`, **even after the guest runs `mount -o remount,rw`**. Enforced by the host-side server |
| Guest `chown`/`mknod`/`mkfifo`/`chmod 4755` on an rw share | They succeed in the guest. On the host they are **plain 0600 regular files owned by the host user**, with guest metadata in an xattr `user.containers.override_stat` (e.g. `1000:1000:0104755`) |
| Guest `ln -s /etc/passwd lnk` on an rw share | Creates a **real host symlink** `lnk -> /etc/passwd`. Host tools must not follow it (the registry already never follows) |
| Case-insensitive APFS share | `echo > readme` **overwrote** the existing `Readme` |
| Non-root guest user on an rw share | Default semantics: host files show as `uid 501 gid 0`, so uid 65534 gets `EACCES`. With `KRUN_SEMANTICS_LINUX_SIMPLIFIED`, every guest uid is the owner and writes succeed |
| Default **TSI** networking | Public egress works. **The guest reached a host service bound to `127.0.0.1:47123`**. A guest `nc -l 18080` was **auto-published on the host as `*:18080` (wildcard)** because no port map was set |
| TSI off (`disable_implicit_vsock` + `add_vsock(0)`) | Network unreachable, host loopback refused, no host listener, and vsock port mapping still works |
| Kernel cmdline | `… init=/init.krun KRUN_INIT=… KRUN_WORKDIR=/ "PATH=…" "HOME=…" tsi_hijack -- "<argv>"`. **argv and env are on the cmdline**. A newline in argv made libkrun **panic and abort the process** (`InvalidAscii`, `vmm/src/builder.rs`) |
| No entitlement (ad-hoc signed by the linker) | `krun_start_enter` → `-EINVAL`, log `VmSetup(VmCreate)` |
| No signature at all | The process is killed at exec (arm64 requires a signature) |
| `libkrunfw` lookup | libkrun `dlopen`s **`libkrunfw.5.dylib` by leaf name**. It failed until `DYLD_FALLBACK_LIBRARY_PATH` was set. Wrapping the call in SIP-protected binaries (e.g. `/usr/bin/time`) **strips `DYLD_*`** |
| macOS `sun_path` | 104 bytes. A deep scratch path failed with `AF_UNIX path too long` |
| brew `e2fsprogs` 1.47.4 | `mke2fs -d <tarball>` is **unsupported** (built without libarchive). Directory input works but keeps the host uid |

Observed dylib digests (Homebrew bottles, arm64_tahoe):
`libkrun.1.19.8.dylib 3c2ffda8f326e6cdcbb504d8e7dc450f84dca8c57b001fce7b6bc6dbdaca5b5a`,
`libkrunfw.5.dylib 1ccb5283921f36fedaa8f85ba6265e298a6b86d56be49f642c768b32102b91df`.

Nothing in the spike needed sudo or a GUI approval.

## 2. Process model

`krun_start_enter` never returns on success. It takes over stdio and `exit()`s with the guest's status.
libkrun can also **abort the whole process** on bad input **[spike]**. It must therefore never run inside the SDK
process. One VMM per process is also libkrun's model.

**Decision: per-sandbox helper `sandcrab-krun`.**

```
SDK process (no entitlement)                         sandcrab-krun (signed, entitled)
LibkrunBackend::spawn                                  read HelperConfig (stdin, ≤64 KiB, deny_unknown_fields)
  mkdir run/<id> 0700                                  verify pins: libkrun + libkrunfw sha-256, rootfs sha-256
  write meta.json (owner pid+start, helper pid)        verify workspace roots (dev, ino) == lease identity
  Command::new(helper)                                 libloading::Library::new(<pinned abs path>)
    .stdin(piped)  ── HelperConfig JSON + keep open    krun_create_ctx / set_vm_config / add_disk(root, ro)
    .stdout/stderr → run/<id>/vmm.log                  add_disk(scratch) / set_root_disk_remount(/dev/vda, ext4, "ro")
    .env_clear() + DYLD_FALLBACK_LIBRARY_PATH|         add_virtiofs3(ws<N>, path, 0, ro) …
         LD_LIBRARY_PATH = <pinned libkrunfw dir>      disable_implicit_vsock + add_vsock(0) + set_port_map([])
    .process_group(0)  .kill_on_drop(true)             add_vsock_port2(1, ctl.sock, true), (1024, v.sock, true)
  readiness: ctl.sock → init.configure → v.sock hello  set_exec("/sbin/crabd", ["init"], ["KRUN_INIT_PID1=1", …])
  watch child exit (tokio task) → handle.dead          spawn lifeline thread: read(stdin) → EOF → kill(self, SIGKILL)
                                                       krun_start_enter  (never returns)
```

- **Config channel to the helper:** JSON on stdin (`HelperConfig`, versioned, `deny_unknown_fields`) rather than argv.
  The config holds host paths, which we keep out of `ps`, and argv parsing would duplicate validation.
- **Lifeline instead of `PDEATHSIG`:** after reading the config, the helper starts a thread that blocks on `read(stdin)`.
  When the owning process dies, for any reason including SIGKILL, the pipe hits EOF and the helper SIGKILLs itself.
  This works on macOS, which has no `PR_SET_PDEATHSIG`, and on Linux, with safe std APIs only. On Linux it is
  belt-and-braces with `rustix::process::set_parent_process_death_signal`.
- **Crash detection:** a tokio task `wait()`s the helper. Exit status ≠ 0 before ready → `SandboxError::Backend` with
  the tail of `vmm.log`. libkrun's documented codes 125, 126 and 127 map to "guest init failed". The helper reserves
  `70..=79` for pre-start failures (pin mismatch 71, workspace identity mismatch 72, libkrun config error 73, …).
  An exit after ready marks the handle dead, so the next connector call fails fast with `SandboxError::Backend("vm exited: …")`.
- **Graceful stop:** `init.shutdown{grace_ms}` on the control port → `crabd init` SIGTERMs, syncs, unmounts and
  reboots → the VMM exits 0 **[spike: `reboot -f` → exit 0]**. Budget: +1.5 s → `SIGTERM` (immediate, **[spike]**)
  → +1.5 s → `SIGKILL` the process group. macOS also offers `krun_get_shutdown_eventfd` (macOS/aarch64 only). It is not
  needed because the control-port path is portable.
- **Orphan reaping:** the lifeline makes orphaned VMs self-terminate. The reaper (run at backend construction and on
  `doctor --fix`, mirroring FC `reap.rs`) handles leftovers: for each `run/*/meta.json` with
  `backend="libkrun"` whose owner (pid + start time) is dead, SIGKILL the helper if pid + start time still
  match, then remove the run dir. There are no workspace images to recover (live virtio-fs).
  Start time on macOS comes from `sysctl kern.proc.pid` via `ps -o lstart=`. **Rejected:** a `libc`
  `sysctl` call, which would put `unsafe` in the backend.
- **Rejected:** `fork()` and calling libkrun in the child (not async-signal-safe in a tokio process, and it would put
  the entitlement on every SDK consumer). Also rejected: a long-lived daemon multiplexing VMs (libkrun is
  one-VM-per-process, and a daemon is a privilege/IPC surface we don't need yet; `sandcrab-server` is Phase 3).

## 3. FFI, linking, version pin, feature gating

**Decision: hand-written fn-pointer table + `libloading`, only in the helper.**

```rust
// crates/sandcrab-backend-libkrun/src/ffi.rs   (#![allow(unsafe_code)] scoped to this module)
pub struct Krun { _lib: libloading::Library, pub create_ctx: unsafe extern "C" fn() -> i32,
                  pub set_vm_config: unsafe extern "C" fn(u32, u8, u32) -> i32,
                  pub add_disk: unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32,
                  /* set_root_disk_remount, add_virtiofs3, disable_implicit_vsock, add_vsock,
                     add_vsock_port2, set_port_map, set_exec, set_env, set_workdir, set_log_level,
                     set_console_output, set_rlimits, get_max_vcpus, check_nested_virt,
                     add_net_unixstream (Phase B, optional symbol), start_enter */ }
impl Krun { pub fn load(path: &Path, expected_sha256: &str) -> Result<Self>; }   // hash → dlopen → dlsym all
pub struct Ctx<'k>(&'k Krun, u32);  // safe wrapper: CString conversion, NUL/ASCII validation, errno mapping
```

Rationale:
- **Runtime loading** lets the crate, its unit tests and CI build on any host with **no libkrun installed**. We can
  **hash the exact file before loading it**, and load from an **absolute pinned path**, which avoids the leaf-name
  lookup we hit with libkrunfw. It also means no `build.rs` and no `pkg-config` at build time.
- **We validate every string we pass to libkrun** (printable ASCII, no NUL/newline, length caps), because
  libkrun aborts on bad cmdline input **[spike]**. argv and env are fixed by us anyway (§6).
- **Rejected:** bindgen (a libclang build dep, and a 1,457-line header full of GPU/display/TEE APIs we don't want
  exposed). **Rejected:** `krun-sys` (bindgen output, link-time coupling to the installed version, still needs
  the same safe wrapper). **Rejected:** linking with `-lkrun` (cannot verify the hash before load, and it fails to
  build without libkrun).
- **Workspace lint:** `unsafe_code = "forbid"` stays for the backend lib. The helper's `ffi.rs` module is the single
  `#[allow(unsafe_code)]` island. The crate root uses `deny`, not `forbid`, so the module can opt out. Every `unsafe` block gets a `// SAFETY:` comment.

**Library location and pins** (`artifacts.rs`, the same pattern as FC):

| Host | libkrun | libkrunfw | Source |
|---|---|---|---|
| macOS arm64 | 1.19.8, sha `3c2ffda8…5b5a` | 5.6.4, sha `1ccb5283…91df` | Homebrew bottle, copied by `sandcrab libkrun install` into `~/.sandcrab/artifacts/libkrun/<ver>/` so `brew upgrade` cannot change what runs |
| Linux x86_64 | 1.19.8 (built) | 5.6.4 (built) | `scripts/fetch-libkrun.sh` builds both from the pinned upstream tags in a throwaway container and records the shas. Distro packages lag |

Resolution order is the `LibkrunConfig.lib_dir` override, then `~/.sandcrab/artifacts/libkrun/<ver>/`, then **error**. We never
search `/opt/homebrew` or `LD_LIBRARY_PATH` implicitly. `doctor` prints exactly which files and shas are in use.
Supported API floor is `1.19` (`add_vsock_port2`, `disable_implicit_vsock`, `add_vsock`, `set_root_disk_remount`,
`add_virtiofs3`). `Krun::load` fails if any symbol is missing.

**Feature gating:**

```toml
# sandcrab-sdk
libkrun = ["dep:sandcrab-backend-libkrun"]            # off by default (Docker stays the default)
[target.'cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))'.dependencies]
sandcrab-backend-libkrun = { workspace = true, optional = true }
```

The CLI and bench get `libkrun = ["sandcrab-sdk/libkrun"]`. We recommend adding it to the CLI `default` once e2e is
green: it costs nothing at build time thanks to runtime loading, and FC is in `default` the same way. Selection is
always explicit (`--backend libkrun` / `.libkrun(LibkrunConfig)`). macOS x86_64 is unsupported (Hypervisor.framework
works there, but libkrunfw for x86 macOS is not shipped by Homebrew) → "not compiled in".

## 4. macOS code signing

- The entitlement is required by the **process that creates the VM**, which is only `sandcrab-krun`. The SDK, CLI,
  `crabagent`, `cargo test` binaries and the bench need no signature beyond the linker's ad-hoc one.
- `crates/sandcrab-backend-libkrun/entitlements.plist` contains only `com.apple.security.hypervisor`. We add no
  `disable-library-validation`: an ad-hoc-signed binary without hardened runtime already loads Homebrew-signed dylibs **[spike]**.
- `scripts/sign-krun-helper.sh <path>`: on macOS it runs `codesign --force -s - --entitlements … <path>` and verifies with
  `codesign -d --entitlements -`. On Linux it is a no-op. It is idempotent.
- **Dev:** `cargo build -p sandcrab-backend-libkrun --bin sandcrab-krun && scripts/sign-krun-helper.sh target/debug/sandcrab-krun`.
  `sandcrab libkrun install` copies the signed helper to `~/.sandcrab/bin/`.
- **Tests/e2e:** the integration test resolves `env!("CARGO_BIN_EXE_sandcrab-krun")`, runs the sign script once
  (guarded by a `OnceLock`, and skipped if the entitlement is already present), then points `LibkrunConfig.helper` at it.
  Unit tests never touch libkrun.
- **Errors:** before spawn, `doctor` and the backend check `codesign -d --entitlements -` output for the key. If it is
  missing, the error says "`sandcrab-krun` lacks com.apple.security.hypervisor; run scripts/sign-krun-helper.sh". If
  `krun_start_enter` still returns `-EINVAL` with `VmCreate`, it maps to the same hint **[spike]**.
- **Release signing (decided 2026-11-24: ad-hoc now, Developer ID later).** Design for it from the start:
  `scripts/sign-krun-helper.sh` takes an optional identity (`SANDCRAB_SIGN_IDENTITY`, default `-` = ad-hoc). With a
  Developer ID it signs with `--options runtime --timestamp` (hardened runtime), and the same `entitlements.plist` gains
  `com.apple.security.cs.disable-library-validation` only if the release build still dlopens third-party dylibs (prefer
  bundling pinned, re-signed dylibs in the release so it is not needed). Notarization (`xcrun notarytool submit --wait`,
  then `stapler`) is a separate release-pipeline step, documented in `docs/hosting.md` and not run in dev or CI.

## 5. Guest artifacts

**Kernel.** We use libkrunfw's bundled kernel (6.12.115 in 5.6.4). It is a dylib *payload* that libkrun loads, so we pin
libkrunfw by version and dylib sha-256 (§3). No separate kernel file exists. Its config already has virtio-fs, vsock,
virtio-blk, ext4, overlayfs, cgroup2 and devtmpfs **[spike: mounts observed]**. We need nothing else in Phase A.
**Rejected:** `krun_set_kernel` with the FC 6.18 kernel. It is aarch64-incompatible on Mac (FC CI ships x86_64), and it
would bypass `init.krun`'s block-root remount that we rely on (§6).

**Rootfs: ext4 block device, decided.**

| | virtio-fs directory root | **ext4 block (existing pipeline)** |
|---|---|---|
| Case-insensitive APFS | **breaks**: `Readme`/`readme` collide **[spike]**. distro trees can contain case-colliding paths (e.g. terminfo entries) | not affected (ext4 inside an image file) |
| Ownership, modes, devices | stored in libkrun-private xattrs on host files | native ext4 |
| Per-sandbox isolation | needs a full tree copy (or overlay in a second share) | **shared ro base + sparse scratch + guest overlay**, the same as FC |
| Boot time | p50 127 ms | p50 125 ms **[spike]** |
| Reuse | none | FC pipeline, `drives.rs` serials, `overlay_root`, cache keys |

Pipeline changes:
- Extract `crates/sandcrab-backend-firecracker/src/rootfs/*` and the `sandcrab-rootfs` bin into a new
  **`sandcrab-rootfs` crate** (pure std + `tar`/`flate2`/`sha2`, no VMM code). FC and libkrun both depend on it. FC
  re-exports for compatibility.
- Add an **arch dimension**: `docker create --platform linux/<arm64|amd64>`. The cache key becomes
  `sha256(image_digest ‖ arch ‖ crabd_sha256 ‖ PIPELINE_VERSION)`. `crabd` is injected for the matching arch.
- **On macOS**, `docker export` needs Docker running, and `mke2fs -d <tar>` is unavailable in brew e2fsprogs
  **[spike]**. We use the pipeline's existing `MkfsStrategy::Container` (`debian:trixie-slim` + e2fsprogs 1.47.2 +
  libarchive). Docker is therefore a **build-time** prerequisite for rootfs images on macOS (once per template version,
  cached). It is not needed at runtime. Open question 3 covers a Docker-free importer.
- **Scratch disk:** `std::fs::copy` of a cached blank ext4. On APFS it uses `fclonefileat` (instant CoW). On Linux
  it is the FC sparse copy. Size is `ResourceLimits.disk_mb`.

**crabd.** `aarch64-unknown-linux-musl` built **natively on macOS** with `rust-lld` (proven, 55 s cold). Teach
`scripts/build-crabd.sh` to set `CARGO_TARGET_<T>_LINKER=rust-lld` on Darwin, where there is no `musl-gcc`. On Linux nothing
changes. The x86_64 crabd for `kvm-dedicated` builds there. **Rejected:** building in Docker (slower, and it needs a running
daemon).

**Templates on arm64.** The Mac only runs `linux/arm64` guests (HVF does not emulate x86).
- `base` pins a **multi-arch index digest** and `render` already branches on `TARGETARCH`. `python-dev` (uv image
  digest) and `ts-dev` (node/bun digests) must be checked: each pinned `FROM …@sha256` must be an **index** digest, or
  needs a per-arch digest. Downloaded binaries (uv, bun, typst) need a per-arch sha.
- `sandcrab.toml` `sha256` pins a **locally built image ID**, which is arch-specific. Add
  `[template.sha256_by_arch] amd64 = "…", arm64 = "…"`. `sha256` stays as the fallback for single-arch hosts. The core
  `TemplateManifest` validates both. `templates/build.sh --platform linux/arm64 [--pin]` writes the right key.
  *Superseded:* image IDs are now pinned host-locally, per arch, outside the manifests; see
  [template-pins.md](../template-pins.md).
- The rootfs `manifest.json` already records the rootfs sha. The per-arch image ID feeds the cache key.

## 6. Guest init and config

The libkrun boot chain, with no custom kernel cmdline: libkrun's kernel cmdline → `init.krun` (libkrun's C init)
mounts `/proc /sys /dev /dev/pts /dev/shm cgroup2`, mounts the block root
(`KRUN_BLOCK_ROOT_DEVICE=/dev/vda`, `…_FSTYPE=ext4`, `…_OPTIONS=ro`, via `krun_set_root_disk_remount`), and with
`KRUN_INIT_PID1=1` **execs** `/sbin/crabd init` as PID 1 **[spike]**.

`crabd init` (FC design §3) then runs unchanged except for:
1. **Early mounts must tolerate "already mounted"** (`EBUSY` on `proc`/`sysfs`/`devpts` → skip). `init.krun` did them.
2. Drive discovery by **virtio-blk serial**: `root` and `scratch` **[spike]**, then `overlay_root(scratch)` + `pivot_root`, exactly as on FC.
3. `init.configure` gains **virtio-fs mounts**. `GuestMount` becomes a source enum:
   `#[serde(tag="source")] enum MountSource { Block { drive_id, label }, VirtioFs { tag } }`, where `Block` is the serde default
   so FC stays wire-compatible. `VirtioFs` → `mount("<tag>", guest_path, "virtiofs", MS_NOSUID|MS_NODEV [|MS_RDONLY])`.
   The guest `MS_RDONLY` is defense in depth; the host enforces read-only regardless **[spike]**.
4. No `ip=` handling (there is no NIC in Phase A). The vsock control port is 1 and the tools port 1024, unchanged.

**Why not pass config via argv/env:** both end up on the kernel cmdline (`/proc/cmdline` is readable by every guest
process), are ASCII-only, and abort libkrun on a newline **[spike]**. argv is fixed (`["init"]`), and env is fixed
(`KRUN_INIT_PID1=1`, `PATH`). User env, hostname, user, mounts and `default_cwd` go over `init.configure`, which already
exists. **Rejected:** `krun_disable_implicit_init` plus our own init (we would lose the block-root remount, and
`init.krun` is small).

## 7. Transport and readiness

The helper maps guest **listening** vsock ports to host sockets: `krun_add_vsock_port2(ctx, 1, "<run>/ctl.sock", true)`
and `(1024, "<run>/v.sock", true)`. A host `connect()` to the socket becomes a guest vsock connection **[spike]**. There is
no `CONNECT` handshake (unlike FC), so the connector is the core `Endpoint::Unix` connector the Docker backend uses.

Readiness is the same as FC: poll `ctl.sock` (backoff 2 ms ×1.5 ≤ 5 ms, deadline `boot_timeout`, default 10 s) →
`init.configure` → poll `v.sock` → first `hello` = `t_ready`. A connect before the guest listens returns
`ECONNREFUSED`/EOF; the spike took 12–17 attempts within ~100 ms. If the helper exits during the poll, it aborts immediately with the log tail.

**Socket path budget:** macOS `sun_path` is 104 bytes (Linux 108). `run/<uuid>/ctl.sock` under a default
`~/.sandcrab` is about 80 bytes. Spawn asserts ≤ 103 and errors with a hint to shorten `SANDCRAB_HOME`. It never truncates.

## 8. Workspaces

**Decision: live virtio-fs, one device per mount.** `supports_host_dir_mounts = true`. The Docker semantics are the
model, not FC's copy/sync.

| Concern | Docker (bind) | **libkrun (virtio-fs)** | FC (image per spawn) |
|---|---|---|---|
| rw writes visible on host | live | **live** (VMM process writes as the host user) | at teardown (sync-back) |
| ro enforcement | kernel bind `ro` | **host virtio-fs server returns `EROFS`**; guest remount cannot lift it **[spike]** | FC `is_read_only` |
| Guest root powers on the host | real uid 0 in the container userns | **none**: files are created as the host user, 0600. chown/mknod/setuid become xattr metadata **[spike]** | none |
| Symlinks written by the guest | real symlinks | **real host symlinks** **[spike]** | unpacked as symlinks |
| Crash semantics | writes landed | writes landed (modulo guest page cache at SIGKILL) | turn lost, image kept for recovery |
| Spawn/teardown cost | ~0 | ~0 | O(size) |

Mechanics:
- **Leases are unchanged.** The manager takes the rw (exclusive) or ro (shared) lease. The helper is handed the canonical
  `host_path` and `root_identity`. Host mutation while a lease is held stays refused (`WorkspaceBusy`). Between
  turns, the host mutates freely.
- **(dev, ino) pinning and the TOCTOU:** libkrun takes a *path*, and its server opens it at VM build time. The helper
  re-verifies `(dev, ino)` **immediately before** `krun_add_virtiofs3`, then calls `start_enter`. The residual
  window (verify → libkrun's open, about µs to ms, in our own process) is the same class as the deferred Docker bind race.
  Fix candidate to test in `lk-helper`: pass `/dev/fd/<n>` of an `O_DIRECTORY` fd the helper opened and verified, if
  libkrun's server accepts it. If it does, the race closes.
- **Traversal:** guest paths are validated by core (`normalize_guest_path`) and again by `crabd init` (existing
  `validate_mount_point`). Host side, libkrun's passthrough server is rooted at the shared dir. Escapes would be a
  libkrun bug (risk 2). Guest-created symlinks are inert to the registry because it never follows them (existing guards).
- **uid model:** with default (`LINUX_COMPLETE`) semantics, host files appear as the host uid. A non-root template user
  gets `EACCES` **[spike]**. So, like Docker, the **effective sandbox user defaults to the host uid:gid**, and `--user`
  overrides it. **Rejected as default:** `LINUX_SIMPLIFIED`, where every guest uid owns everything and mode bits are written to
  the real host files. It is convenient but weakens in-guest multi-user semantics. It stays available behind
  `LibkrunConfig.virtiofs_semantics` if templates need a fixed non-root user.
- **Case-insensitive APFS:** host-dir workspaces on a default macOS volume behave case-insensitively in the guest **[spike]**.
  This is the same as Docker Desktop on Mac. `doctor` warns. Option: managed workspaces on a case-sensitive APFS sparse image (open question 4).
- `BlockVolume` workspaces: attach with `krun_add_disk` (rw/ro) through the existing `MountSource::Block` path. This is cheap
  to support and keeps parity with FC.
- **Linux:** libkrun's passthrough runs as the unprivileged host user. Ownership emulation and xattr behaviour must be
  verified on `kvm-dedicated` (risk 3).

## 9. Networking and ports

The spike proved that **TSI is unsafe as a default**. It gives the guest the **host's loopback** (it read a
`127.0.0.1`-only host service) and **auto-publishes guest listeners on the host wildcard address** unless a port map is set.

| `network.policy` | libkrun mapping |
|---|---|
| `disabled` (default) | `krun_disable_implicit_vsock` + `krun_add_vsock(ctx, 0)` (vsock without TSI) + `krun_set_port_map([])`. No NIC. Only `lo`. Verified unreachable **[spike]** |
| `unrestricted` (Phase B, **shipped**, step `lk-net-userspace`) | A virtio-net NIC on the pinned userspace netstack, **behind the helper's egress filter** (§9.1): passt on Linux, gvproxy on macOS. Needs the netstack installed (`sandcrab libkrun install`); otherwise **`PolicyUnenforceable`** with the install hint. TSI stays off |
| `unrestricted` + TSI opt-in (Phase A, **deprecated**) | `LibkrunConfig.allow_tsi_unrestricted = true` / CLI `--libkrun-tsi` (documented as **"guest can reach host loopback services"**) → TSI with `KRUN_TSI_HIJACK_INET` and an **empty port map**. An installed netstack wins over the opt-in (the opt-in is then ignored with an error-level log on every spawn, since the safe path exists); the opt-in only applies where no netstack is installed, and a missing netstack never falls back to TSI without it |
| `egress_restricted` | `PolicyUnenforceable` (Phase 3 egress proxy), the same as other backends |

### 9.1 Phase B: userspace netstack behind an egress filter

**Spike (2026-11-24).** Neither netstack is loopback-safe on its own terms in every case:

- **gvproxy v0.9.0** (`-listen-stdio accept -ssh-port -1`): crafted SYNs to `127.0.0.1` / `0.0.0.0` are dropped, but
  a SYN to **`192.168.127.254`** (its NAT alias for the host's `127.0.0.1`) **reached a listener bound only to host
  loopback**. It also serves a port-forward HTTP API (`/services/forwarder/expose`) on the gateway `192.168.127.1:80`,
  which a guest could use to publish ports on the host.
- **passt** (`--no-map-gw`): drops frames to loopback / `0.0.0.0` (`tap.c`), and a 127.0.0.1-only listener got RST. But
  a SYN to the **host's own address** reached a listener bound to `0.0.0.0` on the host (expected for a NAT; still a
  host service).

So the stock netstacks are not the security boundary. **The helper is.** Design:

```
guest virtio-net ──(libkrun, krun_add_net_unixstream fd, 4-byte BE length frames)──► helper
helper "net-egress" thread: parse Ethernet/IPv4, EgressPolicy::check, drop or forward ──► netstack (socketpair)
helper "net-ingress" thread: netstack frames (length-checked) ──► guest
netstack: passt `--fd 0` (BE32 frames) | gvproxy `-listen-stdio accept` (LE16 frames on stdin/stdout)
```

- **Addressing (fixed, both platforms):** guest `192.168.127.2/24`, gateway + DNS `192.168.127.1`, MAC
  `5a:94:ef:e4:0c:ee`, MTU 1500, no virtio-net offloads (`features = 0`). `init.krun` configures `eth0` by DHCP
  (`NET_FLAG_DHCP_CLIENT`) before `crabd`; `crabd` writes `resolv.conf` (`nameserver 192.168.127.1`).
- **Egress filter** (`netstack.rs`, guest → netstack, default deny): only IPv4 and ARP (IPv6 is dropped: no v6 on the
  NIC); to the gateway only DNS (UDP/TCP 53) and DHCP (UDP 67); broadcast only DHCP; never
  `0/8 10/8 100.64/10 127/8 169.254/16 172.16/12 192.0.0/24 192.0.2/24 192.168/16 198.18/15 198.51.100/24
  203.0.113/24 224/4 240/4` (the FC egress policy's reserved ranges, which cover gvproxy's `192.168.127.254` alias and
  cloud metadata), nor the host's own addresses and on-link subnets (`getifaddrs` at spawn), nor TCP 25/465/587 (SMTP).
  Malformed frames are dropped, and so is every fragmented TCP/UDP packet (MF set or a non-zero offset) and any
  TCP/UDP packet shorter than an 8-byte L4 header, so the port checks always see the ports (a tiny first fragment
  could otherwise slip past the SMTP block where the netstack reassembles). The guest never needs to fragment
  (MTU 1500, PMTU in the netstack); fragmented ICMP still passes the destination check. Netstack → guest frames are only length-checked.
- **Netstack process:** pinned by SHA-256 (the helper hashes it right before spawning; mismatch → exit 71), spawned
  by the helper as the same unprivileged user with an empty environment, `cwd /`, in the helper's process group and
  (Linux) cgroup leaf. It **listens on nothing**: its only channel is the socketpair, so when the helper dies — however
  it dies — the netstack reads EOF and exits (passt `--fd` is one-shot; gvproxy exits on stdio EOF). No sockets on
  disk, so no socket dir to protect. A watcher thread reaps it and logs the frame counters. Spawn failure → exit 76.
- **Netstack readiness and quarantine:** before the VM boots, the helper sends an ARP probe for the gateway (re-sent
  every 250 ms) and waits up to 10 s for the netstack's first frame. A netstack that exits or stays silent fails the
  spawn with exit 76 and a clear error, never a guest with a dead NIC. Cause seen on macOS: a freshly written gvproxy
  is scanned on first exec, and a file carrying `com.apple.quarantine` is blocked by Gatekeeper behind a dialog (the
  upstream release is not notarized). At backend construction, after the netstack matched its pinned SHA-256, the
  quarantine attribute is removed from that exact file; without pin verification such a file is refused with the
  fix. The pinned hash is of the original file and is never changed.
- **Pins** (`netstack_pins.rs`, `<home>/artifacts/libkrun-net/<kind>-<version>/manifest.json`): gvproxy `v0.9.0`
  release asset `gvproxy-darwin` (universal, ad hoc signed; sha `e2a6825b…`, installed byte for byte); passt
  `2026_10_02.cba3570` (commit `cba35706…`), built `make static` from the pinned tag by `scripts/fetch-libkrun.sh`
  (not reproducible: the installed sha is recorded with the commit, as for libkrun on Linux). The Linux libkrun build
  adds `NET=1` (`krun_add_net_unixstream`); the symbol is optional in `Krun::load`, required only for `userspace`.

**What the filter cannot block** (documented, accepted for `unrestricted`):

- **The public internet**, by definition, including anything the host exposes on a public address it did not have at
  spawn time.
- **DNS through the host's resolver.** The netstack forwards gateway:53 to the host's configured resolver, which may
  be a loopback stub (`127.0.0.53` with systemd-resolved, `mDNSResponder` on macOS). That one resolver port is the only
  host-loopback service the guest reaches, by design.
- **Addresses the host gains after spawn** (a new interface, VPN). The deny list is a snapshot.
- **Other hosts' private services reached through a public address** (hairpin NAT on the router).
- **Traffic volume / abuse** (no rate limit; Phase 3 egress proxy).

### 9.2 Ports

**Ports (`SandboxConfig.ports`):** a backend-side tokio listener on **`127.0.0.1:<ephemeral>`**. Each accepted connection opens a
new `v.sock` connection → `tunnel.open{port}` → raw splice to the guest `127.0.0.1:<port>`. This is the FC design §6 primitive.
It works with no NIC, needs no privileges, and is identical across FC and libkrun. **TSI port maps are never used**
(they bind the wildcard address and only work with TSI). Implemented in `ports.rs` (step `lk-ports`) on core's
`PortForwards`: one forwarder per port, reported by `published_ports()`, closed on destroy, on drop and when the helper exits.
Port 0, duplicates and more than 64 ports are `InvalidConfig` before anything starts.

## 10. Resources, timeouts, teardown

- **vCPU/RAM:** `krun_set_vm_config(vcpus as u8, memory_mb)`. We check `vcpus ≤ krun_get_max_vcpus()` and `≤ 255`, else
  `InvalidConfig`. HVF maps guest RAM lazily. Host RSS grows towards `memory_mb` plus about 100 MB of VMM overhead **[spike: ~105 MB idle]**.
  There is no balloon in Phase A (`krun_set_balloon_free_page_reporting_mode` is a later lever).
- **Disk:** `disk_mb` sizes the scratch image. `/tmp` and the overlay upper live there.
- **Host-side caps:** **Linux** puts the helper in a cgroup v2 leaf (`memory.max = mem + 128 MiB`, `memory.swap.max = 0`, `pids.max = 64`), reusing the FC
  `CgroupLeaf` (moved to core or `sandcrab-rootfs`… see the DAG: copied first, deduplicated later). **macOS has no cgroups.** The guest RAM
  bounds the guest, and VMM overhead is unbounded. This is documented and surfaced by `doctor`.
- **Guest limits:** PIDs, files and so on inside the VM are the guest's business. The VM is the boundary. We do not use `krun_set_rlimits`.
- **Timeouts:** `boot_timeout` (10 s default) covers helper start → ready. TTL is enforced by the manager. Tool timeouts are `crabd`'s (`killpg`).
- **Teardown:** ① `init.shutdown{grace_ms:500}`. There is no workspace export (live virtio-fs). ② helper exit is expected within about 100 ms
  (to be measured). ③ +1.5 s → SIGTERM to the process group. ④ +1.5 s → SIGKILL. ⑤ unlink the sockets, rm the scratch image and the run dir,
  rmdir the cgroup leaf (Linux). It is idempotent. `destroy` after a crash only does ⑤.

## 11. Capabilities

```rust
BackendCapabilities { hardware_isolated: true, multi_tenant_safe: false,
                      supports_snapshots: false, supports_host_dir_mounts: true }
```

- `hardware_isolated: true`: a separate guest kernel behind HVF/KVM.
- `multi_tenant_safe: **false**`. The device model (virtio-fs server, vsock, block) runs **in-process as the host user**,
  without seccomp on macOS and without jailer/Landlock on Linux. An escape through device emulation therefore yields that user's full
  privileges, including every other tenant's workspaces. The virtio-fs passthrough is a large, guest-reachable parser.
  libkrun's own positioning is process isolation for trusted-ish workloads. Upgrade path (Phase C): confine the helper with
  **Seatbelt** (`sandbox-exec` profile: only its run dir, pinned dylibs, the leased workspace roots) on macOS, and
  **Landlock + seccomp + a per-sandbox uid** on Linux. Re-evaluate after a review. BRIEF §3 already adopted "host
  worker confinement (Landlock/Seatbelt)".
- `supports_snapshots: false`. libkrun has no snapshot/restore API.
- `supports_host_dir_mounts: true`. Live virtio-fs (§8).

## 12. Testing

| Layer | What | Where / gate |
|---|---|---|
| Unit (any host, CI) | `HelperConfig` validation (ASCII/NUL/newline, lengths, vCPU bounds), socket-path budget, policy mapping table (`disabled`/TSI flag/`egress_restricted`/ports), capability values, artifact pin resolution with a fake lib dir, `ffi` loader rejecting a wrong sha **before** `dlopen` | `cargo test -p sandcrab-backend-libkrun` |
| Guest unit | `MountSource` serde (FC compat default), virtio-fs mount planning, EBUSY-tolerant early mounts | `sandcrab-guest` (macOS-native unit tests, as today) |
| **Fake helper** (CI, any host) | The test binary re-executes itself as a fake `sandcrab-krun` (the FC `fake_vmm.rs` trick). It reads `HelperConfig`, logs it, serves `ctl.sock` (fake `init.configure`/`init.shutdown`) and `v.sock` (core `testing` mock crabd), and honours `fake-mode` (`crash_before_ready`, `hang`, `exit_73`, `ignore_shutdown`, `lifeline_test`). It covers spawn/ready/destroy, crash mapping, SIGTERM→SIGKILL escalation, lifeline EOF, the reaper, and leases released on every path | `tests/fake_helper.rs`, not ignored |
| e2e macOS | Real libkrun + signed helper + arm64 `base` rootfs. The shared e2e matrix (tools, rw/ro mounts, host mutation between turns, `default_cwd`, cross-backend workspace handoff with Docker). Plus libkrun-specific: ro survives guest remount, **host loopback unreachable with `disabled`**, no host listener appears, case-insensitivity warning | `tests/libkrun_vm.rs`, `#[ignore]`, `cfg(all(target_os="macos", target_arch="aarch64"))` |
| e2e Linux KVM | The same file, `cfg(target_os="linux")`, needs `/dev/kvm` rw. Run on `kvm-dedicated` | `#[ignore]` |
| CI | Unit and fake-helper tests run everywhere. **GitHub-hosted macOS arm64 runners are themselves VMs and are not expected to allow nested Hypervisor.framework guests** (Apple Silicon nested virt needs M3+ and macOS 15+; to be confirmed), so real e2e is **local-only on the Mac** and on `kvm-dedicated` (self-hosted runner later). Linux hosted runners with `/dev/kvm` (larger runners) could run the Linux e2e. That is optional |

### 12.1 Results on the Mac (step `lk-e2e-macos`, 2026-11-24, M2, macOS 26.2, libkrun 1.19.8)

- `tests/libkrun_vm.rs`: 7/7 (boot/hello/bash, `NoNewPrivs: 1`, no capabilities, orphans reaped
  by PID 1; live rw/ro virtio-fs both ways, symlinks inert, files host-owned; ro share refuses
  `mount -o remount,rw`; share root pinned by fd: swapping the host dir after spawn does not
  redirect the share; TSI-off regressions; `unrestricted` refused without the opt-in; published
  ports; rejected policies leave no run dir or helper).
- SDK e2e (`SANDCRAB_E2E_BACKEND=libkrun`): 14/14. Differences, asserted: `unrestricted` is
  `PolicyUnenforceable` (the dev-server scenario runs with `disabled` over tunnels).
- **TSI off holds:** with network `disabled` the guest cannot reach a `127.0.0.1` listener on the
  Mac nor the Mac's LAN address, and a guest listener on `0.0.0.0` is not reachable from the
  host's loopback or LAN address.
- **`dummy0`:** libkrunfw's kernel always creates `dummy0` (TSI's interface). With TSI off it is
  down, unaddressed and without routes, and the sandbox user has no `CAP_NET_ADMIN`. Deleting it
  in `crabd init` (`RTM_DELLINK`) works but adds ~25 ms to `init.configure` (netdev teardown),
  so it is kept and the tests accept a down `dummy*` link only on libkrun.
- **Half-close on `v.sock`:** a guest `SHUT_WR` reaches the host as EOF; a host `SHUT_WR`
  breaks the stream (reply lost, no EOF). So `forward_half_close = false`, and `crabd` closes
  vsock tunnels on target EOF (same behaviour as Firecracker; `docs/protocol.md`).
- **Spawn time (warm, `base`, 1 vCPU):** ~235 ms → **~165 ms**. Where it goes now: helper
  launch ~3 ms; pin check of libkrun + libkrunfw ~20 ms (was ~95 ms: sha2 now uses the ARMv8 SHA2
  instructions); libkrun VM setup until `ctl.sock` exists ~60 ms; guest kernel + `init.krun` +
  `crabd init` ~80 ms (`guest_boot_ms`); `init.configure` + `hello` ~1 ms (the second `hello` by
  the manager costs ~0.3 ms). The first spawn of a process adds ~60 ms (debug build: up to
  ~260 ms) in `rootfs_ready`: a cache hit still runs `docker image inspect` (~150 ms with
  Docker Desktop) and hashes `crabd`; later spawns hit the in-process cache. A cold rootfs build
  is 9–22 s (`base`) and ~60 s (`ts-dev`). The CLI pays the first-spawn cost on every `run`.
  Teardown is ~25 ms (`init.shutdown`).

### 12.2 Results on Linux KVM (step `lk-e2e-linux`, 2026-11-24, `kvm-dedicated`, Xeon D-2141I, kernel 7.0, libkrun 1.19.8)

Run as the unprivileged `<user>` (in the `kvm` group), no root, no nftables rules (TSI off
needs none; the host's Firecracker nftables setup is unrelated).

- `tests/libkrun_vm.rs`: 8/8, the Mac's seven plus `helper_runs_in_its_cgroup_leaf` (Linux).
  SDK e2e (`SANDCRAB_E2E_BACKEND=libkrun`): 14/14. The differences from Docker are the same as
  on the Mac.
- **Gaps fixed:**
  - `scripts/fetch-libkrun.sh` built libkrun without `BLK=1`, so `krun_add_disk` was missing
    (exit 74 at the first spawn). It now builds with `BLK=1` and checks the exported symbols
    before installing.
  - **`/dev/fd/<n>` share roots do not work on Linux.** libkrun's passthrough server opens its
    root with `O_PATH | O_NOFOLLOW`, and `/dev/fd/<n>` ends in the `/proc/self/fd/<n>` magic
    link, so it opened the link itself and every guest access failed with `EIO`. The helper
    now passes `/proc/self/fd/<n>/.`, which crosses the magic link as an intermediate
    component and ends at the verified directory. The identity-swap test passes.
  - libkrun renames the helper's main thread (`comm` = "libkrun VM"), so the tests'
    "no helper left" check matches the command line, not the name.
- **virtio-fs ownership (default `LINUX_COMPLETE`, no `CAP_SETUID` in the helper):** the guest
  runs as the host uid:gid. Guest-created files, directories and symlinks land on the host
  owned by `<user>`, with the guest's mode bits. A guest `chown` to another uid fails with
  `EPERM`. A guest `chmod u+s` does set the setuid bit on the host file (setuid to `<user>`,
  so no gain for the guest; the same as a Docker bind mount). Read-only shares return `EROFS`,
  and the unprivileged guest user cannot remount.
- **cgroup v2:** works with a delegated parent (`docs/hosting.md`, "libkrun on Linux"). The
  owning process must run inside the delegated tree, because moving a pid needs write access
  to the common ancestor's `cgroup.procs`. A process in an SSH session scope gets `EACCES`.
  The backend checks a configured parent at construction (`InvalidConfig` with the fix), warns
  once when none is configured, and `doctor` reports `lk:cgroup`. Measured leaf for `base`:
  `memory.max` 640 MiB (512 + 128), `memory.current` ~66 MiB, 13 pids.
- **Library pinning:** the helper's environment is exactly `LD_LIBRARY_PATH=<pinned artifact
  dir>`. A caller's `LD_PRELOAD` or `LD_LIBRARY_PATH` does not reach it, and only the pinned
  `libkrun.so.1` and `libkrunfw.so.5` are mapped.
- **TSI off and ports:** the same results as on the Mac. The guest cannot reach the host's
  loopback or public address, a guest `0.0.0.0` listener is unreachable from the host, and
  `dummy0` is down. Half-close: a guest FIN reaches the host, and a host `SHUT_WR` is not
  forwarded (`forward_half_close = false`).
- **Spawn time (warm, `base`, 1 vCPU, release helper): p50 ~407 ms** (Mac ~165 ms). The
  breakdown: helper launch ~1 ms; **pin check ~187 ms** (31 MB of libkrun + libkrunfw: this
  Xeon has no SHA-NI, and coreutils `sha256sum` takes the same ~200 ms; CPUs with SHA
  extensions do it in ~20 ms); libkrun VM setup ~110 ms; guest boot ~100 ms
  (`guest_boot_ms`); `init.configure` + `hello` ~5 ms. The first spawn of a process adds
  ~400 ms in `rootfs_ready` (`docker image inspect` + hashing `crabd`). Teardown is 30–70 ms.

### 12.3 Phase B results (step `lk-net-userspace`, 2026-11-25)

`cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1`, two new tests:

- **`userspace_network_reaches_internet_not_the_host`**: `eth0` up with `192.168.127.2` from DHCP, default route,
  `nameserver 192.168.127.1`, no IPv6 beyond link-local; `getent hosts example.com` + `curl https://example.com/` →
  `HTTP 200`. Then, in parallel from the guest, TCP to a host listener bound **only to `127.0.0.1`** and one on
  `0.0.0.0`, each at `127.0.0.1`, `192.168.127.254` (gvproxy's host-loopback alias), `192.168.127.1` (gateway),
  `10.0.2.2`, and the host's own LAN / public address (plus a listener bound to it); gateway `:80` (gvproxy API) and
  `:22`; `169.254.169.254:80`. Every probe: `closed`; **no host listener accepted a connection** (`127.0.0.1`,
  `0.0.0.0`, `[::1]`, LAN). SMTP submission (`smtp.gmail.com:587`) times out. No netstack left after destroy.
- **`userspace_disabled_has_no_nic_and_netstack_dies_with_helper`**: with the netstack installed, `disabled` is still
  loopback only and spawns no netstack; after `SIGKILL` of the helper (its pid from `meta.json`), the netstack exits.

| Host | Netstack | Result |
|---|---|---|
| Mac (M2, macOS 26.2, Homebrew libkrun 1.19.8) | gvproxy v0.9.0 | full suite 10/10; spawn → ready 386 ms with the NIC; SDK `e2e_security` (incl. `unrestricted_network_never_reaches_host_loopback`) 6/6 and `e2e_tools` 4/4 (`dev-server` now on `unrestricted`) |
| `kvm-dedicated` (Ubuntu 26.04, `apparmor_restrict_unprivileged_userns=1`) | passt `2026_10_02.cba3570`, static | full suite 11/11 (incl. the cgroup-leaf test, under `systemd-run --user --scope`); spawn → ready 813 ms with the NIC; SDK `e2e_security` 6/6 and `e2e_tools` 4/4 |

Findings:

- **`krun_set_port_map` must come before the NIC.** libkrun refuses it once `net_index != 0` (EINVAL), so the plan is
  `add_vsock(0)` → `set_port_map([])` → `add_net_unixstream`.
- **passt under Ubuntu's userns restriction:** the pinned static binary (no AppArmor profile, which only attaches to
  `/usr/bin/passt`) starts, runs its own sandbox and exits on socket EOF. No sudo, sysctl or profile change was needed.
- **passt re-execs `<its path>.avx2`** when the CPU has AVX2 and that file exists. The installer installs only `passt`
  (passt logs "Can't run AVX2 build, using non-AVX2 version" and continues), so nothing unpinned is executed. The
  install dir is the user's own (`0555` binary), the same trust as the helper.
- **Host deny list:** on a host with a Firecracker tap pool, `getifaddrs` yields 128 `/30` + `/32` entries in
  `172.30/16`. They are dropped as already covered by `172.16/12`, so the list stays small (`netstack::host_prefixes`);
  more than 256 remaining prefixes fail validation (never truncated).
- **Netstack lifetime:** the socketpair is the lifeline (EOF → exit, verified after `SIGKILL` of the helper); the
  netstack is also in the helper's process group (killed with it on destroy) and, on Linux, in its cgroup leaf.

## 13. Bench

- `sandcrab-bench`: `BackendId::Libkrun` (`"libkrun" | "krun"`), `open_libkrun` gated the same way as the SDK, and
  "unavailable" (never substituted) when the helper is unsigned, pins mismatch, or `/dev/kvm` is missing.
- Timings: `spawn_requested`, `rootfs_ready` (cache hit + scratch clone), `helper_started` (exec), `control_connected`,
  `ready` (first hello), `guest_boot_ms` (from `init.configure`), `teardown_*`. There is no `ws_attach`/`ws_sync_back` cost (live
  virtio-fs). This difference **is** the story vs FC in the report.
- Memory sampling: **Linux** uses the cgroup leaf + `smaps_rollup` PSS (as FC). **macOS** uses the helper's `phys_footprint` via
  `footprint -p`/`ps -o rss=` (no cgroups). This is labelled "RSS (macOS)" and kept in a separate table, never mixed into Linux PSS columns.
- Reports: `--backends docker,libkrun` on the Mac (Docker Desktop's VM is itself a VM, so note that it is not apples to apples; the report records
  host kind) and `--backends docker,firecracker,libkrun` on `kvm-dedicated`. `scripts/bench-report-data.py` groups by host.

## 14. Alternatives considered

**Apple Containerization framework / `container` CLI (macOS 26, Swift, Virtualization.framework).** One lightweight VM
per container, ext4 block rootfs, vsock agent (`vminitd`, gRPC), good boot times, Apple-maintained.
*Against as the backend:* (a) **macOS-only**, while libkrun gives one backend for Mac *and* Linux KVM, so the Linux e2e
on `kvm-dedicated` exercises the same code. (b) It is Swift. Driving it from Rust means a Swift bridge or shelling out
to the `container` CLI, which would make *its* agent the guest protocol and violate the "crabd everywhere" invariant (or
we fight its init to run crabd). (c) Virtualization.framework requires the `com.apple.security.virtualization`
entitlement plus its own lifecycle, so it needs the same helper-process design anyway. (d) Less control over devices (no
TSI-off-but-vsock-on knob, no per-device virtio-fs read-only semantics we verified).
*For as a complement later:* a `sandcrab-backend-apple-vz` could come later if Apple's stack proves faster or better
supported on macOS, behind the same `RuntimeBackend` trait. It is not Phase 2.

**krunkit** (libkrun CLI used by podman machine). It boots **EFI full-distro VMs** (libkrun-efi), aimed at long-lived
machines with GPU. *Against:* it is a CLI process with its own flags (no in-process control of vsock and port-map
decisions), and it uses EFI boot (slower, a heavier guest). Driving it would be a "shell out to someone else's VMM wrapper"
shortcut. Our ~200-line helper is smaller than the integration glue. *Use it as:* a debugging reference only.

**libkrun in-process (no helper).** Rejected in §2 (`start_enter` never returns, abort on bad input, entitlement on every
consumer).

## 15. Module layout

```
crates/sandcrab-rootfs/                      # NEW (extracted from backend-firecracker; FC re-exports)
  src/lib.rs  src/mkfs.rs  src/augment.rs  src/cache.rs  src/arch.rs   src/bin/sandcrab-rootfs.rs
crates/sandcrab-backend-libkrun/             # NEW
  Cargo.toml                                 # deps: sandcrab-core, -protocol, -rootfs, tokio, serde(_json), sha2,
                                             #       thiserror, tracing, libloading (helper only), rustix
  entitlements.plist
  src/lib.rs        # LibkrunBackend, LibkrunConfig, capabilities, re-exports
  src/config.rs     # LibkrunConfig { helper, lib_dir, state_dir, boot_timeout, allow_tsi_unrestricted, virtiofs_semantics }
  src/helper_proto.rs  # HelperConfig (serde, versioned) + validation, shared by backend and helper
  src/artifacts.rs  # pins: libkrun/libkrunfw per (os, arch), resolution, sha checks
  src/backend.rs    # RuntimeBackend impl, policy mapping, enforces_network_policy
  src/vm.rs         # run dir, spawn helper, lifeline pipe, readiness, timings, crash watch
  src/control.rs    # init.configure / init.shutdown over ctl.sock (shares protocol::init types)
  src/workspace.rs  # ResolvedMount → MountSource::VirtioFs / Block, identity re-check data for the helper
  src/net.rs        # policy → vsock/TSI/NIC decisions (disabled / userspace / deprecated TSI)
  src/netstack.rs   # Phase B: egress filter, frame shuttles, passt / gvproxy spawn (used by the bin)
  src/netstack_pins.rs # passt / gvproxy pins, manifest, resolve, install
  src/ports.rs      # 127.0.0.1 listeners → tunnel.open (after vsock-tunnel)
  src/reap.rs       # orphan run dirs (meta.json, pid + start time)
  src/cgroup.rs     # Linux only (copied from FC, dedup later)
  src/ffi.rs        # libloading table + safe Ctx wrapper   (the only unsafe; used by the bin)
  src/bin/sandcrab-krun.rs   # the helper: read config → verify → configure → lifeline → start_enter
  tests/fake_helper.rs  tests/libkrun_vm.rs
scripts/sign-krun-helper.sh   scripts/fetch-libkrun.sh   scripts/build-crabd.sh (rust-lld on Darwin)
```

## 16. Implementation DAG (paste into `docs/plans/libkrun-backend.flow.yaml`)

Parallelism is the absence of an edge. Steps that write the same files are chained. The root `Cargo.toml`/`Cargo.lock`
are written only by `rootfs-crate` → `lk-scaffold` (chained). FC files are touched by `rootfs-crate` (`rootfs/*`,
`lib.rs`, `Cargo.toml`) and `protocol-mounts` (`control.rs` call sites only), which are disjoint files.

```yaml
steps:
  - id: lk-design
    phase: plan
    agent: turing
    title: libkrun backend design + macOS feasibility spike
    depends_on: []
    artifacts: [docs/design/libkrun-backend.md]
    status: done

  - id: lk-approve
    phase: plan
    agent: nova
    title: Human approval of design + answers to open questions (network escape hatch, pins source, CI)
    depends_on: [lk-design]
    status: todo

  # ---- frontier 1 (parallel)
  - id: rootfs-crate
    phase: implement
    agent: ada
    title: Extract OCI→ext4 pipeline into crates/sandcrab-rootfs (lib + sandcrab-rootfs bin), add --arch/--platform, arch in cache key; FC re-exports; build-crabd.sh uses rust-lld on Darwin
    depends_on: [lk-approve]
    artifacts: [crates/sandcrab-rootfs/, crates/sandcrab-backend-firecracker/src/rootfs/, crates/sandcrab-backend-firecracker/src/lib.rs, crates/sandcrab-backend-firecracker/Cargo.toml, Cargo.toml, scripts/build-crabd.sh, scripts/build-rootfs.sh]
    status: todo

  - id: protocol-mounts
    phase: implement
    agent: ada
    title: "protocol::init GuestMount → MountSource {Block (serde default), VirtioFs{tag}}; core TemplateManifest sha256_by_arch; update FC control.rs call sites"
    depends_on: [lk-approve]
    artifacts: [crates/sandcrab-protocol/src/init.rs, crates/sandcrab-core/src/template.rs, crates/sandcrab-backend-firecracker/src/control.rs]
    status: todo

  # ---- frontier 2
  - id: lk-scaffold
    phase: implement
    agent: ada
    title: New crate sandcrab-backend-libkrun (Cargo, lib.rs, config.rs, helper_proto.rs HelperConfig + validation, entitlements.plist), scripts/sign-krun-helper.sh
    depends_on: [rootfs-crate]
    artifacts: [crates/sandcrab-backend-libkrun/Cargo.toml, crates/sandcrab-backend-libkrun/src/lib.rs, crates/sandcrab-backend-libkrun/src/config.rs, crates/sandcrab-backend-libkrun/src/helper_proto.rs, crates/sandcrab-backend-libkrun/entitlements.plist, scripts/sign-krun-helper.sh, Cargo.toml]
    status: todo

  - id: guest-virtiofs
    phase: implement
    agent: ada
    title: "crabd init: EBUSY-tolerant early mounts under init.krun, virtio-fs mount planning (MS_RDONLY for ro), unit tests"
    depends_on: [protocol-mounts]
    artifacts: [crates/sandcrab-guest/src/init/]
    status: todo

  - id: templates-arm64
    phase: implement
    agent: ada
    title: "Templates buildable for linux/arm64: index digests / per-arch shas in Dockerfiles, build.sh --platform, sha256_by_arch pins"
    depends_on: [protocol-mounts]
    artifacts: [templates/]
    status: todo

  # ---- frontier 3 (parallel, disjoint files in the new crate)
  - id: lk-helper
    phase: implement
    agent: ada
    title: "ffi.rs (libloading table, sha-before-dlopen, safe Ctx) + bin/sandcrab-krun (config→verify→configure→lifeline→start_enter, exit codes 70-79); test /dev/fd/N for virtio-fs"
    depends_on: [lk-scaffold]
    artifacts: [crates/sandcrab-backend-libkrun/src/ffi.rs, crates/sandcrab-backend-libkrun/src/bin/sandcrab-krun.rs]
    status: todo

  - id: lk-artifacts
    phase: implement
    agent: ada
    title: "artifacts.rs pins (macOS arm64 brew bottles, Linux x86_64 source builds), scripts/fetch-libkrun.sh, `sandcrab libkrun install` support fn"
    depends_on: [lk-scaffold]
    artifacts: [crates/sandcrab-backend-libkrun/src/artifacts.rs, scripts/fetch-libkrun.sh]
    status: todo

  - id: lk-backend
    phase: implement
    agent: ada
    title: "RuntimeBackend: vm.rs (run dir, helper spawn, lifeline, readiness, timings, crash watch), control.rs, workspace.rs, net.rs (disabled + TSI opt-in), reap.rs, cgroup.rs (Linux), capabilities"
    depends_on: [lk-scaffold, protocol-mounts]
    artifacts: [crates/sandcrab-backend-libkrun/src/backend.rs, crates/sandcrab-backend-libkrun/src/vm.rs, crates/sandcrab-backend-libkrun/src/control.rs, crates/sandcrab-backend-libkrun/src/workspace.rs, crates/sandcrab-backend-libkrun/src/net.rs, crates/sandcrab-backend-libkrun/src/reap.rs, crates/sandcrab-backend-libkrun/src/cgroup.rs]
    status: todo

  - id: vsock-tunnel
    phase: implement
    agent: ada
    title: "tunnel.open in protocol + crabd serve + core port forwarder (127.0.0.1); shared by FC and libkrun"
    depends_on: [guest-virtiofs]
    artifacts: [crates/sandcrab-protocol/src/, crates/sandcrab-guest/src/server.rs, crates/sandcrab-core/src/client.rs]
    status: todo

  # ---- frontier 4
  - id: lk-fake-tests
    phase: implement
    agent: ada
    title: Fake sandcrab-krun integration tests (spawn/ready/destroy, crash, escalation, lifeline, reaper, leases)
    depends_on: [lk-backend, lk-helper]
    artifacts: [crates/sandcrab-backend-libkrun/tests/fake_helper.rs]
    status: todo

  - id: lk-ports
    phase: implement
    agent: ada
    title: ports.rs (127.0.0.1 listeners → tunnel.open) for libkrun; wire FC published ports on the same primitive
    depends_on: [lk-backend, vsock-tunnel]
    artifacts: [crates/sandcrab-backend-libkrun/src/ports.rs, crates/sandcrab-backend-firecracker/src/backend.rs]
    status: todo

  - id: lk-wire
    phase: implement
    agent: ada
    title: "SDK feature libkrun + .libkrun(), CLI --backend libkrun / --libkrun-tsi / `libkrun install`, doctor checks (hv_support, entitlement, pins, /dev/kvm, APFS case), bench BackendId::Libkrun + macOS RSS sampler"
    depends_on: [lk-backend, lk-artifacts]
    artifacts: [crates/sandcrab-sdk/, crates/sandcrab-cli/, crates/sandcrab-bench/src/backend.rs, crates/sandcrab-bench/src/sample.rs, crates/sandcrab-bench/Cargo.toml]
    status: todo

  # ---- frontier 5 (real hardware)
  - id: lk-e2e-macos
    phase: implement
    agent: ada
    title: tests/libkrun_vm.rs (#[ignore]) on the Mac — shared e2e matrix + loopback/listener/ro-remount regressions; fix gaps
    depends_on: [lk-wire, lk-fake-tests, guest-virtiofs, templates-arm64, lk-ports]
    artifacts: [crates/sandcrab-backend-libkrun/tests/libkrun_vm.rs]
    status: todo

  - id: lk-e2e-linux
    phase: implement
    agent: ada
    title: Same suite on kvm-dedicated (x86_64 KVM); verify virtio-fs ownership as unprivileged user, cgroup leaf; fix gaps
    depends_on: [lk-e2e-macos]
    status: todo

  - id: lk-bench
    phase: implement
    agent: ada
    title: "Bench: docker,libkrun on the Mac; docker,firecracker,libkrun on kvm-dedicated; regenerate docs/site/bench-report.html"
    depends_on: [lk-e2e-linux]
    artifacts: [docs/bench/, docs/site/bench-report.html, scripts/bench-report-data.py]
    status: todo

  - id: lk-net-userspace
    phase: implement
    agent: ada
    title: "Phase B: unrestricted via passt (--no-map-gw, Linux) / gvproxy (macOS) with host-loopback-unreachable regression; deprecate TSI flag"
    depends_on: [lk-e2e-linux]
    artifacts: [crates/sandcrab-backend-libkrun/src/net.rs]
    status: todo

  # ---- verify / document
  - id: lk-review
    phase: verify
    agent: nebula
    title: QA + security review (helper unsafe island, TSI exposure, virtio-fs traversal/identity race, capability honesty, lifeline)
    depends_on: [lk-bench]
    status: todo

  - id: lk-live
    phase: verify
    agent: nova
    title: Live crabagent session on --backend libkrun (Mac) with a host-dir workspace
    depends_on: [lk-bench]
    status: todo

  - id: lk-docs
    phase: document
    agent: rosetta
    title: hosting.md (macOS + Linux libkrun setup), cli.md, README backend table, ROADMAP, CHANGELOG
    depends_on: [lk-review, lk-live]
    artifacts: [docs/hosting.md, docs/cli.md, README.md, docs/ROADMAP.md, CHANGELOG.md]
    status: todo
```

Frontiers: {rootfs-crate, protocol-mounts} → {lk-scaffold, guest-virtiofs, templates-arm64} →
{lk-helper, lk-artifacts, lk-backend, vsock-tunnel} → {lk-fake-tests, lk-ports, lk-wire} → e2e → bench → verify.
`lk-net-userspace` is Phase B and may be split into its own flow.

## 17. Risks

1. **TSI misuse.** A future change that drops `disable_implicit_vsock` or the empty port map silently exposes host loopback and
   publishes guest ports on the LAN **[spike]**. Mitigation: the e2e regression tests in §12, plus a unit test asserting
   the helper's call sequence for `disabled` (the fake helper logs the config, and the real helper logs each `krun_*` call at debug).
2. **virtio-fs server as attack surface.** It runs in the helper as the host user, so an escape reaches all of that user's files.
   This is why `multi_tenant_safe=false` (Seatbelt/Landlock confinement in Phase C).
3. **Linux virtio-fs semantics unverified.** Unprivileged ownership/xattr behaviour and the libkrun Linux build (and
   libkrunfw needing a kernel build, roughly 10–20 min, or prebuilt release assets) are untested until `kvm-dedicated`.
4. **Pins vs Homebrew churn.** `brew upgrade` replaces the dylibs. The copy-to-`~/.sandcrab/artifacts` step and sha checks
   turn that into a clear error, not drift. libkrun's dependent dylibs (`libepoxy`, `molten-vk`) are still loaded from
   `/opt/homebrew` by absolute install name and are not pinned. They are only used by the GPU path.
5. **libkrun aborts on bad input** (panic=abort in the VMM). It is contained by the helper process, and we validate strings first.
6. **arm64 template drift.** A template that works on x86_64 may fail on arm64 (binary downloads, native npm modules).
   Mitigation: `templates-arm64` and per-arch pins.
7. **macOS memory accounting.** There are no cgroups. VMM overhead is unbounded and RSS is a weaker metric than PSS. The bench labels it.
8. **The identity TOCTOU** (§8) remains until `/dev/fd/N` is proven.
9. **Boot numbers come from a minimal Alpine root.** Debian templates add rootfs size (page-in) but not kernel time. The
   `lk-bench` numbers are authoritative.

## 18. Decisions (human, 2026-11-24)

1. **`unrestricted` in Phase A:** ship the explicit, documented TSI opt-in (`allow_tsi_unrestricted` / `--libkrun-tsi`).
2. **Artifact source:** the install script offers **both**. It defaults to the Homebrew bottles (copied into
   `~/.sandcrab/artifacts` and pinned by SHA-256) when `brew` is present, with `--from-source` to build libkrun/libkrunfw
   from pinned tags. Without Homebrew, it offers only the from-source build. On Linux, from-source is the path.
3. **Docker at build time on macOS** for `sandcrab rootfs build`: accepted for Phase 2.
4. **Case-sensitive APFS sparse images** for managed workspaces: yes, later (not in this flow).
5. **Signing:** ad-hoc for early dev on the maintainer's Mac. Developer ID + hardened runtime + notarization are
   designed in now (§4) and implemented before the first public release.
6. **CLI default features:** add `libkrun` to the CLI's default features (on macOS aarch64 and Linux) once e2e is green.
7. **Apple Containerization:** "maybe later", outside Phase 2.
