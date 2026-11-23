# Sandcrab — Project Brief

> **Sandcrab** (*sandbox + Rust's crab mascot*) is an open-source, self-hostable, Rust-based ephemeral sandbox manager and SDK for AI agents.

## 1. Mission & Business Model

Modern AI agents generate and execute untrusted code, shell commands, builds, and browser automations. Standard containers share the host kernel and are not a sufficient multi-tenant security boundary.

**Sandcrab** provides on-demand, hardware-isolated ephemeral sandboxes with built-in agent computer-use tools, curated templates, and persistent mountable workspaces:

- **Open-Source & Self-Hostable Core**: Embeddable Rust SDK, CLI, guest daemon (`crabd`), single-node control plane, and curated starter templates.
- **Commercial Path (B2B Cloud Tier)**: Managed multi-tenant cloud runtime (sub-50ms warm microVM restores, global snapshot caching, tenant quotas, SSO/RBAC, egress firewalling & secret-injection proxies) and a future **Template Marketplace**.

---

## 2. Core Concepts

### Templates (`templates/<name>/`)
Pre-configured OCI images paired with a `sandcrab.toml` manifest for common agent workloads (`base`, `python-dev`, `ts-dev`, `render`, etc.). Artifacts are content-addressed via **SHA-256 digests** to support reproducible execution and a future community/enterprise marketplace.

### Workspaces (`sandcrab-core::workspace`)
Durable storage decoupled from ephemeral compute:
- Register persistent host directories (`HostDir`), Sandcrab-managed directories (`ManagedDir`), or block volumes (`BlockVolume` for Firecracker/cloud).
- **Mutate between turns**: While no sandbox is running, the host/orchestrator can add user attachments, inject repo files, or inspect outputs with zero idle compute cost.
- **Selective multi-mount on spawn**: Mount zero, one, or multiple workspaces into a sandbox at explicit guest paths with kernel/VMM-enforced `ReadWrite` or `ReadOnly` modes and an explicit `default_cwd`.
- **Cross-template handoffs**: Run Turn 1 in `python-dev`, tear down, and mount the same workspace into `render` for Turn 2.

### Guest Daemon (`crabd`)
A tiny, statically linked Rust binary (`musl`) running inside every sandbox. Exposes strongly-typed computer-use tools (`bash`, `file_read`, `file_write`, `file_edit`, `file_delete`, `file_move`, `list_dir`, `glob`, `grep`, `process_*`, file transfers) over a single wire protocol (`sandcrab-protocol`) transported via `virtio-vsock` (microVMs) or Unix socket/loopback (local Docker dev).

### Pluggable `RuntimeBackend` (Zero Silent Fallback)
Every backend explicitly declares its isolation capabilities (`hardware_isolated`, `supports_snapshots`, `supports_host_dir_mounts`, `multi_tenant_safe`). Sandcrab **never** silently downgrades from hardware isolation to a shared-kernel container.

---

## 3. Architecture Research Summary

### Why not build on `kombuse-microvm`?
We evaluated [`kombuse-microvm`](https://docs.rs/kombuse-microvm/latest/microvm/) (`0.2.x`, MIT).
- **Adopt**: SHA-256 pinned boot inputs, explicit backend selection with no silent fallback, and host worker confinement (Landlock/Seatbelt).
- **Do not depend on as core**: `kombuse-microvm` is designed around single-command batch *"Tree In/Out"* desktop jobs (`/job/job.json`), not persistent interactive agent sessions over `vsock`, nor multi-tenant cloud Firecracker snapshots (`jailer`, `dm-thin`, `userfaultfd`). Owning the runtime trait, VMM drivers, and `crabd` is Sandcrab's core technical moat.

### MicroVM Backend Tradeoffs
| Backend | Isolation | Workspace Mounting | Snapshots | Role |
| :--- | :--- | :--- | :--- | :--- |
| **Firecracker** | Hardware VM (Linux KVM, ~5 MB overhead) | `virtio-blk` only (no `virtio-fs`) | Sub-50ms warm restore | Cloud / high-density production |
| **Cloud Hypervisor** | Hardware VM (Linux KVM, `rust-vmm`) | Native `virtio-fs` + `virtio-blk` | Supported | Self-hosted Linux host-folder mounts & future GPU/hotplug |
| **libkrun** | Hardware VM (macOS HVF / Linux KVM) | Native `virtio-fs` | Limited | Local macOS Apple Silicon & Linux hardware-isolated dev |
| **DockerDev (`bollard`)** | Shared kernel (**dev/CI only**) | Native bind mounts | No | Phase 1 local dev & CI integration testing |

---

## 4. Open Decisions (To Finalize Before / During Phase 1)

1. **Tool Catalog Sign-Off**: Finalize Tier 1 / Tier 2 / Tier 3 tool names and parameter shapes (see `docs/ROADMAP.md`).
2. **Wire Framing**: Length-prefixed JSON-RPC 2.0 over `vsock`/UDS (recommended for tiny static `crabd` binary) vs. `tonic` gRPC.
3. **OSS License**: Apache-2.0 across all OSS crates vs. Apache-2.0 SDK + AGPL/BSL control plane.
4. **Phase 2 MicroVM Priority**: `Firecracker` (`virtio-blk` volumes) + `libkrun` vs. `Cloud Hypervisor` (`virtio-fs` + `virtio-blk`) + `libkrun`.
5. **Day-1 B2B Security Hooks**: Egress domain allowlists and host-side secret-injection HTTP proxy interfaces in `SandboxConfig`.
