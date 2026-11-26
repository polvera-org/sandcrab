# sdk-milestone: live run on macOS (libkrun, Apple Silicon)

Step `live-mac` of the sdk-milestone flow (2026-11-26). This was the first time anything from the
milestone ran on macOS. The setup is the first adopter's: the client application is a host process
that starts `sandcrab server --lifeline-stdin` as its child, on libkrun over Hypervisor.framework.

**Machine:** `mac-m2`, an Apple M2 with 8 cores and 16 GB RAM, on macOS 26.2. It runs libkrun 1.19.8
and libkrunfw 5.6.4 (Homebrew bottles, pinned), gvproxy 0.9.0 and Docker Desktop 29.1.3 (used only to
build images). The build was debug `sandcrab` and `sandcrab-krun` plus a release `crabd` for
aarch64-unknown-linux-musl linked with `rust-lld`. Node is 26.12.

## Verdict

| # | Item | Result |
| --- | --- | --- |
| 1 | Build, helper install and signing, rootfs rebuild with the new `crabd`, `doctor` | **pass** (two doc errors fixed, one low finding) |
| 2 | `sandcrab selftest --backend libkrun --template base` | **pass**: 15/15 lines, 557 ms |
| 3 | Directory-mount cap on aarch64/HVF | **no blocker**: 1 to 64 mounts boot. The binding cap is Sandcrab's own 16 |
| 4 | TS SDK e2e on libkrun, server started as a child with a lifeline | **pass** after five test-only fixes. There were no product bugs |
| 5 | Three consecutive green e2e runs | **pass**: 3 × (52 pass, 0 fail, 2 skipped), about 39 s each |
| 6 | Client killed, so the server and every VM helper must die | **pass**: everything was gone 0.14 s after the SIGKILL, with nothing left behind |

## 1. Setup, step by step

| Step | Result | Notes |
| --- | --- | --- |
| `cargo build -p sandcrab-cli -p sandcrab-backend-libkrun --bin sandcrab --bin sandcrab-krun` | ok | debug profile, `CARGO_INCREMENTAL=0` |
| `scripts/build-crabd.sh aarch64` | ok, 1 min | `rust-lld`, static, 5.0 MB |
| `templates/build.sh ts-dev` | ok | The image was missing although its pin existed. The rebuild re-pinned it |
| `sandcrab libkrun install --helper target/debug/sandcrab-krun` into a fresh `SANDCRAB_HOME` | ok | ad hoc signed with `com.apple.security.hypervisor`. Re-installing after each rebuild works, and so does re-signing |
| `sandcrab --backend libkrun doctor` | libkrun: ready | Exit 1 only because `python-dev` and `render` were not built (finding L2) |
| rootfs rebuild with the new `crabd` | ok | The first `run -t base` built it in 7.3 s and `ts-dev` in 20.4 s. After that a `run -- true` takes 0.33 s end to end |

**Doc errors found (fixed in `8eddb52`):** `sdk/typescript/test/e2e/README.md` told macOS users to run
`sandcrab --backend=libkrun rootfs build ts-dev base`. macOS has no `rootfs` command (it is
Firecracker-only, Linux builds), so the README now says to run one sandbox per template there. The
README also hard-coded the x86_64 `crabd` path, which it now picks per architecture.

## 2. Self-test (libkrun, `base`)

All 15 lines pass in 557 ms: caps-empty, no-new-privs, userns-refused, net-loopback-only,
net-external-refused, dns-refused, gateway-refused, metadata-refused, forward-reaches-target,
forward-only-declared, forward-fails-closed, fs-outside-mounts, other-sandbox-unreachable,
exec-env-exact and tree-kill. Through the server with `ts-dev` it takes 511 to 553 ms. The old ~62 s
figure is not seen here.

## 3. Directory-mount cap on aarch64 / Hypervisor.framework

The probe created sandboxes through the server with N host-directory mounts, alternating rw and ro,
then checked from the guest that every mount is present and in its mode.

| N | network `disabled` | network `unrestricted` (NIC) | create (warm) |
| --- | --- | --- | --- |
| 1, 4, 5, 8, 12, 16 | boots, modes right | boots, modes right | 160 to 220 ms |
| 17, 24, 32, 48, 64 (caps raised to 64 in a scratch build, then reverted) | | boots, modes right | 170 to 370 ms |

**Answer:** this host is not limited by devices. On aarch64, libkrun places virtio-mmio devices on GIC
interrupts, so the x86_64 limit (legacy IRQs, 4 mounts) does not apply. 64 directory mounts plus a NIC
boot. The binding limit is Sandcrab's own `MAX_SANDBOX_MOUNTS` = 16 (core), together with the libkrun
`MAX_VIRTIOFS_SHARES` = 16 and the SDK's `MAX_MOUNTS` = 16. `caps.max_mounts = 16` is therefore true
on this host. The feed-forward fix (one share per root, with bind mounts in the guest) is **not needed
for the Mac** and was not built.

What is left:

- **M1 (medium, adopter):** the adopter needs 7 mounts plus 2 per extra folder. At 16, a run with
  more than 4 extra folders is refused. On aarch64 the 16 can safely go to at least 32. That is one
  constant in each of core, libkrun `helper_proto` and the TS SDK. The decision belongs to the owner.
- **M2 (high, Linux x86_64 only, not verifiable here):** `caps.max_mounts` still reports 16 on x86_64
  libkrun, where the real cap is 4 (3 with a NIC). The server takes the value from
  `MAX_SANDBOX_MOUNTS` and never asks the backend. Suggested fix: an optional `max_mounts` on
  `BackendCapabilities` (libkrun x86_64 `Some(X86_64_DEVICE_ROOM)`), with the server reporting the
  minimum of the two. Belongs to `live-linux` or review-fixes.

## 4. TS SDK e2e on libkrun (`SANDCRAB_E2E_BACKEND=libkrun`)

The first run failed 25 cases, plus one cancelled. Every failure came from the tests assuming Linux.
None was a product bug:

| Commit | Cause |
| --- | --- |
| `65eb214` | libkrun's `init.krun` on macOS forks a **root clock worker** (`/init.krun init`, guest time sync after host sleep, built with TIMESYNC). It lives beside `crabd init`, and the leftover check counted it in every process and exec case (24 failures) |
| `e599be7` | The lifetime cases looked for leftover helpers in host `/proc`, which a Mac does not have. They now use `pgrep -x -U` |
| `0781b76` | The pipeline case sampled server RSS with a synchronous `ps` every 50 ms. On macOS one `ps` takes ~80 ms while VMs run, so the client's event loop stayed blocked, 256 MiB flowed at ~150 KB/s and the case hit its timeout. The sampler is now asynchronous |
| `5ca77a7` | With `SANDCRAB_E2E_DATA=/tmp` the tests compared against `/tmp/...` while the sandbox, correctly, uses `/private/tmp/...`. They also expected the client's gid on a sandbox write, but macOS gives a new file its directory's group (`wheel` under `/private/tmp`). The client's own write beside it gets the same `501:0` |

Numbers, from three green runs (min to max):

| What | mac-m2 |
| --- | --- |
| warm exec `/bin/true` through the SDK, 200 sequential | p50 4.7 to 4.8 ms, p95 5.0 to 5.1 ms |
| warm exec, 8 × 50 concurrent | p50 4.7 to 5.6 ms, p95 5.7 to 9.0 ms |
| warm exec with vcpu 8, 8 × 50 concurrent | p50 5.4 to 5.9 ms, p95 6.7 to 8.1 ms |
| forward throughput, one 256 MiB stream | 385 to 436 MB/s |
| 16 parallel forward connections | ok, no reset and no refusal |
| pipeline of 256 MiB between two sandboxes through the client, with a pausing reader | 5.5 s, server RSS 20 MiB, peak +0.5 to 0.7 MiB |
| sandbox create (`ts-dev`, warm cache, through the server) | 160 to 220 ms. The first one after a server start takes ~410 ms, and the first `unrestricted` one ~1.2 s (gvproxy start) |
| 16 sandboxes created at once | 803 to 899 ms |
| self-test through the server (`ts-dev`) | 511 to 553 ms |
| TTL 2 s until `sandboxDestroyed` | ~3.07 s |

Mounts:

- **Paths under `/Users`** keep their exact path in the guest. Under `/tmp` (`SANDCRAB_E2E_DATA=/tmp`)
  the sandbox gets the resolved `/private/tmp/...` path, as the SDK reports in `resolvedPath`. A
  program in the guest that uses the client's `/tmp/...` spelling will not find the mount.
- **Ownership:** the sandbox runs as `501:20`, the server's own uid:gid. Its writes land on the host as
  `501:20` under `/Users`, and as `501:0` under `/private/tmp` (BSD group inheritance, the same as the
  client's own writes there).
- **Host delete lag:** on macOS a host-side delete of a file the guest had already looked up shows in
  the guest after **4 to 5 ms**. The ~5 s lag measured on Linux KVM does not happen on this virtio-fs
  implementation. Docs should give the lag as Linux-only.

**The macOS `ECONNREFUSED` retry branch:** a raw burst of connects to one sandbox's `v.sock` showed
that macOS refuses a full listen queue with `ECONNREFUSED` (64 → 7 refused, 256 → 72, 1024 → 57;
`somaxconn` 128). That is exactly the case `listener_busy` retries on macOS. Through the connector, the
bursts the e2e makes (16 parallel forwards, 8 × 50 execs, 32 processes at once, 16 sandboxes at once)
saw no refusal. The retry absorbs them.

## 6. Lifeline: kill the client

The adopter's setup was reproduced: a Node client started the server as its child (stdin pipe,
`--lifeline-stdin`), made 3 `ts-dev` sandboxes with rw mounts and left a `sleep 600` running.

- **SIGKILL to the client:** 0.14 s later the server and all 3 `sandcrab-krun` helpers had exited.
  `run/krun/` was empty and the socket was removed (`pgrep -x sandcrab-krun` found none, and no
  gvproxy was left).
- **SIGKILL to the server only:** the helpers died within 100 ms (their own stdin lifeline). Their run
  directories stay until the next server start or `doctor`, which reaped all 3 (`lk:orphans`).
- The Mac going to sleep was out of scope.

## Findings

| Id | Severity | Finding | State |
| --- | --- | --- | --- |
| M2 | high (Linux x86_64) | `caps.max_mounts` says 16 where libkrun x86_64 boots 4 | open, for live-linux or review-fixes (§3) |
| M1 | medium | The 16-mount cap is Sandcrab's own. aarch64 boots 64. The adopter is limited to 4 extra folders | owner decision (§3) |
| L1 | low | Every client readiness probe logged `WARN no peer credentials ... (os error 57)`. macOS answers ENOTCONN once the peer has closed | **fixed** `13c4d5f` (now logged at debug, with a unit test) |
| L2 | low | `doctor --backend libkrun` exits 1 when templates that are never used (`python-dev`, `render`) are not built, even though libkrun is ready | open |
| I1 | info | libkrun's macOS `init.krun` keeps a root clock-worker process in every guest (time sync over vsock). It belongs to libkrun's TCB, not to the sandbox user. Tests and tooling that list guest processes must ignore it | test fixed `65eb214`. Docs could mention it |
| I2 | info | The virtio-fs delete lag is ~5 ms on macOS, not ~5 s | docs step |
| I3 | info | `/tmp` paths resolve to `/private/tmp` in the guest, and files under `/private/tmp` get group `wheel` | adopter note |
| I4 | info | The e2e README told macOS users to run `rootfs build`, and hard-coded the x86_64 `crabd` path | **fixed** `8eddb52` |

## Commits

`65eb214`, `e599be7`, `0781b76`, `5ca77a7` (e2e tests), `8eddb52` (e2e README) and `13c4d5f` (server
log level). The full gates (fmt, clippy, workspace tests) were not run on the Mac and still have to
run on the Linux build host. Only `cargo test -p sandcrab-server --lib a_peer_gone` and `cargo fmt`
ran here.
