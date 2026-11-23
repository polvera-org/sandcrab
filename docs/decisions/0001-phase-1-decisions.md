---
title: "ADR 0001 — Phase 1 / V1 decisions"
status: accepted
date: 2026-11-23
---

# ADR 0001 — Phase 1 / V1 decisions

Resolves the open decisions in `BRIEF.md` §4. Approved by the maintainer on 2026-11-23.

1. **Tool catalog**: all of Tier 1 (`bash`, `file_read`, `file_write`, `file_edit`, `file_delete`, `file_move`,
   `list_dir`, `glob`, `grep`). Tier 2: `process_start/poll/stdin/kill/list` and `upload`/`download`
   (single file, base64; `tar.zst` later). `SandboxConfig.ports` publishes guest ports to host loopback for previews.
   Tier 3 browser tools are deferred.
2. **Wire framing**: u32 big-endian length-prefixed JSON-RPC 2.0, with `method` = tool name and `params` = typed params.
   One protocol over UDS, TCP and vsock. No gRPC.
3. **License**: Apache-2.0 for all OSS crates. Control-plane licensing gets revisited in Phase 3.
4. **MicroVM priority**: **Firecracker is pulled into V1** so it can be benchmarked against DockerDev on the same
   KVM host (kvm-homelab). Cloud Hypervisor and libkrun follow in Phase 2.
5. **Security hooks**: `SandboxConfig` gets typed `network { policy, allow_domains }` plus reserved `secrets`.
   A backend that cannot enforce a requested policy fails at spawn time and never ignores it silently.
6. **Test harness**: in-repo Rust `crabagent` (pi-style agent loop) on the Anthropic Messages API, with a
   configurable model (default `claude-sonnet-5-5`). A pi-coding-agent adapter comes later.

## Consequences

- `DockerDevConfig::allow_insecure_dev_backend` defaults to `false`.
- Firecracker has no `virtio-fs`: workspaces need a block-volume model (see `docs/design/firecracker-backend.md`).
- The V1 Firecracker backend reports `multi_tenant_safe = false` until `jailer` confinement lands.
