# Next milestone: TypeScript SDK and `sandcrab-server` (brief)

Status: **draft, on hold.** The flow is written once the first adopter's codebase has settled. The flow then
goes in `docs/plans/sdk-milestone.flow.yaml`.

## Decisions (2026-11-25)

- **Backends in focus:** Docker and libkrun. The rest of Phase 2 is on hold: Cloud Hypervisor, the Firecracker
  `jailer`, and the review backlog that isn't blocking.
- **No MCP server.** Tools are used through the SDKs, the CLI and the exported JSON Schemas.
- **The TypeScript and Python SDKs are the next milestone**, ahead of the rest of Phase 3. TypeScript comes first.
- **Sandcrab stays project-agnostic.** The adopter keeps its own policy layer. Sandcrab provides only generic
  primitives.

## Why a daemon, not a native binding

The first adopter is a TypeScript agent product. It runs as a hardened container with no Docker socket, on
purpose. A native binding would need `docker.sock` inside that container, which is root-equivalent and would
undo its hardening. So the SDK talks to **`sandcrab-server`**, a host daemon with backend access, over a
Unix socket. The adopter mounts only that socket. The Python SDK will reuse the same server later.

## Scope

1. **`sandcrab-server` (minimal).**
   - It listens on a Unix socket only: JSON-RPC 2.0 for control, raw byte streams beside it for process I/O. No
     token: the socket's owner, group and mode are the access control. A token per client comes only with a TCP
     listener, which is not in this milestone (decided 2026-11-26).
   - It offers sandbox create/destroy, workspaces, tool calls, and process output streaming.
   - It can label sandboxes and sweep them by label.
2. **TypeScript SDK (`@sandcrab/sdk`).**
   - Async and cancellable (`AbortSignal`), with typed errors (unavailable vs. failed) and many concurrent sandboxes.
   - `exec(argv, {env, cwd, timeout, signal})`, with streamed output and a bounded tail.
   - `process_start` with stdin kept open, plus a line or JSONL stream (`AsyncIterable`), and cancellation that
     waits a grace period before kill-tree.
   - Upload and download.
3. **Socket forwards (generic networking primitive).** The sandbox has no network, and each listed host Unix
   socket appears inside the sandbox at `127.0.0.1:<port>`, relayed by `crabd`.
   - On Docker: a container with network `none` and a bind-mounted socket.
   - On libkrun: a vsock port mapped to a host socket, the same mechanism as the control socket.
   - This lets an adopter keep its own egress proxy. Sandcrab's own egress and secret proxy, planned for Phase 3,
     later becomes one more forwarded socket.
4. **Spawn options.**
   - The uid the process runs as, so host-dir workspace files keep their ownership.
   - An exec-capable `/tmp`, as an option.
   - Configurable pids, memory and nofile limits, since monorepo builds need more than the defaults.
   - Workspaces mounted at fixed absolute paths.

## Adoption path (adopter side; planned in the adopter's repo)

1. **Sandboxed check steps:** the adopter already has a small "step runtime" interface that runs `argv` as a uid,
   in a workspace, with network `none` or `proxy(socket, port)`. Put a Sandcrab implementation behind it. The
   adopter gains a container and a cgroup per step, and can drop its in-container namespace workaround.
2. **Agent runs:** these use the adopter's planned launcher interface (`spawn`/`exec`/`kill`/`sweep`). Sandcrab
   fills the role of the box daemon it had planned to build, and the agent CLI runs inside a sandbox with forwards
   for the adopter's proxies.
3. **Egress:** move the adopter's egress and secret-injection proxy into Sandcrab as a generic feature, carefully.

## Open questions for the flow

- The wire format of `sandcrab-server`: JSON-RPC over the existing framing, or HTTP and WebSocket with OpenAPI.
  The second suits code-generated SDKs.
- Whether the forward relay lives in `crabd` (portable) or the backend (Docker sidecar or vsock).
- How a sandboxed uid maps onto host-dir ownership on libkrun (virtio-fs) vs. Docker (bind mounts).
- Probes the adopter can run in its own boot self-test, such as "no network except the forwards" and "no user
  namespaces".
