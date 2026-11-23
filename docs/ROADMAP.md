# Sandcrab — Phased Implementation Roadmap

This document captures the detailed technical blueprint for executing Sandcrab across four phases so any engineer or AI agent can pick up and plan individual phases cleanly.

---

## V1 status

Phase 1 was extended into **V1**: DockerDev plus a Firecracker backend, benchmarked
(decisions: [ADR 0001](decisions/0001-phase-1-decisions.md); execution flow:
[`plans/phase-1-mvp.flow.yaml`](plans/phase-1-mvp.flow.yaml)). State on branch `feat/phase-1`:

**Done**

- `sandcrab-protocol`: u32 big-endian length-prefixed JSON-RPC 2.0 (16 MiB max frame),
  typed params/results, error codes, JSON Schema export. Reference: [protocol.md](protocol.md).
- `crabd`: all Tier 1 tools, `process_start/poll/stdin/kill/list`, `upload`/`download`
  (single file, base64), `--unix` / `--tcp` / `--vsock`, PID 1 `init` mode for microVMs.
- `sandcrab-core`: `WorkspaceRegistry` (managed and host-dir workspaces, cross-process
  leases, traversal/symlink guards, overlap refusal), `TemplateRegistry` with pins,
  `SandboxManager` with TTL reaper and lifecycle timings.
- `sandcrab-backend-docker`: hardened DockerDev backend (explicit `--allow-insecure`).
- `sandcrab-backend-firecracker`: owned API client, rootfs pipeline (OCI → ext4 with
  `crabd` as init), pinned kernel/binary, vsock, block-volume workspaces with sync-back,
  tap pool + nftables networking. Tested against a fake VMM.
- `sandcrab-sdk`, `sandcrab` CLI ([cli.md](cli.md)), `crabagent` harness
  ([crabagent.md](crabagent.md)), `sandcrab-bench`.
- Templates `base`, `python-dev`, `ts-dev` (now with `python3`), `render`, pinned.
- Verification: QA + security review (3 rounds, PASS), live crabagent run
  ([bench/live-check.md](bench/live-check.md)) and its fixes (core dumps off, `/tmp noexec`
  documented, image blocks, `--keep-alive`, workspace caches, lock files moved to
  `<state root>/locks/`), DockerDev benchmark ([site/bench-report.html](site/bench-report.html)).

**Pending (blocked on a KVM host)**

- A KVM host: the planned Proxmox VM has no `vmx`/`svm` (QEMU 2.5+ CPU type). It needs
  nested virtualization and `cpu: host`, or a bare-metal host. See [hosting.md](hosting.md#2-kvm-host-firecracker).
- Firecracker end-to-end tests on real KVM (`firecracker_kvm.rs`, currently ignored) and
  fixes for any gaps they find.
- Firecracker benchmark columns, run on the same host as DockerDev.

**Deferred to Phase 2**

- MEDIUM (review): two registries with different `SANDCRAB_HOME` can nest workspace roots;
  the overlap check is per registry.
- MEDIUM (review): the DockerDev bind and the Firecracker pack resolve the workspace path
  after verification (a narrow race, 4 wins in 80 attempts). Fix: pack/commit through the
  verified root fd, and a `crabd` dev/ino check after start.
- Firecracker `jailer` confinement (until then `multi_tenant_safe = false`), snapshots,
  published ports on Firecracker, `tar.zst` transfers, `stream_event` output streaming.
- Cloud Hypervisor and libkrun backends; Tier 3 browser tools.

**Phase 3** still owns the egress allowlist and secret-injection proxy:
`egress_restricted` and `secrets` are refused at spawn time in V1.

---

## Tool Catalog (Target Agent Interface)

Every tool exports a JSON Schema via `schemars` so LLM tool-calling (Anthropic, OpenAI, Gemini) and MCP servers bind directly to `sandcrab-protocol`.

### Tier 1 — Core Computer-Use Tools (Phase 1 MVP)
| Tool | Capabilities |
| :--- | :--- |
| `bash` | Run shell commands with `cwd` (defaults to sandbox `default_cwd`), `env`, `timeout_ms`, full process-group cleanup (`killpg`), and bounded head+tail output truncation. |
| `file_read` | Read file contents with 1-based `offset` and `limit`, line numbers, and safe binary-file detection. |
| `file_write` | Create/overwrite files with auto-created parent directories, optional `append`, and atomic write-via-rename. |
| `file_edit` | Exact-match search-and-replace (`old_string` -> `new_string`, optional `replace_all`), failing cleanly if missing or ambiguous, returning a compact unified diff. |
| `file_delete` | Delete file or directory (`recursive: true` required for directories). |
| `file_move` | Move/rename files or directories inside the sandbox. |
| `list_dir` | Directory listing with entry type, size, permissions, optional `depth`, and `.gitignore` support. |
| `glob` | Fast pattern matching via Rust `ignore` + `globset` crates (respects `.gitignore`). |
| `grep` | High-speed regex/literal content search via `ripgrep` core crates (`grep-searcher` + `grep-regex` + `ignore`), returning file, line, match, and optional context lines. |

### Tier 2 — Process, Transfer & Lifecycle Tools (Phase 1–2)
| Tool | Capabilities |
| :--- | :--- |
| `process_start` / `process_poll` / `process_stdin` / `process_kill` | Manage long-running background or interactive PTY processes (dev servers, REPLs, watchers). |
| `upload` / `download` | Stream files or `tar.zst` archives between host and sandbox. |
| `expose_port` | Expose an internal TCP port via local forward (dev) or signed ephemeral preview URL (server/cloud). |
| `sandbox_snapshot` / `sandbox_fork` | *(Orchestrator/SDK)* Checkpoint sandbox disk + memory state and branch parallel agent attempts. |

### Tier 3 — Template-Gated Visual / Browser Tools (Phase 2–3)
| Tool | Capabilities |
| :--- | :--- |
| `browser_navigate` / `browser_screenshot` / `browser_eval` | Headless Chromium/Playwright automation enabled on `render` template. |
| `desktop_action` *(future)* | Optional Xvfb + pointer/keyboard/screenshot primitives for GUI agents. |

---

## Phase 1 — Foundation & Working Local MVP

**Goal**: End-to-end working Rust SDK, CLI, guest daemon (`crabd`), persistent Workspaces, and curated templates running locally via `DockerDevBackend` (with the exact same `crabd` wire protocol used by future microVM backends).

### Deliverables
1. **`sandcrab-protocol`**:
   - Framed RPC envelope (length-prefixed JSON-RPC 2.0 recommended; confirm with user before locking).
   - Full `ToolCall`, `ToolResult`, `StreamEvent`, `ProtocolError`, and `tool_definitions()` JSON Schema export.
   - Serialization round-trip and JSON Schema snapshot unit tests.
2. **`sandcrab-guest` (`crabd`)**:
   - Async Tokio server accepting `--unix <path>`, `--tcp <addr>`, and (Linux) `--vsock <port>`.
   - Complete Tier 1 tools + `process_*` + `upload`/`download`.
   - Native unit tests covering timeouts/`killpg`, binary detection, ambiguous `file_edit`, `.gitignore` handling in `glob`/`grep`, and PTY/process cleanup.
3. **`sandcrab-core`**:
   - `WorkspaceRegistry`: `create`, `register_host_dir`, `list`, `put_file`, `read_file`, `delete_file`, `remove` with canonical path-traversal guards.
   - `TemplateRegistry`: load & validate `sandcrab.toml` manifests and SHA-256 artifact pins.
   - `SandboxManager`: orchestrate `RuntimeBackend`, resolve mounts & `default_cwd`, enforce TTL timeouts, proxy typed tool calls to `crabd`.
4. **`sandcrab-backend-docker`**:
   - `bollard`-based dev backend explicitly marked `hardware_isolated = false, multi_tenant_safe = false`.
   - Hardened container defaults (`--cap-drop=ALL`, memory/CPU/PIDs limits, configurable network mode).
   - Multi-workspace bind mounts (`rw`/`ro`), `crabd` binary startup, UDS/TCP client transport.
5. **`sandcrab-sdk` & `sandcrab-cli`**:
   - Ergonomic Rust API (`Sandcrab`, `Sandbox`, `WorkspaceClient`).
   - CLI commands: `sandcrab doctor`, `sandcrab workspace ...`, `sandcrab template list|build`, `sandcrab run`.
6. **Curated Templates**:
   - Working Docker builds for `base` (Debian 13 `trixie-slim`), `python-dev` (Python 3.15 + `uv`), `ts-dev` (Node.js 26 Active LTS + `pnpm`/`bun`), and `render` (Chromium + Playwright + `ffmpeg` + `imagemagick` + `typst`).

### Verification Gate
- `cargo test --workspace` passes (protocol, core, guest tools).
- `cargo test --workspace -- --ignored` passes end-to-end against real Docker containers + `crabd` + persistent workspace mutation between turns.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass cleanly.

---

## Phase 2 — Production MicroVM Isolation

**Goal**: True hardware-virtualized isolation with zero shared kernel on Linux servers and macOS developer laptops.

### Deliverables
1. **OCI → MicroVM RootFS Pipeline (`sandcrab-templates` / `sandcrab-core`)**:
   - Convert OCI image layers into ext4 rootfs images with `crabd` injected as init/service and SHA-256 manifest pinning.
2. **`sandcrab-backend-libkrun`**:
   - Native hardware-isolated microVMs on Apple Silicon macOS (Hypervisor.framework) and Linux (KVM) with `virtio-fs` workspace mounts and `virtio-vsock` to `crabd`.
3. **Linux Production MicroVM Backend(s)** *(order decided by Open Decision #4 in `BRIEF.md`)*:
   - **`sandcrab-backend-firecracker`**: Owned HTTP-over-UDS Firecracker client, `jailer` confinement (chroot, cgroups v2, seccomp), `virtio-vsock` to `crabd`, `virtio-blk` persistent ext4/`dm-thin` workspaces, and **memory + disk snapshot/restore** (<50ms warm starts).
   - **`sandcrab-backend-cloud-hypervisor`**: Linux KVM with native `virtio-fs` (`virtiofsd`) host-folder workspace mounts, snapshots, and future GPU/hotplug support.

---

## Phase 3 — Self-Hostable Control Plane, Multi-Language SDKs & B2B Security

**Goal**: Turn Sandcrab into a deployable self-hosted platform consumable from any language or agent framework.

### Deliverables
1. **`sandcrab-server`**: Axum REST + WebSocket + OpenAPI daemon, API key auth, workspace & sandbox lifecycle endpoints, TTL reaper, signed preview-URL reverse proxy.
2. **`sandcrab-mcp`**: Model Context Protocol server exposing Sandcrab sandboxes and tools directly to MCP clients.
3. **Egress & Secret-Injection Proxy**: Per-sandbox domain allowlists plus host-side HTTP proxy that injects API credentials on outbound requests so secrets never exist inside sandbox memory/env.
4. **TypeScript & Python SDKs**: Thin clients generated from OpenAPI + streaming WebSocket/SSE helpers.

---

## Phase 4 — Managed B2B Cloud Tier & Template Marketplace

**Goal**: Horizontal multi-node fleet orchestration and commercial ecosystem.

### Deliverables
1. Multi-node placement scheduler, tenant isolation, usage metering, and quotas.
2. Remote snapshot storage with `userfaultfd` lazy memory paging across nodes.
3. Signed **Template Registry & Marketplace** (public curated templates + private org templates with vulnerability scanning and SHA-256 provenance).
