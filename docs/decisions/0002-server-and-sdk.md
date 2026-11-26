---
title: "ADR 0002 — Server, TypeScript SDK and forwards"
status: accepted
date: 2026-11-26
---

# ADR 0002 — Server, TypeScript SDK and forwards

Decisions for the `sdk-milestone` flow. Items 1 to 9 were taken by the maintainer on 2026-11-25 and
2026-11-26 and are recorded here. Items 10 to 17 are the design choices of
[`docs/design/server-and-sdk.md`](../design/server-and-sdk.md), which has the reasoning and the
`path:line` references.

"The adopter" is the first application that puts Sandcrab behind its own process-launcher interface. It
is not named in this repository.

## Decided by the maintainer

1. **A host daemon.** `sandcrab-server` (a new crate) and the `sandcrab server` command. It owns exactly one
   backend, named in its config. There is never a fallback. Docker and libkrun are in scope. Firecracker
   refuses forwards with an explicit error.
2. **Wire.** JSON-RPC 2.0 over a Unix socket for control, raw byte streams beside it for process I/O and
   forwarded connections. No HTTP, no gRPC, no MCP server.
3. **Access.** No token on the Unix socket and no TCP listener in this milestone. Mandatory guards:
   - the server starts only with an explicit list of allowed peer uids and checks every connection
     against it (`SO_PEERCRED`, `getpeereid` on macOS);
   - socket mode `0600` by default, as a second layer;
   - it refuses to start on a socket directory that others can write;
   - it refuses any mount that contains its own socket.

   A per-client token comes only if a TCP listener is ever added. The per-sandbox token between host and
   `crabd` (protocol 0.2) is unchanged.
4. **Forwards end at the client.** Guest `127.0.0.1:<port>` → `crabd` → server → SDK, and the SDK dials a
   loopback TCP port or a Unix socket that the client itself named. The server never opens a host socket
   or port on a client's say-so. The sandbox keeps network `disabled`. A later server-side target
   (Sandcrab's own egress proxy, Phase 3) stays possible and is not designed here.
5. **Sandboxes outlive a client connection.** TTL and labels decide their end, never a disconnect. Labels
   are stored with the backend object, so listing and destroying by label works after a client restart
   and after a server restart.
6. **Deployment.**
   - Linux: the server is a host service with Docker access. A containerised client mounts only the
     server socket. Its data is in a Docker named volume, and the server's path map points at that
     volume's host mount point.
   - Mac: the client is a host process beside the server, on libkrun. The path map is the identity.
7. **The adapter lives with the adopter.** The code that implements the adopter's interface is in the
   adopter's repository. Sandcrab ships generic primitives only: no adopter name, path or policy here.
8. **The adopter's launcher is the acceptance test.** The TypeScript e2e mirrors its invariants; the pilot
   runs its real suite.
9. **Out of scope.** The egress allowlist and the secret-injection proxy stay Phase 3. The Python SDK
   follows on the same server.

## Design choices

10. **Control framing: the u32 big-endian length prefix** that `crabd` uses (ADR 0001 §2), not
    newline-delimited JSON. A stream connection switches from one frame to raw bytes, and only a length
    prefix makes that boundary exact. One codec serves both protocols.
11. **Stream attachment: a new connection whose first frame names it**, the pattern of `tunnel.open`.
    `process.spawn` carries the spec and becomes the process's stdin and stdout. `stream.open` is a sink
    for stderr chunks, opened before the spawn that uses it. `forward.attach` carries one forwarded
    connection. The exit comes from `process.wait` on the control connection. A process lives as long as
    its stdio connection.
12. **Protocol 0.3 between host and `crabd`.** A `process.open` connection per process, carrying small
    binary records (stdin, stdin end, stdout, stderr, exit) instead of raw bytes, because half-close does
    not survive either microVM vsock bridge. Argv exec with an exact environment, per-process rlimits, an
    OOM score, stderr as tail, merged or streamed, a guest-side timeout and output cap. New errors
    `SPAWN_FAILED`, `LIMIT_EXCEEDED`, `UNSUPPORTED_VERSION`. A `hello` from another version is refused.
    The shell-string tools (`bash`, `process_*`) stay unchanged for LLMs.
13. **Tree kill: one supervisor per exec that is a child subreaper** (`crabd supervise`), the same on
    every backend. It finds its tree in `/proc`, asks, waits, forces, and reports whether the tree is
    empty. It needs no capability, so it works in the hardened Docker container and in the microVM
    guests. It takes its connection as stdin and stdout and its spec from an in-memory file, so the guest
    stays free of `unsafe`. A cgroup per exec and a pid namespace per exec were rejected: both need
    privileges the sandbox does not have on purpose. Where the subreaper is unavailable, the fallback to
    the process group is reported in `hello`, in the capabilities and by a failing self-test line.
14. **Forwards are announced, never dialled, by the less trusted side.** `crabd` announces a connection on
    a host-held `forward.listen` connection; the host opens `forward.accept`. The server announces to the
    client; the client dials its target and opens `forward.attach`. Every connection is opened from the
    more trusted side. No new transport and no libkrun helper change is needed. Ports are fixed at spawn,
    loopback only, capped, and every failure closes the guest's connection.
15. **Mounts: same path, mapped by a table.** The client names an absolute path; the sandbox sees it at
    that path. The server maps it through configured `client → host` roots, which are also the allowed
    roots, and opens it beneath a held root descriptor without following any symlink. Mounts of a sandbox
    do not nest. A mount inside another live sandbox's read-write mount is refused by default. On Docker
    the mount's identity is checked again through `crabd` after start. The server's own socket, state and
    pin directories are refused by identity, not by path string. Registered workspaces keep their model.
16. **Labels on the backend object, no server database.** Container labels on Docker; the run record on
    libkrun. After a restart the server lists what its predecessor left on Docker as `orphaned`: it can be
    listed and destroyed, not used, and is removed after a grace period. On libkrun nothing survives the
    server, because the helper's lifeline ends the VM. `destroy_by_label` answers "empty" only after a
    fresh backend listing.
17. **The isolation claim is earned by a self-test.** Named lines run as `crabd probe` in fresh sandboxes,
    so they need nothing from the image. A line passes only on an observed refusal with a specific
    `errno`; a timeout is `inconclusive` and never a pass. The report is available from the CLI, at server
    start, through `server.selftest`, and as a summary in the capabilities.

## Consequences

- **One server serves one owner.** Every client that passes the uid check controls every sandbox on that
  server. Clients that must be kept apart get a server each.
- **A server restart ends every running process**, and on libkrun every sandbox. Reattaching to a Docker
  sandbox would need the per-sandbox tokens on disk and is deferred.
- **`sandcrab-protocol` gains the server API types** and their JSON Schema, so the TypeScript SDK generates
  its types from a snapshot and depends on no other step.
- **New crate `sandcrab-server`.** It depends on `sandcrab-sdk` (backends through its features),
  `sandcrab-protocol` and `sandcrab-core`; no backend crate directly, no HTTP stack, no TCP listener.
  `sandcrab-cli` may depend on it. AGENTS.md's crate map gets the row in the document step.
- **`BackendCapabilities` gains `supports_forwards` and `persistent_labels`.** A backend that cannot do
  what was asked fails the spawn; Firecracker does so for forwards, ad-hoc mounts and the new options.
- **Bring-your-own-image templates** are manifests with `external = true` and no Dockerfile. They are
  pinned host-locally like built templates; an unpinned image is never trusted.
- **`RLIMIT_NPROC` on Docker is host-wide per uid.** The per-sandbox `pids_max` is the dependable cap
  there.
- **The legacy `bash` and `process_*` tools keep their `setsid` limit** in this milestone.
- **Open on the adopter's side** (design §14): the synchronous `started` field, execs that declare no
  folders, a grouping key for `sweep`, and two conformance assertions that read guest process ids in the
  client's process table.
