# Benchmark results

One JSON file per `sandcrab-bench run` (schema: `schema.json`). Summarise with `sandcrab-bench summarize <file>`.

| File | Host | Commit | Valid | Status |
|---|---|---|---|---|
| `20261123T091401Z-dev-vps.json` | dev-vps (VPS, 2 vCPU EPYC 9354P, KVM guest, no nested virt) | 0d85b83 | false: 1-min load 0.89 > 0.5 at start | **canonical** (Docker V1). Drift -8.7% (pass), 0 failures, host quiet during the run |
| `20261123T103824Z-dev-vps.json` | dev-vps | 0d85b83 | false: drift +61.5% | rerun from a quieter start (load 0.12); external interference during the run (p50s 10-40% higher, density spawns up to 1.91 s). Kept for transparency, not canonical |
| `20261124T183907Z-kvm-dedicated.json` | kvm-dedicated (bare metal, Xeon D-2141I 8C/16T, 32 GB, **2x HDD RAID1**) | 65f4a2c | false: drift -11.9% (Docker got faster) | **first Docker vs Firecracker run** on real KVM. 0 failures, load 0.42 at start. Docker numbers are HDD-bound; see below |

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
