# AGENTS.md — Sandcrab Engineering & Handoff Guide

This file is the canonical instruction set for AI coding agents (Claude Code, Codex, Antigravity, Cursor, etc.) working in the **Sandcrab** repository.

Before planning or writing code, read:
1. [`BRIEF.md`](BRIEF.md) — product mission, research conclusions, and open decisions.
2. [`docs/ROADMAP.md`](docs/ROADMAP.md) — full phased implementation plan and tool catalog.

---

## 1. Non-Negotiable Architectural Invariants

1. **No Silent Security Downgrades**:
   - Every `RuntimeBackend` MUST explicitly report `BackendCapabilities` (`hardware_isolated`, `multi_tenant_safe`, `supports_snapshots`, `supports_host_dir_mounts`).
   - Never silently fall back from a hardware microVM backend (`Firecracker`, `CloudHypervisor`, `Libkrun`) to Docker. Docker is an officially supported backend (shared-kernel isolation, hardened; reports `hardware_isolated=false`, `multi_tenant_safe=false`) and must be selected explicitly.
2. **One Guest Protocol Across All Backends**:
   - Host ↔ sandbox communication ALWAYS uses `sandcrab-protocol` talking to `crabd` (`sandcrab-guest`), whether over `virtio-vsock` (microVMs) or Unix socket/loopback (Docker).
   - Never bake Docker-specific `docker exec` shortcuts into `sandcrab-core` or `sandcrab-sdk`; all tools execute through `crabd` so local dev tests the real production protocol.
3. **Native Rust Tools Inside `crabd`**:
   - File and search tools (`file_read`, `file_write`, `file_edit`, `file_delete`, `file_move`, `list_dir`, `glob`, `grep`) must be implemented natively in Rust inside `sandcrab-guest` (using crates like `ignore`, `globset`, `grep-searcher`, `similar`) rather than shelling out to `sed`/`grep`/`find`.
   - `bash` must enforce timeouts with full process-group cleanup (`killpg`) and bounded output truncation to protect LLM context windows.
4. **First-Class Persistent Workspaces**:
   - Ephemeral sandboxes and durable workspaces are separate lifecycles.
   - Workspaces must support host-side mutation between turns, multi-mount (`ReadWrite` / `ReadOnly`) on spawn, explicit `default_cwd`, and path-traversal/symlink safety.
5. **Pinned & Reproducible Templates**:
   - Templates are standard OCI `Dockerfile`s + `sandcrab.toml` manifests.
   - Keep runtime versions modern (as of late 2026: Debian 13 `trixie-slim`, Python 3.15 + `uv`, Node.js 26 Active LTS).
   - Pin built rootfs/kernel/init artifacts with SHA-256 digests.
6. **Own Core Infrastructure**:
   - Do NOT pull in `kombuse-microvm` as a core dependency (see `BRIEF.md` §3). Keep `sandcrab-core`, `sandcrab-protocol`, and `sandcrab-guest` clean, owned, and modular.

---

## 2. Crate Map & Boundaries

| Crate | Role | Allowed Dependencies |
| :--- | :--- | :--- |
| `sandcrab-protocol` | Wire envelopes, `ToolCall`/`ToolResult`, JSON Schema export (`schemars`), protocol errors | Pure data/serde/schemars; **no** Docker, VMM, or Tokio runtime assumptions |
| `sandcrab-guest` (`crabd`) | Static Linux binary running inside sandboxes; implements all agent tools | `sandcrab-protocol`, async runtime, Rust FS/search/PTY crates; keep binary small & `musl`-compatible |
| `sandcrab-core` | `RuntimeBackend` trait, `Workspace` registry & mount model, `TemplateManifest`, `SandboxConfig`, `SandboxManager` | `sandcrab-protocol`, core async/IO crates; **no** backend-specific crates (`bollard`, Firecracker, etc.) |
| `sandcrab-backend-docker` | Officially supported `RuntimeBackend` via hardened Docker containers (`bollard`) running real `crabd`; shared kernel, for single-tenant / trusted-host workloads | `sandcrab-core`, `sandcrab-protocol`, `bollard` |
| `sandcrab-rootfs` | Shared microVM rootfs pipeline (lib + `sandcrab-rootfs` bin): OCI image → `docker export` → augment (inject `crabd`, mountpoints) → rootless `mke2fs -d` → read-only ext4, content-addressed cache keyed by `sha256(image_id ‖ arch ‖ crabd_sha256 ‖ PIPELINE_VERSION)`, `--arch linux/amd64\|linux/arm64`, scratch drives, SHA-256 helpers, shared artifact `Error` | std, `tar`, `sha2`, `serde`/`serde_json`, `chrono`, `uuid`, `thiserror`; `clap`/`toml` for the bin. Drives the `docker` and `mke2fs`/`fakeroot` binaries. **No** VMM code, **no** Tokio, **no** `sandcrab-core` |
| `sandcrab-backend-firecracker` | Linux KVM `RuntimeBackend`: owned HTTP-over-UDS Firecracker API client, vsock `CONNECT` transport, pinned kernel/binary (`sandcrab-fc-fetch` bin), block-volume workspaces, tap pool; re-exports `sandcrab-rootfs` as `rootfs` | `sandcrab-core`, `sandcrab-protocol`, `sandcrab-rootfs`, Tokio, `rustix`, `tar`/`flate2`, `sha2`; drives the `firecracker`, `mke2fs`, `docker` and `curl` binaries. **No** Firecracker/VMM SDK crates, **no** `bollard` |
| `sandcrab-sdk` | Public ergonomic Rust SDK (`Sandcrab`, `Sandbox`, `WorkspaceClient`) | `sandcrab-core`, `sandcrab-protocol`; backends only behind feature flags (`docker` default, `firecracker` opt-in, Linux only) |
| `sandcrab-cli` (`sandcrab`) | Operator & developer CLI (`doctor`, `workspace`, `template`, `rootfs`, `run`, `tool`, `tools schema`) | `sandcrab-sdk` (backends via its features), `clap`, `anyhow`, `tokio` |
| `sandcrab-agent` (`crabagent`) | Test harness: Claude coding agent (Anthropic Messages API) whose tools are exactly the Sandcrab tools | `sandcrab-sdk` (`docker` feature), `reqwest`, `rustyline`, `clap`, `dotenvy`; never a backend crate directly |
| `sandcrab-bench` | Backend benchmark runner (scenarios, cgroup/`/proc` sampling, JSON results + schema) | `sandcrab-sdk`, `clap`, `schemars`; `bollard` (optional, `docker` feature) for resource sampling only |

Future crates (Phase 2+): `sandcrab-backend-cloud-hypervisor`, `sandcrab-backend-libkrun`, `sandcrab-server`, `sandcrab-mcp`.

---

## 3. Development & Verification Commands

```bash
# Check & build entire workspace
cargo check --workspace --all-targets

# Run unit tests (protocol, core, guest tool tests)
cargo test --workspace

# Run Docker end-to-end integration tests (requires Docker daemon)
cargo test --workspace -- --ignored

# Lint & formatting (must pass before committing)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Note on macOS local dev:
- `crabd` runs inside Linux containers/microVMs; cross-compile via `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` (or build inside a multi-stage Docker template stage) while unit-testing `sandcrab-guest` tool modules natively on macOS.

---

## 4. Git & Commit Conventions

- Use **Conventional Commits** (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`) with optional crate scope (e.g., `feat(guest): implement file_edit tool with unified diff`).
- Keep commits **atomic**: one logical change per commit (docs, scaffold, single crate feature, single tool, etc.).
- Never commit secrets, `.sandcrab/` local state, or build artifacts.

---

## 5. Picking Up From Here (Agent Handoff)

Phase 1 (V1) is complete on the Docker backend; see [`docs/ROADMAP.md`](docs/ROADMAP.md) (V1 status) and the flow
[`docs/plans/phase-1-mvp.flow.yaml`](docs/plans/phase-1-mvp.flow.yaml). Next agent:
1. Bring up a KVM host (bare metal, or a VM with nested virtualization and `cpu=host`) per [`docs/hosting.md`](docs/hosting.md).
2. Run the pending Firecracker e2e suite (`cargo test -p sandcrab-backend-firecracker -- --ignored`) and the benchmark with `--backends docker,firecracker`, then regenerate `docs/site/bench-report.html` (`scripts/bench-report-data.py`).
3. Close the deferred review item: pack/commit workspaces through the verified root fd and have `crabd` re-check root dev/ino after start (cross-registry nesting race).
4. Phase 2: Cloud Hypervisor + `libkrun` backends, then the egress proxy (Phase 3).
