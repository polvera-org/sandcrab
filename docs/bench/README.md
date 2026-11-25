# Sandcrab benchmarks

`sandcrab-bench` (`crates/sandcrab-bench`) measures Sandcrab's runtime backends. It follows
[`docs/plans/phase-1-mvp.bench-spec.md`](../plans/phase-1-mvp.bench-spec.md), which is the
authority on metrics, scenarios, statistics, fairness rules and the decision framework.
The harness talks to the backends only through the public SDK, so the same code measures
every backend. Each run writes one schema-versioned JSON file into `results/`.

**V1 scope:** only **Docker** is measured. Firecracker is wired in: `--backends
firecracker` compiles into the default build. On a host without a usable `/dev/kvm`, or
without the pinned FC artifacts, the backend shows up in the results as
`{"status": "unavailable", "reason": "...", "todo": true}`, both in `backends[]` and on every
matrix cell. It is never replaced by another backend. Measuring Firecracker for real needs a
KVM host (kvm-homelab), which is still to do.

## Prerequisites

- A Linux host with a rootful Docker daemon, and your user in the `docker` group.
- Template images built and pinned on this host: `templates/build.sh` (at least `base`, plus `ts-dev`
  if you bench it).
- A static `crabd`: `scripts/build-crabd.sh`, or point `--crabd` / `SANDCRAB_CRABD` at one.
- Run from the repo root. `--templates-dir` defaults to `templates`, and git info is read
  from the current directory.
- Optional, as root: dropping page caches is needed for the `cold` cells. Without root those
  cells are recorded as `not_measured`, with the reason.

## Benchmark tiers: pick the smallest that answers the question

A full run is hours of machine time, mostly the `resources` cells (each run samples a 60 s idle
window, ~72 s per iteration). Choose the tier on purpose:

| Tier | Flags | Time (2 backends, 2 templates) | Use it for |
|---|---|---|---|
| **Smoke** | `--quick` | ~5 min | Revalidating after a change to a backend, the bench or the guest. The result is marked invalid and is never published. **This is the default for agents.** |
| **Standard** | `--runs 30 --warmups 5 --resource-runs 10` (add `--allow-small-n`) | ~1 h | Step-level numbers, and before/after comparisons for perf work. The result is marked invalid for resources; say so in the README note. |
| **Full** | the canonical command below | ~3 h+ | Published numbers only (bench report, landing page, presentation): once per host per release, or when a change plausibly moves the headline numbers. Requires an explicit request from the maintainer. |

Reuse results instead of re-running: if a backend's code and the host have not changed, its
last valid Full result still stands. Add the new backend in its own run and say in the README
note which runs are combined.

## Running

```bash
cargo build --release -p sandcrab-bench

# Smoke run (tiny N, short windows; the result is marked invalid). A few minutes on Docker.
target/release/sandcrab-bench run --quick --backends docker,firecracker

# Full spec run (N=30, 5 warmups, ABAB-interleaved, seeded cell order)
target/release/sandcrab-bench run \
  --backends docker,firecracker --templates base,ts-dev --scenarios all \
  --runs 30 --warmups 5 --density-budget 75% --density-cap 64 \
  --noise-declared "nothing else running" --out docs/bench/results/

# Only some cells: key = template/cache/workspace/scenario
target/release/sandcrab-bench run --only base/warm/W0/lifecycle --runs 50

# Host declaration block (CPU, memory, virt kind, KSM, THP, storage, Docker, crabd, load)
target/release/sandcrab-bench host-info          # --json for machine-readable output

# Table of p50/p95/p99/min/max/stddev for a results file
target/release/sandcrab-bench summarize docs/bench/results/<file>.json

# Regenerate the committed JSON Schema (a unit test fails if it is stale)
cargo run -p sandcrab-bench -- schema > docs/bench/results/schema.json
```

Useful flags: `--seed` (to reproduce fixtures, cell order and bootstrap; otherwise a random
seed is chosen and recorded), `--allow-small-n` (needed for N < 30, and marks the run
invalid), `--calls`/`--call-warmups` (steady-state calls per sandbox), `--resource-runs`,
`--idle-window-s` (default 60), `--settle-s` (default 5), `--density-cap`/`--density-reps`/
`--density-floor-mib`, `--touch-mib` (default 64), `--host-kind` (overrides bare-metal/vm
detection), `--max-load` (default 0.5).

`--quick` sets N=3, 1 warmup, 3 calls per metric, 1 resource run, a 2 s idle window, a 1 s
settle, density capped at 3 with 1 rep, and an 8 MiB touch.

## What is measured

| Scenario | Cells | Metrics |
|---|---|---|
| `lifecycle` (+ `calls`) | `base`: {warm, cold} × {W0, W1, W2}. `ts-dev`: warm × {W0, W2} | `spawn_ready`, `first_call`, `ws_attach`, `ws_sync_back` (W1/W2), `call.bash_echo`, `call.file_rw_1m`, `call.grep` (W1/W2), `teardown`, `cpu.lifecycle_s`, `disk.per_sandbox`, plus `phases` (backend timing marks) |
| `agent-turn` | per template × cache, W1 | `agent_turn`: spawn → the 10-call script in spec §1.1 → teardown verified |
| `resources` | per template, warm, W1 | `mem.host_pss_idle`/`_post`, `mem.cgroup_*` (current/anon/file/kernel/peak), `mem.shim_pss`, `mem.guest_available[_idle]`, `cpu.idle_60s`, `disk.per_sandbox`, `disk.shared`; VMM backends add `mem.vmm_guest_pss_*`, `mem.vmm_overhead[_post]` and `mem.vmm_rss_*` (libkrun: `mem.helper_rss_*`) |
| `density` | per backend × {idle, touched} × reps | `max_n`, `stop_reason` (`budget`/`failure`/`cap`/`floor`), and per step: spawn latency, host `MemAvailable`, ΣPSS, Σcgroup memory |
| `drift` | `base` warm W0 at start and end | `drift_check` (flagged if the p50 moves by more than 10%) |

Fixtures are generated from the seed: a 1 MiB base64 blob (checked by SHA-256 on read-back),
W1 (about 300 files and 2 MB; also the grep corpus, with the expected `fn \w+_handler` count
checked), and W2 (5,000 files, 50 MB, mounted read-only at `/data`). W1 is mounted
read-write at `/workspace` and is a fresh copy for every iteration.

Teardown counts as finished only once the resources are verified gone. On Docker that means
`inspect` returns 404 **and** the container cgroup directory has been removed, polled every
1 ms with a 10 s timeout.

## Reading results

A results file is `results/<UTC>-<host>.json` (schema: `results/schema.json`). Check these
first:

- **`valid` / `invalid_reasons`**: N < 30, a 1-minute load above `--max-load` at start, drift
  over 10%, or any cell with more than 1% failures. An invalid run is still useful, but it
  must not drive the recommendation.
- **`backends[]`**: which backends ran, which were `unavailable` (with the reason), and their
  capability flags.
- **`host`**: `kind` (`bare-metal` / `vm` / `vm-nested`), `virt`, `kvm_available`,
  `nested_virt`, governor, turbo, THP, KSM, storage, and the Docker/runc/containerd and crabd
  versions and SHA-256s.
- **`noise`**: load at start and end, top 10 processes by CPU, any Proxmox guests, the
  operator declaration, and `noisy`.
- **`cells[]`**: each metric has its raw `samples` plus `stats` (n, min, p50, p95, p99, max,
  mean, stddev, bootstrap 95% CI of p50, and `p99_indicative` when n < 100). Units are `ns`,
  `bytes` or `cpu_s`. A metric with `not_measured` explains why it has no samples. A cell
  with `status: "not_measured" | "unavailable"` carries its `reason` and `todo`.
- **`notes`**: deviations from the spec for this run. The report must repeat them.

`summarize` prints all of this as a table.

## Fairness notes and known deviations (Docker, V1)

- **Interleaving:** iterations alternate between backends (ABAB) within each cell, the
  starting backend alternates per cell, and the cell order is shuffled from the recorded
  seed. With a single available backend, its iterations run back to back.
- **Network:** every backend runs with `disabled`. `egress_restricted` cannot be enforced in
  V1, so per spec §3 both backends drop to "none". This is recorded in `notes`.
- **Bind mounts are inside `spawn_ready` on Docker.** `ws_attach` is the post-ready
  `list_dir` verification. Use `spawn_ready(W1/W2) − spawn_ready(W0)` for the mount cost.
  Firecracker's image build will appear in `ws_attach`/`ws_sync_back` once it is measured.
- **`cold`** needs root to drop caches; otherwise it is `not_measured`. **`cold-uncached`**
  is always recorded as `not_measured`: it requires deleting images or rootfs caches, which
  is destructive on a shared host, so it is a manual procedure.
- **PSS:** without root, `/proc/<pid>/smaps_rollup` cannot be read for the container's
  processes (even though crabd runs as the invoking uid, the kernel's ptrace-read check
  refuses it) or for the root-owned `containerd-shim`. `mem.host_pss_*` and `mem.shim_pss`
  are then `not_measured`, with the EACCES reason, and the `mem.cgroup_*` metrics
  (`memory.current`/`memory.stat`/`memory.peak`) are the per-sandbox signal. Run the bench
  as root to get PSS. `dockerd`/`containerd` RSS is reported once per run under
  `shared_overhead`.
- **`cpu.lifecycle_s`** = cgroup `usage_usec` + shim ticks + the `dockerd` tick delta, read
  just before `destroy()`. CPU spent during teardown is not included. For a VMM without a
  cgroup leaf it is the VMM process's `utime + stime` (`/proc/<pid>/stat`, all threads).
- **VMM memory (Firecracker, libkrun on Linux).** The VMM pid comes from the sandbox's
  `meta.json` in the backend run dir. Without a cgroup leaf (Firecracker unless
  `SANDCRAB_BENCH_FC_CGROUP` names a delegated cgroup v2 parent; libkrun unless
  `SANDCRAB_BENCH_KRUN_CGROUP`), the VMM process itself is sampled from `/proc/<pid>/smaps`:
  - `mem.host_pss_*` = whole VMM **PSS**, including the guest RAM touched so far (the spec's
    `host_pss_total` for a microVM). Guest pages stay resident once touched (no balloon).
  - `mem.vmm_guest_pss_*` = PSS of the guest-RAM mapping (the anonymous mapping(s) whose size
    equals the template's `memory_mb`; if none match exactly the split is `not_measured`).
  - `mem.vmm_overhead` (idle) / `mem.vmm_overhead_post` = PSS of every other mapping: the
    VMM's own cost, **excluding** guest RAM.
  - `mem.vmm_rss_*` (libkrun: `mem.helper_rss_*`) = whole VMM **RSS**, including guest RAM;
    shared pages (binary, libraries) count fully.
  - `cpu.idle_60s` = VMM process CPU (`utime + stime`, all threads incl. vCPUs) over the idle
    window: `/proc/<pid>/stat` on Linux (clock-tick resolution), `ps -o time=` on macOS (10 ms).
    macOS results recorded before this was unified use the id `cpu.helper_idle_60s`.
  - Docker has no VMM, so `mem.vmm_overhead` is `not_measured` there by design.
- **`mem.guest_available` on Docker** reads the host's `/proc/meminfo` (there is no lxcfs). It
  is recorded but is not a per-sandbox figure.
- **`disk.per_sandbox`** is Docker's `SizeRw` (apparent bytes in the writable layer).
  `disk.shared` is the image size. Real `du` of the overlay upper dir needs root.
- **Density** stops at the first of: the budget (75% of `MemTotal` minus the memory used at
  start), a spawn failure, `--density-cap` (default 16, versus 500 in the spec, to keep small
  hosts safe), or host `MemAvailable` falling below `--density-floor-mib`.
- **Governor/turbo/KSM** are recorded, not changed: the harness never writes host settings.
  Set them by hand before a real run (spec §4).
- **Harness overhead:** `harness.noop_overhead_ns` (median cost of timing an empty future)
  and `harness.sampler_cpu_pct` (the 10 Hz cgroup sampler's CPU) are reported for every run.

## Firecracker TODO (KVM host)

- Run on kvm-homelab: `--backends docker,firecracker`. Firecracker opens through
  `SandcrabBuilder::firecracker` when `/dev/kvm` and the pinned artifacts are present.
- FC probes still missing in `crates/sandcrab-bench/src/backend.rs`: `disk_rw` (the per-VM
  scratch drive plus workspace image bytes). Memory/CPU and teardown verification (VMM
  exited, run dir with its sockets and drives removed) are done.
- Record `templates[].rootfs_sha256` from the rootfs pipeline.
