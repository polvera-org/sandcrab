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
5. **Pinned Templates (local integrity now, reproducible later)**:
   - Templates are standard OCI `Dockerfile`s + `sandcrab.toml` manifests.
   - Keep runtime versions modern (as of late 2026: Debian 13 `trixie-slim`, Python 3.15 + `uv`, Node.js 26 Active LTS).
   - Base images are digest-pinned in the repo (`FROM …@sha256:…`).
   - Built template images are pinned **host-locally** (`~/.sandcrab/template-pins/`, written by `templates/build.sh` or
     `sandcrab template pin`). That is local integrity ("unchanged since this host built it"), not provenance. Never
     commit host image IDs to `sandcrab.toml`, and never auto-trust an unpinned image. See
     [`docs/template-pins.md`](docs/template-pins.md).
   - microVM rootfs, kernel, libkrun libraries and `crabd` are pinned with SHA-256 digests.
   - Reproducible template content digests (pinnable in the repo, i.e. provenance) are a planned follow-up.
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
| `sandcrab-backend-libkrun` | macOS (Apple Silicon, Hypervisor.framework) + Linux KVM `RuntimeBackend` on libkrun: one signed `sandcrab-krun` helper process per sandbox (reads a validated `HelperConfig` from stdin, becomes the VMM, stdin lifeline), libkrun `dlopen`ed at runtime from SHA-256-pinned paths, ext4 block rootfs, live virtio-fs workspaces, TSI never implicit; `unrestricted` = pinned passt / gvproxy spawned by the helper behind its egress filter (`netstack.rs`); `entitlements.plist` + `scripts/sign-krun-helper.sh` | `sandcrab-core`, `sandcrab-protocol`, `sandcrab-rootfs`, Tokio, `rustix`, `sha2`, `nix` (`net`: `getifaddrs`), `libloading` (helper only). `unsafe_code = "deny"` with `src/ffi.rs` as the single `#[allow(unsafe_code)]` island. **No** link-time libkrun, bindgen or `krun-sys`, **no** `bollard`; builds and tests without libkrun installed |
| `sandcrab-sdk` | Public ergonomic Rust SDK (`Sandcrab`, `Sandbox`, `WorkspaceClient`; explicit `.docker()` / `.firecracker()` / `.libkrun()`, never a fallback) | `sandcrab-core`, `sandcrab-protocol`; backends only behind feature flags (`docker` default; `firecracker` opt-in, Linux only; `libkrun` opt-in, macOS aarch64 + Linux, target-gated dependency) |
| `sandcrab-cli` (`sandcrab`) | Operator & developer CLI (`doctor`, `workspace`, `template`, `rootfs`, `libkrun install`, `run`, `tool`, `tools schema`; `--backend docker\|firecracker\|libkrun`, `--libkrun-tsi`) | `sandcrab-sdk` (backends via its features: default `docker`, `firecracker`, `libkrun`; the last two compile to nothing off their targets, since Cargo default features cannot be target-specific), `clap`, `anyhow`, `tokio` |
| `sandcrab-agent` (`crabagent`) | Test harness: Claude coding agent (Anthropic Messages API) whose tools are exactly the Sandcrab tools | `sandcrab-sdk` (backends via its features, like `sandcrab-cli`: default `docker`, `firecracker`, `libkrun`; the last two compile to nothing off their targets), `reqwest`, `rustyline`, `clap`, `dotenvy`; never a backend crate directly |
| `sandcrab-bench` | Backend benchmark runner (scenarios, cgroup/`/proc` sampling, macOS helper RSS for libkrun, JSON results + schema) | `sandcrab-sdk` (`docker`, `firecracker`, `libkrun` features), `clap`, `schemars`; `bollard` (optional, `docker` feature) for resource sampling only |

Future crates (Phase 2+): `sandcrab-backend-cloud-hypervisor`, `sandcrab-server`, `sandcrab-mcp`.

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

### Benchmarks: smallest tier that answers the question

`sandcrab-bench` has three tiers (see [`docs/bench/README.md`](docs/bench/README.md#benchmark-tiers-pick-the-smallest-that-answers-the-question)):
**Smoke** (`--quick`, minutes, never published) is the default for revalidation; **Standard**
(`--resource-runs 10`, ~1 h) for step-level and perf before/after numbers; **Full** (canonical, hours) only
for published numbers and only when the human asks for it. Reuse earlier valid results when
the backend and host have not changed.

### Disk hygiene (clean up after yourself)

This repo is heavy on disk: Rust `target/` dirs (several GB each), template images, rootfs images, and Docker
build cache. Dev machines can run out of space mid-task, which breaks Docker and every tool at once. So:

- **Check free space** (`df -h .`) before large builds, image builds or benchmarks, and again every so often in long
  tasks. If it drops below ~15 GB, clean up before you continue.
- **Share one build dir.** Parallel worktrees should not each build a full `target/`. Point them at a shared
  `CARGO_TARGET_DIR`, or delete a worktree's `target/` once its step is merged.
- **Remove what you created when you're done:** merged worktrees (`git worktree remove`), scratch rootfs/ext4 images,
  stopped test containers, and dangling images.
- **Prune Docker regularly:** `docker system df`, then `docker builder prune -f` and `docker image prune -f`. Only use
  `docker system prune -a` when no pinned template images are needed. Never remove volumes or images you didn't create
  without asking.
- **Clear tool caches when space is tight:** `cargo clean -p <crate>` for stale crates, `brew cleanup -s`,
  `npm cache clean --force`, `uv cache clean`.
- Never delete anything outside build output, caches and your own scratch files without asking the human.

---

## 4. Git & Commit Conventions

- Use **Conventional Commits** (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`) with optional crate scope (e.g., `feat(guest): implement file_edit tool with unified diff`).
- Keep commits **atomic**: one logical change per commit (docs, scaffold, single crate feature, single tool, etc.).
- Never commit secrets, `.sandcrab/` local state, or build artifacts.

---

## 5. Picking Up From Here (Agent Handoff)

**Done.** Phase 1 (V1) and the libkrun backend flow (Phase 2, partial) are complete on `feat/phase-1`; see
[`docs/ROADMAP.md`](docs/ROADMAP.md#status) and the flows [`docs/plans/phase-1-mvp.flow.yaml`](docs/plans/phase-1-mvp.flow.yaml)
and [`docs/plans/libkrun-backend.flow.yaml`](docs/plans/libkrun-backend.flow.yaml). Three backends ship: Docker (Linux,
and macOS over the `tcp` transport), Firecracker (Linux KVM; KVM e2e passes) and libkrun (Apple Silicon macOS and
Linux KVM; e2e passes on both). Also done: protocol 0.2 connection auth, guest hardening that fails closed,
host-local template pins, pins verified through the fd that is used, a per-host bench report. The final review
([`docs/reviews/libkrun-backend-review.md`](docs/reviews/libkrun-backend-review.md)) says "ship with follow-ups".
Host setup for every backend: [`docs/hosting.md`](docs/hosting.md).

**Next**, roughly in this order:
1. The review backlog ([`docs/ROADMAP.md`](docs/ROADMAP.md#next)). The top three (R18 private-dir checks, N2 stamps bound to
   digests, N3 quarantine cleared through the fd) are the `integrity-fixes` step; then R10 (sign and notarize with a
   real Developer ID; note that `sandcrab libkrun install` re-signs every helper), the `doctor` checks for swap and
   TSI precedence, I1 (fd passing for Firecracker drives before the jailer), R9 (setuid bits on virtio-fs shares).
   Also drop the stale "e2e pending" note from the libkrun `doctor` summary.
2. Benchmarks ([`docs/bench/README.md`](docs/bench/README.md#open-bench-todo)): Docker vs libkrun on mac-m2 in an idle
   weekend window (`--backends docker,libkrun`); one valid, interleaved `docker,firecracker,libkrun` run on an SSD/NVMe
   KVM host (kvm-dedicated's HDDs skew Docker). Then regenerate `docs/site/bench-report.html`
   (`scripts/bench-report-data.py`).
3. The older deferred review items: pack/commit workspaces through the verified root fd and have `crabd` re-check root
   dev/ino after start; the cross-registry nesting race.
4. Phase 2 remainder: Cloud Hypervisor backend, Firecracker `jailer` (then reconsider `multi_tenant_safe`), libkrun helper
   confinement (Seatbelt / Landlock + seccomp), reproducible template content digests.
5. Phase 3: the egress allowlist and secret-injection proxy.
