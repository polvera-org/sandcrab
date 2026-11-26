# sdk-milestone: results

Date: 2026-11-26. What the server and SDK milestone verified, where, with which numbers, and what it
did not verify. Sources: the step reports of the flow
([`docs/plans/sdk-milestone.flow.yaml`](../plans/sdk-milestone.flow.yaml)), the live checks
([Linux](sdk-milestone-live-linux.md), [macOS](sdk-milestone-live-mac.md)) and the final review
([sdk-milestone-review.md](sdk-milestone-review.md)).

## Hosts

| Host | Machine | Backends run |
| --- | --- | --- |
| `dev-vps` | Ubuntu 26.04 VPS, 2 vCPU, 7.7 GB RAM, no KVM, other containers running (load 1 to 3) | Docker |
| `kvm-dedicated` | Bare-metal Linux with KVM, RAID1 HDDs | Docker, libkrun (KVM) |
| `mac-m2` | Apple M2, 8 cores, 16 GB RAM, macOS 26.2 | libkrun (Hypervisor.framework); Docker Desktop only to build images |

## What was verified where

| What | Where | Result |
| --- | --- | --- |
| Rust unit and integration tests | CI hosts and `kvm-dedicated` | 1037 tests green at the final review |
| TypeScript unit tests against the fake server | Any host | 64 of 64 |
| TypeScript e2e against a real server | Docker on `kvm-dedicated` (5 runs), libkrun on `kvm-dedicated` (3 runs), libkrun on `mac-m2` (3 runs) | 52 pass, 2 skipped, 0 fail on each run. Skipped: the cross-uid case (needs a second local user) and the mount-cap case for the other platform |
| Root systemd service, a client on the host and a client in a hardened container | `dev-vps` | Pass after two fixes (socket directory, self-test scratch under `UMask=0077`), see the Linux report |
| Server as a child of the client with `--lifeline-stdin` | `mac-m2` | Pass. Killing the client removed the server and every VM helper in 0.14 s |
| Self-test, 15 lines | Docker on `dev-vps` (as the root service's client), libkrun on `mac-m2` and `kvm-dedicated` | 15 of 15 pass |
| Restart: `server.shutdown`, reconnect, old ids gone | `dev-vps` | Pass; reconnect 255 ms after `disconnect` from a container |
| Crash: the new server removes the predecessor's containers | `dev-vps` | Pass |
| Directory mounts on libkrun aarch64 | `mac-m2` | 1 to 64 mounts boot; the cap stays Sandcrab's 16 |
| Directory mounts on libkrun x86_64 | `kvm-dedicated` | At most 4 devices boot; the server reports `max_mounts` 4 and refuses a fifth before boot |
| The adopter's own conformance suite through a pilot adapter | libkrun on `mac-m2`, Docker on `kvm-dedicated` | 14 cases, 3 extra cases and the self-test pass on both, 0 skipped |

## Numbers

Targets from design §13: one forwarded stream at **50 MB/s or more**, 16 parallel forwarded connections
with no reset, warm exec of `true` on Docker with **p50 at or under 30 ms**, timed at the client. All
were met on every host. Numbers are min to max over the runs made; "SDK" means timed in a TypeScript
client through the server.

### `dev-vps` (Docker, 2 vCPU, shared)

| What | Value | Target |
| --- | --- | --- |
| Warm exec `true`, SDK, 200 sequential | p50 8.8 to 9.8 ms, p95 13.2 to 15.2 ms | p50 ≤ 30 ms: met |
| One forwarded stream (256 MiB HTTP download in the sandbox) | 150 to 254 MB/s (client on the host), 243 MB/s (client in a container) | ≥ 50 MB/s: met |
| 16 parallel forwarded connections | No reset, no refusal | met |
| Cold create and first exec, 8 sequential | p50 421 to 449 ms, max 495 to 637 ms | none |
| 16 sandboxes created at once | 3.1 s for all | none |
| 100 MiB through stdout | 159 to 193 MB/s idle; 35 to 95 MB/s during a cargo build | none |
| Self-test `base` | 1.1 to 1.4 s | none |
| `sandboxDestroyed` after a 3 s TTL | 3.7 to 4.2 s after create returned | none |
| Server RSS | 9 to 11 MB idle, 12.4 MB peak | none |

### `kvm-dedicated` (Docker and libkrun on KVM, HDD storage)

| What | Value | Target |
| --- | --- | --- |
| Warm exec, SDK, Docker | p50 6.5 ms | p50 ≤ 30 ms: met |
| Warm exec, SDK, Docker, 8 × 50 concurrent in a 2-vCPU sandbox | p95 48 to 50 ms (the container's CPU quota) | none |
| Warm exec, SDK, libkrun | p50 14.7 ms | none |
| One forwarded stream, SDK | 295 MB/s Docker; about 270 MB/s libkrun | ≥ 50 MB/s: met |
| 16 parallel forwarded connections | No reset, no refusal, both backends | met |
| Self-test through the server | 5 s Docker; 1.7 s libkrun | none |
| Warm exec, Rust host to `crabd`, Docker | p50 9.2 ms | none |
| Process through the server, Rust client, Docker | spawn to exit p50 10.3 ms | none |
| Forward, Rust host to `crabd`, Docker | 374 to 390 MB/s (377 / 331 MB/s through the server) | none |
| Forward, Rust host to `crabd`, libkrun | 142 to 172 MB/s on the loaded host; 556 to 620 MB/s idle | none |
| Warm `process.open`, Rust host, libkrun | p50 10 ms | none |
| Sandbox create per call (pilot) | 1.5 to 2.7 s on Docker, busy host | none |

### `mac-m2` (libkrun, Hypervisor.framework)

| What | Value | Target |
| --- | --- | --- |
| Warm exec `true`, SDK, 200 sequential | p50 4.7 to 4.8 ms, p95 5.0 to 5.1 ms | none (the target is for Docker) |
| Warm exec, 8 × 50 concurrent | p50 4.7 to 5.6 ms, p95 5.7 to 9.0 ms; with 8 vCPUs p95 6.7 to 8.1 ms | none |
| One forwarded stream (256 MiB) | 385 to 436 MB/s | ≥ 50 MB/s: met |
| 16 parallel forwarded connections | No reset, no refusal | met |
| Sandbox create (`ts-dev`, warm cache) | 160 to 220 ms; about 410 ms for the first after a server start; about 1.2 s for the first `unrestricted` one | none |
| 16 sandboxes created at once | 803 to 899 ms | none |
| Self-test through the server | 511 to 553 ms (557 ms from the CLI) | none |
| 256 MiB pipeline between two sandboxes through the client | 5.5 s, server RSS 20 MiB | none |
| `sandboxDestroyed` after a 2 s TTL | about 3.07 s | none |
| Host delete visible in the guest | 4 to 5 ms (about 5 s on libkrun on Linux) | none |

A sandbox per call is 40 to 400 times slower than a warm exec on these hosts. A launcher that runs
many short commands should keep a pool of warm sandboxes.

## Not verified

- **macOS Docker over the `tcp` transport through the server.** Docker on macOS works for the backend's
  own tests; no server or SDK run used it.
- **`unrestricted` network through the server, end to end.** The backends' own tests cover it; no SDK
  test or live run used it, except that sandbox creation with it was timed on `mac-m2`.
- **The cross-uid guarantee end to end.** Ownership is unit-tested; the e2e case is skipped because the
  test hosts have no second local user set up for it.
- **A program that kills its supervisor.** A known limit: its tree runs until the sandbox is destroyed.
  `crabd serve` is not a subreaper. No test.
- **`tree_kill` in the capabilities** says `subreaper` before any sandbox reported anything. The
  self-test line `tree-kill` is the evidence that counts.
- **The systemd hardening options one by one.** The unit ran on `dev-vps` with all of them; which
  capabilities a root server could drop was not settled, and the unit drops none.
- **Firecracker** refuses forwards, mounts and the new process options by design; nothing of the
  server was run on it.
- **The guest clock after the Mac sleeps.** Out of scope for `live-mac`.
- **Three libkrun netstack e2e cases on `kvm-dedicated`** fail because that host's libkrun build lacks
  the `net` feature. They fail the same way on the commit before the milestone (`7dd2ad0`).
- **A mount under the guest's `/tmp` on Docker.** Not tested; on libkrun `/tmp` itself is refused as a
  mount target and mounts under `/private/tmp` work on macOS.
- **A backend object that dies by itself** (a container or VM killed from outside) sends no
  `sandbox.destroyed`; the sandbox stays listed until a destroy, its TTL or a server stop.
- **A race can send `sandbox.destroyed` twice** for one sandbox (destroy and TTL at once). Clients
  should treat it as idempotent.
- **A sandbox is not visible to its owner** between its spawn and the moment the server records the
  owner. A `listSandboxes` in that window misses it. Not a leak: it is destroyed with the server or
  its TTL.

Open items from the reports that are not bugs in the verified paths: the default log level shows no
refused connections or sandbox events (set `RUST_LOG=info`); `doctor --server-config` runs the checks
of every backend; `doctor --backend libkrun` exits 1 when templates it does not need are unbuilt;
libkrun's `init.krun` keeps a root clock worker in every guest. The full list is in
[ROADMAP.md](../ROADMAP.md#next).
