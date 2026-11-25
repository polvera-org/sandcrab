# seccomp in microVM guests (design note)

Status: the seccomp profile is **evaluated, deferred** (flow step `guest-hardening-2`). Its cheap substitute, the
kernel-interface hardening recommended below, is **implemented** (flow step `guest-hardening-3`): hardening sysctls,
read-only `/proc/sys` (and Docker's other `ReadonlyPaths`) and a read-only `/sys`.

## Where the guest stands

On Docker, crabd and every tool process run under Docker's default seccomp profile. The microVM backends
(Firecracker, libkrun) have no guest seccomp filter. Everything else on the Docker baseline is matched by `crabd init`:

| Docker | microVM guest (`crabd init` / `crabd serve`) |
|---|---|
| `cap_drop: ALL` (`CapBnd` 0) | bounding set emptied before `crabd serve` is forked |
| `no-new-privileges` | `no_new_privs` set by `crabd serve` |
| `pids_limit` 1024 | cgroup v2 `/sandbox` with `pids.max` 1024 (fallback `RLIMIT_NPROC`) |
| `/tmp` tmpfs `nosuid,nodev,noexec,size=256m` | same tmpfs |
| `RLIMIT_CORE` 0 | same |
| read-only cgroupfs | same (remounted read-only once `crabd serve` is placed) |
| read-only `/sys` (sysfs mounted `ro`) | `/sys` bind-remounted `ro,nosuid,nodev,noexec` at boot |
| `ReadonlyPaths`: `/proc/bus`, `/proc/fs`, `/proc/irq`, `/proc/sys`, `/proc/sysrq-trigger` | same paths bind-mounted read-only over themselves |
| default seccomp profile | **none**; the hardening sysctls below close its most valuable denials |
| host-level sysctls (whatever the host has) | hardening sysctls set by `crabd init` |
| `MaskedPaths` (`/proc/kcore`, `/proc/keys`, `/proc/timer_list`, `/sys/firmware`, ...) | **none** (see "What is left") |

## Hardening sysctls (implemented)

`crabd init` writes these before any sandbox code runs (`crabd serve` only starts at `init.configure`). The list is
`sandcrab_protocol::init::HARDENING_SYSCTLS`. Each value is read back. A sysctl the kernel lacks is skipped and logged.
`/proc/sys` is made read-only right after, so a sandbox user with uid 0 cannot write them back. Several of these handlers have no capability check (for example
`perf_event_paranoid`, `io_uring_disabled`, `unprivileged_userfaultfd`), so without the read-only mount, root's owner
permission bits would be enough to undo them.

| sysctl | value (first that sticks) | Firecracker guest kernel 6.18 | libkrunfw 5.6 (kernel 6.12) |
|---|---|---|---|
| `kernel.unprivileged_userns_clone` | `0` | absent (Debian/Ubuntu patch) | absent |
| `user.max_user_namespaces` | `0` | `0` | `0` |
| `kernel.unprivileged_bpf_disabled` | `1` (sticky), else `2` | `1` | `1` |
| `vm.unprivileged_userfaultfd` | `0` | `0` | `0` |
| `kernel.io_uring_disabled` | `2`, else `1` | `2` | `2` |
| `kernel.perf_event_paranoid` | `3`, else `2` (mainline treats ≥ 2 as 2) | `3` | `3` |
| `kernel.kptr_restrict` | `2` | `2` | `2` |
| `kernel.dmesg_restrict` | `1` | `1` | `1` |
| `fs.protected_symlinks` / `fs.protected_hardlinks` | `1` | `1` | `1` |
| `fs.protected_fifos` / `fs.protected_regular` | `2`, else `1` | `2` | `2` |
| `fs.suid_dumpable` | `0` | `0` | `0` |

The kernel columns are what the e2e suite read back on an x86_64 KVM host. Kernels built for another arch can differ;
`crabd init` logs what it could not set (`sysctls not supported by this kernel`, `sysctl not hardened`).

### Failure policy: fail closed

Guest hardening **fails closed**. `crabd init` keeps booting so that the host gets a clear error instead of a boot
timeout, but `init.configure` refuses (before it mounts a workspace or starts `crabd serve`) with
`guest hardening failed, refusing to start the sandbox: <steps>`, and the spawn fails with that message. It refuses
when:

- a hardening sysctl exists but none of its values sticks (`sysctl not hardened`);
- a read-only remount fails: `/proc/sys` or another of Docker's `ReadonlyPaths`, or `/sys`;
- the cgroupfs seal after `crabd serve` is spawned fails (`crabd serve` is killed before the host ever connects);
- there is no pids cap at all, or only the `RLIMIT_NPROC` fallback and the sandbox user is uid 0 (the kernel never
  applies `RLIMIT_NPROC` to uid 0).

Only a sysctl the kernel does not have (`ENOENT`) stays a log line: the guest kernel is pinned, and a missing key means
the feature it guards is missing too. A `ReadonlyPaths` entry that does not exist is skipped for the same reason.

`/sys` is read-only only after PID 1 is done with it: block devices are scanned and the pids cgroup is set up first.
The cgroup v2 mount under it is a separate mount, sealed separately.

### Effects on workloads

- **No user namespaces at all** (`user.max_user_namespaces=0` applies to root too): `unshare -U`, rootless
  containers (`podman`, `buildah`, rootless BuildKit), `bwrap`/Flatpak and similar tools fail (`ENOSPC` from
  `unshare(CLONE_NEWUSER)`). This is the same outcome as on Docker, where the seccomp profile refuses the call with
  `EPERM`.
- **Chromium-family browsers** (Chrome, Electron, Puppeteer, Playwright) cannot start their user-namespace sandbox, and
  the setuid sandbox fallback cannot work under `no_new_privs`. Run them with `--no-sandbox` (Playwright:
  `chromiumSandbox: false`, the default for its bundled Chromium), as is already required on Docker. The sandbox VM
  stays the isolation boundary.
- **io_uring is off** (`io_uring_setup` fails with `EPERM`): libuv (Node.js) falls back to its thread pool; programs
  that require io_uring (`tokio-uring`, plain `liburing` users) fail, as on Docker. Node.js, npm, Python, `uv`, `git` and
  `bash` are unaffected (checked on the `base`, `python-dev` and `ts-dev` templates on both microVM backends).
- **No unprivileged `perf`, `bpf`, `userfaultfd`, `dmesg`**: profilers such as `perf` and eBPF tooling do not work in
  the sandbox (they do not on Docker either).

### Docker backend

These sysctls are not namespaced per network or IPC namespace, so Docker refuses them in a container's `sysctls` and
they stay whatever the host has. Sandcrab never touches host sysctls. Docker covers the same ground differently:
its seccomp profile refuses namespace creation, `bpf`, `userfaultfd`, `perf_event_open` and (Docker ≥ 25) `io_uring`
to a container without capabilities, and `/sys` and `/proc/sys` are read-only. The e2e test
`kernel_interfaces_sysfs_sysctls_userns` checks the read-only mounts, a failing `/sys` and sysctl write, and a failing
`unshare -U` on every backend; it checks the sysctl values on the microVM backends only.

## What is left

- **The seccomp profile** (below): deferred.
- **`MaskedPaths`**: Docker also hides `/proc/acpi`, `/proc/kcore`, `/proc/keys`, `/proc/latency_stats`,
  `/proc/timer_list`, `/proc/timer_stats`, `/proc/sched_debug`, `/proc/scsi`, `/sys/firmware` and
  `/sys/devices/virtual/powercap` (bind-mounting `/dev/null` or an empty read-only tmpfs over them). In the guest
  these show only the guest kernel's own state, and `kptr_restrict=2` already zeroes kernel pointers, so this is an
  information-leak follow-up, not a boundary.

## What applying Docker's profile would take

1. **Profile source.** Vendor a pinned copy of moby's `profiles/seccomp/default.json`, converted at build time
   (`build.rs`, or a checked-in generated file) to the filter format of the
   [`seccompiler`](https://github.com/rust-vmm/seccompiler) crate. seccompiler is rust-vmm's library, the one Firecracker
   itself uses with musl. Docker's profile is an allow-list (default action `EPERM`) with argument filters (`clone` /
   `clone3` namespace flags, `personality`, `socket` families). seccompiler expresses all of these. The rules Docker
   makes conditional on capabilities (`includes.caps`) resolve statically to the "no capabilities" branch, because the
   bounding set is empty. We need one filter each for x86_64 and aarch64.
2. **Where to install.** In `crabd serve`, after `no_new_privs`, which unprivileged seccomp requires and serve already
   sets. The filter must go in after the vsock listener is bound: check whether the pinned profile still denies
   `socket(AF_VSOCK, …)` (recent moby profiles do). Use seccompiler's `apply_filter_all_threads` (TSYNC), so the tokio
   workers that exist by then are covered as well. Every tool process inherits the filter, and nothing can remove it.
   Doing this in `crabd serve` instead of a per-child `pre_exec` keeps the workspace free of `unsafe`.
3. **crabd's own syscalls.** Under Docker, serve already runs inside this profile (Unix socket, PTYs, process groups,
   `killpg`), so the only new surface is vsock: `accept4` on the bound listener, and `connect` for tunnels, which go to
   loopback TCP.
4. **Binary size.** seccompiler is small, pure Rust over `libc`. If the BPF is compiled at build time, the runtime cost
   is the filter bytes plus a small loader. Expect roughly 100–200 KiB on a ~5 MB static crabd. **Measure** this before
   committing to it; AGENTS.md requires a small, musl-compatible crabd.
5. **Tests.** e2e on every backend: `grep Seccomp /proc/self/status` → `2`, and a denied syscall (e.g. `keyctl`) fails
   with `EPERM` on Docker and the microVMs alike.

## Is it worth it?

It is not the primary boundary. On the microVM backends the VM is the isolation boundary. A guest-kernel exploit gives
root **inside the guest**. From there an attacker gets PID 1, the control channel, and the raw virtio queues, so the VMM
device-emulation code becomes the real attack surface. The host kernel is not directly exposed. seccomp in the guest
shrinks the guest kernel's attack surface, which is the first hop of that chain. On Docker the same filter is the
**main** defence, because the kernel is shared.

Most of the value comes from a handful of denials: user namespaces, `bpf`, `userfaultfd`, `perf_event_open`,
`io_uring`, `keyctl`/`add_key`, and kernel-module and `kexec` calls. Without capabilities most of these already fail,
and the sysctls above shut off the rest except `keyctl`/`add_key` (the session keyring is usable unprivileged).

**Recommendation:** keep the full profile deferred. Revisit it together with the jailer and Landlock work, when
`multi_tenant_safe` is pursued for the microVM backends.

A side effect worth knowing for benchmarks: Docker containers pay the per-syscall BPF cost and microVM guests currently
do not, so Docker-vs-VM syscall-heavy comparisons are slightly tilted towards the VMs.
