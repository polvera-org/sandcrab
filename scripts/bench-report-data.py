#!/usr/bin/env python3
"""Embed benchmark data into docs/site/bench-report.html.

Reads the committed results JSON (canonical + rerun), the BackendCapabilities
literals from the backend crates, and the crab glyph SVGs, then rewrites the
block between the BENCH-DATA markers in the report. No number on the page is
hand-typed: the page renders every table and chart from this block.

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
CANONICAL = "20261123T091401Z-dev-vps.json"
RERUN = "20261123T103824Z-dev-vps.json"
REPORT = ROOT / "docs/site/bench-report.html"
GLYPHS = ROOT / "docs/site/assets/crab/svg"
BEGIN = "<!-- BENCH-DATA:BEGIN -->"
END = "<!-- BENCH-DATA:END -->"

# Bench runs before the Docker backend rename recorded it as "docker-dev"; new
# runs record "docker". Normalise on read so old and new results line up.
LEGACY_BACKENDS = {"docker-dev": "docker"}
LEGACY_RE = re.compile(r"\b(" + "|".join(map(re.escape, LEGACY_BACKENDS)) + r")\b")

# Steady-state call metrics have n=900; keep stats only to keep the page small.
SAMPLES_SKIP = ("call.",)
HOST_KEYS = (
    "hostname", "kind", "virt", "hypervisor", "cpu_model", "sockets", "cores", "threads",
    "smt", "kernel", "governor", "turbo", "mem_total_bytes", "swap_bytes", "thp", "ksm",
    "kvm_available", "nested_virt", "storage", "docker", "crabd",
)
CONFIG_KEYS = (
    "argv", "runs", "warmups", "calls_per_sandbox", "call_warmups", "resource_runs",
    "idle_window_s", "settle_s", "density_budget", "density_cap", "density_reps",
    "touch_mib", "network", "scenarios", "workspaces", "caches",
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


def slim_run(path: Path, keep_samples: bool) -> dict:
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
            "phases": {k: slim_metric(k, {"unit": "ns", **p}, keep_samples)
                       for k, p in c["phases"].items()},
        })
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
    return normalise_backends({
        "file": f"docs/bench/results/{path.name}",
        "sha256": hashlib.sha256(raw).hexdigest(),
        "run_id": d["run_id"], "git": d["git"], "seed": d["seed"],
        "started_at": d["started_at"], "finished_at": d["finished_at"],
        "valid": d["valid"], "invalid_reasons": d["invalid_reasons"], "notes": d["notes"],
        "host": {k: d["host"].get(k) for k in HOST_KEYS},
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
    data = {
        "generator": "scripts/bench-report-data.py",
        "canonical": slim_run(RESULTS / CANONICAL, keep_samples=True),
        "rerun": slim_run(RESULTS / RERUN, keep_samples=False),
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
