#!/usr/bin/env python3
"""Embed benchmark data into docs/site/bench-report.html.

Reads the committed results JSON listed in RUNS (grouped by host), the
BackendCapabilities literals from the backend crates, and the crab glyph SVGs,
then rewrites the block between the BENCH-DATA markers in the report. No number
on the page is hand-typed: the page renders every table and chart from this
block. To add a run, add its file to RUNS; to show a backend a host has not
measured yet, add a PENDING entry (it renders as "pending", never as numbers).

Usage (from the repo root):
    python3 scripts/bench-report-data.py
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "docs/bench/results"
REPORT = ROOT / "docs/site/bench-report.html"
GLYPHS = ROOT / "docs/site/assets/crab/svg"
BEGIN = "<!-- BENCH-DATA:BEGIN -->"
END = "<!-- BENCH-DATA:END -->"

# Every results file the report shows, in page order (runs of one host adjacent),
# with its role on the page:
#   primary  - its available backends become comparison columns
#   rerun    - same host and code as a primary run; shown only as a noise check
#   density  - a density-only rerun that replaces the host's primary density data
# The bool keeps raw per-iteration samples (needed for the distribution charts).
RUNS = (
    ("20261123T091401Z-dev-vps.json", "primary", True),
    ("20261123T103824Z-dev-vps.json", "rerun", False),
    ("20261124T183907Z-kvm-dedicated.json", "primary", True),
    ("20261125T014108Z-kvm-dedicated.json", "primary", True),
    ("20261124T222350Z-mac-m2.json", "primary", True),
    ("20261125T020230Z-mac-m2.json", "density", False),
)

# Backends a host has not measured yet: a pending column, never numbers.
PENDING = (
    {"host": "mac-m2", "backend": "docker",
     "status": "pending: scheduled for a reserved idle window",
     "why": "Docker on macOS only became possible with the Docker backend's tcp transport "
            "(docs/hosting.md, \"Docker on macOS\"), after this host's libkrun run. Docker Desktop runs "
            "containers in its own Linux VM, whose overhead is not attributed per container."},
)

# Bench runs before the Docker backend rename recorded it as "docker-dev"; new
# runs record "docker". Normalise on read so old and new results line up.
LEGACY_BACKENDS = {"docker-dev": "docker"}
LEGACY_RE = re.compile(r"\b(" + "|".join(map(re.escape, LEGACY_BACKENDS)) + r")\b")

# Metric ids renamed after some results were recorded: old id -> current id. A
# measured current id wins; otherwise the old metric is shown under the current
# id, and the run lists the rename in its "renamed" field.
LEGACY_METRICS = {"cpu.helper_idle_60s": "cpu.idle_60s"}

# Steady-state call metrics have n=900; keep stats only to keep the page small.
SAMPLES_SKIP = ("call.",)
HOST_KEYS = (
    "hostname", "kind", "virt", "hypervisor", "cpu_model", "sockets", "cores", "threads",
    "smt", "kernel", "governor", "turbo", "mem_total_bytes", "swap_bytes", "thp", "ksm",
    "kvm_available", "nested_virt", "storage", "docker", "firecracker", "crabd",
)
CONFIG_KEYS = (
    "argv", "runs", "warmups", "calls_per_sandbox", "call_warmups", "resource_runs",
    "idle_window_s", "settle_s", "density_budget", "density_cap", "density_reps",
    "touch_mib", "network", "scenarios", "workspaces", "caches", "quick", "allow_small_n",
)


def num(v):
    """Integers stay integers; floats are kept at full precision."""
    if isinstance(v, float) and v.is_integer():
        return int(v)
    return v


def normalise_backends(o):
    """Rewrite legacy backend names in every key and string (cells, density,
    backends list, drift check, notes, not_measured reasons)."""
    if isinstance(o, dict):
        return {normalise_backends(k): normalise_backends(v) for k, v in o.items()}
    if isinstance(o, list):
        return [normalise_backends(v) for v in o]
    if isinstance(o, str):
        return LEGACY_RE.sub(lambda m: LEGACY_BACKENDS[m.group(1)], o)
    return o


def slim_metric(mid: str, m: dict, keep_samples: bool) -> dict:
    out = {"unit": m.get("unit")}
    if m.get("not_measured") is not None or m.get("stats") is None:
        out["not_measured"] = m.get("not_measured") or "no samples"
        return out
    out["stats"] = {k: num(v) if not isinstance(v, list) else [num(x) for x in v]
                    for k, v in m["stats"].items()}
    if keep_samples and not mid.startswith(SAMPLES_SKIP):
        out["samples"] = [num(x) for x in m["samples"]]
    return out


def rename_metrics(cells: list) -> list:
    """Show legacy metric ids under their current id (see LEGACY_METRICS)."""
    done = set()
    for c in cells:
        for old, new in LEGACY_METRICS.items():
            if old not in c["metrics"]:
                continue
            m_old = c["metrics"].pop(old)
            m_new = c["metrics"].get(new)
            if m_new is None or (m_new.get("not_measured") and not m_old.get("not_measured")):
                c["metrics"][new] = m_old
                done.add(f"{old} -> {new}")
    return sorted(done)


def tier_of(cfg: dict) -> str:
    """Benchmark tier (docs/bench/README.md, "Benchmark tiers") from the recorded config."""
    if cfg.get("quick"):
        return "Smoke"
    full = cfg.get("runs", 0) >= 30 and cfg.get("resource_runs", 0) >= 30 and not cfg.get("allow_small_n")
    if cfg.get("scenarios") == ["density"]:
        return ("Full" if full else "Standard") + " (density only)"
    return "Full" if full else "Standard"


def arch_of(host: dict) -> str | None:
    """CPU architecture, from the target triple of the crabd binary the run used."""
    m = re.search(r"(x86_64|aarch64)-unknown-linux", (host.get("crabd") or {}).get("path") or "")
    return {"x86_64": "x86_64", "aarch64": "arm64"}[m.group(1)] if m else None


def slim_run(path: Path, role: str, keep_samples: bool) -> dict:
    raw = path.read_bytes()
    d = json.loads(raw)
    cells = []
    for c in d["cells"]:
        cells.append({
            "backend": c["backend"], "template": c["template"], "cache": c["cache"],
            "workspace": c["workspace"], "scenario": c["scenario"], "status": c["status"],
            "reason": c.get("reason"), "todo": c.get("todo"), "warmup": c.get("warmup"),
            "n": c.get("n"), "failures": len(c.get("failures") or []),
            "unreliable": c.get("unreliable"),
            "metrics": {k: slim_metric(k, m, keep_samples) for k, m in c["metrics"].items()},
            # only the p50 of each timing mark is drawn
            "phases": {k: slim_metric(k, {"unit": "ns", **p}, False)
                       for k, p in c["phases"].items()},
        })
    renamed = rename_metrics(cells)
    density = []
    for r in d["density"]:
        density.append({
            "backend": r["backend"], "template": r["template"], "variant": r["variant"],
            "rep": r["rep"], "budget_bytes": r["budget_bytes"], "cap": r["cap"],
            "max_n": r["max_n"], "stop_reason": r["stop_reason"], "error": r["error"],
            # [i, spawn_ready_ns, host_mem_available, cgroup_mem_total]
            "steps": [[s["i"], s["spawn_ready_ns"], s["host_mem_available"], s["cgroup_mem_total"]]
                      for s in r["steps"]],
        })
    host = {k: d["host"].get(k) for k in HOST_KEYS}
    host["arch"] = arch_of(d["host"])
    return normalise_backends({
        "file": f"docs/bench/results/{path.name}",
        "sha256": hashlib.sha256(raw).hexdigest(),
        "role": role, "tier": tier_of(d["config"]), "renamed": renamed,
        "run_id": d["run_id"], "git": d["git"], "seed": d["seed"],
        "started_at": d["started_at"], "finished_at": d["finished_at"],
        "valid": d["valid"], "invalid_reasons": d["invalid_reasons"], "notes": d["notes"],
        "host": host,
        "noise": d["noise"], "harness": d["harness"], "drift_check": d["drift_check"],
        "shared_overhead": d["shared_overhead"],
        "config": {k: d["config"].get(k) for k in CONFIG_KEYS},
        "templates": d["templates"], "backends": d["backends"],
        "cells": cells, "density": density,
    })


def rust_caps(rel: str) -> dict:
    """Pull the BackendCapabilities literal and network-policy match out of a backend crate."""
    src = (ROOT / rel).read_text()
    m = re.search(r"fn capabilities\(&self\) -> BackendCapabilities \{\s*BackendCapabilities \{(.*?)\}",
                  src, re.S)
    if not m:
        sys.exit(f"capabilities() not found in {rel}")
    caps = {k: v == "true" for k, v in re.findall(r"(\w+):\s*(true|false)", m.group(1))}
    comments = re.findall(r"//\s*(.+)", m.group(1))
    nets = re.findall(r"NetworkPolicy::(\w+)",
                      re.search(r"matches!\(\s*network\.policy,(.*?)\)", src, re.S).group(1))
    return {"source": rel, "caps": caps, "comments": comments, "enforced_network": nets}


def glyphs() -> dict:
    out = {}
    for f in sorted(GLYPHS.glob("*.svg")):
        svg = f.read_text().strip()
        svg = re.sub(r'\sstyle="color:#[0-9A-Fa-f]+"', "", svg)
        svg = re.sub(r"<title>.*?</title>\s*", "", svg, flags=re.S)
        svg = svg.replace('role="img"', 'aria-hidden="true"')
        out[f.stem] = svg
    return out


def main() -> None:
    runs = [slim_run(RESULTS / f, role, samples) for f, role, samples in RUNS]
    hosts = []
    for r in runs:
        name = r["host"]["hostname"]
        if not hosts or hosts[-1]["id"] != name:
            if any(h["id"] == name for h in hosts):
                sys.exit(f"RUNS: the runs of host {name} must be adjacent")
            hosts.append({"id": name, "runs": []})
        hosts[-1]["runs"].append(r)
    for p in PENDING:
        if not any(h["id"] == p["host"] for h in hosts):
            sys.exit(f"PENDING: unknown host {p['host']}")
    data = {
        "generator": "scripts/bench-report-data.py",
        "hosts": hosts,
        "pending": list(PENDING),
        "code_caps": {
            "docker": rust_caps("crates/sandcrab-backend-docker/src/backend.rs"),
            "firecracker": rust_caps("crates/sandcrab-backend-firecracker/src/backend.rs"),
        },
        "glyphs": glyphs(),
    }
    blob = json.dumps(data, separators=(",", ":"), ensure_ascii=False).replace("</", "<\\/")
    block = f'{BEGIN}\n<script id="bench-data" type="application/json">{blob}</script>\n{END}'
    html = REPORT.read_text()
    if BEGIN not in html or END not in html:
        sys.exit(f"markers missing in {REPORT}")
    pre, rest = html.split(BEGIN, 1)
    _, post = rest.split(END, 1)
    REPORT.write_text(pre + block + post)
    print(f"embedded {len(blob):,} bytes into {REPORT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
