# Benchmark results

One JSON file per `sandcrab-bench run` (schema: `schema.json`). Summarise with `sandcrab-bench summarize <file>`.

| File | Host | Commit | Valid | Status |
|---|---|---|---|---|
| `20261123T091401Z-dev-vps.json` | dev-vps (VPS, 2 vCPU EPYC 9354P, KVM guest, no nested virt) | 0d85b83 | false: 1-min load 0.89 > 0.5 at start | **canonical** (Docker V1). Drift -8.7% (pass), 0 failures, host quiet during the run |
| `20261123T103824Z-dev-vps.json` | dev-vps | 0d85b83 | false: drift +61.5% | rerun from a quieter start (load 0.12); external interference during the run (p50s 10-40% higher, density spawns up to 1.91 s). Kept for transparency, not canonical |
| `20261124T183907Z-kvm-dedicated.json` | kvm-dedicated (bare metal, Xeon D-2141I 8C/16T, 32 GB, **2x HDD RAID1**) | 65f4a2c | false: drift -11.9% (Docker got faster) | **first Docker vs Firecracker run** on real KVM. 0 failures, load 0.42 at start. Docker numbers are HDD-bound; see below |
| `20261124T222350Z-mac-m2.json` | mac-m2 (Apple M2 8C, 16 GB, macOS 26.2, arm64, internal SSD/APFS; developer laptop) | 18aa046 | false: 1-min load 4.37 > 0.5 at start | **first libkrun run** (Hypervisor.framework). libkrun only: Docker cannot run on macOS (see below). Drift -2.1%, 0 failures. Its density section is void (budget clamped to 0); use the next file |
| `20261125T020230Z-mac-m2.json` | mac-m2 | 18aa046 | false: 1-min load 1.22 > 0.5 at start | density-only rerun, same settings (75% budget, cap 16), after desktop memory pressure eased |
| `20261125T014108Z-kvm-dedicated.json` | kvm-dedicated | 97d116c | false: resource n 10 < 30 (Standard tier, by design) | **libkrun on Linux KVM**, Standard tier. Combine with `20261124T183907Z` for Docker/Firecracker on the same host (separate run, not interleaved) |

The report (`docs/site/bench-report.html`) shows all of these, grouped by host; see
[`../README.md`](../README.md#the-bench-report-docssitebench-reporthtml).

Command (both runs): `target/release/sandcrab-bench run --backends docker,firecracker --templates base,ts-dev --scenarios all --runs 30 --warmups 5 --noise-declared "..." --out docs/bench/results/` (density cap 16, budget 75%).
Firecracker is recorded as unavailable/TODO (no /dev/kvm). Cold cells, cold-uncached and PSS are not measured (no root); see the `notes` in each file.

These files predate the `DockerDev` → `Docker` rename, so they record the backend as
`docker-dev`; it is the same backend. New runs record `docker`.

## kvm-dedicated (Docker vs Firecracker, 2026-11-24)

`20261124T183907Z-kvm-dedicated.json`: same matrix as the dev-vps runs
(`--backends docker,firecracker --templates base,ts-dev --scenarios all --runs 30 --warmups 5
--density-budget 75% --density-cap 16`), on a dedicated bare-metal KVM host: Intel Xeon D-2141I
(8 cores / 16 threads), 32 GB RAM, Ubuntu 26.04 (kernel 7.0), Docker 29.10, Firecracker v1.18.0.
Storage is **2x HDD (7200 rpm) in mdadm RAID1** (`storage.type = hdd`), not SSD.

- **Docker is HDD-bound on this host, not comparable to dev-vps.** Synchronous 4 KiB writes cost
  ~22 ms here, and a bare `docker create` / `docker rm -f` (no Sandcrab involved) take ~1 s each, so
  Docker `spawn_ready` (~1.5 s p50) and `teardown` (~1.1 s) are 7-11x the dev-vps (SSD) values while
  `first_call` and steady calls match. Firecracker's rootfs is a cached read-only image plus a sparse
  scratch copy, so it is far less sensitive to this. Do not read the Docker-vs-Firecracker gap here
  as a general result; an SSD/NVMe KVM host is needed for that.
- `valid: false` only because the drift check moved -11.9% (Docker `spawn_ready` got faster over the
  run, consistent with warming HDD caches); 0 failed iterations in any cell.
- Firecracker `ws_sync_back` is measured at teardown (sync-back backend): teardown start to the
  backend's `ws_sync_done` mark. Docker's is the live bind-mount visibility. `ws_attach` on
  Firecracker includes building the workspace ext4 image (host `mke2fs -d` with libarchive).
- Firecracker memory/CPU probes (`mem.*`, `cpu.idle_60s`, PSS, `disk.per_sandbox`) are not measured
  yet (no host probe for the VMM process); density records host `MemAvailable` only.
- The run used locally re-pinned template images (images are not byte-reproducible across hosts), so
  `git.dirty` is true; no code differed from the recorded commit.
- Cold cells, cold-uncached and Docker PSS are not measured (no root), as on dev-vps.

## kvm-dedicated (libkrun on Linux KVM, 2026-11-25)

`20261125T014108Z-kvm-dedicated.json`: `--backends libkrun --templates base,ts-dev --runs 30 --warmups 5
--resource-runs 10 --allow-small-n` (Standard tier) on the same bare-metal host as the Docker/Firecracker run above,
built from 97d116c. Docker and Firecracker were not rerun: reuse `20261124T183907Z-kvm-dedicated.json`.

- **Not interleaved with Docker/Firecracker.** Same host, same n=30 per latency cell, but a different run, commit
  and tier (resource n 10 vs 30). libkrun-vs-Docker on this host is a cross-run comparison: indicative only.
- `valid: false` by design (resource n < 30). Drift +0.6%, 0 failures, load 0.38 at start.
- **Memory is real PSS and cgroup** (the helper ran in a delegated cgroup leaf): `base` ~66 MiB PSS idle, ~139 MiB
  post. The guest-RAM / VMM-overhead split (`mem.vmm_guest_pss_*`, `mem.vmm_overhead*`) is `not_measured`: the bench
  did not find the guest-RAM mapping in the helper's `smaps`. `disk.*` is not implemented for libkrun.
- **Spawn includes the per-spawn SHA-256 pin check** of libkrun/libkrunfw, slow on this Xeon without SHA
  extensions (`docs/design/libkrun-backend.md` §12.2). That is most of the gap to the Mac's ~160 ms.
- The HDD caveat above applies to Docker only; libkrun boots from a cached read-only rootfs image.
- Cold cells not measured (no root). Hostname and home paths sanitized (`/home/dev`).

## mac-m2 (libkrun on macOS, 2026-11-24)

`20261124T222350Z-mac-m2.json`: same matrix and settings as the canonical runs (`--templates base,ts-dev
--scenarios all --runs 30 --warmups 5 --density-budget 75% --density-cap 16`), but **`--backends libkrun` only**,
on an Apple M2 (8 cores, 16 GB, macOS 26.2, arm64), internal SSD (APFS). libkrun 1.19.8 / libkrunfw 5.6.4,
signed `sandcrab-krun` helper. No reduced N: the full matrix took 1 h 19 min.

- **Update:** the Docker backend has since gained a `tcp` transport that works on macOS (`docs/hosting.md`,
  "Docker on macOS"). Docker on mac-m2 is pending, scheduled for a reserved idle window. The paragraph below
  describes the state at the time of this run.
- **Docker was not measured: the Docker backend could not run on macOS.** `sandcrab --backend docker run` times out
  waiting for crabd. There are two causes, both in Docker Desktop's file sharing (virtiofs into its Linux VM):
  (1) crabd's `chmod 0660` on its socket in the bind-mounted run dir fails with `EINVAL`, so crabd exits at once;
  (2) even a guest listener that skips the chmod is unreachable: the host sees a socket inode, but `connect()`
  returns `ECONNREFUSED`, because the listening socket lives in the VM's kernel. AF_UNIX does not cross the VM
  boundary, so this is not a configuration problem. Making Docker work on macOS needs a different transport
  (e.g. crabd on a published loopback TCP port; `Endpoint::Tcp` exists in core but the Docker backend only emits
  `Endpoint::Unix`). That is an open design question; it is not fixed here.
- **Not a quiet host.** A developer laptop with a desktop session: WindowServer, a terminal, a web browser,
  several coding-agent CLI sessions and Docker Desktop's VM (idle) were running; nothing was stopped. Load was
  4.37 at start and 1.66 at the end, so `valid: false`. All the same, every lifecycle p50 has a tight spread (spawn_ready
  stddev 5-8 ms), drift was -2.1%, and no cell had failures.
- **Memory is helper RSS (macOS), not PSS.** macOS has no cgroups and no PSS. `mem.helper_rss_*` is the
  `sandcrab-krun` process's RSS, which counts shared pages (the libkrun/libkrunfw dylibs, the resident part of
  guest RAM) in full. Do not put it in a Linux PSS column. Idle CPU is `cpu.helper_idle_60s`: the helper
  process's CPU time (VMM plus vCPU threads) over the 60 s idle window (later runs record the same
  measurement as `cpu.idle_60s`, the id every other host uses). Base: 113.5 MiB idle, 221 MiB after the
  workload (64 MiB touched), 0.06 CPU-s per 60 s idle. ts-dev: 142 MiB idle, 262 MiB post, 0.08 CPU-s.
  If Docker is ever measured on a Mac, note that Docker Desktop's VM overhead is not attributed per container.
- **Density: use the rerun.** In the main run, `75% × MemTotal − used` was negative (the desktop had about
  13 GB in use, ~6 GB of it in the memory compressor), so the budget clamped to 0 and every rep stopped at one
  sandbox. `20261125T020230Z-mac-m2.json` reran density alone with the same settings once memory had freed up:
  **16/16 (cap) in all 6 reps**. spawn_ready stays flat at 145-173 ms up to 16 concurrent VMs. Host
  `MemAvailable` dropped by ~61 MiB per idle sandbox and ~131 MiB per touched one. On macOS, `MemAvailable` is an estimate
  (free + file-backed + purgeable pages from `vm_stat`). The first spawn of that process took 3.1 s (one-time
  in-process rootfs/crabd hash check); later ones were steady.
- **`ws_attach` is not a mount cost on libkrun.** Workspaces are live virtio-fs shares (`ws_sync_back` ≈ 0.03 ms).
  `ws_attach` is the post-ready `list_dir` verification walking the tree over virtio-fs: 23 ms for W1, 320-367 ms
  for W2 (5,000 files).
- `first_call` is bimodal: p50 2.8 ms, but p95 is ~12 ms.
- Consistent with earlier one-off measurements: spawn→ready p50 156-162 ms (earlier ~157 ms), helper RSS ~114 MiB idle.
- Cold cells and cold-uncached are not measured (no root: dropping caches on macOS needs `sudo purge`).
  `harness.sampler_cpu_pct` is `null` (there is no cgroup sampler on macOS).
- The host block was collected with the macOS probes added in this step (sysctl, `vm_stat`, `diskutil`). The
  hostname and paths were replaced with `mac-m2`, a repo-relative crabd path and `<scratch>` for the work/out dirs.
