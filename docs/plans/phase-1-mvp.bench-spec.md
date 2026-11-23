---
title: "Phase 1 benchmark spec — DockerDev vs Firecracker"
status: draft
date: 2026-11-23
flow: docs/plans/phase-1-mvp.flow.yaml
step: bench-spec
consumers: [bench-harness, bench-run, bench-report]
---

# Phase 1 benchmark spec: DockerDev vs Firecracker

## 0. Purpose and stance

This spec says how to measure Sandcrab's two V1 backends and how to turn the numbers into a recommended default backend for each deployment context:

- **DockerDev**: shared-kernel container via `bollard`, `crabd` over UDS.
- **Firecracker (FC)**: KVM microVM, `crabd` as PID 1, connected over vsock.

**Bias guard.** The maintainer expects FC to win. That expectation must not shape the method. The rules below exist so either outcome is believable: interleaved runs, identical inputs, metrics and the decision framework fixed before any data exists, and raw data published. FC V1 has **no snapshot restore**, so every FC number is a cold boot. The <50 ms warm-restore figure in `BRIEF.md` must not be credited to V1. Per ADR 0001, V1 FC reports `multi_tenant_safe = false` until `jailer` lands. The report must say this and must not overstate FC's isolation.

## 1. Latency metrics

All timestamps come from `CLOCK_MONOTONIC` in the bench process (`std::time::Instant`) and are recorded in ns. Each metric records its own start and stop points. A metric is **invalid** if any RPC in it returns an error. An invalid run is recorded with `ok=false` and is excluded from the stats; it still counts toward the failure rate.

| ID | Metric | Start (t0) | Stop (t1) |
|---|---|---|---|
| `spawn_ready` | Spawn to crabd ready | just before `RuntimeBackend::spawn()` | `hello` RPC response received and parsed (first successful one; retries are inside the window) |
| `spawn_ready.phases` | Breakdown (optional, backend-reported) | — | Docker: create / start / UDS connect. FC: rootfs prep / VMM start / `InstanceStart` / vsock connect. Backend timing hooks emit these |
| `first_call` | First tool call | right after `spawn_ready` t1 | response to `bash {cmd:"true"}` |
| `call.bash_echo` | Steady-state bash | send `bash {cmd:"echo ok"}` | response |
| `call.file_rw_1m` | 1 MiB write + read | send `file_write` (1 MiB of seeded random bytes, base64) | `file_read` response whose bytes match (verify the SHA-256) |
| `call.grep` | Grep over a fixture repo | send `grep {pattern:"fn \\w+_handler", path:<fixture>}` | response, with the match count checked against the expected value |
| `ws_attach` | Workspace attach/sync | start of workspace preparation for the sandbox (Docker: bind-mount spec; FC: host-dir → ext4 image build or image attach, per `fc-design`) | crabd `list_dir` on the mount returns the expected entries |
| `ws_sync_back` | Host visibility of guest writes | after the guest `file_write` to the rw mount | the host-side reader sees the bytes (Docker: immediately; FC: after the extract/sync step defined by `fc-design`) |
| `teardown` | Destroy | just before `destroy()` | resources are verified gone: Docker container is absent from `inspect` (404) and the cgroup dir is removed; FC VMM PID has exited, tap device, vsock UDS, API socket, and per-sandbox rootfs/overlay are removed. Polled every 1 ms, 10 s timeout = failure |
| `agent_turn` | End-to-end turn | before `spawn` | `teardown` t1. Contents: spawn, mount workspace (W1), then the 10-call fixed script in §1.1, then teardown |

Steady-state `call.*` metrics are measured after 5 discarded warm calls inside the same sandbox, then 30 timed calls per sandbox.

`ws_attach` is measured **separately** from `spawn_ready` so that FC's block-image cost is not hidden inside "boot". It is also reported as `spawn_ready_with_ws = spawn_ready(W*) − spawn_ready(W0)`. Only `agent_turn` folds it in.

### 1.1 Agent-turn script (fixed, seeded)

1. `list_dir /workspace`
2. `file_read src/main.rs`
3. `grep "TODO"`
4. `file_edit` (one hunk)
5. `bash "ls -la && wc -l src/*.rs"`
6. `file_write notes.md` (4 KiB)
7. `glob "**/*.rs"`
8. `bash "sleep 0.05 && echo done"`
9. `file_move notes.md notes2.md`
10. `file_delete notes2.md`

## 2. Resource metrics

The sampler is a host-side `/proc` + cgroup v2 reader running at 10 Hz in the bench process. Its own overhead is recorded.

**Memory per sandbox.** Record every one of these. None of them is the headline alone.

- Docker: `memory.current` and `memory.stat` (`anon`, `file`, `kernel`) of the container cgroup, plus the summed PSS (`/proc/<pid>/smaps_rollup`) of all processes in the cgroup. Also record the `containerd-shim` PSS, which sits outside the container cgroup.
- FC: the VMM process RSS and PSS (`smaps_rollup`), split into the guest-memory mapping and VMM overhead (sum of the non-guest mappings). If `jailer`/cgroup is used, also record its `memory.current`.
- **Headline, for fairness:** `host_pss_total` = PSS of everything attributable to the sandbox. Docker: cgroup PSS + shim PSS. FC: VMM PSS. Daemons that are shared and amortised (`dockerd`, `containerd`) are reported once per run as `shared_overhead`, not per sandbox.
- **Guest-memory-touched caveat.** FC RSS grows as the guest touches pages and does not shrink without a balloon. A guest page cache that has been filled stays as host RSS. Docker's page cache is charged to the cgroup but is reclaimable and shared. So the report must show FC idle (just after ready) **and** FC after the workload. It must also record guest-side `MemAvailable` (via `bash cat /proc/meminfo`) next to the host number. Neither backend may use a balloon in V1.
- Sample at **idle** (ready + 5 s) and **post-workload** (after the §1.1 script plus a 64 MiB file write/read, + 5 s settle).

**CPU.** `cpu_seconds_lifecycle` = host CPU time (user+sys) attributable to one sandbox from spawn to teardown. Docker: cgroup `cpu.stat usage_usec` + shim + the dockerd delta (from `/proc/<pid>/stat`, attributed during serial runs only). FC: VMM process + the per-sandbox helper processes. Also record idle CPU per sandbox over 60 s.

**Disk.** `disk_per_sandbox` = bytes newly allocated on the host per live sandbox (`du --apparent` and real `du` on the container upper dir or the FC per-VM rootfs copy/overlay + workspace images). `disk_shared` = image layers, the base ext4 rootfs, and the kernel. Report both, plus the reflink/sparse status of the filesystem.

**Density.** Spawn idle `base` sandboxes one at a time with a fixed spec (1 vCPU, 512 MiB), stopping at the first of: **host memory budget = 75% of `MemTotal` minus the baseline used**, the first spawn failure, or N = 500. Record `max_n`, the spawn latency of each i-th sandbox (to plot degradation), host `MemAvailable` per step, and `host_pss_total`/N. Run 3 repetitions per backend; this is the exception to the N ≥ 30 rule. Run a second density variant with each sandbox's workload touched (the post-workload state) to show the guest-memory caveat.

## 3. Scenario matrix

| Axis | Values |
|---|---|
| Backend | `docker-dev`, `firecracker` |
| Template | `base` (mandatory, 1 vCPU / 512 MiB), `ts-dev` (secondary, 2 vCPU / 1536 MiB) |
| Cache | **cold**: `sync; echo 3 > /proc/sys/vm/drop_caches` before each run, image/rootfs present on disk but not in page cache. **cold-uncached**: the image/rootfs artifact has never been built or pulled, run once per backend and template (3 reps), reported separately. **warm**: artifacts cached, previous run completed within 5 s |
| Workspace | **W0** none. **W1** one rw small (fixture repo ~2 MB, ~300 files). **W2** one rw (W1) + one ro ~50 MB (~5,000 files, mixed sizes, seeded) |

Mandatory cells: `base` × {cold, warm} × {W0, W1, W2} × 2 backends = 12 cells. `ts-dev` runs warm × {W0, W2} only (4 cells). Both backends use the same template image digest. FC's rootfs is derived from that same OCI digest by `rootfs-pipeline`, and the digest is recorded. vCPU and memory limits are identical: Docker gets `--cpus`, `--memory`, and a `--memory-swap` equal to memory, while FC uses `vcpu_count` and `mem_size_mib`.

The network policy is `egress_restricted` on both. If one backend cannot enforce it, both run with `none` and the deviation is recorded.

## 4. Statistics

- 5 warmup iterations per cell, discarded. **N ≥ 30** measured iterations per cell (the default is 50 for `base` warm W0).
- Report: n, failures, min, p50, p95, p99, max, mean, stddev, and a bootstrap 95% CI for p50 (10k resamples). p99 is flagged "indicative" when n < 100.
- **Interleaving:** ABAB at the iteration level within a cell, with the order of the starting backend alternated per cell. Cell order is randomised with a recorded seed.
- Comparisons: ratio of p50s with a bootstrap CI. A difference counts as meaningful only if the CI excludes 1.0 **and** |Δ| > 5%.
- Host prep: set the governor to `performance` and record whether that succeeded. Disable turbo if possible and record it. No swap, or record that swap is present. THP setting recorded.
- **Noise declaration (required):** before and after the run, capture the load average, the top 10 processes by CPU, and the list of running Proxmox guests (`qm list`, `pct list`). The run is **invalid** if any other guest uses >5% CPU (the mean `mpstat` idle check over a 30 s window) or the 1-minute load is >0.5 at start. The operator declares other workloads in `noise.declared`.

## 5. Fairness rules and threats to validity

1. **Same host, same run, interleaved.** No cross-run comparisons.
2. **Host type matters.** Record whether the bench is running on the bare Proxmox host or in a VM with nested virtualization. Nested virt penalises FC (EPT-on-EPT, vmexits) and not Docker. If nested, the report says "FC is disadvantaged" in the headline, and the recommendation can't rest on boot-latency deltas below 2×.
3. **Image pull/build time is excluded** from `spawn_ready` and reported only in cold-uncached. Docker pulls from a registry while FC builds ext4 from OCI. These are different operations, and each is shown once.
4. **Page cache:** cold runs drop caches. Both backends read their image from the same filesystem and device, and the record states which.
5. **Docker daemon warm:** dockerd runs the whole time, which is the realistic setup. Its RSS is reported as `shared_overhead`. FC has no daemon, so FC's "orchestrator" cost is the bench process itself, which is equal for both.
6. **Proxmox KSM and ballooning:** KSM must be **off** during the run, or recorded as on along with `pages_sharing` before and after, because KSM dedups FC guest pages and distorts PSS. If the bench host is itself a VM, Proxmox ballooning for that VM must be disabled.
7. **Guest kernel vs host kernel:** FC boots a pinned guest kernel and Docker uses the host kernel. Record both, and don't attribute tool-call differences to "the backend" without noting this.
8. **Transport:** UDS vs vsock is part of each backend's cost, so it is measured as-is and not normalised.
9. **Workspace semantics differ:** Docker bind mounts are live. FC needs build and extract steps. That cost is real and counts (`ws_attach`, `ws_sync_back`).
10. **Harness self-overhead:** measure a no-op loop (time an empty future) and the sampler's CPU use, and report both.
11. **Thermal/clock drift:** detected by repeating `base` warm W0 at the start and end. If the p50 drifts >10%, the run is flagged.
12. **Failure accounting:** every failure is logged with its error code. A backend with >1% failures in a cell has that cell marked unreliable.

## 6. Output JSON

The output path is `docs/bench/results/<UTC-ISO8601-compact>-<host>.json` (e.g. `20261201T140000Z-kvm-homelab.json`). One file per run. It is schema-versioned, and the harness ships the JSON Schema at `docs/bench/results/schema.json`.

```jsonc
{
  "schema_version": "1",
  "run_id": "uuid", "started_at": "RFC3339", "finished_at": "RFC3339",
  "git": { "commit": "sha", "dirty": false, "branch": "..." },
  "seed": 0,
  "host": {
    "hostname": "", "kind": "bare-metal|vm-nested", "hypervisor": "proxmox x.y|null",
    "cpu_model": "", "sockets": 0, "cores": 0, "threads": 0, "smt": true,
    "kernel": "uname -r", "governor": "", "turbo": "on|off|unknown",
    "mem_total_bytes": 0, "swap_bytes": 0, "thp": "", "ksm": { "run": 0, "pages_sharing": 0 },
    "storage": { "device": "", "type": "nvme|ssd|hdd|zfs|lvm-thin", "fs": "", "reflink": false },
    "docker": { "engine": "", "containerd": "", "runc": "", "cgroup": "v2", "storage_driver": "" },
    "firecracker": { "version": "", "jailer": false, "guest_kernel": "", "guest_kernel_sha256": "" }
  },
  "noise": { "load_start": [0,0,0], "load_end": [0,0,0], "other_guests": [], "declared": "", "top_cpu": [] },
  "harness": { "noop_overhead_ns": 0, "sampler_cpu_pct": 0 },
  "templates": [{ "name": "base", "oci_digest": "sha256:", "rootfs_sha256": "sha256:", "vcpu": 1, "memory_mb": 512 }],
  "cells": [{
    "backend": "docker-dev|firecracker",
    "capabilities": { "hardware_isolated": false, "multi_tenant_safe": false, "supports_snapshots": false, "supports_host_dir_mounts": true },
    "template": "base", "cache": "cold|warm|cold-uncached", "workspace": "W0|W1|W2",
    "warmup": 5, "n": 30, "failures": [{ "iter": 0, "metric": "", "error": "" }],
    "metrics": {
      "<metric_id>": {
        "unit": "ns|bytes|cpu_s",
        "samples": [0],
        "stats": { "min": 0, "p50": 0, "p95": 0, "p99": 0, "max": 0, "mean": 0, "stddev": 0, "p50_ci95": [0,0] }
      }
    },
    "phases": { "<phase_name>": { "stats": {} } }
  }],
  "density": [{ "backend": "", "variant": "idle|touched", "rep": 0, "budget_bytes": 0, "max_n": 0, "stop_reason": "budget|failure|cap",
                "steps": [{ "i": 0, "spawn_ready_ns": 0, "host_mem_available": 0, "host_pss_total": 0 }] }],
  "drift_check": { "start_p50_ns": 0, "end_p50_ns": 0, "flagged": false },
  "valid": true, "invalid_reasons": []
}
```

Metric IDs: `spawn_ready`, `first_call`, `call.bash_echo`, `call.file_rw_1m`, `call.grep`, `ws_attach`, `ws_sync_back`, `teardown`, `agent_turn`, `mem.host_pss_idle`, `mem.host_pss_post`, `mem.vmm_overhead`, `mem.guest_available`, `cpu.lifecycle_s`, `cpu.idle_60s`, `disk.per_sandbox`, `disk.shared`. Raw samples are always kept so the report can recompute anything.

## 7. Decision framework: recommended default backend

**Hard gate (applied before scoring), per AGENTS.md invariant 1.** DockerDev is **never** eligible as the production or multi-tenant default, whatever its scores. It is eligible only for the local-dev/CI context. If no hardware-isolated backend passes its gates, the report recommends "no production default yet", not Docker.

**Second gate.** A backend whose failure rate is >1% in mandatory cells is ineligible for any default until it's fixed.

**Scoring.** Each criterion gets a 0–5 score using the rule in the table, multiplied by a context weight. Weights are fixed now, before data, and each context's weights sum to 100.

| Criterion | Scoring rule (relative or absolute) | Prod | Self-host Linux | Local dev |
|---|---|---|---|---|
| Security isolation | hardware_isolated: 5 if also multi_tenant_safe, else 3. Shared kernel: 1 | 35 | 25 | 10 |
| Boot latency (`spawn_ready` p50/p95, base warm W0) | 5 = ≤1.1× best, 4 ≤1.5×, 3 ≤2×, 2 ≤4×, else 1. Also 5 at any absolute p95 < 250 ms | 15 | 15 | 20 |
| Teardown (`teardown` p95) | same ratio rule, with absolute p95 < 100 ms = 5 | 5 | 5 | 5 |
| Memory density (`max_n` touched, `host_pss_post`) | ratio rule on `max_n` (higher is better) | 15 | 10 | 5 |
| Workspace ergonomics (`ws_attach` + `ws_sync_back` W2 p50, plus live host mutation support) | ratio rule, −1 if host mutation between turns requires a resync step | 10 | 20 | 25 |
| Platform availability | 5 runs on macOS, Windows, and no-KVM Linux. 3 = Linux+KVM only. 1 = bare-metal/specific HW | 5 | 10 | 25 |
| Operational complexity | rubric: deps, root/caps needed, kernel/rootfs pipeline, networking setup, debuggability. 5 = trivial, 1 = heavy | 15 | 15 | 10 |

The report shows per-context weighted totals (max 500, normalised to 100), the per-criterion sub-scores with the evidence, and a **sensitivity check**: does the winner change if any single weight moves ±10 points? If yes, the result is "close call" and the swing criteria are named. Separate recommendations per context are expected and acceptable. For example: prod = FC (with the caveat "jailer pending"), self-host Linux = FC, local dev = DockerDev.

## 8. Acceptance criteria

### bench-harness (`crates/sandcrab-bench/`)
- [ ] A CLI `sandcrab-bench run --backends docker-dev,firecracker --matrix <file> --out <dir>` plus `--seed`, `--n`, `--warmup`, and `--only <cell>`.
- [ ] It implements every metric ID in §1–2 with the start/stop points as specified. Start and stop are taken around the public SDK/`RuntimeBackend` calls, not inside the backend, except for the optional `phases`.
- [ ] Teardown verification checks the actual resource absence per backend (§1 table).
- [ ] ABAB interleaving, randomised cell order from a seed, warmup discard, and N configurable with a minimum of 30 enforced unless `--allow-small-n` is given (the result is then marked invalid).
- [ ] Host fingerprint and noise capture are automatic (§4, §6). `kind` (bare/nested) is detected via `systemd-detect-virt`, with an operator override.
- [ ] It writes JSON that validates against `docs/bench/results/schema.json`, and keeps raw samples.
- [ ] Seeded fixture generators for the 1 MiB file, the W1 repo, the W2 50 MB ro tree, and the grep corpus, with expected match counts.
- [ ] A density mode with a budget, stop reasons, and the idle and touched variants.
- [ ] Unit tests for the stats (percentiles, bootstrap) and the schema. `cargo clippy -D warnings` and `cargo fmt` are clean.
- [ ] Smoke mode (`--n 3`) runs against DockerDev on the VPS in under 5 minutes.

### bench-run (`docs/bench/results/`)
- [ ] It runs on kvm-homelab with both backends in the same invocation, and records bare vs nested.
- [ ] Governor, KSM, ballooning, and swap are set per §4–5, or the deviation is recorded.
- [ ] Noise is declared, and the run is `valid: true` (or rerun). The drift check passes.
- [ ] All 12 mandatory `base` cells, the 4 `ts-dev` cells, cold-uncached, and density ×3 per backend are complete.
- [ ] The result JSON and a `README` line (command, commit, host) are committed. Failures are investigated and noted, not dropped.

### bench-report (`docs/site/bench-report.html`, local static file)
- [ ] It is generated only from the committed JSON, and the file name and commit are cited on the page.
- [ ] The host/validity panel is shown first, with a prominent nested-virt warning if applicable.
- [ ] Per-metric distributions (box/violin or CDF) are shown, not just bars of p50. Tables give p50/p95/p99 plus the CI.
- [ ] `spawn_ready` and `ws_attach` are shown separately and combined. The density degradation curve is included. FC idle vs post-workload memory is shown with the caveat text.
- [ ] An isolation section: the capability matrix from `capabilities`, shared kernel vs KVM, the FC V1 `multi_tenant_safe=false` (jailer pending) caveat, and the note that V1 has no snapshots.
- [ ] §7 scoring per context, with the hard gate applied, the sensitivity check, and the recommendation.
- [ ] A threats-to-validity section listing every deviation from this spec.
- [ ] Light and dark themes are both checked. The page renders offline (no Claude artifacts).

## 9. Open questions for the maintainer

1. Weights in §7: confirm them, especially that local dev weights platform availability at 25, and that prod gives FC credit at 3/5 for isolation while `jailer` is pending.
2. Density budget: is 75% of `MemTotal` acceptable, or should it be a fixed number (e.g. 32 GiB) so results are comparable across hosts?
3. If kvm-homelab turns out to be nested-only, should the benchmark run anyway (flagged), or wait for bare-host access?
4. Should a third reference point be added, Docker + gVisor or Kata (`runsc`), as an "isolated container" baseline? It is out of scope by default.
