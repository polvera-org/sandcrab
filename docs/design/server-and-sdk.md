# Server, TypeScript SDK and forwards: design

Status: **implemented** (2026-11-26) by the `sdk-milestone` flow ([plan](../plans/sdk-milestone.plan.mdx),
[flow](../plans/sdk-milestone.flow.yaml)). Decisions are recorded in
[ADR 0002](../decisions/0002-server-and-sdk.md). What was verified, where, and the measured numbers:
[reviews/sdk-milestone-results.md](../reviews/sdk-milestone-results.md). Operator guide:
[server.md](../server.md). Wire reference: [protocol.md](../protocol.md).

This page was the build contract for the flow's implement steps. Facts about the code before the
milestone carry a `path:line` reference; those line numbers are from that time and have moved since.
The text below has been corrected where the build differs, and each correction is listed here.

## As built: what changed against this design

| § | The design said | As built |
| --- | --- | --- |
| Scope, §12.1, §12.2 | Every admitted client controls every sandbox | **A sandbox belongs to the peer uid that created it.** Another uid gets `SANDBOX_NOT_FOUND` for it, never sees it in a list, cannot cancel its processes or attach to its forwards |
| §3.5 | The spec goes to `crabd supervise` in a `memfd` on fd 2; the connection is its stdin and stdout; cancel is two signals | The spec and later cancel bytes go over a **control pipe** on the supervisor's stdin; the connection is its stdout; stderr is inherited. No `memfd` |
| §3.5, §12.3 | `crabd serve` is a subreaper too and kills what a dead supervisor leaves | **Only the per-process supervisor is a subreaper.** A program that kills its supervisor leaves its tree running until the sandbox is destroyed; the exit says `tree_clean: false` (§3.5, Known limits) |
| §3.4, §5.4 | Early cancels are remembered | Remembered for 60 s, at most 1024 ids, in `crabd` and in the server alike |
| §4.2 | Forward caps per forward are set per forward | `crabd` holds a fixed 64 per forward and 256 per sandbox; a lower `max_connections` is enforced on the host by rejecting. Connection ids are a counter from 1 |
| §5.3 | `sandbox.destroyed` for `destroyed`, `ttl`, `backend_exit` | Sent to the owner uid's control connections for explicit destroys and TTL expiry. `backend_exit` is never sent |
| §5.6 | Fixed limits | Plus `max_disk_mb` (config), a server-wide cap on forwarded connections, and connection caps sized from `RLIMIT_NOFILE`, which the server raises at start |
| §5.9, §6.4 | Any non-zero `user` is accepted | A sandbox runs as its client's uid and gid. Another uid or gid needs `allow_sandbox_uids` / `allow_sandbox_gids` |
| §5.10 | Socket directory unspecified in places | `/run/sandcrab-server`. Not `/run/sandcrab`, which every sandbox uses for `crabd`'s own socket |
| §6.2 | 16 mounts on every backend | 16 on Docker and on libkrun on Apple Silicon (64 measured to boot there). **libkrun on x86_64 boots at most 4** directory mounts (3 with a NIC); `max_mounts` reports the backend's real room |
| §6.1 | Mount points must exist in the image | libkrun's guest root is an overlay, so `crabd init` creates a missing mount point on the scratch layer. No image change. Some system directories are refused as mount targets |
| §8 | The self-test runs as the server's sandbox default | It runs **as the calling peer's uid:gid**, makes its scratch directory through descriptors only, and keeps one report per template and user. A template never tested reports `selftest: null`, read as "not proven" |
| §11 | `nix` for macOS peer credentials | Tokio's `peer_cred` on both platforms; no `nix` in `sandcrab-server` |
| §16 | Twelve open points | Settled; results in §16 |

## Words and scope

**Scope: an MVP.** The common case is one user on a Mac or a Linux box, at most two or three users on
one host. The defaults are safe. There is no multi-tenant machinery. Clients of different uids are kept
apart by ownership (a sandbox belongs to the uid that created it); clients of the same uid share
everything.

Words used here: the **client** is a program that talks to the server through an SDK. The **adopter** is
the first such program. Its **launcher interface** is the one interface through which it starts every
process (`spawn`, `exec`, `sweep`, `reap`, `capabilities`). A **run** is one agent session of the
adopter. A **check step** is one bounded command it runs in isolation. The **adapter** implements the
launcher interface on top of the SDK and lives in the adopter's repository.

## 1. Overview

```text
 client process (TypeScript SDK)                host                       sandbox
┌──────────────────────────────┐   Unix socket  ┌──────────────────┐  crabd transport  ┌─────────────────────┐
│ control connection (JSON-RPC)│◄──────────────►│ sandcrab-server  │◄─────────────────►│ crabd serve         │
│ one connection per process   │◄── records ───►│  one backend     │  process.open     │  └ crabd supervise  │
│ one per forwarded connection │◄──── raw ─────►│  SandboxManager  │  forward.accept   │      └ the program  │
│ dials its own target         │                │                  │                   │ 127.0.0.1:<port>    │
└──────────────────────────────┘                └──────────────────┘                   └─────────────────────┘
```

| Layer | Speaks | Owns |
| --- | --- | --- |
| `crabd` (guest), protocol 0.3 | framed JSON-RPC, plus one dedicated connection per process and per forwarded connection | exact-env argv exec, limits, tree kill, timeouts, output caps, the guest end of forwards |
| `sandcrab-server` (host) | framed JSON-RPC on a Unix socket, process and forward connections beside it | the backend, who may connect, what may be mounted, labels, TTL, relaying |
| SDK (client) | the server API | the process handle, bounded `exec`, forward targets, typed outcomes |

The choices this page makes:

- **Control framing:** the u32 big-endian length prefix `crabd` already uses (§5.1).
- **Process streams:** one dedicated connection per process, from the client to the guest. It carries
  the same small binary records on both hops: stdin, stdin end, stdout, stderr, exit (§3.3, §5.4).
- **Raw bytes** only for forwarded connections between client and server (§4.4).
- **Tree kill:** one supervisor process per exec that is a child subreaper, on every backend (§3.5).
- **Forwards:** the guest announces a connection, the host opens the connection that carries it. The same
  happens again between server and client. Nothing ever connects from guest to host or from server to
  client (§4).
- **Mounts:** a list of allowed roots. The guest path equals the client's path equals the host path (§6).
- **Lifetime:** labels live in the server's memory. A client disconnect destroys nothing. A server
  restart ends every run (§5.7, §7).

## 2. Mapping: the launcher interface to Sandcrab

Where each part is enforced: **G** guest (`crabd`), **S** server, **K** TypeScript SDK, **A** the
adopter's adapter.

### 2.1 Spawn and exec input

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| A label per process, for logs and recovery | `labels` on `sandbox.create` (§7). The adapter puts the label on the sandbox it runs the process in | S |
| argv, no shell | `argv` of `process.open` (§3.2). `argv[0]` is looked up in the `PATH` of the given env when it has no `/` | G |
| The exact environment | `env` of `process.open`. The supervisor starts the program with a cleared environment plus exactly this map (§3.5) | G |
| An absolute working directory | `cwd` of `process.open`. Missing or not a directory is a could-not-start error | G |
| Identity `host` | Nothing. The adapter hands these to the adopter's local launcher | A |
| Identity "run", with an OS user the sandbox launcher ignores | A sandbox. `user` on `sandbox.create` sets the uid:gid inside it (§6.4) | S |
| Folders at the same absolute path, `ro` or `rw`, not nested | `mounts` on `sandbox.create` (§6) | S, backend |
| Folders absent on an exec | The adapter's job: see §14, point 2 | A |
| Network `none` | `network: "disabled"`, no forwards | backend |
| Network "endpoints": loopback ports, optionally backed by a Unix socket of the client | `network: "disabled"` plus `forwards` (§4). The SDK dials the port or the socket, or hands the connection to a handler | G, S, K |
| Network `open` | `network: "unrestricted"` | backend |
| Per-process limits: nproc, data, fsize, nofile | `limits` of `process.open`, set with `setrlimit` before the program starts (§3.5). For nproc on Docker see §3.5 | G |
| OOM-victim flag | `oom_score_adj` of `process.open` | G |
| stderr as a tail, or merged into stdout | `stderr: "tail"` or `"merge"`. Merge is one pipe for both descriptors, so the kernel keeps the order | G |
| Backend-local options (a user-namespace switch) | Ignored by the adapter. User namespaces stay off in a sandbox; the self-test proves it (§8) | A |
| Spawn: input written once, then stdin closed | `STDIN` records, then `STDIN_EOF` | K, G |
| Spawn: stdin kept open | `stdin: "pipe"`, no end until `end()` | K |
| Exec: a required timeout; a kill grace, 0 means force at once | `timeout_ms` and `kill_grace_ms`, timed in the guest (§3.6) | G |
| Exec: input as text, bytes or a readable stream | The SDK writes `STDIN` records from any of them (§10.4) | K |
| Exec: output into a writable stream, with its backpressure | The SDK writes each `STDOUT` record into the stream and stops reading while it is full, which stops the guest (§3.3) | K |
| Exec: output cap, then kill or keep the tail | `output_cap {max_bytes, on_overflow}` (§3.6), or a ring in the SDK (§10.4) | G, K |
| Exec: stderr tail length | `stderr_tail_bytes`. The adapter asks for four bytes per character and trims | G, A |
| Exec: a callback per chunk, tagged stdout or stderr | `stderr: "stream"`: `STDERR` records on the same connection | G, K |
| Exec: an abort signal | The SDK calls `process.cancel` with the exec's kill grace and marks the outcome aborted (§10.4) | K, G |

### 2.2 Handle, exit and outcome

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| A handle returned synchronously | `sandbox.spawn()` returns at once; the connection is opened behind it | K |
| stdout as ordered bytes, one consumer, ends when the output ends | `STDOUT` records, exposed as `AsyncIterable<Uint8Array>` | G, S, K |
| `write`, `end`; writes after the process has gone are dropped | `STDIN` records, `STDIN_EOF`. Errors after exit are swallowed | K |
| `cancel(grace)`: ask, wait, force; idempotent; never rejects; resolves when nothing is left | `process.cancel {grace_ms}` answers after the supervisor reports the tree empty (§3.5). The SDK keeps one promise per handle | G, K |
| `exit` that resolves after the last byte and never rejects | The `EXIT` record is the last record on the connection (§3.3) | G, K |
| `started`, read at once | `started: Promise<boolean>` in the SDK. See §14, point 1 | K, A |
| A record to store for crash recovery (pid and command) | None. Recovery goes by label | A |
| Exit: code, signal, stderr tail (empty with merge) | The `EXIT` record | G |
| Exit: could not start (`ENOENT`, `EACCES`) | Error `SPAWN_FAILED {errno}` (§3.7) | G |
| Exit: backend unavailable, nothing ran | Error `UNAVAILABLE`, or the request was never written (§10.4) | S, K |
| Outcome: timed out, aborted, overflow | `timed_out`, `cancelled`, `overflow` in the `EXIT` record | G, K |

### 2.3 Sweep, reap, capabilities

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| `sweep`: stop everything of an identity, true only when confirmed empty | `sandbox.destroy_by_label`, whose `empty` is true only when every destroy succeeded and a second look finds no match (§7) | S, backend |
| `sweep` rejects for `host`; takes no hold and starts no exec | The adapter rejects before calling Sandcrab. Destroy is a backend call | A, S |
| `reap` by label after a crash | `sandbox.list {labels, created_after}` then `sandbox.destroy`. The adapter maps: destroyed something = stopped, nothing listed = gone, server unreachable = unknown. After a server restart nothing is listed | S, A |
| `capabilities`, read synchronously, with "isolation proven" and a reason | The self-test report of the template the client names (§8). The SDK keeps the last one and offers a synchronous snapshot | S, K |
| A second capability for the adopter's own extra walls | The adapter combines the self-test with its own probes | A |

### 2.4 The nine invariants

| # | Invariant | How it holds |
| --- | --- | --- |
| 1 | Exact env, no shell | The supervisor clears the environment and passes argv to `execve` unparsed (G). Self-test line `exec-env-exact` |
| 2 | Never throws at run time; only a malformed spec throws, before anything starts | The SDK validates the spec synchronously and turns every later failure into an exit or outcome (K) |
| 3 | Nothing outlives the handle | Tree kill by the supervisor (§3.5); the tree dies with its leader; a closed connection stops the tree (G). Exception: a program that kills its supervisor; the exit then says `tree_clean: false` and the client must destroy the sandbox |
| 4 | Output ordered and complete up to the caps; exit after the last byte | One reader per pipe, one connection per process, backpressure end to end (G, S); `EXIT` is the last record |
| 5 | `host` runs on the host | Outside Sandcrab (A) |
| 6 | At most what was declared | Allowed roots and non-nesting (S); network `disabled` plus forwards (backend, G); a part that cannot be enforced shows up in the self-test, not at spawn |
| 7 | Folders at the same absolute path | Guest path = the path the client named (§6.1) |
| 8 | `sweep` takes no hold and starts no exec | See §2.3 |
| 9 | No pid, uid or host path leaves the launcher | The SDK handle exposes no pid. Mount paths are the client's own |

### 2.5 The conformance suite

Each check of the adopter's suite has one test in `ts-contract` (§15); §2.1 and §2.2 name the mechanism
it rests on. Three changes to the suite itself must land before it can pass through a sandbox launcher. See
§14, point 9.

## 3. Protocol 0.3 (`crabd`)

`PROTOCOL_VERSION` becomes `"0.3"` (`crates/sandcrab-protocol/src/lib.rs:74`). Framing, the `auth` frame
and every existing tool are unchanged.

### 3.1 What changes

| Item | 0.2 | 0.3 |
| --- | --- | --- |
| `process.open` | none | New connection-level verb: one process per dedicated connection (§3.2) |
| `process.cancel` | none | New method on a tool connection (§3.4) |
| `forward.listen`, `forward.accept`, `forward.stats` | none | Two connection-level verbs and one method (§4) |
| `fs.identity` | none | New method on a tool connection: `{paths}` → `[{dev, ino}]` (§6.3). Not an LLM tool |
| `hello` result | `crabd_version`, `protocol_version`, `tools`, `default_cwd` | plus `features: string[]` and `forward_ports: u16[]` |
| `hello` with another version | logged as a warning (`crates/sandcrab-guest/src/server.rs:171-179`) | error `-32009 UNSUPPORTED_VERSION {crabd, host}`. A `hello` without a version is still answered |
| Error codes | up to `-32008` (`docs/protocol.md:121-135`) | plus `-32009 UNSUPPORTED_VERSION`, `-32010 SPAWN_FAILED`, `-32011 LIMIT_EXCEEDED`, `-32012 CANCELLED` |
| `init.configure` | see `docs/protocol.md:480` | plus `tmp_exec`, `tmp_size_mb`, `pids_max`, `nofile`, `forward_ports` (§6.5) |
| `stream_event` | reserved, never sent (`docs/protocol.md:67-69`) | removed from the docs. `process.open` replaces it |

**0.2 is refused cleanly.** The host already refuses a `crabd` whose `hello` reports another version
(`crates/sandcrab-core/src/client.rs:457-463`). 0.3 adds the refusal in the other direction. In practice
the two cannot differ: Docker bind-mounts the host's `crabd` into every container
(`crates/sandcrab-backend-docker/src/spec.rs:65-66`, `:306`), and the microVM rootfs cache key includes the
`crabd` digest (AGENTS.md, crate map).

`features` lists what this `crabd` can do: `"process.open"`, `"forward"`, and exactly one of
`"tree_kill:subreaper"` or `"tree_kill:process_group"` (§3.5).

**The shell-string tools stay.** `bash`, `process_start`, `process_poll`, `process_stdin`, `process_kill`
and `process_list` keep their params, results and behaviour: `bash -c`, the daemon's environment plus the
given variables (`crates/sandcrab-guest/src/tools/exec.rs:4-6`, `:68-77`), polled text. They are the LLM
tools. `process.open` is the programmatic interface. It is not in `TOOL_NAMES` and not in `hello.tools`,
like `tunnel.open`. The two do not share process ids.

### 3.2 `process.open`

A fresh connection, `auth` first where a token is set, then one frame:

```rust
/// First frame of a process connection. Unknown fields are rejected, as for every params type.
pub struct ProcessOpenParams {
    /// Chosen by the caller: a UUID. Lets a cancel arrive before the ack.
    pub process_id: String,
    /// argv[0] is the program. Not empty. No element contains NUL.
    pub argv: Vec<String>,
    /// The exact environment. Nothing is added, nothing inherited.
    pub env: BTreeMap<String, String>,
    /// Absolute. Default: the sandbox's default cwd.
    pub cwd: Option<String>,
    pub stdin: StdinMode,                 // "null" (default) | "pipe"
    pub stderr: StderrMode,               // "tail" (default) | "merge" | "stream"
    /// Bytes of stderr kept for the exit with `tail` and `stream`. Default 4096, max 65536.
    pub stderr_tail_bytes: Option<u32>,
    pub limits: Option<ExecLimits>,
    /// 0..=1000. Only raises the score. Default: unchanged.
    pub oom_score_adj: Option<u16>,
    /// Wall-clock bound, timed in the guest. None: no bound.
    pub timeout_ms: Option<u64>,
    /// Grace between ask and force at the bound, at the cap and on a closed connection. Default 500.
    pub kill_grace_ms: Option<u64>,
    pub output_cap: Option<OutputCap>,
}

pub struct ExecLimits {               // each None = inherit the sandbox's limit
    pub nproc: Option<u64>,              // RLIMIT_NPROC
    pub data_bytes: Option<u64>,         // RLIMIT_DATA, at least 32 MiB
    pub fsize_bytes: Option<u64>,        // RLIMIT_FSIZE
    pub nofile: Option<u64>,             // RLIMIT_NOFILE, at least 64
}

pub struct OutputCap {
    pub max_bytes: u64,
    pub on_overflow: Overflow,           // "kill" | "tail"
}

pub struct ProcessOpenAck { pub process_id: String, pub pid: u32 }
```

The ack is sent **after** the program was started, so an ack means it runs. A program that could not be
started gets an error response instead (§3.7), and `crabd` closes the connection. After the ack the
connection leaves frame mode and carries records (§3.3) until `crabd` closes it.

A limit above the sandbox's own hard limit is `-32602` naming the limit. It is never clamped silently.
`tail` with `max_bytes` above 8 MiB is `-32602`: larger tails are computed by the caller from the stream.

### 3.3 Records on a process connection

```text
+---------+----------------------+------------------+
| u8 kind | u32 length, big-end. | `length` bytes   |
+---------+----------------------+------------------+
```

| Kind | Direction | Payload | Max length |
| --- | --- | --- | --- |
| `0x01 STDIN` | towards the guest | bytes for the program's stdin | 64 KiB |
| `0x02 STDIN_EOF` | towards the guest | empty. Closes the program's stdin | 0 |
| `0x11 STDOUT` | from the guest | stdout bytes (and stderr with `merge`) | 64 KiB |
| `0x12 STDERR` | from the guest | stderr bytes, only with `stderr: "stream"` | 64 KiB |
| `0x1F EXIT` | from the guest | JSON `ProcessExit`. Always the last record; the sender closes after it | 256 KiB |

An unknown kind or an oversized length closes the connection, which stops the tree. The same format is
used between client and server (§5.4).

```rust
pub struct ProcessExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Last bytes of stderr, lossy UTF-8, cut at a character boundary. Empty with `merge`.
    pub stderr_tail: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// stdout went over `output_cap.max_bytes`.
    pub overflow: bool,
    /// False: something of the tree could not be stopped. The caller should destroy the sandbox.
    pub tree_clean: bool,
    pub duration_ms: u64,
}
```

**Why records and not raw bytes.** stdin needs an end-of-file that leaves stdout open. On a raw
connection that is a half-close, and neither microVM vsock bridge carries one in both directions
(`docs/protocol.md:449-455`, measured). A five-byte header gives an explicit end-of-file, a tagged stderr
and an in-band exit on every transport. It is not multiplexing: one connection still carries one process,
so the reasons `tunnel.open` rejected multiplexed frames (`docs/protocol.md:408-411`) still hold.

**Order, completeness, backpressure, bounded memory.**

- Each output pipe is read by one task. Each read becomes one record. Nothing reorders within a stream,
  and stdout and stderr records are sent in the order the supervisor read them.
- The reader reads a pipe only after the previous record was written to the connection. A slow receiver
  therefore fills the pipe and blocks the program. Memory per stream is one 64 KiB buffer.
- stdin is the mirror image: the next `STDIN` record is read from the connection only after the previous
  one was written to the program. A program that does not read stdin blocks the sender.
- Cancel does not travel on this connection, so a blocked stdin cannot hold it up (§3.4).
- `EXIT` is written after both output pipes reached EOF. Exit after the last byte holds by construction.
- No decoding: bytes are never treated as lines or as UTF-8. Only `stderr_tail` is text.

**The tree dies with its leader.** When the program (the leader) exits, the supervisor kills whatever
the program left behind, then drains the pipes for at most 2 s, then writes `EXIT` with the leader's
status. A grandchild that kept the pipes open therefore cannot delay the exit. This is what `bash` does
today for its process group (`crates/sandcrab-guest/src/tools/exec.rs:26-28`), extended to the whole
tree.

### 3.4 `process.cancel`

An ordinary method on a tool connection: `{process_id, grace_ms}` (default 5000; 0 = force at once) →
`{was_running, tree_clean}`. Ask, wait, force. It answers when the supervisor has exited, so nothing of
the tree is left. It is idempotent: a second call waits for the same end; an unknown or finished id
answers `was_running: false`.

**A cancel cannot lose a race with the open.** `crabd` puts the `process_id` in its table when it has
read the `process.open` frame, before it starts anything. A cancel for an id that is in the table but has
no supervisor yet is remembered: the supervisor is not started and the open is answered `-32012
CANCELLED`.

A closed process connection is the other way to stop a process: `crabd` treats it as ask, wait
`kill_grace_ms`, force. So a host that dies leaves nothing running.

### 3.5 Tree kill: one supervisor per exec

**Mechanism.** `crabd serve` starts each `process.open` program through a short-lived copy of itself,
`crabd supervise`. The supervisor:

1. reads its spec (below),
2. marks itself not dumpable,
3. makes itself a **child subreaper** (`PR_SET_CHILD_SUBREAPER`),
4. applies `limits` and `oom_score_adj` to itself,
5. starts the program with a cleared environment, the given env, cwd and argv, in its own process group,
6. answers the `process.open` request on the connection (ack or `SPAWN_FAILED`),
7. pumps records until the tree is gone, then writes `EXIT` and exits.

It asks for no signal on its parent's death: a closed connection or a closed control pipe covers that
case, and such a signal follows the thread that started the child, which is wrong under a multi-thread
runtime (`crates/sandcrab-guest/src/main.rs:138-139`).

A subreaper becomes the parent of every descendant whose own parent died. A grandchild that calls
`setsid`, leaves the process group or double-forks is still a descendant of the supervisor, so it stays
findable: no unprivileged process can change who its ancestors are. The tree of an exec is "every process
whose parent chain leads to the supervisor", read from `/proc/<pid>/stat`.

Stopping a tree:

1. **Ask:** `SIGTERM` to the leader's process group and to every other process of the tree.
2. **Wait** for the grace, or until the tree is empty.
3. **Force:** `SIGKILL` to every process of the tree, reap, list again. Repeat until two listings in a
   row are empty, for at most 5 s. A process that forks while it is being killed has its child adopted
   by the supervisor, so the next listing finds it.
4. `tree_clean` is true when the loop ended empty. It is false when the 5 s passed, or a process could
   not be signalled.

**How the supervisor gets its connection and its spec, without `unsafe`.** The workspace forbids
`unsafe_code` (`Cargo.toml:84`), which rules out `pre_exec` hooks
(`crates/sandcrab-guest/src/hardening.rs:10-12` makes the same point). The design needs none:

- `crabd serve` authenticates the connection and reads the `process.open` frame. `read_frame` reads
  exact lengths with no buffered reader (`crates/sandcrab-guest/src/codec.rs:21`), so nothing after the
  frame is consumed.
- As built, it starts `crabd supervise` with three descriptors, all through `Stdio::from(OwnedFd)`:
  - **stdin: the read end of a control pipe.** `crabd serve` writes the spec (the request id and the
    params, env included) as one frame, then keeps the pipe for cancel bytes (`A` for ask, `F` for
    force). The spec, with its env, never appears on a command line, and a cancel is ordered after the
    spec without a signal race. End-of-file on the pipe (`crabd serve` stopped or died) stops the tree.
  - **stdout: the connection itself**, a duplicate of the Unix, TCP or vsock socket. `crabd serve` drops
    its own copy after the start and is not in the data path.
  - **stderr: inherited**, so the supervisor's logs go where `crabd serve`'s go.
- The descriptor handed over is non-blocking (Tokio set it, and the flag lives in the open file
  description). The supervisor keeps it that way and drives it with its own single-thread Tokio runtime.
- The program gets pipes the supervisor creates, or one pipe for both with `merge`.

The design first put the spec in a `memfd` on descriptor 2. The control pipe replaced it because the
supervisor needs a channel for cancel anyway, and one pipe carries both in order.

`crabd serve` keeps a table `process_id → supervisor`. `process.cancel` writes the ask byte, then the
force byte after the grace, on the control pipe. The supervisor exits 0 when the tree is clean and with a
distinct status when something could not be stopped; `crabd serve` reads `tree_clean` from that status.
If the connection closes, the supervisor stops the tree by itself.

**On each backend.**

| | Docker | libkrun and Firecracker guest |
| --- | --- | --- |
| Who `crabd serve` is | An unprivileged process in a container with `cap_drop: ALL` and `no-new-privileges`; `docker-init` is PID 1 (`crates/sandcrab-backend-docker/src/spec.rs:7-8`, `:392-395`) | An unprivileged child of `crabd init`, which is PID 1 and root (`crates/sandcrab-guest/src/hardening.rs:3-8`) |
| Privilege needed | None. `PR_SET_CHILD_SUBREAPER` and `/proc` reads need no capability | None |
| Backstop | Sandbox destroy. `crabd serve` is **not** a subreaper (the design said it would be; it was not built). Processes orphaned by a dead supervisor are re-parented to PID 1 (`docker-init`, which reaps but does not kill) | Sandbox destroy. Orphans go to `crabd init` (PID 1), which reaps but does not kill them |

**Rejected.**

| Mechanism | Why not |
| --- | --- |
| A cgroup per exec with `cgroup.kill` | Docker mounts the container's cgroup filesystem read-only. In the microVM guests `crabd init` seals the cgroup mount read-only on purpose (`crates/sandcrab-guest/src/init/sys.rs:286-293`) |
| A pid namespace per exec | Needs `CAP_SYS_ADMIN` or a user namespace. The sandbox has neither, and the self-test asserts that (§8) |
| The process group alone | A `setsid` escapes it. This is the documented limit of today's `bash` (`docs/protocol.md:197-201`) |

**Fallback, named.** Where `PR_SET_CHILD_SUBREAPER` is refused (a non-Linux development build), the
supervisor falls back to the process group. `hello.features` then says `tree_kill:process_group`, the
server's capabilities repeat it, and the self-test line `tree-kill` fails. It is never silent.

**Known limits.**

- **A program can kill its supervisor.** They run as the same uid. As built, the connection then closes
  without `EXIT`, the SDK reports the process as `lost` with `treeClean: false`, and whatever the program
  started keeps running in the sandbox until the sandbox is destroyed. Nothing in the guest sweeps it up.
  A client must destroy a sandbox whose process reported `tree_clean: false` before it runs anything else
  there. The self-test line `tree-kill` covers the normal case only. Making `crabd serve` a subreaper that
  kills what a dead supervisor leaves is on the backlog (review item M1).
- **`RLIMIT_NPROC` on Docker counts every process of that uid on the host**, in every container, because
  containers share the kernel's per-uid accounting. A client should leave `nproc` out on Docker and set
  the sandbox's `pids_max`, a cgroup limit, instead. The server says which case applies in
  `capabilities.nproc_scope` (`"host_uid"` on Docker, `"sandbox"` in a microVM).

### 3.6 Timeout and output cap, in the guest

Both are enforced by the supervisor, so they hold when the host, the server or the client is gone.

| | Behaviour |
| --- | --- |
| `timeout_ms` | At the bound: stop the tree (ask, `kill_grace_ms`, force). `EXIT` has `timed_out: true` |
| `output_cap`, `kill` | stdout is streamed as usual. When the count would pass `max_bytes`, the record is cut at the cap, the tree is stopped, `EXIT` has `overflow: true`. The receiver gets at most `max_bytes` |
| `output_cap`, `tail` | stdout is not streamed. The supervisor keeps the last `max_bytes` in a ring (at most 8 MiB) and sends them as `STDOUT` records when the program has ended. `overflow` is true when bytes were dropped. The program is not stopped |

Only stdout counts against the cap; with `merge` that includes stderr.

### 3.7 Errors

| Situation | Where it shows | Shape |
| --- | --- | --- |
| The program could not be started | Response to `process.open` | `-32010 SPAWN_FAILED`, `data: {errno: "ENOENT" \| "EACCES" \| "ENOEXEC" \| "ENOTDIR" \| ..., what: "program" \| "cwd"}` |
| Too many processes in this sandbox | Response to `process.open` | `-32011 LIMIT_EXCEEDED`, `data: {what: "processes", limit}` |
| Cancelled before it started | Response to `process.open` | `-32012 CANCELLED` |
| Bad params | Response to `process.open` | `-32602` |
| The sandbox or the backend cannot be reached | Not a `crabd` answer. The host fails to connect | `SandboxError::Unavailable` in `sandcrab-core` (new variant), `UNAVAILABLE` in the server API |

`SPAWN_FAILED` means: the sandbox works, this program does not start, trying again will not help.
Unavailable means: nothing ran, trying again may work. A connection that closes after the request was
written and without an `EXIT` record is a third case, **lost**: the program may have run and its end is
unknown.

Error `data` never contains env values or stdin bytes (§12.4).

### 3.8 Limits

| Limit | Value | Flag |
| --- | --- | --- |
| Processes open at once per sandbox (`process.open`) | 64 | `--max-processes` |
| Record payload | 64 KiB data, 256 KiB `EXIT` | fixed |
| Drain after the leader exited | 2 s | fixed |
| Force loop | 5 s | fixed |

A process connection gives back its `--max-connections` slot after the ack, as a tunnel does
(`crates/sandcrab-guest/src/server.rs:26-27`). The per-connection in-flight cap of 64
(`crates/sandcrab-guest/src/server.rs:127-128`) does not apply to it.

## 4. Forwards

A forward makes one loopback port inside a sandbox reach a target that the **client** chose. The sandbox
keeps network `disabled`.

### 4.1 Path of one connection

```text
program in the sandbox ── connect 127.0.0.1:P ──► crabd serve (listener)
crabd ── notification forward.incoming {conn_id, port} ──► host      (on the forward.listen connection)
host  ── new connection, first frame forward.accept {conn_id} ──► crabd   (then records, both ways)
server ── notification forward.incoming {conn_id, sandbox_id, guest_port} ──► client   (on its control connection)
client ── connects its target (a loopback port, a Unix socket, or an in-process handler)
client ── new connection, first frame forward.attach {conn_id} ──► server   (then raw bytes, both ways)
```

Every connection is opened from the more trusted side towards the less trusted one: host to guest,
client to server. The guest and the server only announce. That keeps what exists today: every guest
connection is opened by the host and starts with the `auth` frame (`docs/protocol.md:71-75`), and the
server never connects to anything. A forward needs no new transport, no change to the libkrun helper
config and no extra Docker mount.

### 4.2 Guest side (`crabd`)

- **A fixed port list, set at spawn.** `crabd serve --forward-port <p>` (repeatable), from the Docker
  command line or from `init.configure` (`forward_ports`). `crabd` binds `127.0.0.1:<p>` (and `[::1]:<p>`
  when the guest has IPv6 loopback) before it answers `hello`. A port that cannot be bound fails the
  start, so the spawn fails. Ports cannot be added later.
- **`forward.listen`** (connection-level, first frame, no params) → `{ports}`. The connection then stays
  in frame mode. `crabd` sends `forward.incoming {conn_id: u64, port}` notifications on it; the host may
  send `forward.reject {conn_id}`. One listen connection at a time; a second is refused with `-32001`.
- **`forward.accept {conn_id}`** (connection-level, first frame of a new connection) → `{port}`, then
  records until either side closes: `0x21 DATA` (bytes, at most 64 KiB) and `0x22 EOF` (this side will
  send no more), both in both directions. A `conn_id` is used once. An unknown one is `-32004`.
- **`forward.stats`** (a method on a tool connection) → `[{port, accepted, refused: {no_listener, cap,
  unclaimed, rejected}}]`. Counters since start.

Records again, for the reason in §3.3: an explicit `EOF` gives a correct half-close in both directions
on every transport. Upload-until-EOF works.

**Fail closed.** Every refusal closes the program's connection and adds one to a counter.

| Situation | What the program in the sandbox sees | Counter |
| --- | --- | --- |
| No `forward.listen` connection | The connection is accepted and closed at once | `no_listener` |
| Over a cap (below) | Closed at accept. Nothing queues | `cap` |
| Announced, but not accepted within 10 s | Closed | `unclaimed` |
| `forward.reject` (the target could not be reached) | Closed | `rejected` |
| The host connection of a forwarded connection drops | Closed | none |
| A port that is not in the list | `ECONNREFUSED` from the kernel: nothing listens | none |

A refusal looks like a flaky network from inside. The counters are how a client finds out why:
`sandbox.get` returns them per forward (§5.3).

**Limits.**

| Limit | Default | Where |
| --- | --- | --- |
| Forwards per sandbox | 16 | `sandcrab-core` validation |
| Connections per forward | 64 | per forward, `max_connections` |
| Connections per sandbox, all forwards | 256 | `crabd --max-forward-conns` |
| Announced and not yet accepted | 32 | `crabd` |
| Buffer per direction | 32 KiB guest, 64 KiB host | fixed, as for tunnels (`docs/protocol.md:435-436`) |

Forward connections do not take `--max-connections` slots.

**As built (`crabd`).**

- `conn_id` is a `u64` counter that starts at 1 per `crabd`, and each id is used once. An unknown or
  used id on `forward.accept` is `-32004`.
- A second `forward.listen` is refused with `-32001` while the first is alive.
- `forward.reject {conn_id}` closes the program's connection. An unknown id is ignored.
- An announced connection survives a dropped listen connection until its 10 s deadline, so the host
  can still accept it after a reconnect.
- The per-forward cap in `crabd` is fixed at 64. A lower `max_connections` is enforced on the host,
  which rejects the excess; those count under `rejected`, not `cap`. Caps count a connection from its
  announcement to the end of its relay.
- `--max-forward-conns` defaults to 256.

### 4.3 Host side (`sandcrab-core`)

```rust
/// Decides where a forwarded connection goes. Called once per connection.
#[async_trait]
pub trait ForwardTarget: Send + Sync {
    async fn connect(&self, sandbox: SandboxId, guest_port: u16) -> io::Result<Box<dyn AsyncStream>>;
}

pub struct ForwardSpec { pub guest_port: u16, pub max_connections: Option<u32> }
```

`ForwardAcceptor` holds the `forward.listen` connection of a sandbox. For each announcement it calls the
target; on success it opens the `forward.accept` connection and relays between the two, translating
end-of-file to `EOF` records and back. On failure it sends `forward.reject`. If the listen connection
drops while the sandbox lives, it reconnects with backoff (50 ms up to 5 s, as built); until then the
guest refuses connections. As built, `serve_forwards` is called once per sandbox, and the target is
resolved per incoming connection. A spawn with forwards is refused when the guest's `hello` lacks the
forward feature or the ports.

`sandcrab-core` ships the trait and the acceptor, and no target. The server implements one target that
passes the connection to the client (§4.4). The self-test implements one in its own process (§8). A
later server-side target (Sandcrab's own egress proxy, Phase 3) plugs in at the same trait.

### 4.4 Server and client

- `sandbox.create` takes `forwards: [{guest_port, max_connections?}]`. The target is **not** part of the
  API. It exists only inside the SDK.
- Forwards of a sandbox are **served** by one control connection: the one that created the sandbox, or
  the last one that called `forward.serve {sandbox_id}`. After a reconnect or a client restart the SDK
  calls `forward.serve` again.
- The server sends `forward.incoming {conn_id, sandbox_id, guest_port}` on the serving connection.
  `conn_id` is 128 random bits, hex.
- The client connects its target **first**. Then it opens a connection to the server socket with the
  first frame `forward.attach {conn_id}`. After the ack the connection carries raw bytes both ways; a
  half-close on the Unix socket is an end-of-file. When the target failed, the client sends
  `forward.reject {conn_id}` on the control connection.
- Not attached within 10 s, no serving connection, or the serving connection is gone: rejected. Fail
  closed. As built, the server waits 9 s for the attach, so its reject reaches the guest inside the
  guest's 10 s deadline.
- As built, only a control connection of the sandbox's owner uid can serve its forwards, and an attach
  is matched only to an announcement made to the same uid. Forwarded connections across all sandboxes
  are capped server-wide (§5.6); one past the cap is rejected.

### 4.5 Backends

| Backend | Forwards |
| --- | --- |
| Docker | Supported, on both transports. Network stays `none`; the container's own loopback carries the listener. No bridge, no published port |
| libkrun | Supported, over the existing tools socket. No TSI, no netstack, no helper change |
| Firecracker | **Refused.** `BackendCapabilities::supports_forwards` is false; the manager fails the spawn with `PolicyUnenforceable("forwards are not supported by the firecracker backend")` |

## 5. Server API

### 5.1 Socket and framing

- A Unix stream socket. No TCP listener.
- **Frames:** u32 big-endian length, then one JSON-RPC 2.0 object, at most 16 MiB. The same codec as
  `crabd` (`docs/protocol.md:26-45`, `sandcrab_protocol::framing`).

The length prefix was chosen over newline-delimited JSON because a connection switches from one frame to
records or raw bytes, and only a length prefix makes that boundary exact: the reader knows where the
answer ends. One codec then serves both protocols (ADR 0001 §2).

### 5.2 Connections

Every connection starts with one frame. Its method decides the kind of connection.

| First frame | Kind | After the answer |
| --- | --- | --- |
| `server.hello` | **control** | Frames: requests, responses, notifications. The SDK uses one |
| `process.spawn` | one **process** | Records (§5.4) |
| `forward.attach` | one **forwarded connection** | Raw bytes both ways |

A connection that sends no first frame within 2 s is closed. Any other first method is `-32600`.

One control connection carries any number of sandboxes. A sandbox does not belong to a connection; the
only per-connection state is "serves the forwards of these sandboxes". As built, a sandbox belongs to the
**peer uid** that created it: every method that names a sandbox, `process.spawn`, `process.cancel` and
`forward.attach` answer as if it did not exist when the caller's uid is not the owner.

### 5.3 Methods on a control connection

Sketches; the real types live in `sandcrab_protocol::api` (§11). Params reject unknown fields, results
accept them, as in `crabd`.

| Method | Params | Result |
| --- | --- | --- |
| `server.hello` | `client: string`, `api_version: string` | `server_version`, `api_version`, `features: string[]`, `backend`, `limits` (§5.11) |
| `server.capabilities` | `template?` | `Capabilities` (below) |
| `server.selftest` | `template`, `max_age_secs?` | `SelftestReport` (§8). Runs a fresh one unless a report for that template younger than `max_age_secs` exists |
| `sandbox.create` | `SandboxCreate` (below) | `SandboxInfo` |
| `sandbox.get` | `sandbox_id` | `SandboxInfo`, with fresh forward counters |
| `sandbox.list` | `labels?: map` (all must match), `created_after?`, `created_before?` (RFC 3339) | `SandboxInfo[]` |
| `sandbox.set_ttl` | `sandbox_id`, `ttl_secs: u64 \| null` | `SandboxInfo` |
| `sandbox.destroy` | `sandbox_id` | `{destroyed: bool}` (false: it was already gone) |
| `sandbox.destroy_by_label` | `labels: map` (not empty) | `{matched, destroyed, remaining: SandboxId[], empty: bool}` (§7) |
| `process.cancel` | `process_id`, `grace_ms?` | `{was_running, tree_clean}` |
| `forward.serve` | `sandbox_id` | `{ports}` |
| `forward.reject` | `conn_id`, `reason?` | `{}` |

Notifications from the server: `forward.incoming {conn_id, sandbox_id, guest_port}`,
`sandbox.destroyed {sandbox_id, reason: "destroyed" | "ttl" | "backend_exit"}`,
`server.shutdown {grace_ms}`.

As built:

- `sandbox.destroyed` goes to every control connection of the owner uid. Reason `destroyed` follows
  `sandbox.destroy` and `sandbox.destroy_by_label`; `ttl` follows a TTL expiry, which the server notices
  by polling every 500 ms (and on every `sandbox.list`), so it can arrive up to about a second late.
  `backend_exit` is in the schema but **never sent**: the core keeps a sandbox whose container or VM
  died until something destroys it, so there is no event to pass on. A race can deliver
  `sandbox.destroyed` twice for one sandbox.
- `server.shutdown {grace_ms: 5000}` goes to every control connection when the server starts to stop.
- `sandbox.list` returns forward counters as zero; `sandbox.get` fetches the real ones from the guest.
- Sandbox ids are UUIDs.

**Not in this API yet.** File transfer calls, a pass-through for the LLM tools, template listing and
verification, a terminal mode and signal delivery for the new process call, a process listing, and
registered workspaces. Each comes with its first caller. Files move through a mounted folder or through a
process's stdin and stdout.

```rust
pub struct SandboxCreate {
    pub template: String,
    /// At most 16. Key [a-z0-9][a-z0-9._-]{0,62}, not starting with "sandcrab.". Value at most 256 bytes.
    pub labels: BTreeMap<String, String>,
    /// None: the server's default. Explicit null: no TTL.
    pub ttl_secs: Option<Option<u64>>,
    /// The path is the client's path, the host path and the guest path (§6).
    pub mounts: Vec<MountSpec>,                    // { path, mode: "ro" | "rw" }
    pub network: NetworkMode,                      // "disabled" (default) | "unrestricted"
    pub forwards: Vec<ForwardSpec>,
    /// "uid:gid". Default: the connecting peer's (§6.4).
    pub user: Option<String>,
    pub resources: Resources,                      // vcpu, memory_mb, disk_mb, pids_max, nofile
    pub tmp_exec: bool,
    pub tmp_size_mb: Option<u64>,
}

pub struct SandboxInfo {
    pub sandbox_id: String,
    pub template: String,
    pub labels: BTreeMap<String, String>,
    pub state: SandboxState,                       // "running" | "destroying"
    pub created_at: String,                        // RFC 3339
    pub expires_at: Option<String>,
    pub backend: String,
    pub mounts: Vec<MountSpec>,
    pub forwards: Vec<ForwardInfo>,                // { guest_port, accepted, refused: { ... } } (§4.2)
    pub forwards_served: bool,
}

pub struct Capabilities {
    pub backend: String,
    pub hardware_isolated: bool,
    pub multi_tenant_safe: bool,
    pub supports_host_dir_mounts: bool,
    pub supports_forwards: bool,
    pub tree_kill: String,                         // "subreaper" | "process_group"
    pub nproc_scope: String,                       // "host_uid" | "sandbox" (§3.5)
    pub mount_identity_checked: bool,              // §6.3
    pub max_mounts: u32,
    /// For the template named in the request. None: no template named, or never run.
    pub selftest: Option<SelftestSummary>,         // { template, ok, reason, age_secs }
}
```

### 5.4 `process.spawn`: one connection per process

```text
client ── new connection ── {"method":"process.spawn","params":{...}} ──► server
client ◄── {"result":{"process_id":"..."}}                       the program runs
client ── STDIN records ... STDIN_EOF ──►
client ◄── STDOUT / STDERR records ... EXIT                      then the server closes
```

Params: `process_id` (client-chosen UUID), `sandbox_id`, then the fields of `ProcessOpenParams` (§3.2).
The server opens `process.open` in the guest with the same id and values, and relays the records of §3.3
unchanged in both directions. It checks each record's kind and length and decodes nothing else.

- The answer comes after the guest's ack. An error answer (`SPAWN_FAILED`, `UNAVAILABLE`, `CANCELLED`,
  ...) closes the connection.
- `EXIT` is the last record. There is no separate call for the exit and nothing is kept after it.
- With `stdin: "null"` any `STDIN` record closes the connection.
- **Backpressure** is the sockets' own. The server reads the next record from one side only after it
  wrote the previous one to the other. A client that does not read stops the program. Server memory is
  one record per direction.
- **A process lives as long as its connection.** If the client closes or loses it, the server closes the
  guest connection and the tree is stopped (§3.4). If the guest connection ends without `EXIT`, the
  server closes the client's without one. A lost control connection does not stop a process.
- **Cancel cannot lose a race with spawn.** The server puts the `process_id` in the sandbox's table when
  it has read the request, before any await. A `process.cancel` for an id that is in the table but not
  yet open in the guest is remembered: if the guest open was not written yet, it is never written and
  the spawn is answered `CANCELLED`; otherwise the cancel is sent to the guest at once, where the same
  rule holds (§3.4). A `process_id` used twice is `CONFLICT`. As built, `CONFLICT` applies only while the
  first process with that id lives; a cancel for an id the server has not seen is remembered for 60 s
  (at most 1024); and a cancel from another uid finds nothing.

A bounded exec is not a server call. The SDK builds it on this path (§10.4).

### 5.5 Error codes

`crabd`'s codes pass through unchanged for `process.spawn` and `process.cancel`
(`docs/protocol.md:121-135`, plus §3.1). The server adds:

| Code | Name | Meaning | `data` |
| --- | --- | --- | --- |
| -32101 | `UNAVAILABLE` | The backend could not start or reach the sandbox: daemon down, VM failed to boot, no `crabd` connection. Nothing ran. Trying again may work | `{detail}` |
| -32102 | `SANDBOX_NOT_FOUND` | No such sandbox | `{sandbox_id}` |
| -32103 | `MOUNT_NOT_ALLOWED` | A mount outside the allowed roots, nested, with a symlink on the way, over the cap, or one that touches the server's own directories | `{path, reason, cap?}` |
| -32104 | `LIMIT_EXCEEDED` | A server limit (sandboxes, processes, resources, connections) | `{what, limit}` |
| -32105 | `POLICY_UNENFORCEABLE` | The backend cannot enforce what was asked (forwards on Firecracker, a user that is not allowed) | `{detail}` |
| -32106 | `TEMPLATE_ERROR` | Template not found, unpinned, or its pin does not match | `{kind, template}` |
| -32107 | `SHUTTING_DOWN` | The server no longer accepts this | `{}` |
| -32108 | `CONFLICT` | A `process_id` used twice, a sandbox that is being destroyed | `{detail}` |
| -32009 | `UNSUPPORTED_VERSION` | `server.hello` with an `api_version` the server does not serve (§5.11) | `{server, client}` |
| -32008 | `UNAUTHORIZED` | The peer's uid is not allowed. One frame, then close | `{}` |

### 5.6 Limits

Server config, with defaults. Over a limit is `LIMIT_EXCEEDED`. Nothing queues.

| Limit | Default (as built) | Set by |
| --- | --- | --- |
| Connections, all kinds | 1024, or a quarter of the soft `RLIMIT_NOFILE` if that is less | fd limit |
| Forwarded connections, all sandboxes together | 4096, or an eighth of the soft `RLIMIT_NOFILE` if that is less | fd limit |
| Requests in flight per control connection | 64; beyond that the server stops reading | fixed |
| Sandboxes | 64 | `[limits] max_sandboxes` |
| Processes per sandbox | 64 (the guest's `--max-processes`) | fixed |
| Mounts per sandbox | 16; 4 on libkrun x86_64 (§6.2) | backend |
| Resources per sandbox | `max_vcpu` 8, `max_memory_mb` 16384, `max_pids` 8192, `max_nofile` 1048576 | fixed |
| `resources.disk_mb` | 32768 | `[limits] max_disk_mb` |
| Default TTL, maximum TTL | 30 min (`crates/sandcrab-core/src/sandbox.rs:37-38`), 24 h | `[limits] default_ttl_secs`, `max_ttl_secs` |
| A first frame | within 2 s | fixed |

**Descriptors (as built).** Each connection and each forwarded connection holds up to two server
descriptors. At start the server raises its soft `RLIMIT_NOFILE` towards the hard limit (at most 65536;
on macOS at least 10240 is always allowed) and sizes the two connection caps from the result, so a busy
server refuses with `LIMIT_EXCEEDED` before `accept` or a dial meets `EMFILE`. The systemd unit sets
`LimitNOFILE=65536`. `max_connections` in `server.hello` is the cap in force.

**Disk (as built).** `resources.disk_mb` sizes the sparse scratch drive on libkrun and Firecracker.
Docker does not enforce it. Without the ceiling a mistaken value could fill the host disk.

Self-test sandboxes are not counted against `max_sandboxes` (at most two per template at a time).

### 5.7 Lifetime: disconnect, restart, shutdown

| Event | Effect |
| --- | --- |
| A control connection is lost | Its requests in flight are dropped. Forwards it served become unserved (fail closed). **No sandbox is destroyed and no process is stopped** |
| A process connection is closed or lost | That process's tree is stopped |
| A forward connection is lost | That one forwarded connection ends |
| The client restarts | Its sandboxes are still there. It lists them by label and destroys them, or serves their forwards again |
| The server stops (`SIGTERM`, `SIGINT`, or its lifeline, §5.10) | New requests get `SHUTTING_DOWN`; `server.shutdown {grace_ms}` is sent; every process is cancelled with a 5 s grace; **every sandbox is destroyed**; the socket is removed |
| The server starts | It removes what a predecessor left (below) |

**A server restart ends every run, on both backends.** The server keeps no database. Labels, TTLs and
the per-sandbox tokens exist only in its memory (`crates/sandcrab-backend-docker/src/backend.rs:531`), so
a new server cannot use an old sandbox and does not try. Clients should drain their runs before a server
upgrade.

What a new server removes at start:

| Backend | Left by a server that died | Removed by |
| --- | --- | --- |
| Docker | Containers keep running, with no process in them (their connections closed) | The server calls the existing `reap_orphans` with `ReapScope::Owned` at start (`crates/sandcrab-sdk/src/lib.rs:545`): same uid, same state directory, owning process gone (`crates/sandcrab-backend-docker/src/reap.rs:9-12`). Today only `doctor --fix` calls it |
| libkrun | Nothing runs: the helper's lifeline ends the VM when its owner dies (`crates/sandcrab-backend-libkrun/src/vm.rs:12`). Run directories remain | The backend removes them when it is constructed (`crates/sandcrab-backend-libkrun/src/backend.rs:335`). No change |

If no server is started again on Docker, the empty containers stay until `sandcrab doctor --fix`.

### 5.8 Access control

All of it is checked before the first frame is read.

| Guard | Rule |
| --- | --- |
| Allowed peer uids | `allow_uids` in the config, or `--allow-uid <uid>` (repeatable). **Absent: only the server's own uid.** Any other uid must be listed. Checked on every accepted connection (`SO_PEERCRED` on Linux, `getpeereid` on macOS). Another peer gets one `UNAUTHORIZED` frame and a close |
| Socket mode | `socket_mode`, default `0600`, owner = the server's user. `socket_group` (optional) sets the group, for a client in another uid with `0660`. The mode is the second layer; the uid list is the first |
| Socket directory | Holds the socket and nothing else. The server refuses to start when the directory is writable by a user other than root and its own (`sandcrab_core::private_dir::check_foreign_dir`, `crates/sandcrab-core/src/private_dir.rs:125`) |
| Stale socket | Replaced only when it is a socket owned by the server's user and nothing answers on it |
| Own files in a mount | Refused (§6.2) |
| Ownership (as built) | A sandbox belongs to the uid that created it. Another admitted uid cannot see or touch it |
| Sandbox user (as built) | The client's own uid and gid unless the config allows others (§6.4) |

There is no token and no TCP listener. A process that can open the socket and passes the uid check has
full control of its own uid's sandboxes, within the config. The per-sandbox token between host and
`crabd` is unchanged.

The uid in `SO_PEERCRED` is the uid as the host kernel sees it. For a containerised client without a
user-namespace remap that is the container's numeric uid.

### 5.9 Configuration

`sandcrab server --config <file>`, TOML. Flags override.

```toml
socket = "/run/sandcrab-server/server.sock"   # its directory holds nothing else
socket_mode = "0660"                 # default "0600"
socket_group = "sandcrab"            # optional
allow_uids = [1500]                  # default: the server's own uid
backend = "docker"                   # required: "docker" | "libkrun" | "firecracker". Never a fallback
state_dir = "/var/lib/sandcrab"
templates_dirs = ["/etc/sandcrab/templates"]
default_template = "base"            # used when a create names no template
selftest_on_start = ["base"]         # templates to test at start; default none
allow_root_user = false              # may a client ask for uid or gid 0 in a sandbox
# allow_sandbox_uids = [[60000, 62999]]  # as built: other uids a client may name (default: none)
# allow_sandbox_gids = [[60000, 62999]]  # as built: the same for gids

[limits]
max_sandboxes = 64
default_ttl_secs = 1800
max_ttl_secs = 86400
max_disk_mb = 32768                  # as built

[[mount_roots]]                      # the allowed roots (§6)
path = "/srv/app-data"
mode = "rw"                          # "rw" (ro and rw allowed) | "ro"
```

### 5.10 Where the server runs

**Linux: a root service.** The server runs as root under systemd. Docker access is root-equivalent
anyway, and root is what lets the server open every allowed root and connect to the `crabd` socket of a
sandbox that runs as another user (the socket is `0660`, owned by the sandbox user,
`crates/sandcrab-guest/src/server.rs:477`).

- The unit gets ordinary hardening: `NoNewPrivileges`, `ProtectSystem=strict` with write access to the
  state and socket directories only, protected kernel tunables and modules, `AF_UNIX` only. As built
  (`packaging/systemd/sandcrab-server.service`), no capability is dropped from the bounding set, read-write
  mount roots are added with `ReadWritePaths=` in a drop-in, and `LimitNOFILE=65536` is set. The unit ran
  on `dev-vps` with these options; they were not tested one by one.
- Because the server is root, its default `allow_uids` is `[0]`. The application's uid must be listed.
- As built, the socket directory is **`/run/sandcrab-server`** (`RuntimeDirectory=sandcrab-server`). It
  must not be `/run/sandcrab`: every sandbox has its own `/run/sandcrab` for `crabd`'s socket, and the
  self-test's "server socket not reachable" probe would find that one and fail.
- A containerised client mounts the socket's **directory**, not the socket file. A bind mount of the file
  pins its inode, and the server replaces the socket at every start. The directory must therefore
  survive a server restart (`RuntimeDirectoryPreserve=yes`, or a directory systemd does not manage).
- The client's uid reaches the socket through `socket_mode = "0660"` and a `socket_group` whose numeric
  gid the container also holds.
- The client's data directories are host directories bound into its container **at the same absolute
  path** (§6.1).

**Mac: a child of the client.** The client starts `sandcrab server` as a child process, on libkrun, with
the socket in a private directory the client made (mode `0700`). There is no launchd service in this
milestone. Client, server and helper are one user, so the default `allow_uids` is already right.

- `--lifeline-stdin`: the server shuts down (§5.7) when its stdin reaches EOF. The client keeps the pipe
  open, so a client that dies takes its server and its sandboxes with it. This is the lifeline the
  libkrun helper already uses towards its owner.
- Templates are built on the Mac with Docker for now: the rootfs pipeline starts from `docker export`. A
  prebuilt rootfs artifact is a later step.

### 5.11 Versions

`server.hello` returns `api_version` as `"major.minor"` and `features: string[]`. The first release is
`"1.0"`. Features name optional abilities (`"forwards"`, `"mounts"`, `"selftest"`, later ones as they
come).

- **The server's rule:** a server at `1.N` serves clients that speak `1.N` and `1.(N-1)`. Nothing such a
  client sends is removed or changes meaning. Another major, or an older minor, is answered
  `UNSUPPORTED_VERSION {server, client}`.
- **The SDK's rule:** it carries the minimum server version it needs. Below that, or on
  `UNSUPPORTED_VERSION`, it starts nothing and reports capabilities as not ok with a reason that names
  both versions, for example "sandcrab server speaks API 1.0, this SDK needs 1.1 or later: upgrade the
  server". A field added after its minimum is sent only when `features` names it.
- Unknown params stay rejected. That is why the two rules above exist.

## 6. Mounts

### 6.1 Model

A mount is `{path, mode}`. `path` is absolute. It is the path the client sees, the path on the host and
the path in the sandbox. There is no path translation: a client in a container must have the directory
bound at the same absolute path as on the host. There is no registry entry and no lease. The client may
change the directory while a sandbox uses it; that is the point.

The server's config lists **allowed roots** (`[[mount_roots]]`, each with a highest mode). No roots: every
mount is refused. At start the server resolves each root with `realpath` (on macOS `/tmp` and `/var` are
symlinks), opens it, and holds the descriptor.

The SDK resolves each mount path with `realpath` before it sends it, and reports the resolved path
(§10.3). The sandbox sees the folder at the **resolved** path, so that is the path a client must use in
`cwd` and env. A path the server receives with a symlink in it is refused.

On macOS, `/tmp` is a symlink to `/private/tmp`. A mount under `/tmp` therefore appears in the sandbox
at `/private/tmp/...` (verified on `mac-m2`). A program that uses the client's `/tmp/...` spelling will
not find it.

**Mount points (as built).** On Docker a bind mount creates its mount point. On libkrun the guest's root
is an overlay (the read-only rootfs image below, the per-sandbox scratch drive above), so `crabd init`
creates a missing mount point on the scratch layer, one component at a time and never through a symlink.
The rootfs image is never written and no image change is needed. libkrun refuses a host mount that
would replace a directory the guest runs from: exactly `/bin`, `/sbin`, `/lib*`, `/usr`, `/etc`, `/var`,
`/opt`, `/mnt`, `/run`, `/tmp`, `/boot`, `/root`, any direct child of `/usr`, and anything inside `/run`
or `/mnt/scratch`. A directory below one of them (`/var/lib/app`, `/tmp/build`) is fine.

Registered workspaces (`WorkspaceRegistry`) stay as they are in `sandcrab-core` and the Rust SDK. The
server API does not expose them in this milestone.

In `sandcrab-core`: `SandboxConfig.host_mounts: Vec<HostMount { path, mode }>`, and on the manager a
`MountPolicy { roots: Vec<MountRoot { path, max_mode }>, protected: Vec<PathBuf> }`. No policy: every
host mount is refused.

A resolved host mount reaches the backend as a `ResolvedMount` whose `host_path` equals its `guest_path`,
with its `root_identity` (`crates/sandcrab-core/src/mounts.rs:21-38`). Backends treat it as they treat a
host-dir workspace.

### 6.2 Resolution

For each `{path, mode}`:

1. **Normalise** lexically: absolute, UTF-8, no NUL, no `..` (`normalize_guest_path`,
   `crates/sandcrab-core/src/mounts.rs:49`). It must pass `validate_mount_target`: not `/`, not under
   `/proc`, `/sys`, `/dev`, `/run/sandcrab`, `/opt/sandcrab` (`mounts.rs:17-18`, `:72`).
2. **Allowed root.** Find the root that is the longest component-wise prefix of the path. None, or `mode`
   above the root's: `MOUNT_NOT_ALLOWED`.
3. **Open beneath the held descriptor**, without following any symlink: `openat2(RESOLVE_BENEATH |
   RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`, or component by component with `O_NOFOLLOW` where
   `openat2` is missing (`crates/sandcrab-core/src/fsx.rs:10-12`, `:180`). A symlink anywhere, or
   something that is not a directory: `MOUNT_NOT_ALLOWED`. The directory must exist; nothing is created.
   `fstat` gives its identity `(dev, ino)`.
4. **No nesting inside one sandbox.** No two paths of the sandbox are equal or nested (the rule of
   `resolve_mounts`, `mounts.rs:92-97`, `:115-121`).
5. **Not the server's own.** The mount is refused when its directory is, contains, or lies inside the
   server socket's directory, the server's state directory, the backend's state and run directories, or
   the template pin store. The check compares `(dev, ino)`, not path strings, so a bind mount or a second
   name for the same directory does not get around it. Without it a client could mount the socket into a
   sandbox, and a program there running as an allowed uid would control the server.
6. **Cap.** At most 16 mounts per sandbox (the core's `MAX_SANDBOX_MOUNTS` and the libkrun helper's
   share limit, `crates/sandcrab-backend-libkrun/src/helper_proto.rs:53`). The error names the cap:
   `MOUNT_NOT_ALLOWED {reason, cap}`. **As built, the cap depends on the backend and the architecture:**

   | Backend | Cap | Why |
   | --- | --- | --- |
   | Docker | 16 | Sandcrab's own cap |
   | libkrun, Apple Silicon (aarch64) | 16 | Sandcrab's own cap. 64 directory mounts plus a NIC were measured to boot on `mac-m2`: devices sit on GIC interrupts there |
   | libkrun, Linux x86_64 | 4 devices: directory mounts, block volumes and a NIC together (so 4 mounts, 3 with `unrestricted` network) | A sandbox without mounts already uses seven virtio-mmio devices on legacy IRQs 5 to 11, which end at 15. A fifth mount made `krun_start_enter` fail with `EINVAL` (libkrun 1.19.8, `kvm-dedicated`). Refused before boot |

   `server.hello` `limits.max_mounts` and `server.capabilities` `max_mounts` report the lower of the core
   cap and the backend's room. Raising the 16 on Apple Silicon is one constant each in the core, the libkrun
   helper protocol and the TypeScript SDK.

There is **no rule about nesting across sandboxes**. Two sandboxes may mount the same folder, or folders
inside each other. The limit, in one sentence: a program in a sandbox with a read-write mount can change
what lies under it, including a path component of a folder another sandbox is about to mount, so clients
that run mutually hostile sandboxes should not nest their folders.

### 6.3 The identity is checked again after start

Both backends finally hand a **path** to something that resolves it again: Docker's bind source is a
string resolved by the Docker daemon (`crates/sandcrab-backend-docker/src/spec.rs:243-260`) after
`verify_mount_roots` ran (`backend.rs:757-764`); libkrun's helper checks the identity immediately before
`krun_add_virtiofs3` (`crates/sandcrab-backend-libkrun/src/workspace.rs:16-21`).

On Docker the manager closes the remaining window with one call. Before the sandbox is handed to the
caller it asks `crabd` once, `fs.identity {paths}`, for `(dev, ino)` of every mount point and compares
each with the identity from step 3. A bind mount shows the host's numbers, because the kernel is shared.
A mismatch destroys the sandbox and fails the create. `capabilities.mount_identity_checked` is true on
Docker on Linux and false on libkrun, where the helper's own check stands.

### 6.4 Users and ownership

| | Docker bind mount | libkrun virtio-fs |
| --- | --- | --- |
| Whose uid touches the host files | The container process's uid, unmapped | The helper's uid, whatever the guest uid is |
| Default sandbox user today | The spec's user, else the server's uid:gid (`crates/sandcrab-backend-docker/src/spec.rs:237-241`) | The spec's user, else the configured default, else the host uid:gid (`crates/sandcrab-backend-libkrun/src/backend.rs:384-387`) |
| What the guest sees as owner | The host's numeric uid | The host's uid; guest-only changes live in xattrs (`LinuxComplete`, `helper_proto.rs:240-247`) |

Rules for the server:

- `user` on `sandbox.create` defaults to the uid:gid of the connecting peer, not the server's. On Linux
  with a root server and a containerised client, files written in the sandbox are then owned exactly as
  the client's own writes. On the Mac all three are the same user.
- **Other users (as built, stricter than designed).** The design accepted any other non-zero uid:gid,
  with "the roots are the bound". They are not: Docker cannot mount a bind mount `nosuid`, so a
  sandbox running as uid X with a read-write mount can leave a setuid file there that the host honours
  as X, and a setgid file of any group it runs as (for example the host's `docker` group, which is
  root-equivalent). So a non-root client's sandbox runs as the client's own uid and gid. Another uid
  needs an `allow_sandbox_uids` range, another gid an `allow_sandbox_gids` range (inclusive `[first,
  last]` pairs, default none). uid or gid 0 always needs `allow_root_user = true`, also as the default
  user of a root peer. A root client may name any other user. A refusal is `POLICY_UNENFORCEABLE`.
- **The sandbox user may differ from the server's.** On Docker's `unix` transport the backend then makes
  the socket directory `0777` and the token `0444` (`crates/sandcrab-backend-docker/src/backend.rs:198-233`),
  and a root server can connect to the `0660` socket. As built and verified: a root server takes any
  sandbox user; a non-root server needs the sandbox user to share its uid or a gid it holds, otherwise
  the spawn is `InvalidConfig`. A sandbox with no mounts whose uid:gid has no passwd entry in the image
  runs a command; `HOME` and the rest come only from the given env.
- On libkrun the files a guest writes appear on the host as the helper's (the server's) uid:gid. A sandbox
  user other than the server's can read them but not write. On a Mac, run the sandbox as the server's uid
  (the default when client and server are one user). Under `/private/tmp` macOS gives a new file its
  directory's group (`wheel`), the same as for the client's own writes there.
- A read-write mount keeps the setuid-bit risk of review item R9
  (`docs/reviews/libkrun-backend-review.md:61`). Keep mount roots on a filesystem mounted `nosuid` where
  you can, and make them directories that other local users cannot traverse.

### 6.5 `/tmp`, pids, nofile

| Option | Today | New |
| --- | --- | --- |
| `/tmp` | tmpfs, `nosuid,nodev,noexec`, 256 MiB (`crates/sandcrab-backend-docker/src/spec.rs:402-408`, `crates/sandcrab-protocol/src/init.rs:50`) | `tmp_exec: true` drops `noexec` only. `tmp_size_mb` sets the size (counts against the sandbox's memory) |
| pids | 1024, fixed (`init.rs:47`; `crates/sandcrab-backend-docker/src/config.rs:104`) | `resources.pids_max`, up to the server's `max_pids`. Docker `pids_limit`; microVM: the pids cgroup `crabd init` creates (`crates/sandcrab-guest/src/init/sys.rs:254`) |
| nofile | Docker's default | `resources.nofile`: a Docker `nofile` ulimit; microVM: `RLIMIT_NOFILE` on `crabd serve` |

In `sandcrab-core`: `ResourceOverrides` gains `pids_max` and `nofile`
(`crates/sandcrab-core/src/policy.rs:219-228`); `SandboxConfig` gains `tmp_exec`, `tmp_size_mb`,
`host_mounts`, `forwards` and `labels` (`crates/sandcrab-core/src/sandbox.rs:47-78`). The microVM guests
get the values through new `init.configure` fields, and `crabd init` passes them to `crabd serve`
(`crates/sandcrab-guest/src/init/command.rs:23`). A backend that does not implement an option refuses the
spawn; it never ignores it.

## 7. Labels, TTL and reaping

- **Labels live in the manager's memory**, set at create and immutable. They are not written to the
  backend object. The existing owner labels on a Docker container (uid, pid, boot id, state directory,
  `crates/sandcrab-backend-docker/src/spec.rs:105-110`) stay; they are how a new server recognises what
  its predecessor left (§5.7).
- **`sandbox.list`** returns the manager's sandboxes. Every given label must match exactly;
  `created_after` and `created_before` go by `created_at`. `created_after` lets a client reap only what
  it made since a point in time when it reuses a label.
- **`sandbox.destroy_by_label`:** select as `list` does; destroy each (which closes its process
  connections, so every tree in it is stopped first); look at the manager again. `empty` is true only
  when every backend destroy succeeded and the second look finds no match. Otherwise `remaining` names
  what is left. A sandbox created with the same labels while the call runs makes `empty` false, which is
  correct.
- **`reap` after a client restart** is `sandbox.list {labels}` plus `sandbox.destroy`. After a server
  restart the list is empty: the answer is "gone".
- **TTL:** the manager's reaper, unchanged (`crates/sandcrab-core/src/manager.rs:357-360`).
  `sandbox.set_ttl` changes the expiry. A long run renews it; the SDK has a keep-alive helper. Only TTL,
  `destroy`, `destroy_by_label` and a server stop end a sandbox. A disconnect does not. As built, a TTL
  expiry sends `sandbox.destroyed {reason: "ttl"}` to the owner (§5.3).
- **As built, every call here is within the caller's uid.** `list` and `destroy_by_label` see only the
  caller's own sandboxes. A sandbox between its backend spawn and its owner record is invisible, so a
  create that races a sweep with the same labels can let the sweep answer `empty: true` (review item m4).

## 8. Self-test

The isolation claim is earned by running probes in a real sandbox, not by reading the backend's static
capabilities.

**Where.** `sandcrab_sdk::selftest::run(&Sandcrab, SelftestConfig { template, .. }) -> SelftestReport`. It
spawns two fresh sandboxes of the **named template**: A with network `disabled`, one forward, one
read-only and one read-write mount in a temporary directory; B plain.

**Keyed by template.** The claim a client repeats is about its own template, so the client names it. The
server keeps the last report per template. `server.selftest {template}` runs or returns one;
`server.capabilities {template}` carries its summary.

**As built.**

- **It runs as the calling peer.** The self-test sandboxes get the peer's uid:gid, the user that peer's
  own sandboxes get by default, so a root server tests what its clients get, not 0:0. Reports are kept
  per template **and** sandbox user; `server.capabilities` returns the one for the caller's user.
  `selftest_on_start` runs as the server's own uid, or on a root server as the first admitted non-root
  uid with its primary group from `/etc/passwd`.
- **`selftest: null` means not proven.** A template never tested for that user has no summary; the SDK
  reads that as isolation not proven.
- **A report that cannot be made is not ok.** No read-write mount root for the scratch, a missing or
  unpinned template, a backend refusal, or uid/gid 0 without `allow_root_user` gives `ok: false`, no
  lines, and a reason that starts "isolation not proven". A whole run has 180 s; past that it is
  inconclusive.
- **The scratch directory is made through descriptors only.** It lives in the first read-write mount
  root, which clients can write. The server may be root, so the directory, its `ro` and `rw` children
  and the canaries are made with `mkdirat` / `openat(O_NOFOLLOW)` relative to held descriptors, and
  handed to the sandbox user with `fchown`. A rename or symlink swap at its name redirects nothing. It is
  removed when the run ends or is dropped. (The first version used path names and followed symlinks;
  the review found that a client could make a root server `chown` any host file. Fixed before release.)
- One run at a time per template.

**No dependency on the image.** Probes run as `crabd probe <name>`, started through `process.open` from
the `crabd` binary that every sandbox already contains (`/opt/sandcrab/crabd` on Docker, `/sbin/crabd`
in a microVM, `crates/sandcrab-backend-libkrun/src/helper_proto.rs:64`). An image without a shell can be
tested. A probe prints one JSON line: what it tried and the `errno` it got.

| Line | The probe does | Passes only when |
| --- | --- | --- |
| `caps-empty` | reads its `/proc/self/status` | `CapInh`, `CapPrm`, `CapEff`, `CapBnd`, `CapAmb` are all zero |
| `no-new-privs` | the same | `NoNewPrivs: 1` |
| `userns-refused` | tries to create a user namespace | the call fails with `EPERM`, `ENOSPC` or `EINVAL` |
| `net-loopback-only` | lists the network interfaces | exactly `lo` |
| `net-external-refused` | TCP connect to two public literal addresses | `ENETUNREACH` or `EHOSTUNREACH`, at once |
| `dns-refused` | UDP send to each resolver in `/etc/resolv.conf` and to a public resolver | `ENETUNREACH`, `EHOSTUNREACH`, or `ECONNREFUSED` for a loopback resolver |
| `gateway-refused` | TCP connect to the usual bridge and user-network gateway addresses | `ENETUNREACH` or `EHOSTUNREACH` |
| `metadata-refused` | TCP connect to the link-local metadata address | `ENETUNREACH` or `EHOSTUNREACH` |
| `forward-reaches-target` | connects to the forward, sends a nonce | the self-test's target got the nonce and the probe got the answer |
| `forward-only-declared` | connects to a loopback port that is not forwarded | `ECONNREFUSED`, and the target saw no other connection |
| `forward-fails-closed` | connects to the forward while the target refuses | the connection is closed with zero bytes delivered |
| `fs-outside-mounts` | opens a canary file beside the mounted directory, and the server's socket path | `ENOENT` for both; the canary inside the mount is readable; a write to the read-only mount is `EROFS` |
| `other-sandbox-unreachable` | A connects to a port B listens on; A lists `/proc` | `ECONNREFUSED`; no process of B is visible |
| `exec-env-exact` | prints its environment | exactly the variables given, and nothing of `crabd`'s |
| `tree-kill` | starts a grandchild in a new session that ignores `SIGTERM`; the self-test cancels with a short grace | `tree_clean: true`, and a second probe finds no process carrying the nonce |

**A timeout is never a pass.** Every probe has a deadline (5 s). A line ends as `pass`, `fail` or
`inconclusive`. A deadline gives `inconclusive`: a silently dropped packet can mean "routed, and filtered
somewhere else today", and a probe that hung or never started also ends in a timeout. Only a refusal with
a specific `errno` shows a wall. `ok` is true only when every required line is `pass`.

`SelftestReport` is `{ok, reason, backend, template, started_at, duration_ms, lines}`. `reason` is one
sentence naming the first line that did not pass. A line is `{id, status, required, detail, evidence}`,
with `evidence` holding what was tried and the `errno`.

| Surface | |
| --- | --- |
| `sandcrab selftest --template <name> [--backend ...]` | Prints the lines; exit status 1 unless `ok` |
| Server start | Runs it for each template in `selftest_on_start`. A failure does not stop the server; the capabilities then say isolation is not proven, and why |
| `server.selftest {template}` | Runs a fresh one, or returns the last for that template and user when it is younger than `max_age_secs`. One runs at a time per template |
| TypeScript SDK | `client.selftest(template)` and a synchronous `client.capabilitiesSnapshot(template)` (§10.3) |

Measured duration of one run: 0.5 s on `mac-m2` (libkrun), 1.1 to 1.4 s on `dev-vps` (Docker), 1.7 s
(libkrun) and 5 s (Docker) on `kvm-dedicated`. The ~62 s seen once through the server was host load and
did not reproduce.

## 9. Bring-your-own-image templates

A template is already a manifest that names an image (`[template] image`, required,
`crates/sandcrab-core/src/template.rs:118-119`). What is new is a template with **no Dockerfile**, whose
image somebody else built.

```toml
# <templates dir>/myapp/sandcrab.toml: the usual manifest, plus one key
[template]
name = "myapp"
version = "1.4.0"
image = "registry.example/myapp:1.4.0"
external = true                            # new: never built by templates/build.sh

[tools]
tiers = ["core"]                           # leave out "process" when the image has no bash
```

**`crabd` injection.**

| Backend | How | Change |
| --- | --- | --- |
| Docker | As today: the host's static `crabd` is bind-mounted read-only at `/opt/sandcrab/crabd` and is the pinned entrypoint, so an image `ENTRYPOINT` cannot wrap it (`crates/sandcrab-backend-docker/src/spec.rs:65-66`, `:306`, `:368-369`) | None |
| libkrun | Through `sandcrab-rootfs`: `docker export` of the image, `crabd` and the mount points added, a read-only ext4 image, cached by image id, arch and `crabd` digest (AGENTS.md, crate map) | The pipeline must accept an image without a Dockerfile in the template directory and must not assume a shell in it. **Not verified** |

**Pinning: the same rule as built templates.** The operator pulls the image, then
`sandcrab template pin myapp` records the local image id host-locally with `source: "adopt"`
(`docs/template-pins.md:55-56`). **An unpinned image is refused** at spawn, as today
(`crates/sandcrab-backend-docker/src/backend.rs:185-194`). Sandcrab does not pull at spawn.
`templates/build.sh` skips `external = true` templates.

**What the image must provide.**

| Requirement | Why |
| --- | --- |
| Linux, the host's architecture (`linux/amd64` or `linux/arm64`) | `crabd` and the kernel |
| Nothing else for `process.open` | `crabd` is a static binary (`crates/sandcrab-backend-docker/src/config.rs:178`); it needs no libc and no shell from the image |
| `bash` on `PATH`, only for the LLM tools `bash` and `process_*` | They run `bash -c` (`crates/sandcrab-guest/src/tools/exec.rs:68-70`) |
| The template's `workdir` exists, or is a mount target | It is the default cwd |
| A world-writable or user-owned `HOME`, if the programs need one | The sandbox user is an arbitrary uid |
| No reliance on `ENTRYPOINT`, `CMD` or an init system | None of them runs |
| Whatever the client's programs need, at the paths the client uses | Program files are not mounts (§14, point 6) |

Image `ENV` and `WORKDIR` do not matter to `process.open`: it takes an exact env and an explicit cwd.

## 10. TypeScript SDK

### 10.1 Package

`sdk/typescript`, package name `@sandcrab/sdk`. Node 26 or later, ESM only, no native code, no runtime
dependency: `node:net`, `node:stream`, `node:crypto`, `node:events`, `node:fs`.

Until the package is on a registry it is consumed as a **built tarball** (`npm pack` output: ESM
JavaScript and declarations, no sources to compile), attached to the release, installed by URL or as a
vendored file with its integrity hash.

Layout: `schema/server-api.schema.json` (copied from the Rust snapshot), `scripts/generate-types.mjs`,
`src/{index,client,framing,sandbox,process,forwards,selftest,errors}.ts`, `src/generated/api.ts`
(generated, committed), `src/testing/fake-server.ts` (exported as `@sandcrab/sdk/testing`), `test/unit`,
`test/e2e`.

### 10.2 Types from the schemas

- The server API types live in `sandcrab_protocol::api` and derive `JsonSchema`, like the tool types do
  today (`crates/sandcrab-protocol/src/schema.rs:27`).
- A snapshot test in `crates/sandcrab-protocol/tests/snapshots/` writes and pins one JSON Schema
  document: `$defs` for every params, result, error data and notification type, and a `methods` table
  (`name → {params, result, kind: "control" | "connection"}`).
- `scripts/generate-types.mjs` turns the copied schema into `src/generated/api.ts` with
  `json-schema-to-typescript` (a dev dependency) and emits a typed `Methods` map.
- CI fails when the copied schema differs from the Rust snapshot, or when regenerating changes the
  committed file.

### 10.3 Public API

```ts
export function connect(o: {
  socketPath: string; clientName?: string; signal?: AbortSignal;
  reconnect?: boolean | { minMs: number; maxMs: number };      // default: backoff from 50 ms to 2 s
}): Promise<SandcrabClient>;

export interface SandcrabClient {
  /** What server.hello answered. */
  readonly server: { version: string; apiVersion: string; features: readonly string[] } | null;
  readonly connected: boolean;
  createSandbox(spec: SandboxSpec, o?: { signal?: AbortSignal }): Promise<Sandbox>;
  /** A handle for a sandbox that exists: after a client restart, or from another process. */
  sandbox(id: string): Sandbox;
  listSandboxes(filter?: { labels?: Labels; createdAfter?: Date; createdBefore?: Date }): Promise<SandboxInfo[]>;
  destroyByLabel(labels: Labels): Promise<{ matched: number; destroyed: number; remaining: string[]; empty: boolean }>;
  capabilities(template?: string): Promise<Capabilities>;
  /** The last capabilities seen for this template, refreshed in the background. Never throws. */
  capabilitiesSnapshot(template: string): { backend: string | null; isolation: { ok: boolean; reason: string | null }; ageMs: number | null };
  selftest(template: string, o?: { maxAgeMs?: number }): Promise<SelftestReport>;
  on(event: "connect" | "disconnect", listener: () => void): this;
  on(event: "shutdown", listener: (e: { graceMs: number }) => void): this;            // as built
  on(event: "sandboxDestroyed", listener: (e: { sandboxId: string; reason: string }) => void): this; // as built
  close(): Promise<void>;
}

export type Labels = Readonly<Record<string, string>>;

export interface SandboxSpec {
  template: string;
  labels?: Labels;
  ttlMs?: number | null;
  mounts?: readonly { path: string; mode: "ro" | "rw" }[];
  network?: "disabled" | "unrestricted";
  forwards?: readonly ForwardSpec[];
  user?: { uid: number; gid: number };
  resources?: { vcpu?: number; memoryMb?: number; diskMb?: number; pidsMax?: number; nofile?: number };
  tmpExec?: boolean;
  tmpSizeMb?: number;                       // as built
}

/** The target stays in the client. The server only learns `guestPort`. */
export interface ForwardSpec {
  guestPort: number;
  maxConnections?: number;
  target:
    | { port: number; host?: "127.0.0.1" | "::1" }   // a loopback TCP port of the client
    | { socketPath: string }                          // a Unix socket of the client
    | { handler(socket: Duplex): void };              // in-process code; no listener needed
}

export interface Sandbox {
  readonly id: string;
  /** `path` is what was asked, `resolvedPath` is what the sandbox sees. */
  readonly mounts: readonly { path: string; resolvedPath: string; mode: "ro" | "rw" }[];
  info(): Promise<SandboxInfo>;
  /** Returns at once. Throws InvalidSpecError for a malformed spec, before anything starts; never otherwise. */
  spawn(spec: SpawnOptions): ProcessHandle;
  /** Rejects only with InvalidSpecError. Every other failure is in the outcome. */
  exec(spec: ExecOptions): Promise<ExecOutput>;
  /** After a client restart: serve the sandbox's existing forwards with these targets. */
  serveForwards(forwards: readonly ForwardSpec[]): Promise<void>;
  setTtl(ttlMs: number | null): Promise<void>;
  /** Renews the TTL on an interval until the returned function is called. */
  keepAlive(ttlMs: number): () => void;
  destroy(): Promise<boolean>;
}

interface ProcessBase {
  argv: readonly string[];
  env: Readonly<Record<string, string>>;
  cwd?: string;
  limits?: { nproc?: number; dataBytes?: number; fsizeBytes?: number; nofile?: number };
  oomScoreAdj?: number;
  stderr?: "tail" | "merge";
  stderrTailBytes?: number;
  killGraceMs?: number;
}

export interface SpawnOptions extends ProcessBase {
  input?: string | Uint8Array;             // written to stdin, which is then closed; not with "open"
  stdin?: "end" | "open";
  timeoutMs?: number;
}

export interface ProcessHandle {
  readonly stdout: AsyncIterable<Uint8Array>;   // ordered bytes, one consumer
  write(data: string | Uint8Array): void;       // never throws
  end(): void;
  /** Ask, wait `graceMs` (default 5000), force. Resolves when nothing is left. Idempotent. Never rejects. */
  cancel(graceMs?: number): Promise<void>;
  cancelled(): boolean;
  readonly started: Promise<boolean>;           // false: it could not be started. Never rejects
  readonly exit: Promise<ProcessExit>;          // after the last byte. Never rejects
}

export interface ProcessExit {
  code: number | null;
  signal: string | null;
  stderrTail: string;
  spawnError: { errno: string; what: "program" | "cwd"; message: string } | null;
  rejected: { code: string; message: string } | null;
  unavailable: boolean;
  lost: boolean;
  timedOut: boolean;
  cancelled: boolean;
  overflow: boolean;
  treeClean: boolean;                      // false: something could not be stopped; destroy the sandbox
}

export interface ExecOptions extends ProcessBase {
  timeoutMs: number;
  input?: string | Uint8Array | Readable;
  output?: Writable;                       // stdout goes here and is not captured
  maxOutputBytes?: number;                 // default 8 MiB
  overflow?: "kill" | "tail";
  onOutput?(chunk: Uint8Array, stream: "stdout" | "stderr"): void;
  signal?: AbortSignal;
}

export interface ExecOutput extends Omit<ProcessExit, "stderrTail" | "cancelled"> {
  stdout: Uint8Array;
  stderr: string;
  aborted: boolean;
}
```

### 10.4 Behaviour that the types do not show

**`spawn` is synchronous.** It validates, creates the handle, and connects in the background.

**How an exit is classified.** `exit` never rejects.

| What happened | The exit says |
| --- | --- |
| The request was never written (no connection to the server, the version check failed), or the server answered `UNAVAILABLE` | `unavailable: true`. Nothing else sets it |
| The server answered `SPAWN_FAILED` | `spawnError` |
| The server answered `CANCELLED` | `cancelled: true`; nothing ran |
| The server answered any other error | `rejected` |
| The connection closed after the request was written and before an `EXIT` record | `lost: true`. This includes a server that went away mid-run. A caller must not retry as if nothing ran |
| An `EXIT` record arrived | Its fields |

**`write` never drops in the middle of a stream.** Data written before the connection is up is kept, all
of it, and sent in order once it is. `write` does nothing only after the process has gone.

**Reading stdout.**

- The SDK holds at most **1 MiB** of unread stdout per process. Beyond that it stops reading the socket,
  and backpressure reaches the program through the server (one 64 KiB record) and the guest (one 64 KiB
  record and the pipe).
- **Contract point:** a caller that awaits `exit` without reading `stdout` waits for ever once the
  program has written more than those buffers hold. Read stdout, or use `exec`.
- After `cancel()`, and after the stdout iterator was abandoned (`break`, `return()`), the SDK reads and
  discards what still arrives, so `EXIT` is reached and `exit` resolves.

**Cancel and abort.**

- `cancel()` before the request was written: nothing is written; the exit says `cancelled`.
- Otherwise the SDK sends `process.cancel` after the spawn request. The server's rule (§5.4) covers a
  process that is not open yet. If the cancel is answered `was_running: false` and the spawn is then
  answered with success, the SDK sends the cancel once more.
- `exec` with a signal that is **already aborted** starts nothing and answers `aborted: true`.
- An abort during `exec` calls `process.cancel` with `killGraceMs` and sets `aborted`, never `timedOut`.

**`exec` is `spawn` plus collection.** There is no second path.

| Options | What the SDK asks the guest for | What the SDK does |
| --- | --- | --- |
| No `output`, no `onOutput`, `overflow: "kill"` | `output_cap {maxOutputBytes, kill}` | Collects stdout |
| No `output`, no `onOutput`, `overflow: "tail"`, cap at most 8 MiB | `output_cap {maxOutputBytes, tail}` | Collects what arrives at the end |
| `onOutput` or `output`, any `overflow` | **Never `tail`.** `kill`: the guest cap. `tail`: no cap | Streams every chunk as it arrives and, for `tail`, keeps the last `maxOutputBytes` in its own ring |
| `overflow: "tail"` above 8 MiB | No cap | Its own ring |
| `onOutput`, stderr not merged | `stderr: "stream"` | Calls `onOutput` per record, tagged; the tail still comes from `EXIT` |

With `output`, each chunk is written to the stream and the SDK waits for `drain` before it reads on.
`exec` resolves after the stream's `finish`. A `Readable` input is sent as `STDIN` records, then
`STDIN_EOF`.

**Forwards.** For every `forward.incoming` the SDK looks up the sandbox's `ForwardSpec` by guest port,
connects the target (or creates the pair for a handler), then attaches (§4.4). An unknown port is
rejected. It never connects anything else.

**Mount paths.** The SDK resolves each mount path with `realpath` and sends the resolved path.
`sandbox.mounts` shows both. A path that does not exist is an `InvalidSpecError`.

**As built.**

- `ProcessExit.signal` uses Node's signal names (`"SIGKILL"`, `"SIGXFSZ"`), mapped from the Linux numbers
  whatever the client's platform.
- A `lost` exit also has `treeClean: false`: nothing is known to be stopped.
- The SDK returns the server's own wire types (snake_case fields) from `info()`, `listSandboxes()`,
  `capabilities()` and `selftest()`; it does not map them.
- `createSandbox` with mounts or forwards, and `selftest`, refuse with `SandcrabError` code
  `UNSUPPORTED_FEATURE` when the server's `features` lack the one needed.
- `stderr: "stream"` is not offered on `spawn`; `exec` uses it when `onOutput` is given. The stream has
  no cap; the tail in `EXIT` is at most 64 KiB.
- The stderr tail counts **bytes**, not characters; it is cut at a UTF-8 character boundary.

### 10.5 Connections and reconnect

- One control connection per client. One connection per process and per forwarded connection, closed
  with it.
- **Version check.** After `server.hello` the SDK compares `api_version` with its minimum (§5.11). When
  the server is too old, `connect` still resolves, nothing can be started (`unavailable`), and
  `capabilitiesSnapshot()` gives the reason with both versions.
- When the control connection drops: requests in flight fail (`sandbox.get`, `sandbox.list` and
  `server.capabilities` are retried once after reconnect; nothing else is). The SDK reconnects with
  backoff and jitter, sends `server.hello`, then `forward.serve` for every sandbox whose forwards it
  holds.
- Processes do not depend on the control connection. If the server itself went away, their connections
  end and their exits say `lost`.
- `capabilitiesSnapshot()` answers from memory. While disconnected it answers
  `{ ok: false, reason: "not connected to the sandcrab server" }`.

### 10.6 Tests

| Layer | What |
| --- | --- |
| Unit, against `testing/fake-server.ts` (a TypeScript server on a temporary Unix socket, scripted per test) | Frames and records split at every byte boundary; an answer and records in one chunk; a connection dropped at each stage of spawn, each mapped to the right exit (§10.4); a reader that stops reading (backpressure reaches the fake's writer); an abandoned iterator; `cancel` twice, before the write, before the answer; a pre-aborted signal; a large `write` before the connection is up; `write` after `end`; `exit` never rejects in any of these; the tail ring with `onOutput`; reconnect and `forward.serve` again; the three forward targets; **a fake server one minor behind** and one a major ahead |
| Contract (`ts-contract`), against a real server on Docker | The cases named in §15 |
| Full e2e (`ts-e2e`), Docker and libkrun | §15 |
| Types | `tsc --noEmit`; the schema and generated-file checks of §10.2 |

The contract tests use the `ts-dev` template, because they use Node as the test program.

## 11. Rust SDK and crate boundaries

**The Rust SDK gets only what the server and the self-test need:** a process handle (records in, records
out, cancel, exit), forwards with a `ForwardTarget`, host mounts, labels, list and destroy by label, the
mount policy on the builder. No ready-made targets and no ergonomic exec layer; those wait for a Rust
caller.

| Piece | Crate | Note |
| --- | --- | --- |
| 0.3 params, results, records, error codes, `init.configure` additions | `sandcrab-protocol` | Pure data |
| Server API types (`api` module), their schema | `sandcrab-protocol` | Pure data. The TypeScript SDK depends on nothing else |
| `crabd supervise`, `crabd probe`, forward listeners, `fs.identity` | `sandcrab-guest` | More `rustix` features |
| Labels, `host_mounts`, `MountPolicy`, forwards config, `ForwardTarget`, `ForwardAcceptor`, the process-stream client | `sandcrab-core` | No backend crate |
| `/tmp` and limit options, same-path binds, the identity check, `--forward-port` | `sandcrab-backend-docker` | |
| Same-path shares, `init.configure` options | `sandcrab-backend-libkrun` | Helper config unchanged |
| The explicit refusal of forwards, host mounts and the new options | `sandcrab-backend-firecracker` | A few lines |
| Process handle, forwards, mounts, labels, self-test | `sandcrab-sdk` | |
| The daemon | `sandcrab-server` (new) | |
| `sandcrab server`, `sandcrab selftest`, `template pin` for external templates, `doctor` checks | `sandcrab-cli` | |

New row for the crate map in AGENTS.md (added in the document step):

| Crate | Role | Allowed Dependencies |
| :--- | :--- | :--- |
| `sandcrab-server` | Host daemon (lib): Unix-socket listener, peer-uid check, JSON-RPC control connections, process and forward connections, allowed mount roots, relaying, removal of a predecessor's sandboxes at start, graceful shutdown. One explicitly chosen backend, never a fallback | `sandcrab-sdk` (backends only through its features, like `sandcrab-cli`), `sandcrab-protocol`, `sandcrab-core`, Tokio, `rustix` (`net`, `fs`), `nix` (macOS peer credentials), `serde`/`serde_json`, `toml`, `tracing`, `thiserror`, `uuid`. **No** backend crate directly, **no** `bollard`, **no** HTTP stack, **no** TCP listener |

As built, the crate depends on `sandcrab-sdk` only for everything Sandcrab (protocol and core through its
re-exports), plus Tokio, `rustix`, `serde`/`serde_json`, `toml`, `tracing`, `thiserror`, `uuid`, `chrono`
and `getrandom`. No `nix`: peer credentials come from Tokio's `UnixStream::peer_cred` (`SO_PEERCRED` on
Linux, `getpeereid` on macOS), with no `unsafe` in Sandcrab code. The AGENTS.md row is the as-built one.

`sandcrab-cli` may depend on `sandcrab-server` (for the `server` command).

## 12. Security

### 12.1 Trust boundaries

```text
client ──(1)── server ──(2)── backend (Docker daemon / libkrun helper) ──(3)── guest
```

| Boundary | Who is trusted with what |
| --- | --- |
| (1) client ↔ server | A client the uid list admits is trusted **within the server's config**: the allowed roots, `allow_root_user`, the sandbox user ranges and the limits bound it. As built, clients of different uids are separated by ownership; clients of the same uid are not |
| (2) server ↔ backend | The server holds the backend's full power: Docker access is root on the host; the libkrun helper runs as the server's user |
| (3) host ↔ guest | The guest is not trusted. Unchanged: every connection is opened by the host; on Docker each starts with the per-sandbox token |

### 12.2 A client that passes the uid check

| Can | Cannot |
| --- | --- |
| Create, list and destroy the sandboxes of its own uid (as built; the design said every sandbox on the server) | See or touch another uid's sandbox, its processes or its forwards; reach a server whose uid list does not name it, even when the socket file is world-writable |
| Mount any directory under the allowed roots, in the allowed mode | Mount anything else; mount through a symlink; mount the server's socket, state or pins |
| Run programs as its own uid:gid, or as a user in `allow_sandbox_uids` / `allow_sandbox_gids` (as built; the design said any non-zero uid:gid) | Run as 0 unless `allow_root_user` is set; run as another user outside the configured ranges |
| Run the self-test, as itself | Make the server follow a symlink while it builds the self-test scratch (fixed before release, §8) |
| Use up to the configured limits | Exceed them |
| Choose where its own forwards go, inside its own process | Make the server open any socket or port |

### 12.3 A compromised sandbox

| Through | Can | Cannot |
| --- | --- | --- |
| Forwards | Connect to the declared loopback ports, up to the caps. Whatever the client's target then allows | Reach a port that was not declared; choose the target; make the server or the host dial anything; open a connection towards the host (announcements only); hold more than 32 announced connections |
| Process streams | Send any bytes as stdout; block by not reading stdin; kill its own supervisor, which the client sees as a lost process with `tree_clean: false`, and keep what it started running until the sandbox is destroyed (§3.5) | Touch the records of another process (one connection each); make the host hold more than one record per direction |
| Forwards, at scale | Open up to 256 connections per sandbox, which hold server descriptors | Exhaust the server's descriptors: forwarded connections are capped server-wide and sized from the fd limit (§5.6) |
| Mounts | Read and write its mounts in the given mode. Change what lies under a read-write mount, also for another sandbox that mounts a folder inside it (§6.2) | Leave its mounts |
| The `crabd` it shares a uid with | Disturb its own sandbox | Authenticate as the host: the token file is gone (`docs/protocol.md:85-86`) and the connections are opened from outside |
| Resources | Use its limits | Fork past `pids_max`; survive `destroy` |

### 12.4 Logging

**Env values and stdin never appear in logs or in error `data`.** This holds for the server, for
`crabd serve` and for the supervisor, at every log level. argv is logged as `argv[0]` only. Env is logged
as a count of variables, if at all. The first frame of a process connection is never logged as a whole.
A client's env may carry credentials; the control pipe (§3.5) keeps it off command lines.
One test per layer runs a process at trace level with a marker value in env and in stdin and searches the
log for it.

### 12.5 No silent downgrades

- The backend is named in the config. A missing one is a start error.
- Firecracker refuses forwards, host mounts and the new options it does not implement.
- The process-group fallback of the tree kill is reported in `hello`, in the capabilities and by a
  failing self-test line.
- A mount identity check that cannot run is reported as `mount_identity_checked: false`.
- No backend reports `multi_tenant_safe`; the capabilities pass that on unchanged.

## 13. Targets

These were targets. All three were met on every host; the full table is in
[reviews/sdk-milestone-results.md](../reviews/sdk-milestone-results.md).

| Target | Value | Measured (TypeScript SDK through the server) |
| --- | --- | --- |
| One forwarded stream, client target to a program in the sandbox | 50 MB/s or more | 150 to 254 MB/s `dev-vps` Docker; 295 MB/s Docker and ~270 MB/s libkrun on `kvm-dedicated`; 385 to 436 MB/s `mac-m2` libkrun |
| 16 forwarded connections in parallel | No reset, no refusal counter moves | Met on all three hosts |
| Warm exec of `["/bin/true"]` on Docker, sequential, timed at the client | p50 at or under 30 ms | p50 8.8 to 9.8 ms `dev-vps`; 6.5 ms `kvm-dedicated`; on libkrun 14.7 ms `kvm-dedicated`, 4.7 to 4.8 ms `mac-m2` |

The connection pool below was not needed and was not built.

One exec costs: one connection to the server, one new `crabd` connection (`auth`, `process.open`), two
process starts in the guest (the supervisor, then the program), one `/proc` listing at the end. Measure
with 20 warm-up execs, then 200 sequential and 8 × 50 concurrent. CI asserts only a loose bound (p95
under 150 ms) so shared runners do not flake. If the per-exec handshake dominates, the fix is a small
pool of authenticated guest connections in `sandcrab-core`; it is not built unless the numbers ask for
it.

## 14. Contract points for the adopter

These shape the adapter and the adopter's own flow. None blocks Sandcrab work.

| # | Point | What the adopter does | What Sandcrab provides |
| --- | --- | --- | --- |
| 1 | **`started` is read synchronously, right after `spawn` returns.** A remote launcher cannot know it yet | The adapter reports "started" as true at once and delivers a failed start in the exit. The suite already accepts this for a launcher that starts a wrapper | `started: Promise<boolean>` and `exit.spawnError` |
| 2 | **Most short execs as a run identity declare no folders, and their labels name no run.** So the adapter cannot find "the run's sandbox" from the call | Those calls gain folders (at least the folder of their cwd) and a grouping key. The adapter keeps a small pool of warm sandboxes keyed by (grouping key, folder set), each with a short idle TTL. A call with no folders and no key runs as `host` or is rejected | Exec into an existing sandbox by id; mounts fixed at create; cheap warm exec (§13); `keepAlive` and TTL |
| 3 | **`sweep` names an OS user, which a sandbox launcher ignores** | The adopter adds the grouping key its spec foresees. The adapter writes it as a label on every sandbox and sweeps with `destroyByLabel`. Without a key the adapter rejects, as it does for `host` | Labels; destroy by label with a confirmed-empty answer (§7) |
| 4 | **The recovery record is null for a sandbox launcher** | Recover by label only | `listSandboxes` by label after a client restart. After a server restart nothing is listed, which the adapter reports as "gone" |
| 5 | **The test database is not among a run's endpoints** | The adopter adds its port to the run's endpoints when a run may use it | One more forward |
| 6 | **Program files are not folders of the spec** | The image holds the runtimes, through a bring-your-own-image template. Programs installed at run time go into a mounted folder, installed by a command that runs in a sandbox, so they are Linux files on both platforms. Other program folders are extra read-only mounts | External templates (§9), read-only mounts |
| 7 | **One sandbox per run, or one per exec** | One per run, reused. One per batch for check steps | A cheap warm exec, `keepAlive` |
| 8 | **File ownership during the transition** | The adapter passes the uid:gid the local launcher would have used as `user` | `user` on create (§6.4). On Docker that gives identical ownership on the shared directory |
| 9 | **Three changes to the conformance suite must land first** | (a) an option for the test program's path **inside the sandbox**, because the client's own interpreter path need not exist there; (b) the two assertions that look up a process id from inside the sandbox in the client's process table are skipped when the launcher has no recovery record, since pid namespaces differ and a collision is likely; (c) a sweep case by grouping key, so the sweep a sandbox launcher really does is tested | `tree_clean` in the exit and the self-test line `tree-kill` are the evidence that replaces (b) |
| 10 | **stderr limits are counted in characters** | The adapter asks for four bytes per character and trims | `stderrTailBytes` up to 64 KiB |
| 11 | **`lost` has no place in the adopter's outcome** | The adapter maps `lost` to a **failed, non-retryable** outcome with a sentence in stderr. Never to "unavailable", which callers read as "nothing ran, try again" | `lost` as its own field (§10.4) |
| 12 | **Awaiting `exit` without reading stdout blocks** once 1 MiB plus the hop buffers are full (§10.4). A local launcher that queues without bound does not show this | The adopter confirms no caller of `spawn` relies on it; the adapter may drain for callers that never read | The stated buffer size; `exec` for callers that only want the result |
| 13 | **`nproc` on Docker is host-wide per uid** (§3.5) | The adapter leaves `nproc` out when `capabilities.nproc_scope` is `"host_uid"` and sets the sandbox's `pidsMax` | The capability |
| 14 | **At most 16 mounts per sandbox** (as built: 4 on libkrun x86_64, §6.2) | The adopter counts a run's folders plus the adapter's extra mounts against the cap | The cap in the capabilities (`max_mounts`), which reports the backend's real room, and in the error |
| 15 | **Same path on both sides** | On Linux the adopter's data directories become host directories bound into its container at the same absolute path (a deployment change and a one-time copy). The container mounts the socket's directory and holds the socket group's gid. On macOS the adapter uses `resolvedPath` of each mount for cwd and env | §5.10, §6.1 |
| 16 | **A server restart ends every run** | The adopter drains before a server upgrade | `server.shutdown` notification; `lost` exits |
| 17 | **Labels that are reused** | `reap` passes its "since" time | `createdAfter` on list |
| 18 | **Getting the SDK** | Consumed as the built tarball (ESM JavaScript and declarations) until the package is on a registry | The tarball in the release (§10.1) |

## 15. Per flow step

The implement steps of the flow, by their ids. Every step ends with `cargo fmt`, `clippy -D warnings`
and the unit tests green. Builds and Docker e2e run on the remote hosts.

This section is the plan as it was dispatched and is kept as written, except where it named a mechanism
that was not built (marked "as built"). Every step is done.

### `protocol-03`

- **Files:** `crates/sandcrab-protocol/src/{lib.rs,process.rs (new),forward.rs (new),record.rs (new),init.rs,error.rs,rpc.rs,schema.rs}`, `tests/`, `tests/snapshots/`, `docs/protocol.md`.
- **Contains:** §3.1 to §3.4 and §3.7 as types; the record codec (encode, incremental decode, limits); forward verbs, records and `forward.stats` (§4.2); `fs.identity`; the `init.configure` fields (§6.5); `hello` features.
- **Acceptance:** `PROTOCOL_VERSION == "0.3"`. Params reject unknown fields. The sixteen tool schemas are byte-identical to their snapshot. `process.open`, `forward.*` and `fs.identity` are not in `TOOL_NAMES`.
- **Tests:** round trips for every new type; record codec with split input, an oversized length, an unknown kind; error code mapping; `init.configure` without the new fields still parses.

### `api-types`

- **Files:** `crates/sandcrab-protocol/src/api.rs` (new), `src/lib.rs`, `tests/snapshots/server-api.schema.json`.
- **Contains:** every type of §5.3 to §5.5 and §8; the `methods` table; the schema document (§10.2); the API version constant and the feature names (§5.11).
- **Acceptance:** the schema snapshot exists and is stable. Params reject unknown fields, results accept them. The process connection reuses the record types of `protocol-03`.
- **Tests:** round trips; the snapshot test.

### `guest-supervisor`

- **Files:** `crates/sandcrab-guest/src/{supervise.rs (new),proctree.rs (new),server.rs,main.rs,context.rs,lib.rs}`, `Cargo.toml` (`rustix` features). `tools/process.rs` and `tools/exec.rs` are **not** changed.
- **First, a spike** on Docker and on libkrun: the connection handed over as stdin and stdout (Unix socket, TCP relay, vsock), the spec through a memory file, the subreaper, a 1 MiB echo, a grandchild in a new session that ignores `SIGTERM`. Settle the first three items of §16 here. Then the real thing. (As built: the connection is stdout only, the spec and cancel go over a control pipe on stdin, §3.5.)
- **Contains:** `crabd supervise` (§3.5): spec handover, exact env and argv, the record pump for stdin, stdout and exit, the tree listing and tree kill. The `process.open` branch in `serve_stream` (`server.rs:570-584`); `process.cancel` with the remembered cancel (§3.4); `fs.identity`; `crabd serve` as a subreaper (as built: not done, see §3.5 Known limits); the `hello` changes; `--max-processes`.
- **Acceptance:** the program's environment is exactly the given map. 1 MiB of mixed bytes arrives identical. `EXIT` comes after the last byte. A grandchild in a new session that ignores `SIGTERM` is gone when `process.cancel` answers, and `tree_clean` is true. A leader that exits leaving a grandchild on its pipes ends within the drain bound. `ENOENT` and `EACCES` give `SPAWN_FAILED`. A cancel sent right after the open frame is never lost. A closed connection stops the tree. `bash` and `process_*` behave as before.
- **Tests:** unit tests for the tree listing on a fixture `/proc`; integration tests that run `crabd serve` on a Unix socket on Linux and drive it (the pattern the tunnel tests use), one per case above; a fork-heavy program against the force loop.

### `guest-caps`

- **Files:** `crates/sandcrab-guest/src/supervise.rs` only.
- **Contains:** rlimits and the OOM score before exec; the guest-side timeout; the output cap with kill and with tail (§3.6); stderr as tail, merged and streamed; the logging rule (§12.4).
- **Acceptance:** each limit is visible to the program (`/proc/self/limits`). A limit above the sandbox's is `-32602`. Timeout and both overflow modes behave as §3.6. Merge keeps the order of interleaved writes. `STDERR` records arrive only with `stream`; the tail is in `EXIT` with `tail` and `stream`. Nothing is left when the call returns. A marker in env and stdin is absent from a trace-level log.
- **Tests:** one integration test per row above, against a local `crabd serve`.

### `guest-forwards`

- **Files:** `crates/sandcrab-guest/src/{forward.rs (new),server.rs,main.rs}`.
- **Contains:** §4.2, with the counters and `forward.stats`.
- **Acceptance:** listeners are bound before `hello` answers. Loopback only. Every row of the fail-closed table, with its counter. The caps. Half-close in both directions.
- **Tests:** against a local `crabd serve`: echo through a forward; upload-until-EOF; no listen connection; reject; unclaimed timeout; cap; a port not in the list refuses.

### `guest-init`

- **Files:** `crates/sandcrab-guest/src/init/{config.rs,command.rs,sys.rs,pid1.rs}`. Nothing outside `init/`.
- **Contains:** the new `init.configure` fields applied in a microVM: `/tmp` with or without `noexec` and its size; `pids_max` for the cgroup; `nofile`; `--forward-port` passed on to `crabd serve`.
- **Acceptance:** a config without the new fields behaves as today. Each field reaches its effect (mount flags, `pids.max`, the limit, the command line).
- **Tests:** unit tests for the validated config and for `serve_command`; the effect is checked in `libkrun-backend`'s e2e.

### `core-config`

- **Files:** `crates/sandcrab-core/src/{sandbox.rs,policy.rs,mounts.rs,hostmount.rs (new),backend.rs,manager.rs,error.rs,testing.rs,lib.rs}`; the capability and `ResolvedSpec` literals in the three backend crates and `sandcrab-bench`; `crates/sandcrab-backend-firecracker/src/backend.rs` (refusals).
- **Contains:** the new `SandboxConfig` and `ResourceOverrides` fields; labels in the manager and their validation; `MountPolicy` and §6.1 to §6.2; `BackendCapabilities::supports_forwards`; manager `list` (labels, creation time) and `destroy_by_label` (§7); `SandboxError::Unavailable`.
- **Acceptance:** the rule matrix of §6.2 is unit-tested without a backend. Two sandboxes may mount nested folders. A sandbox with forwards or host mounts on Firecracker fails with `PolicyUnenforceable`. Labels and filters work through `FakeBackend`. `destroy_by_label` answers `empty: false` when a destroy fails.
- **Tests:** roots (longest prefix, mode cap, a symlinked root resolved at start, a symlink at each depth, `..`, the protected set through a second name, nesting within one sandbox, the cap and its error); label validation; list filters; old serialized configs still parse.

### `core-streams`

- **Files:** `crates/sandcrab-core/src/{process.rs (new),forward.rs (new),client.rs,manager.rs,testing.rs,lib.rs}`.
- **Contains:** `ProcessStream` (opens `process.open`, record reader and writer with backpressure, cancel); `ForwardTarget`, `ForwardAcceptor` (§4.3), `forward.stats`; the `fs.identity` call; `FakeBackend` support for all of them.
- **Acceptance:** a process stream delivers bytes in order and the exit last; dropping it closes the connection. A forward relays both ways with half-close. A failing target rejects. The acceptor reconnects.
- **Tests:** against the fake `crabd` in `testing.rs` and against a real local `crabd serve`.

### `docker-backend`

- **Files:** `crates/sandcrab-backend-docker/src/{spec.rs,backend.rs,config.rs}`.
- **Contains:** `--forward-port` in the command; `tmp_exec`, `tmp_size_mb`, `pids_max`, `nofile`; same-path binds; the identity check after start (§6.3).
- **Acceptance (e2e, ignored tests):** a container with network `none` reaches its forward and nothing else. **A sandbox user different from the server's, with a root server, starts on the `unix` transport and runs a process.** `/tmp` runs a binary only with `tmp_exec`. A swapped mount root fails the create.
- **Tests:** spec unit tests for every new field; the e2e above.

### `libkrun-backend`

- **Files:** `crates/sandcrab-backend-libkrun/src/{backend.rs,workspace.rs,vm.rs}`. `helper_proto.rs` is **not** changed.
- **Contains:** same-path shares through the existing `VirtioFsShare` with identity; the new `init.configure` fields; the forward port list.
- **Acceptance (e2e on macOS and Linux KVM):** forwards work with network `disabled`. A folder at the same path is live from both sides, also one under a symlinked temporary root after resolution. `tmp_exec`, `pids_max` and `nofile` take effect. After the owner dies nothing is left.
- **Tests:** unit tests for the plan; the e2e above.

### `byo-image`

- **Files:** `crates/sandcrab-core/src/template.rs`, `crates/sandcrab-rootfs/src`, `crates/sandcrab-cli/src` (`template pin`), `templates/build.sh`, `docs/template-pins.md`.
- **Contains:** §9.
- **Acceptance:** an external template with a public Node, git and Python image spawns on Docker, and through the rootfs pipeline on libkrun, after `template pin`. Unpinned is refused. `build.sh` skips it.
- **Tests:** manifest parsing; pin; the two spawns.

### `rust-sdk`

- **Files:** `crates/sandcrab-sdk/src/{process.rs (new),forward.rs (new),sandbox.rs,lib.rs}`.
- **Contains:** the list at the top of §11, and nothing more.
- **Acceptance:** a process runs through the handle on Docker: bytes in, bytes out, cancel, exit. Firecracker with a forward is refused with the explicit error.
- **Tests:** unit tests on `FakeBackend`; ignored e2e on Docker.

### `selftest`

- **Files:** `crates/sandcrab-guest/src/{probe.rs (new),main.rs}`, `crates/sandcrab-sdk/src/{selftest.rs (new),lib.rs}`. No CLI file: the command comes with `server-ops`.
- **Contains:** §8.
- **Acceptance:** green on Docker for a named template. Each line fails when its wall is removed in a test double: a probe result with a different `errno`, a timeout, a missing answer. A timeout gives `inconclusive` and `ok: false`.
- **Tests:** the evaluation of each line from recorded probe output (pass, fail, timeout); the e2e run.

### `server-core`

- **Files:** `crates/sandcrab-server/` (new: `lib.rs`, `config.rs`, `listener.rs`, `peer.rs`, `control.rs`, `lifecycle.rs`), `crates/sandcrab-cli/src/{args.rs,main.rs,server.rs (new)}`, `Cargo.toml` (workspace member).
- **Contains:** §5.1 to §5.3 without the process and forward connections; §5.5, §5.6, §5.8, §5.9, §5.11; the allowed roots (server half of §6); labels, list, destroy, TTL (§7); removal of a predecessor's sandboxes at start (§5.7).
- **Acceptance:** with no `allow_uids`, only the server's own uid is admitted. A peer outside the list is refused with the socket at mode `0666`. `socket_mode` and `socket_group` are applied. A bad socket directory stops the start. A mount that contains the socket is refused, also through a second path. `server.hello` returns the version and features; an unsupported client version is `UNSUPPORTED_VERSION`. A client disconnect destroys nothing. A second server started after the first was killed leaves no container of the first (Docker).
- **Tests:** unit tests for config, the peer check and the first-frame dispatch; integration tests with `FakeBackend` over a real Unix socket for every method; e2e on Docker for the restart.

### `server-streams`

- **Files:** `crates/sandcrab-server/src/{process.rs (new),forwards.rs (new),control.rs,listener.rs}`.
- **Contains:** §5.4; §4.4; `forward.serve`, `forward.reject`; the forward counters in `sandbox.get`; the logging rule (§12.4).
- **Acceptance:** records pass unchanged in both directions, with backpressure (a client that stops reading stops the fake guest's writer; server memory stays at one record). `EXIT` is last. A closed process connection stops its tree. A lost control connection stops nothing. A cancel sent before, with and after the spawn request always ends the process, and is answered `CANCELLED` when nothing was opened. A `process_id` used twice is `CONFLICT`. A forward is relayed to the serving connection; unserved, unattached and rejected ones fail closed. A marker in env is absent from a trace-level log.
- **Tests:** integration tests with `FakeBackend` over a real Unix socket. Forwards against a real guest are covered in `ts-e2e`.

### `server-ops`

- **Files:** `crates/sandcrab-server/src/{capabilities.rs (new),shutdown.rs (new),control.rs}`, `crates/sandcrab-cli/src/{args.rs,main.rs,doctor.rs,selftest.rs (new)}`, `docs/hosting.md`.
- **Contains:** `server.capabilities` and `server.selftest` per template (§8), `selftest_on_start`; graceful shutdown and `--lifeline-stdin` (§5.7, §5.10); `sandcrab selftest`; `doctor` checks (the config parses, the socket directory passes, every root exists, the backend is reachable); a hardened systemd unit example for a root server; how to run the server as a child process on a Mac.
- **Acceptance:** shutdown cancels processes, destroys every sandbox and removes the socket. Closing the lifeline does the same. Capabilities for a template never tested say isolation is not proven. The unit starts on the Linux VPS with its hardening on and the socket directory survives a restart.
- **Tests:** integration tests with `FakeBackend`; e2e on Docker for shutdown.

### `ts-sdk`

- **Files:** `sdk/typescript/**` (§10.1), without `test/e2e`.
- **Contains:** §10.
- **Acceptance:** the unit list of §10.6 passes on Node 26, including the fake server one minor behind. No runtime dependency. `tsc` is clean. The schema checks pass. `npm pack` gives a tarball with JavaScript and declarations.
- **Tests:** §10.6, unit layer.

### `ts-contract`

- **Files:** `sdk/typescript/test/e2e/process-contract.test.ts` and its helpers.
- **Contains:** the walking skeleton: the process contract against a real server on Docker, as soon as process streams exist. No mounts, no forwards.
- **Acceptance, one test each:** exact env and argv; 1 MiB of mixed UTF-8 and CRLF through `spawn` and `exec`; stdin end and stdin open; cancel kills a grandchild that ignores `SIGTERM`, and twice is fine; a program that cannot start arrives in the exit; the bound and an abort are told apart, including a signal aborted before the call and one aborted right after it; the cap with kill and with tail; **a tail cap of 100 bytes with `onOutput`: every byte arrives live**; input and output streams; an abandoned stdout reader still gets its exit. The warm-exec numbers of §13 are printed and under the loose bound.

### `ts-e2e`

- **Files:** `sdk/typescript/test/e2e/**`, `.github/workflows/ci.yml`, `scripts/package-release.sh`.
- **Contains:** the full e2e as a matrix over Docker and libkrun (macOS and Linux KVM).
- **Acceptance:** mounts at the same path, `ro` and `rw`, one under a symlinked root. Forwards to a TCP port, a Unix socket and a handler; an undeclared port is refused; the throughput and the 16 parallel connections of §13. A pipeline between two sandboxes with a reader that pauses, server memory bounded. Destroy by label answers `empty` and a list finds nothing. A client killed with `SIGKILL` mid-run, then reaped by a new client. A server restart mid-run: the exit says `lost`, not `unavailable`; the client reconnects; the old sandboxes are gone. 16 sandboxes at once. The self-test is green on both backends. CI runs the SDK's unit tests and type checks. The server binary and the SDK tarball are in the release artifacts.

## 16. Verified and still unverified

Before the build this section listed twelve points to settle first. Their results:

| # | Point | Result |
| --- | --- | --- |
| 1 | A connection duplicated from Tokio is non-blocking, and the flag is shared with the supervisor's copy | **Verified.** The supervisor keeps the descriptor non-blocking and drives it with its own single-thread Tokio runtime; `crabd serve` never reads, writes or shuts down its copy after the handover |
| 2 | `memfd_create`, `setrlimit`, `set_dumpable_behavior` as safe `rustix` functions in a musl `crabd` | **Settled.** `setrlimit` and the dumpable flag are used. `memfd_create` is not: the control pipe replaced the memory file (§3.5). No `unsafe` was added |
| 3 | The handover works on every guest transport | **Verified** on a Unix socket, TCP, the Docker TCP relay and vsock loopback in tests, and in real libkrun guests on Linux KVM and on Apple Silicon (`libkrun-backend` e2e, `live-mac`) |
| 4 | Docker's `unix` transport with a sandbox user different from a root server's | **Verified** (`docker-backend` e2e; `live-linux` ran a root systemd service whose sandboxes run as the client's uid). A non-root server needs the sandbox user to share its uid or a gid it holds |
| 5 | `fs.identity` shows the host's `(dev, ino)` for a bind mount on a Linux Docker host | **Verified.** The manager checks every mount's identity after spawn on Linux; `mount_identity_checked` is true there |
| 6 | A mount point under the guest's tmpfs `/tmp` is mounted in the right order | **Partly verified.** On libkrun mounts under `/private/tmp` work on macOS (`live-mac`); `/tmp` itself is refused as a libkrun mount target. On Docker no test mounts a host directory below `/tmp` |
| 7 | The rootfs pipeline accepts a template with no Dockerfile and an image with no shell | **Verified** (`byo-image`: rootfs test from external images; a Docker e2e with a shell-less external template, refused until pinned) |
| 8 | The user-namespace probe must run before any thread starts | **Verified.** `crabd probe` dispatches before any thread and calls `unshare(CLONE_NEWUSER)` directly. Line `userns-refused` passes on Docker and libkrun |
| 9 | `nix::unistd::getpeereid` on macOS | **Not needed.** Tokio's `peer_cred` covers Linux and macOS |
| 10 | Which capabilities a root server can drop under systemd | **Not settled.** The unit drops none from the bounding set. It ran on `dev-vps` with its other hardening options on (`ProtectSystem=strict`, `ProtectHome=read-only`, `AF_UNIX` only, `NoNewPrivileges`); each option was not tested on its own |
| 11 | The guest clock on libkrun after the Mac slept | **Not verified.** Sleep was out of scope in `live-mac`. libkrun's own `init.krun` on macOS runs a clock worker in every guest for time sync after host sleep; Sandcrab adds nothing |
| 12 | The three targets of §13 | **Met** on every host (§13) |

Still unverified after the milestone (details in
[reviews/sdk-milestone-results.md](../reviews/sdk-milestone-results.md#not-verified)): the macOS Docker
`tcp` transport through the server; `unrestricted` network through the server end to end; the cross-uid
guarantee end to end (unit evidence only); a program that kills its supervisor (a known limit, no test);
the guest clock after a host sleep.
