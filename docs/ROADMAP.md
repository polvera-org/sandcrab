# Sandcrab — Phased Implementation Roadmap

This document captures the detailed technical blueprint for executing Sandcrab across four phases so any engineer or AI agent can pick up and plan individual phases cleanly.

---

## Status

State on branch `feat/phase-1`: Phase 1 (V1) is done, and Phase 2 is partly done (the
libkrun backend, plus the Firecracker backend validated on real KVM). Execution flows:
[`plans/phase-1-mvp.flow.yaml`](plans/phase-1-mvp.flow.yaml) and
[`plans/libkrun-backend.flow.yaml`](plans/libkrun-backend.flow.yaml). Decisions:
[ADR 0001](decisions/0001-phase-1-decisions.md), [design/libkrun-backend.md](design/libkrun-backend.md).

### V1 (Phase 1): done

- `sandcrab-protocol`: u32 big-endian length-prefixed JSON-RPC 2.0 (16 MiB max frame),
  typed params/results, error codes, JSON Schema export. Reference: [protocol.md](protocol.md).
- `crabd`: all Tier 1 tools, `process_start/poll/stdin/kill/list`, `upload`/`download`
  (single file, base64), `--unix` / `--tcp` / `--vsock`, PID 1 `init` mode for microVMs.
- `sandcrab-core`: `WorkspaceRegistry` (managed and host-dir workspaces, cross-process
  leases, traversal/symlink guards, overlap refusal), `TemplateRegistry` with pins,
  `SandboxManager` with TTL reaper and lifecycle timings.
- `sandcrab-backend-docker`: hardened Docker backend, officially supported for
  single-tenant / trusted-host workloads (selected explicitly with `--backend docker`; never a
  fallback; reports `hardware_isolated = false`, `multi_tenant_safe = false`).
- `sandcrab-backend-firecracker`: owned API client, pinned kernel/binary, vsock, block-volume
  workspaces with sync-back, tap pool + nftables networking.
- `sandcrab-sdk`, `sandcrab` CLI ([cli.md](cli.md)), `crabagent` harness
  ([crabagent.md](crabagent.md)), `sandcrab-bench`.
- Templates `base`, `python-dev`, `ts-dev` (with `python3`), `render`.
- Verification: QA + security review (3 rounds, PASS), live crabagent run
  ([bench/live-check.md](bench/live-check.md)) and its fixes, Docker benchmark
  ([site/bench-report.html](site/bench-report.html)).

### libkrun backend flow (Phase 2, partial): done

Final review verdict: **ship with follow-ups** (no critical, high or open medium findings;
[reviews/libkrun-backend-review.md](reviews/libkrun-backend-review.md)).

- **libkrun backend** (`sandcrab-backend-libkrun`) on Apple Silicon macOS (Hypervisor.framework)
  and Linux x86_64 KVM: one signed `sandcrab-krun` helper per sandbox as the VMM, libkrun
  loaded at runtime from SHA-256-pinned paths, ext4 block rootfs, **live virtio-fs workspaces**
  with fd-pinned share roots, published ports over vsock tunnels, TSI never implicit,
  `unrestricted` network through a pinned userspace netstack (gvproxy on macOS, passt on Linux)
  behind the helper's default-deny egress filter, cgroup v2 leaves on Linux.
  `sandcrab libkrun install`, `doctor` `lk:*` checks, crabagent `--backend libkrun`. Reports
  `hardware_isolated = true`, `multi_tenant_safe = false`.
- **`sandcrab-rootfs`**: the OCI → ext4 pipeline extracted from Firecracker and made
  arch-aware (amd64 and arm64 templates).
- **Firecracker on real KVM**: the KVM e2e suite and the SDK e2e suite pass on a bare-metal
  host; published ports over vsock tunnels; SMTP and reserved ranges blocked in
  `fc-net-setup.sh`; first Docker-vs-Firecracker benchmark.
- **Docker on macOS**: a loopback `tcp` transport through a hardened relay container on an
  ICC-off bridge, for Docker Desktop.
- **Protocol 0.2**: per-sandbox 256-bit token on every `crabd` connection, with a pre-auth
  queue that keeps unauthenticated peers out of connection slots.
- **Guest hardening** in microVMs: Docker-parity capabilities, `no_new_privs`, pids cap and
  `/tmp`, then hardening sysctls and read-only `/proc/sys` and `/sys`; fails closed
  ([design/guest-seccomp.md](design/guest-seccomp.md)). A guest seccomp profile was evaluated
  and deferred.
- **Pins verified through the fd that is used**: stat-stamped `0444` rootfs images (libkrun
  and Firecracker), fd-loaded libkrun/libkrunfw, fd-executed netstack and helper on Linux,
  helper install record ([design §12.4](design/libkrun-backend.md)).
- **Host-local template pins** ([template-pins.md](template-pins.md)): local integrity, not
  provenance.
- **Benchmarks** on three hosts (dev-vps, kvm-dedicated, mac-m2) with a per-host report;
  all runs are indicative (none is marked valid).

### Next

In progress (flow step `integrity-fixes`): the top three items of the review backlog below
(R18, N2, N3).

Review backlog, in priority order ([reviews/libkrun-backend-review.md](reviews/libkrun-backend-review.md#consolidated-follow-up-backlog)):

1. **R18:** check (or tighten) the state root, `bin/` and `artifacts/` modes at backend
   construction, so a pre-existing group-writable tree cannot hold a replaced helper.
2. **N2:** bind each stat stamp to the digest it was recorded with.
3. **N3:** clear the netstack's quarantine attribute through the verified fd, at install too;
   fix the hints.
4. **R10:** sign and notarize with a real Developer ID (ad-hoc + hardened runtime is verified).
   `sandcrab libkrun install` re-signs every helper it installs, so a pre-signed helper cannot
   be distributed yet.
5. `doctor` checks for `memory.swap.max` (R7) and TSI-vs-netstack precedence (R12).
6. **I1:** pass the verified root drive fd into the Firecracker child before the jailer lands.
7. **R9:** clear setuid/setgid on virtio-fs shares, or refuse group-/world-accessible share
   roots.
8. **N1** (optional): pre-auth eviction grace period and counters.
9. **I2, I3, I5:** helper-pin reload on mismatch, the legacy-manifest chmod fallback,
   duplicate relay networks on Docker < 25.
10. fs-verity fast path (optional).
11. Docs only: R13 (DNS via the host resolver), R15 (uplink snapshot), R17 (tool processes
    share `crabd serve`'s uid).

Server and SDK milestone review backlog ([reviews/sdk-milestone-review.md](reviews/sdk-milestone-review.md)):

- **M1:** `crabd serve` as a subreaper that kills what a dead supervisor leaves (today:
  `tree_clean: false`, the client destroys the sandbox; stated in [server.md](server.md)).
- **m2:** two codes named `LIMIT_EXCEEDED` (server -32104, guest -32011).
- **m3:** `server.capabilities` says `tree_kill: "subreaper"` with no live sandbox or self-test
  behind it; report `"unknown"` until there is evidence.
- **m4:** `destroy_by_label` can answer `empty: true` while a create of that uid is between
  spawn and `set_owner`.
- **m5:** refuse `/`, `/run`, `/var/run` as mount roots; protect the Docker socket's directory;
  catch a protected directory bind-mounted below a requested mount.
- **m6:** a Linux e2e for the cross-uid guarantee (a second uid on the build host).
- **m7:** sleeps used as synchronisation in server and TS unit tests.
- **m8:** one run of the server under the systemd unit before the pilot.
- **m9:** self-test sandboxes are not counted against `max_sandboxes`.
- Nits: process ids share one namespace across uids; `server.hello` does not advertise
  `max_disk_mb`; `sandbox.destroyed` can be sent twice in a race.

Also next:

- Benchmarks: Docker on mac-m2 (`--backends docker,libkrun`) in an idle weekend window; one
  interleaved, valid `docker,firecracker,libkrun` run on an SSD/NVMe KVM host (kvm-dedicated's
  HDDs skew Docker); rerun Firecracker with its memory/CPU probes ([bench/README.md](bench/README.md#open-bench-todo)).
- Reproducible template content digests, so pins can be committed as provenance
  ([template-pins.md](template-pins.md#follow-up-reproducible-template-digests)).
- Older review items: two registries with different `SANDCRAB_HOME` can nest workspace
  roots; the Docker bind and the Firecracker pack resolve the workspace path after
  verification (fix: pack/commit through the verified root fd, and a `crabd` dev/ino check
  after start).

### Later

- **Phase 2 remainder (on hold):** the focus is Docker and libkrun. Cloud Hypervisor backend; Firecracker `jailer` confinement (until
  then `multi_tenant_safe = false`) and snapshots; libkrun helper confinement (Seatbelt on
  macOS, Landlock + seccomp + per-sandbox uid on Linux); `tar.zst` transfers,
  `stream_event` output streaming; Tier 3 browser tools.
- **Phase 3:** the egress allowlist and secret-injection proxy (`egress_restricted` and
  `secrets` are refused at spawn time until then) and `sandcrab-server`. The TypeScript and Python
  SDKs are pulled forward as the next milestone (see Phase 3).

---

## Tool Catalog (Target Agent Interface)

Every tool exports a JSON Schema via `schemars` so LLM tool-calling (Anthropic, OpenAI, Gemini) binds directly to `sandcrab-protocol`.

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

**Goal**: End-to-end working Rust SDK, CLI, guest daemon (`crabd`), persistent Workspaces, and curated templates running locally via `DockerBackend` (with the exact same `crabd` wire protocol used by future microVM backends).

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
1. **OCI → MicroVM RootFS Pipeline (`sandcrab-templates` / `sandcrab-core`)**: **done** (`sandcrab-rootfs`, arch-aware, stamped cache).
   - Convert OCI image layers into ext4 rootfs images with `crabd` injected as init/service and SHA-256 manifest pinning.
2. **`sandcrab-backend-libkrun`**: **done** (see [Status](#libkrun-backend-flow-phase-2-partial-done)).
   - Native hardware-isolated microVMs on Apple Silicon macOS (Hypervisor.framework) and Linux (KVM) with `virtio-fs` workspace mounts and `virtio-vsock` to `crabd`.
3. **Linux Production MicroVM Backend(s)** *(order decided by Open Decision #4 in `BRIEF.md`)*:
   - **`sandcrab-backend-firecracker`** (built and passing on real KVM; `jailer` and snapshots pending): Owned HTTP-over-UDS Firecracker client, `jailer` confinement (chroot, cgroups v2, seccomp), `virtio-vsock` to `crabd`, `virtio-blk` persistent ext4/`dm-thin` workspaces, and **memory + disk snapshot/restore** (<50ms warm starts).
   - **`sandcrab-backend-cloud-hypervisor`**: Linux KVM with native `virtio-fs` (`virtiofsd`) host-folder workspace mounts, snapshots, and future GPU/hotplug support.

---

## Phase 3 — Self-Hostable Control Plane, Multi-Language SDKs & B2B Security

**Goal**: Turn Sandcrab into a deployable self-hosted platform consumable from any language or agent framework.

### Deliverables
1. **`sandcrab-server`**: Axum REST + WebSocket + OpenAPI daemon, API key auth, workspace & sandbox lifecycle endpoints, TTL reaper, signed preview-URL reverse proxy.
2. **Egress & Secret-Injection Proxy**: Per-sandbox domain allowlists plus host-side HTTP proxy that injects API credentials on outbound requests so secrets never exist inside sandbox memory/env.
3. **TypeScript & Python SDKs** (**next milestone**, pulled ahead of the rest of Phase 3): first-class
   clients so existing TypeScript and Python agent products can replace their homegrown sandbox managers
   with Sandcrab. The transport (server API vs. native binding) is decided in the milestone's plan.

An MCP server is no longer planned: tools are consumed through the SDKs, the CLI and the exported JSON Schemas.

---

## Phase 4 — Managed B2B Cloud Tier & Template Marketplace

**Goal**: Horizontal multi-node fleet orchestration and commercial ecosystem.

### Deliverables
1. Multi-node placement scheduler, tenant isolation, usage metering, and quotas.
2. Remote snapshot storage with `userfaultfd` lazy memory paging across nodes.
3. Signed **Template Registry & Marketplace** (public curated templates + private org templates with vulnerability scanning and SHA-256 provenance).
