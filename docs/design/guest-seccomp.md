# seccomp in microVM guests (design note)

Status: **evaluated, deferred** (flow step `guest-hardening-2`). Nothing in this note is implemented.

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
| default seccomp profile | **none** |

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
5. **Tests.** e2e on every backend: `grep Seccomp /proc/self/status` → `2`, and a denied syscall (e.g. `unshare -U`,
   `keyctl`) fails with `EPERM` on Docker and the microVMs alike.

## Is it worth it?

It is not the primary boundary. On the microVM backends the VM is the isolation boundary. A guest-kernel exploit gives
root **inside the guest**. From there an attacker gets PID 1, the control channel, and the raw virtio queues, so the VMM
device-emulation code becomes the real attack surface. The host kernel is not directly exposed. seccomp in the guest
shrinks the guest kernel's attack surface, which is the first hop of that chain. On Docker the same filter is the
**main** defence, because the kernel is shared.

Most of the value comes from a handful of denials: user namespaces, `bpf`, `userfaultfd`, `perf_event_open`,
`io_uring`, `keyctl`/`add_key`, and kernel-module and `kexec` calls. Without capabilities most of these already fail.
The rest can be shut off with guest **sysctls** set by `crabd init`, at no binary cost and with no allow-list to
maintain:

- `user.max_user_namespaces=0`
- `kernel.unprivileged_bpf_disabled=1`
- `vm.unprivileged_userfaultfd=0`
- `kernel.perf_event_paranoid=3` (or `2` where 3 is not patched in)
- `kernel.io_uring_disabled=2`
- `kernel.dmesg_restrict=1`
- `kernel.kptr_restrict=2`

**Recommendation:** defer the full profile. Set the sysctls above in `crabd init` as a small follow-up step, with an e2e
assertion per backend. Revisit the seccomp profile together with the jailer and Landlock work, when `multi_tenant_safe`
is pursued for the microVM backends.

A side effect worth knowing for benchmarks: Docker containers pay the per-syscall BPF cost and microVM guests currently
do not, so Docker-vs-VM syscall-heavy comparisons are slightly tilted towards the VMs.
