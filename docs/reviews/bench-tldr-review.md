# Review: bench-tldr (TL;DR benchmarks page, preflight, tier validity)

- Step: `tldr-review` (flow `docs/plans/bench-tldr.flow.yaml`)
- Range: `main..015ae24` (feat/bench-tldr tip)
- Reviewer: Nebula (QA, security, spec compliance)
- Date: 2026-11-25

## Verdict

**PASS with one medium finding.** Every number on `benchmarks.html` traces to the committed
results JSON, the generator is idempotent, `recheck --write` provably adds three fields and
changes no other byte, the schema is current, links are right, and no private hostnames, IPs or
usernames appear. The one thing to fix before shipping is takeaway 03's headline ("Docker is the
lightest"). It claims more than the data shows, because Firecracker's idle memory and CPU were
never probed, and the generator's claim check doesn't cover that case. Everything else is low or
info.

## Findings

| ID | Severity | File:line | Scenario | Fix |
|---|---|---|---|---|
| R1 | **Medium** | `docs/site/benchmarks.html:265`, `scripts/bench-tldr-data.py:212-224` | Takeaway 03 is headlined "Docker is the lightest" (superlative across backends), but kvm-dedicated Firecracker has no idle memory or CPU data (`no cgroup/process probe for this backend yet`; the page itself shows "not probed in this run"). The claim check only compares Docker with libkrun. A mutation test that gives Firecracker 1 MiB idle memory still passes `check_claims`. So the generator doesn't enforce the claim as worded. | Reword the headline to "Docker is lighter than a microVM at idle" or "Docker is the lightest of the backends measured", or make `check_claims` require Docker < every microVM with a measured `mem_idle` on the host and fail when a microVM on that host has none. |
| R2 | Low | `docs/site/benchmarks.html:264` | Takeaway 02 says "a whole agent turn ... takes 793 ms to 995 ms" without naming the host. Those are kvm-dedicated numbers. On mac-m2, libkrun's agent turn is 403 ms, outside the stated range. The p95 figures in the same item are also kvm-only and unlabelled. | Add "on the bare-metal Xeon D-2141I" (or `H['kvm-dedicated'].cpu`) to the item. |
| R3 | Low | `docs/site/benchmarks.html:263`, `scripts/bench-tldr-data.py:38-42` | On kvm-dedicated, libkrun comes from a different run than Docker and Firecracker: a day later, a different commit (`97d116c` vs `65f4a2c-dirty`), Standard tier, and not interleaved. Takeaway 01 says "on the same machine", which is true, but the page doesn't say these were separate runs. The footer run list only implies it. The gap is large (424 vs 685 ms), so the conclusion stands. | Add "(separate runs)" to the footer line or to takeaway 01, as `docs/bench/results/README.md` already does. |
| R4 | Low | `docs/site/benchmarks.html:265` | Takeaway 03 sets Docker's 2.0 MiB per container against libkrun's 63 MiB but leaves out the 165 MiB of shared `dockerd` + `containerd`. The footprint row shows the daemons, and the claim check amortizes them over 16 sandboxes, so the conclusion holds. The takeaway text alone is still flattering. | Add "plus ~165 MiB of shared daemons per host" to the sentence. |
| R5 | Low | `scripts/bench-tldr-data.py:58`, `docs/site/benchmarks.html:247` | `LEGACY_METRICS` maps mac-m2's `cpu.helper_idle_60s` (one helper process, `ps` 10 ms resolution) to `cpu.idle_60s` (cgroup). That isn't a rename: it's a different measurement, and it's shown in the same column as the kvm cgroup CPU with no label. Memory names its kind, but CPU doesn't. | Carry a `kind` for CPU the way `MEM_KINDS` does ("helper process" vs "cgroup") and render it under the number. |
| R6 | Low | `scripts/bench-tldr-data.py:116` | Any Apple CPU gets the label "laptop". That's true for mac-m2 today (`noise.declared` says developer laptop), but a Mac mini or Studio host would be mislabelled. | Take the form factor from the host label or `noise.declared`, or label it "Apple M2 Mac". |
| R7 | Low | `scripts/bench-tldr-data.py:165-178` | If a primary run's lifecycle cell isn't `ok` for an available backend, `spawn_ready` is `null` and the page JS (`c.spawn_ready.p50`) throws. The whole page then renders blank, with no generator error. | In `build_host`, `sys.exit` when a non-pending backend lacks `spawn_ready`. |
| R8 | Low | `docs/site/benchmarks.html:220` | "Docker on this host is slowed by HDD storage" is a causal claim the page doesn't show. It's plausible: dev-vps Docker on SSD spawns in 227 ms, and the drift analysis blames the HDD. | Soften it to "Docker's root on this host is on an HDD, which slows spawn and teardown" or link to the full report's caveat. |
| R9 | Low | `crates/sandcrab-bench/src/main.rs:329,383`, `crates/sandcrab-bench/src/preflight.rs:123-129` | The "< 1% by chance" guarantee of `separated()` holds only for 5 + 5 samples. `--probe-samples` is clamped only to ≥ 1. At 2 samples the chance is 1/3. At 1 sample any two different values count as "separated", so any shift over 10% FAILs a Standard/Full preflight. | Clamp `probe_samples` to ≥ 5, or skip the separation-based FAIL (warn only) below 5. |
| R10 | Low | `crates/sandcrab-bench/src/preflight.rs:99-105,406-413` | One probe spawn failure out of 11 aborts that backend's probe and FAILs the preflight, with the fix text "every cell would fail". The run itself tolerates ≤ 1% failures per cell, so a single transient error refuses a multi-hour run, and the message overstates it. For a backend that really can't spawn, FAIL is the right verdict: every cell fails, so the cell is `unreliable` and the run is invalid. That answers the brief's question. | Keep FAIL. Optionally retry once before failing, and reword the fix to "spawns are failing; the run's cells would be unreliable". |
| R11 | Low | `crates/sandcrab-bench/src/preflight.rs:341-368` | The HDD Docker-root rule FAILs only at Full. Standard keeps Full's drift rule (`validity.rs`), so a Standard run with Docker on HDD has the same drift risk and gets only a warning. A shorter run (~1 h vs ~3 h) is a defensible reason, but it isn't documented. | Document why Standard only warns (in the README preflight table), or apply the rule to Standard too. |
| R12 | Low | `crates/sandcrab-bench/src/host.rs:227-233` | The `sys_rotational` fallback (used only without `lsblk`) can't resolve dm names. `findmnt` returns `/dev/mapper/vg-lv`, and neither `/sys/block/mapper/vg-lv` nor `/sys/class/block/mapper/vg-lv` exists. The result is `unknown`, and `unknown` produces no storage finding at all. md and partitions work, and `lsblk` handles dm (dm and md both stack the rotational flag, so RAID1-of-HDD reads `1`, as the committed files show). | Resolve `/dev/mapper/*` through `canonicalize` to `/dev/dm-N` first. Optionally warn on `unknown` storage for Full with Docker. |
| R13 | Low | `crates/sandcrab-bench/src/preflight.rs:205` | If the load average can't be read, `loadavg()` yields NaN. `NaN > max_load` is false, so the load check passes silently, and NaN serializes as `null` in `preflight.load`, which doesn't parse back into `[f64; 3]`. This predates the change for `noise.load_start`. | Treat non-finite load as a tiered finding ("load unknown"). |
| R14 | Info | `docs/site/bench-report.html:500,363` | The full report says "Only a run valid for Full is publishable as headline numbers", while the linked TL;DR page shows headline numbers from runs that aren't valid for Full. The user decided validity stays off the marketing page, so this isn't a request for badges. It's a wording tension a careful reader may notice. | Optionally say "spec-grade" or "decision-grade" instead of "publishable" in the report. |
| R15 | Info | `docs/site/bench-report.html:188` | `site-chrome.py` maps `bench-report.html` to the `bench` nav key, so on bench-report the Benchmarks link to `benchmarks.html` carries `aria-current="page"` although it points to another page. | Use `aria-current="true"` (or "location") for the section match, and `"page"` only on an exact match. |

## Checks run

1. **Generator byte-identity.** Ran `python3 scripts/bench-tldr-data.py` and `python3 scripts/bench-report-data.py` at `015ae24`. `git status` stayed clean, so both committed pages are byte-identical to the generator output.
2. **Number spot-checks (6+).** I recomputed p50 and p95 from the raw `samples` arrays (median plus linear-interpolated p95) and compared them with each file's `stats` and with the embedded TL;DR data. All match exactly:
   - kvm libkrun `spawn_ready` 424.08 / 436.43 ms, n=30 (`20261125T014108Z`)
   - kvm Firecracker `spawn_ready` 685.17 / 693.77 ms (`20261124T183907Z`)
   - kvm Docker `spawn_ready` 1543.45 / 1976.30 ms (`20261124T183907Z`)
   - mac libkrun `spawn_ready` 159.01 / 166.11 ms (`20261124T222350Z`)
   - kvm Docker `mem.cgroup_current_idle` 2,095,104 B (2.0 MiB); kvm libkrun 66,050,048 B (63 MiB), n=10
   - kvm libkrun `cpu.idle_60s` 0.0434 s (43 ms); mac `cpu.helper_idle_60s` 0.06 s; mac `mem.helper_rss_idle` 119,005,184 B (113 MiB)
   - kvm libkrun `agent_turn` 793.46 ms; dev-vps Docker `mem.cgroup_current_idle` 905,216 B
   - shared daemons: kvm 57.9 + 115.4 MB = 165 MiB; dev-vps 618 MiB (from `shared_overhead`)
   - Density: every rep for every backend stopped on `cap` at 16, so the "16+" and "floor, not a limit" wording is correct.
3. **Units and labels.** ns → ms/s, bytes → MiB (binary), `cpu_s` → ms are correct. The memory kind is named per row (cgroup memory vs helper RSS). The "10 tool calls" in the agent turn matches `agent_script` (10 calls). Host labels match `host` in the JSON (Xeon D-2141I bare metal on HDD `/dev/md3`; Apple M2 on internal SSD; 2-vCPU EPYC VPS on SSD).
4. **Claim checks enforce the takeaways.** I mutated the built host data and called `check_claims`. It caught: Firecracker faster than libkrun, a microVM p95 ≥ 1 s (kvm and mac), Docker memory or CPU above libkrun, 2 GiB of daemons, mismatched memory kinds, and Docker reporting hardware isolation. It **missed** a Firecracker memory below Docker's (R1).
5. **Within-host comparison only.** The bars are normalized per host and omitted for single-backend hosts, and the intro says so. The only cross-host statements are the side-by-side libkrun numbers in the hero, both labelled with their hardware, which is fine.
6. **Privacy.** Grepped `benchmarks.html` and the generator for IPs, `private-mesh`, `ubuntu@`, `/home/`, `/Users/` and user names. The only hits are SVG path coordinates, and only the placeholder host ids appear. I also checked the crate, AGENTS.md and README additions: nothing found.
7. **Links.** `python3 scripts/site-chrome.py --check` reports "up to date". The landing (src and built), index.html (nav, card, footer), site chrome on all pages and README all link to `benchmarks.html`. `bench-report.html` is linked only from `benchmarks.html:155`, `presentation.html:227` and README (secondary link), plus the `site-chrome.py` PAGES key, which isn't a link.
8. **Recheck.**
   - `sandcrab-bench recheck docs/bench/results/2026*.json` gives the tiers shown in the committed files. Only `20261125T014108Z-kvm-dedicated.json` is valid for its tier (Standard), and every recorded Full verdict is reproduced.
   - `recheck --write` on copies of the committed files wrote nothing, so it's idempotent.
   - `recheck --write` on the **main-branch** (pre-tier) files reproduced the committed files byte for byte. Measured data can't change: the splice is guarded by a parse-back equality check, and the write is refused if `valid` would change.
9. **Schema.** `sandcrab-bench schema` output is identical to `docs/bench/results/schema.json`.
10. **Tests.** `cargo test -p sandcrab-bench`: 46/46 pass (shared target, `CARGO_INCREMENTAL=0`).
11. **Code read** (`preflight.rs`, `validity.rs`, `main.rs`, `run.rs`, `host.rs`, `results.rs`, `summarize.rs` diffs):
    - The probe metric matches the run's `spawn_ready` window.
    - Gating on probe index 0 matches the run's drift check (`ctx.live[0]`).
    - The density budget formula is the same one the run's stop rule uses.
    - Smoke never refuses: `!would_be_valid && tier != Smoke`, and the probe is off for Smoke in `run`.
    - `--force` sets `forced` and adds a note only when there are failures. `--no-preflight` records nothing.
    - The start load is sampled before backends open.
    - An unavailable backend gives a warning; no live backend at all gives a FAIL.
    - The 2/252 non-overlap probability for 5 + 5 is correct, under exchangeability (see R9 for other N).
    - Very fast hosts: the drift test is relative, but requiring full separation keeps jitter from causing false FAILs. The single warmup spawn may leave a cache-warming shift; the fix text says to rerun.
12. **bench-report tier badges.**
    - Badges read "valid/invalid for <tier>", the "(density only)" suffix is stripped for the badge, and the Standard run gets a "Valid for Standard, not Full" caveat.
    - The decision table still keys on `valid` (Full), which is consistent.
    - The generator refuses files without tier fields instead of guessing.

Not run, as briefed: full workspace gates, remote hosts (kvm-dedicated is busy with a RAID check), new bench runs.
