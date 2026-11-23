# AGENTS.md — Sandcrab Engineering & Handoff Guide

This file is the canonical instruction set for AI coding agents (Claude Code, Codex, Antigravity, Cursor, etc.) working in the **Sandcrab** repository.

Before planning or writing code, read:
1. [`BRIEF.md`](BRIEF.md) — product mission, research conclusions, and open decisions.
2. [`docs/ROADMAP.md`](docs/ROADMAP.md) — full phased implementation plan and tool catalog.

---

## 1. Non-Negotiable Architectural Invariants

1. **No Silent Security Downgrades**:
   - Every `RuntimeBackend` MUST explicitly report `BackendCapabilities` (`hardware_isolated`, `multi_tenant_safe`, `supports_snapshots`, `supports_host_dir_mounts`).
   - Never silently fall back from a hardware microVM backend (`Firecracker`, `CloudHypervisor`, `Libkrun`) to `DockerDev`. `DockerDev` is strictly for local dev/CI and must require explicit selection.
2. **One Guest Protocol Across All Backends**:
   - Host ↔ sandbox communication ALWAYS uses `sandcrab-protocol` talking to `crabd` (`sandcrab-guest`), whether over `virtio-vsock` (microVMs) or Unix socket/loopback (DockerDev).
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
| `sandcrab-backend-docker` | Dev/CI `RuntimeBackend` via Docker (`bollard`) running real `crabd` | `sandcrab-core`, `sandcrab-protocol`, `bollard` |
| `sandcrab-sdk` | Public ergonomic Rust SDK (`Sandcrab`, `Sandbox`) | `sandcrab-core`, `sandcrab-protocol` (optional backend re-exports via feature flags) |
| `sandcrab-cli` (`sandcrab`) | Operator & developer CLI (`doctor`, `workspace`, `template`, `run`) | `sandcrab-sdk`, `sandcrab-core`, `clap`, enabled backends |

Future crates (Phase 2+): `sandcrab-backend-firecracker`, `sandcrab-backend-cloud-hypervisor`, `sandcrab-backend-libkrun`, `sandcrab-server`, `sandcrab-mcp`.

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

The repository is intentionally left at a clean, scaffolded starting point so the next agent can:
1. Confirm the 5 open decisions in `BRIEF.md` §4 / `docs/ROADMAP.md` with the user (or use the documented recommendations if approved).
2. Plan and execute **Phase 1** incrementally:
   - Step 1A: Finalize `sandcrab-protocol` wire framing & full `ToolCall`/`ToolResult` + `schemars` tests.
   - Step 1B: Implement `sandcrab-guest` (`crabd`) Tier 1 & Tier 2 tools + native unit tests.
   - Step 1C: Implement `sandcrab-core` (`WorkspaceRegistry` host-dir ops + `TemplateManifest` loader + `SandboxManager`).
   - Step 1D: Implement `sandcrab-backend-docker` (spawning template images with injected `crabd`, workspace bind mounts, `default_cwd`, and UDS/TCP transport).
   - Step 1E: Wire `sandcrab-sdk`, `sandcrab-cli`, and end-to-end tests against `templates/base` and `templates/python-dev`.
3. Move to **Phase 2** (hardware microVMs: Firecracker / Cloud Hypervisor + `libkrun`).
