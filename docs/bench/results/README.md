# Benchmark results

One JSON file per `sandcrab-bench run` (schema: `schema.json`). Summarise with `sandcrab-bench summarize <file>`.

| File | Host | Commit | Valid | Status |
|---|---|---|---|---|
| `20261123T091401Z-dev-vps.json` | dev-vps (VPS, 2 vCPU EPYC 9354P, KVM guest, no nested virt) | 0d85b83 | false: 1-min load 0.89 > 0.5 at start | **canonical** (DockerDev V1). Drift -8.7% (pass), 0 failures, host quiet during the run |
| `20261123T103824Z-dev-vps.json` | dev-vps | 0d85b83 | false: drift +61.5% | rerun from a quieter start (load 0.12); external interference during the run (p50s 10-40% higher, density spawns up to 0.7 s). Kept for transparency, not canonical |

Command (both runs): `target/release/sandcrab-bench run --backends docker,firecracker --templates base,ts-dev --scenarios all --runs 30 --warmups 5 --noise-declared "..." --out docs/bench/results/` (density cap 16, budget 75%).
Firecracker is recorded as unavailable/TODO (no /dev/kvm). Cold cells, cold-uncached and PSS are not measured (no root); see the `notes` in each file.
