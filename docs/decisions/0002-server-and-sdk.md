---
title: "ADR 0002 — Server, TypeScript SDK and forwards"
status: accepted (implemented)
date: 2026-11-26
---

# ADR 0002 — Server, TypeScript SDK and forwards

Decisions for the `sdk-milestone` flow. [`docs/design/server-and-sdk.md`](../design/server-and-sdk.md) has
the reasoning and the `path:line` references.

"The adopter" is the first application that puts Sandcrab behind its own process-launcher interface. It
is not named in this repository.

**Revised 2026-11-26** after a review from the adopter's side, for an MVP: one user per host as a rule,
two or three at most, safe defaults, no multi-tenant machinery. Items 3, 5, 6, 11, 15 and 16 were
simplified; items 18 and 19 are new.

**Implemented 2026-11-26.** Where the build differs from a decision below, the item carries an "As
built" note. In short: sandboxes are owned per uid (not shared by every admitted client); a sandbox runs
as its client's uid and gid unless the config allows others; the supervisor gets its spec and cancel over
a control pipe (not an in-memory file); `crabd serve` is not a subreaper, so a program that kills its
supervisor leaves its tree until the sandbox is destroyed; the mount cap is 4 on libkrun x86_64. Design
changelog: [design §"As built"](../design/server-and-sdk.md#as-built-what-changed-against-this-design).
Results: [reviews/sdk-milestone-results.md](../reviews/sdk-milestone-results.md).

## Decided by the maintainer

1. **A host daemon.** `sandcrab-server` (a new crate) and the `sandcrab server` command. It owns exactly one
   backend, named in its config. There is never a fallback. Docker and libkrun are in scope. Firecracker
   refuses forwards with an explicit error.
2. **Wire.** JSON-RPC 2.0 over a Unix socket for control, with one dedicated connection per process and
   per forwarded connection beside it. No HTTP, no gRPC, no MCP server.
3. **Access.** No token on the Unix socket and no TCP listener in this milestone. Guards:
   - every connection's peer uid is checked against `allow_uids`. When the list is absent it is the
     server's own uid; any other uid must be listed;
   - socket mode `0600` by default, with an optional group, as a second layer;
   - the server refuses to start on a socket directory that others can write;
   - it refuses any mount that touches its own socket or state directories.

   The per-sandbox token between host and `crabd` (protocol 0.2) is unchanged.

   *As built:* two more guards. A sandbox belongs to the peer uid that created it; another uid cannot see
   or touch it. And a sandbox runs as its client's own uid and gid; another user needs an
   `allow_sandbox_uids` / `allow_sandbox_gids` range in the config, and uid or gid 0 needs
   `allow_root_user`. Without that rule a client could leave a setuid or setgid file of any user in a
   read-write mount, which the host honours (Docker cannot mount a bind mount `nosuid`).
4. **Forwards end at the client.** Guest `127.0.0.1:<port>` → `crabd` → server → SDK, and the SDK connects
   a target the client itself named: a loopback TCP port, a Unix socket, or an in-process handler. The
   server never opens a host socket or port on a client's say-so. The sandbox keeps network `disabled`.
   A later server-side target (Sandcrab's own egress proxy, Phase 3) stays possible.
5. **Sandboxes outlive a client connection, not the server.** TTL and labels decide their end, never a
   disconnect. Labels live in the server's memory. A server restart ends every run on both backends;
   clients drain before an upgrade.
6. **Deployment.**
   - Linux: the server is a root systemd service with ordinary hardening (Docker access is
     root-equivalent anyway). A containerised client mounts the directory that holds the server socket,
     not the socket file, and has its data directories bound at the same absolute path as on the host.
     *As built:* the socket directory is `/run/sandcrab-server`, not `/run/sandcrab` (which every
     sandbox uses for `crabd`).
   - Mac: the client starts the server as a child process, on libkrun, with the socket in a private
     directory. No launchd service. Templates are built on the Mac with Docker for now.
7. **The adapter lives with the adopter.** The code that implements the adopter's interface is in the
   adopter's repository. Sandcrab ships generic primitives only: no adopter name, path or policy here.
8. **The adopter's launcher is the acceptance test.** The TypeScript e2e mirrors its invariants; the pilot
   runs its real suite, on Docker and on libkrun.
9. **Out of scope.** The egress allowlist and the secret-injection proxy stay Phase 3. The Python SDK
   follows on the same server.

## Design choices

10. **Control framing: the u32 big-endian length prefix** that `crabd` uses (ADR 0001 §2), not
    newline-delimited JSON. A connection switches from one frame to records or raw bytes, and only a
    length prefix makes that boundary exact. One codec serves both protocols.
11. **One connection per process, records end to end.** A new connection whose first frame is
    `process.spawn` carries the spec; after the answer it carries the guest's record format in both
    directions: stdin, stdin end, stdout, stderr, exit. The exit is the last record. A process lives as
    long as its connection. Cancel is a call on the control connection, and the server registers a
    process id before it awaits anything, so a cancel cannot lose a race with the spawn. A bounded exec
    is an SDK convenience over the same path. Raw bytes are used only for forwarded connections.
12. **Protocol 0.3 between host and `crabd`.** A `process.open` connection per process with those
    records, because half-close does not survive either microVM vsock bridge. Argv exec with an exact
    environment, per-process rlimits, an OOM score, stderr as tail, merged or streamed, a guest-side
    timeout and output cap. New errors `SPAWN_FAILED`, `LIMIT_EXCEEDED`, `UNSUPPORTED_VERSION`,
    `CANCELLED`. A `hello` from another version is refused. The shell-string tools (`bash`, `process_*`)
    stay unchanged for LLMs.
13. **Tree kill: one supervisor per exec that is a child subreaper** (`crabd supervise`), the same on
    every backend. It finds its tree in `/proc`, asks, waits, forces, and reports whether the tree is
    empty. It needs no capability, so it works in the hardened Docker container and in the microVM
    guests. The decision said it takes its connection as stdin and stdout and its spec from an in-memory
    file. *As built:* it takes the connection as stdout and a control pipe as stdin, which carries the spec and
    then the cancel bytes; the guest stays free of `unsafe`. A cgroup per exec and a pid namespace per
    exec were rejected: both need privileges the sandbox does not have on purpose. Where the subreaper is
    unavailable, the fallback to the process group is reported in `hello`, in the capabilities and by a
    failing self-test line. *As built:* only the supervisor is a subreaper. The program runs as the same
    uid and can kill it; its tree then lives until the sandbox is destroyed, and the exit says
    `tree_clean: false`. Making `crabd serve` a subreaper that sweeps up after a dead supervisor is on the
    backlog.
14. **Forwards are announced, never dialled, by the less trusted side.** `crabd` announces a connection on
    a host-held `forward.listen` connection; the host opens `forward.accept`. The server announces to the
    client; the client connects its target and opens `forward.attach`. No new transport and no libkrun
    helper change is needed. Ports are fixed at spawn, loopback only, capped; every failure closes the
    guest's connection and is counted per forward.
15. **Mounts: allowed roots, one path.** The client names an absolute path. It is the path on the host
    and the path in the sandbox; there is no path translation. The server resolves its configured roots
    at start and opens each mount beneath a held root descriptor without following any symlink. Mounts
    of one sandbox do not nest, and at most 16 are allowed. There is no rule about nesting across
    sandboxes. On Docker the mount's identity is checked again through `crabd` after start. The server's
    own socket, state and pin directories are refused by identity, not by path string. *As built:*
    libkrun on x86_64 boots at most 4 devices for mounts (3 mounts with a NIC); the server reports the
    backend's real cap in `max_mounts`. Apple Silicon was measured to boot 64; the cap there stays 16.
    libkrun's guest root is an overlay, so mount points need no image change.
16. **No server database.** Labels and TTLs are in the manager. A server that starts removes what a
    predecessor left, with the existing reaping in its owned scope. `destroy_by_label` answers "empty"
    only when every destroy succeeded and a second look finds no match.
17. **The isolation claim is earned by a self-test, per template.** Named lines run as `crabd probe` in
    fresh sandboxes of the template the client names, so they need nothing from the image. A line passes
    only on an observed refusal with a specific `errno`; a timeout is `inconclusive` and never a pass.
    *As built:* it runs as the calling client's uid:gid, its scratch directory is made through
    descriptors only (a root server never follows a client's symlink), and a template that was never
    tested reports no summary, which clients read as not proven.
18. **Versions.** `server.hello` returns an API version (major.minor) and a feature list. A server serves
    the current and the previous minor. The SDK carries the minimum server version it needs and, below
    it, reports capabilities as not ok with a reason that names both versions. Unknown params stay
    rejected.
19. **A small API.** The server API and the TypeScript SDK hold what a launcher needs: sandbox create,
    get, list, destroy and destroy by label, a TTL, process spawn and cancel, forwards, capabilities and
    the self-test. File transfer, the LLM tools, template management and a terminal mode come with their
    first caller. The Rust SDK gets only what the server and the self-test need.

## Consequences

- **One server serves one owner per uid.** The decision said every client that passes the uid check
  controls every sandbox. *As built,* a sandbox belongs to the uid that created it, so clients of
  different uids are kept apart; clients of the same uid are not. Clients that must be kept apart
  beyond that get a server each.
- **Limits a client cannot raise** (as built): sandboxes per server, processes per sandbox, resources
  per sandbox including `disk_mb`, connections and forwarded connections sized from the server's fd
  limit.
- **A server restart ends every run.** Reattaching would need the per-sandbox tokens on disk and is not
  planned.
- **Sandboxes may share or nest folders.** A program with a read-write mount can change what lies under
  it, also for another sandbox. Clients that run mutually hostile sandboxes keep their folders apart.
- **A containerised client needs its data at the same path as the host.** That is a deployment change on
  the adopter's side.
- **Three outcomes for a process that did not end normally:** could not start, unavailable (nothing ran;
  only when the request was never written or the server said so), and lost (the connection dropped
  mid-run; never retried as if nothing ran).
- **Env values and stdin are never logged**, and never placed in error data. argv is logged as its first
  element only.
- **New crate `sandcrab-server`**, and server API types with their JSON Schema in `sandcrab-protocol`, from
  which the TypeScript SDK generates its types. `BackendCapabilities` gains `supports_forwards`;
  Firecracker refuses forwards, host mounts and the new options.
- **Bring-your-own-image templates** are manifests with `external = true` and no Dockerfile. They are
  pinned host-locally like built templates; an unpinned image is refused.
- **`RLIMIT_NPROC` on Docker is host-wide per uid.** Clients leave it out there and use the sandbox's
  pids cap; the capabilities say which case applies.
- **Open on the adopter's side** (design §14): the synchronous `started` field, folders and a grouping
  key on short execs, three changes to its conformance suite, and the mapping of a lost process.
  *As built:* the adopter's conformance suite passed through a pilot adapter on Docker and libkrun. A
  sandbox per call costs about 0.2 s on libkrun and 1.5 to 2.7 s on a busy Docker host against 5 to
  7 ms for a warm exec, so the adapter needs a pool of warm sandboxes.
