# Server, TypeScript SDK and forwards: design

Status: design for the `sdk-milestone` flow ([plan](../plans/sdk-milestone.plan.mdx),
[flow](../plans/sdk-milestone.flow.yaml)). Decisions are recorded in
[ADR 0002](../decisions/0002-server-and-sdk.md). Date: 2026-11-26.

This page is the build contract for the flow's implement steps. Facts about today's code carry a
`path:line` reference. Anything not checked against the code is marked **not verified**.

Words used here: the **client** is a program that talks to the server through an SDK. The **adopter** is
the first such program. Its **launcher interface** is the one interface through which it starts every
process (`spawn`, `exec`, `sweep`, `reap`, `capabilities`). A **run** is one agent session of the
adopter. A **check step** is one bounded command it runs in isolation.

- [1. Overview](#1-overview)
- [2. Mapping: the launcher interface to Sandcrab](#2-mapping-the-launcher-interface-to-sandcrab)
- [3. Protocol 0.3 (`crabd`)](#3-protocol-03-crabd)
- [4. Forwards](#4-forwards)
- [5. Server API](#5-server-api)
- [6. Mounts](#6-mounts)
- [7. Sandbox reuse and warm exec](#7-sandbox-reuse-and-warm-exec)
- [8. Labels, TTL and reaping](#8-labels-ttl-and-reaping)
- [9. Self-test](#9-self-test)
- [10. Bring-your-own-image templates](#10-bring-your-own-image-templates)
- [11. TypeScript SDK](#11-typescript-sdk)
- [12. Crate boundaries](#12-crate-boundaries)
- [13. Security analysis](#13-security-analysis)
- [14. Contract points for the adopter](#14-contract-points-for-the-adopter)
- [15. Per flow step](#15-per-flow-step)
- [16. Open questions](#16-open-questions)
- [17. Changes to the flow](#17-changes-to-the-flow)

## 1. Overview

```text
 client process (TypeScript SDK)                host                       sandbox
┌──────────────────────────────┐   Unix socket  ┌──────────────────┐  crabd transport  ┌─────────────────────┐
│ control connection (JSON-RPC)│◄──────────────►│ sandcrab-server  │◄─────────────────►│ crabd serve         │
│ stdio connection per process │◄──── raw ─────►│  one backend     │  process.open     │  └ crabd supervise  │
│ forward connection per conn. │◄──── raw ─────►│  SandboxManager  │  forward.accept   │      └ the program  │
│ dials its own 127.0.0.1:port │                │                  │                   │ 127.0.0.1:<port>    │
└──────────────────────────────┘                └──────────────────┘                   └─────────────────────┘
```

Three layers, each with one job:

| Layer | Speaks | Owns |
| --- | --- | --- |
| `crabd` (guest), protocol 0.3 | framed JSON-RPC, plus one dedicated connection per process and per forwarded connection | exact-env argv exec, limits, tree kill, timeouts, output caps, the guest end of forwards |
| `sandcrab-server` (host) | framed JSON-RPC on a Unix socket, raw byte connections beside it | the backend, who may connect, what may be mounted, labels, TTL, relaying |
| SDK (client) | the server API | the process handle, bounded `exec`, dialling forward targets, typed outcomes |

Summary of the choices this page makes:

- **Control framing:** the u32 big-endian length prefix `crabd` already uses (§5.1).
- **Stream attachment:** a new connection whose first frame names what it is, then raw bytes. This is
  the `tunnel.open` pattern (`docs/protocol.md:398-406`).
- **Between host and guest** a process connection carries small binary records instead of raw bytes,
  because half-close does not survive vsock (§3.3).
- **Tree kill:** one supervisor process per exec that is a child subreaper, on every backend (§3.5).
- **Forwards:** the guest announces a connection, the host opens the connection that carries it. The same
  happens again between server and client. Nothing ever connects from guest to host or from server to
  client (§4).
- **Mounts:** the client names a path, the server maps it through a configured table, the sandbox sees
  it at the path the client named (§6).
- **Labels** are stored on the backend object: container labels on Docker, the run record on libkrun
  (§8).

## 2. Mapping: the launcher interface to Sandcrab

The adapter that implements the adopter's interface lives in the adopter's repository. This table says
which Sandcrab primitive each part rests on and where it is enforced: **G** guest (`crabd`), **S** server,
**K** TypeScript SDK, **A** the adopter's adapter.

### 2.1 Spawn and exec input

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| A label per process, for logs and recovery | `labels` on `sandbox.create` (§8). The adapter puts the label on the sandbox it runs the process in | S |
| argv, no shell | `argv` of `process.open` (§3.2). `argv[0]` is looked up in the `PATH` of the given env when it has no `/` | G |
| The exact environment | `env` of `process.open`. The supervisor starts the program with a cleared environment plus exactly this map (§3.5) | G |
| An absolute working directory, inside a folder or `/` | `cwd` of `process.open`. Missing or not a directory is a could-not-start error | G |
| Identity `host` | Nothing. The adapter hands these to the adopter's local launcher | A |
| Identity "run", with an OS user the sandbox launcher ignores | A sandbox. `user` on `sandbox.create` sets the uid:gid inside it (§6.5) | S |
| Folders at the same absolute path, `ro` or `rw`, not nested | `mounts` on `sandbox.create` (§6) | S, backend |
| Folders absent on an exec | Exec into an existing sandbox: its mounts are the view (§14, point 2) | A |
| Network `none` | `network: "disabled"`, no forwards | backend |
| Network "endpoints": loopback ports, optionally backed by a Unix socket of the client | `network: "disabled"` plus `forwards` (§4). The SDK dials the port, or the socket when one is given | G, S, K |
| Network `open` | `network: "unrestricted"` | backend |
| Per-process limits: nproc, data, fsize, nofile | `limits` of `process.open`, set with `setrlimit` before the program starts (§3.5) | G |
| OOM-victim flag | `oom_score_adj` of `process.open` (the adapter sends 800 for a victim) | G |
| stderr as a tail, or merged into stdout | `stderr: "tail"` or `"merge"` (§3.2). Merge is one pipe for both descriptors, so the kernel keeps the order | G |
| Backend-local options (a user-namespace switch) | Ignored by the adapter. User namespaces stay off in a sandbox; the self-test proves it (§9) | A |
| Spawn: input written once, then stdin closed | SDK writes the input and half-closes the stdio connection; the server sends `STDIN_EOF` | K, S, G |
| Spawn: stdin kept open | `stdin: "pipe"`, no end until `end()` | K |
| Exec: a required timeout | `timeout_ms`, timed in the guest (§3.6) | G |
| Exec: kill grace, 0 means force at once | `kill_grace_ms` | G |
| Exec: input as text, bytes or a readable stream | Unary `sandbox.exec` for small inputs, the stream path otherwise (§5.4) | K |
| Exec: output into a writable stream, with its backpressure | The stdio connection piped into the stream. A slow sink stops the reads, which stops the guest (§3.3) | K |
| Exec: output cap, then kill or keep the tail | `output_cap {max_bytes, on_overflow}` (§3.6) | G |
| Exec: stderr tail length | `stderr_tail_bytes`. The SDK asks for four bytes per character and trims | G, K |
| Exec: a callback per chunk, tagged stdout or stderr | `stderr: "stream"` and a second raw connection (§5.5) | K |
| Exec: an abort signal | SDK calls `process.cancel` with the exec's kill grace and marks the outcome aborted | K, G |

### 2.2 Handle, exit and outcome

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| A handle returned synchronously | `sandbox.spawn()` returns at once; the connection is opened behind it | K |
| stdout as ordered bytes, one consumer, ends when the output ends | The stdio connection, exposed as `AsyncIterable<Uint8Array>` | G, S, K |
| `write`, `end`; writes after the process has gone are dropped | Socket writes, then half-close. Errors after exit are swallowed | K |
| `cancel(grace)`: ask, wait, force; idempotent; never rejects; resolves when nothing is left | `process.cancel {grace_ms}` answers after the supervisor reports the tree empty (§3.5). The SDK keeps one promise per handle | G, K |
| `cancelled()` | A flag in the SDK | K |
| `exit` that resolves after the last byte and never rejects | The SDK resolves it when `process.wait` has answered **and** the stdio connection reached EOF | K |
| `started`, read at once | `started: Promise<boolean>` in the SDK. See §14, point 1 | K, A |
| A record to store for crash recovery (pid and command) | None. Recovery goes by label | A |
| Exit: code, signal | From the supervisor's `EXIT` record | G |
| Exit: stderr tail, empty with merge | `stderr_tail` in the `EXIT` record | G |
| Exit: could not start (`ENOENT`, `EACCES`) | Error `SPAWN_FAILED {errno}` (§3.7) | G |
| Exit: backend unavailable, nothing ran | Error `UNAVAILABLE` (§5.6), or no connection to the server | S, K |
| Outcome: timed out, aborted, overflow | `timed_out`, `cancelled`, `overflow` in the `EXIT` record | G, K |

### 2.3 Sweep, reap, capabilities

| The interface asks for | Sandcrab primitive | Enforced |
| --- | --- | --- |
| `sweep`: stop everything of an identity, true only when confirmed empty | `sandbox.destroy_by_label`, whose `empty` is true only after a fresh backend listing shows nothing (§8.3) | S, backend |
| `sweep` rejects for `host` | The adapter rejects before calling Sandcrab | A |
| `sweep` takes no hold and starts no exec | Destroy is a backend call. No exec is involved | S |
| `reap` by label after a crash | `sandbox.list {labels}` then `sandbox.destroy`. Works after a client restart and after a server restart (§8.4). The adapter maps: destroyed something = stopped, nothing listed = gone, server unreachable = unknown | S, A |
| `capabilities`, read synchronously, with "isolation proven" and a reason | `server.selftest` report (§9). The SDK keeps the last report and offers a synchronous snapshot | S, K |
| A second capability for the adopter's own extra walls | The adapter combines the self-test with its own probes | A |

### 2.4 The nine invariants

| # | Invariant | How it holds |
| --- | --- | --- |
| 1 | Exact env, no shell | The supervisor clears the environment and passes argv to `execve` unparsed (G). Self-test line `exec-env-exact` |
| 2 | Never throws at run time; only a malformed spec throws, before anything starts | The SDK validates the spec synchronously and turns every later failure into an exit or outcome (K) |
| 3 | Nothing outlives the handle | Tree kill by the supervisor (§3.5); the tree dies with its leader; a dropped connection kills the tree (G) |
| 4 | Output ordered and complete up to the caps; exit after the last byte | One pipe, one reader, one connection per process, backpressure end to end (G, S); exit ordering in the SDK (K) |
| 5 | `host` runs on the host | Outside Sandcrab (A) |
| 6 | At most what was declared | Mount table and non-nesting (S); network `disabled` plus forwards (backend, G); a part that cannot be enforced shows up in the self-test, not at spawn |
| 7 | Folders at the same absolute path | Guest path = the path the client named (§6.1) |
| 8 | `sweep` takes no hold and starts no exec | See §2.3 |
| 9 | No pid, uid or host path leaves the launcher | The server API returns no host pid and no host path. Guest pids are not exposed by the SDK handle |

### 2.5 The conformance checks

| Check | What makes it pass |
| --- | --- |
| The process sees exactly its env and its argv, through no shell | Supervisor exec (§3.5). The Linux guest adds no variable |
| A malformed spec throws before anything starts; exec rejects | SDK validation (K): empty argv, relative cwd, timeout not positive |
| A program that cannot start arrives in the exit, never as a throw | `SPAWN_FAILED` becomes the exit's start error. The check accepts a launcher that reports "started" for a wrapper it started |
| stdin end: input written, stdin closed. stdin open: writes, then end; a write after end does nothing | `STDIN` and `STDIN_EOF` records (G), half-close on the stdio connection (S, K) |
| 1 MiB of mixed UTF-8 and CRLF arrives ordered and complete, through spawn and through exec | Binary-safe records and raw bytes; no decoding anywhere on the path. Unary exec carries up to 8 MiB as base64 |
| Cancel stops the tree, also a grandchild that ignores TERM; twice is safe | Subreaper supervisor (§3.5); `process.cancel` is idempotent |
| Exec leaves nothing behind, also a grandchild that kept the pipes; it does not wait for those pipes | When the leader exits the supervisor kills the rest of the tree, so the pipes reach EOF (§3.5) |
| The bound and an abort stop the tree and say which one it was | `timed_out` from the guest timer; `cancelled` from `process.cancel`, mapped to "aborted" by the SDK |
| The output cap stops the tree (kill) or keeps the tail (tail) | `output_cap` (§3.6). Kill delivers at most `max_bytes`; tail delivers exactly the last `max_bytes` |
| Input and output streams; stderr tail and merge; the outcome resolves after the output stream was flushed | Stream path (§5.5); the SDK awaits the sink's `finish` |
| The per-chunk callback gets each stream's chunks, tagged; stderr also stays in the tail | `stderr: "stream"`; the SDK computes the tail from the chunks |
| `host` runs on the host as the client's own user | The adopter's local launcher (A) |
| `sweep` refuses the host and an identity with no grouping; takes no hold | The adapter (A) |

Two checks look up, in the client's own process table, a process id that the command printed inside the
sandbox. A sandbox has its own pid namespace, so the lookup proves nothing and can collide with an
unrelated process. See §14, point 9.

## 3. Protocol 0.3 (`crabd`)

`PROTOCOL_VERSION` becomes `"0.3"` (`crates/sandcrab-protocol/src/lib.rs:74`). Framing, the `auth` frame
and every existing tool are unchanged.

### 3.1 What changes

| Item | 0.2 | 0.3 |
| --- | --- | --- |
| `process.open` | none | New connection-level verb: one process per dedicated connection (§3.2) |
| `process.cancel`, `process.signal` | none | New methods on a tool connection |
| `forward.listen`, `forward.accept` | none | New connection-level verbs (§4) |
| `hello` result | `crabd_version`, `protocol_version`, `tools`, `default_cwd` | plus `features: string[]` and `forward_ports: u16[]` |
| `hello` with another version | logged as a warning (`crates/sandcrab-guest/src/server.rs:171-179`) | error `-32009 UNSUPPORTED_VERSION {crabd, host}`. A `hello` without a version is still answered |
| Error codes | up to `-32008` | plus `-32009 UNSUPPORTED_VERSION`, `-32010 SPAWN_FAILED`, `-32011 LIMIT_EXCEEDED` |
| `init.configure` | see `docs/protocol.md:480` | plus `tmp_exec`, `tmp_size_mb`, `pids_max`, `nofile`, `forward_ports` (§6.6) |
| `stream_event` | reserved, never sent (`docs/protocol.md:67-69`) | removed from the docs. `process.open` replaces it |

**0.2 is refused cleanly.** The host already refuses a `crabd` whose `hello` reports another version
(`crates/sandcrab-core/src/client.rs:457-463`). 0.3 adds the refusal in the other direction. In practice
the two cannot differ: Docker bind-mounts the host's `crabd` into every container
(`crates/sandcrab-backend-docker/src/spec.rs:65-66`, `:306`), and the microVM rootfs cache key includes the
`crabd` digest (AGENTS.md, crate map). An old `crabd` answers the new verbs with `-32601`.

`features` lists what this `crabd` can do: `"process.open"`, `"forward"`, and exactly one of
`"tree_kill:subreaper"` or `"tree_kill:process_group"` (§3.5).

**The shell-string tools stay.** `bash`, `process_start`, `process_poll`, `process_stdin`,
`process_kill` and `process_list` keep their params, results and behaviour: `bash -c`, the daemon's
environment plus the given variables (`crates/sandcrab-guest/src/tools/exec.rs:4-6`, `:68-77`), polled
text. They are the LLM tools. `process.open` is the programmatic interface. It is not in `TOOL_NAMES`
and not in `hello.tools`, like `tunnel.open`. The two do not share process ids.

### 3.2 `process.open`

A fresh connection, `auth` first where a token is set, then one frame:

```rust
/// First frame of a process connection. Unknown fields are rejected, as for every params type.
pub struct ProcessOpenParams {
    /// Chosen by the host: a UUID. Lets the host cancel before the ack arrives.
    pub process_id: String,
    /// argv[0] is the program. Not empty. No element contains NUL.
    pub argv: Vec<String>,
    /// The exact environment. Nothing is added, nothing inherited.
    pub env: BTreeMap<String, String>,
    /// Absolute. Default: the sandbox's default_cwd.
    pub cwd: Option<String>,
    pub stdin: StdinMode,                 // "null" (default) | "pipe"
    pub stderr: StderrMode,               // "tail" (default) | "merge" | "stream"
    /// Bytes of stderr kept for the exit with `tail` and `stream`. Default 4096, max 65536.
    pub stderr_tail_bytes: Option<u32>,
    pub pty: Option<PtySize>,             // { rows, cols }; stdout carries the terminal, stderr is "merge"
    pub limits: Option<ExecLimits>,
    /// 0..=1000. Only raises the score. Default: unchanged.
    pub oom_score_adj: Option<u16>,
    /// Wall-clock bound, timed in the guest. None: no bound.
    pub timeout_ms: Option<u64>,
    /// Grace between ask and force at the bound, at the cap and on a dropped connection. Default 500.
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
| `0x01 STDIN` | host → guest | bytes for the program's stdin | 64 KiB |
| `0x02 STDIN_EOF` | host → guest | empty. Closes the program's stdin | 0 |
| `0x11 STDOUT` | guest → host | stdout bytes (and stderr with `merge`, the terminal with `pty`) | 64 KiB |
| `0x12 STDERR` | guest → host | stderr bytes, only with `stderr: "stream"` | 64 KiB |
| `0x1F EXIT` | guest → host | JSON `ProcessExit`. Always the last record; `crabd` closes after it | 256 KiB |

An unknown kind or an oversized length closes the connection, which kills the tree.

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

**Why records and not raw bytes here.** stdin needs an end-of-file that leaves stdout open. On a raw
connection that is a half-close, and neither microVM vsock bridge carries one in both directions: on
libkrun a host `SHUT_WR` breaks the stream, on Firecracker it becomes a full close and a guest `SHUT_WR`
is never forwarded (`docs/protocol.md:449-455`). A five-byte header gives an explicit end-of-file, a
tagged stderr and an in-band exit on every transport. It is not multiplexing: one connection still
carries one process, so the reasons `tunnel.open` rejected multiplexed frames
(`docs/protocol.md:408-411`) still hold.

**Order, completeness, backpressure, bounded memory.**

- stdout is one pipe read by one task. Each read becomes one `STDOUT` record. Nothing reorders.
- The reader reads the pipe only after the previous record was written to the connection. A slow host
  therefore fills the pipe and blocks the program. Memory per stream is one 64 KiB buffer.
- stdin is the mirror image: the next `STDIN` record is read from the connection only after the previous
  one was written to the program. A program that does not read stdin blocks the host's writes.
- Cancel does not travel on this connection, so a blocked stdin cannot hold it up (§3.4).
- `EXIT` is written after both output pipes reached EOF. Exit after the last byte holds by construction.
- No decoding: bytes are never treated as lines or as UTF-8. Only `stderr_tail` is text.

**The tree dies with its leader.** When the program (the leader) exits, the supervisor kills whatever
the program left behind, then drains the pipes for at most 2 s, then writes `EXIT` with the leader's
status. A grandchild that kept the pipes open therefore cannot delay the exit. This is what `bash` does
today for its process group (`crates/sandcrab-guest/src/tools/exec.rs:26-28`), extended to the whole
tree.

### 3.4 `process.cancel` and `process.signal`

Ordinary methods on a tool connection.

| Method | Params | Result | |
| --- | --- | --- | --- |
| `process.cancel` | `process_id`, `grace_ms` (default 5000; 0 = force at once) | `{was_running, tree_clean}` | Ask, wait, force. Answers when the supervisor has exited, so nothing of the tree is left. Idempotent: a second call waits for the same end; an unknown or finished id answers `was_running: false` |
| `process.signal` | `process_id`, `signal` (`TERM`, `INT`, `HUP`, `USR1`, `USR2`) | `{delivered}` | Sent to the leader only. For programs that take a signal as input |

A closed process connection is the third way to stop a process: `crabd` treats it as ask, wait
`kill_grace_ms`, force. So a host that dies leaves nothing running.

### 3.5 Tree kill: one supervisor per exec

**Mechanism.** `crabd serve` starts each `process.open` program through a short-lived copy of itself,
`crabd supervise`. The supervisor:

1. reads its spec (below),
2. marks itself not dumpable and asks for a signal when its parent dies,
3. makes itself a **child subreaper** (`PR_SET_CHILD_SUBREAPER`),
4. applies `limits` and `oom_score_adj` to itself,
5. starts the program with a cleared environment, the given env, cwd and argv, in its own process group,
6. answers the `process.open` request on the connection (ack or `SPAWN_FAILED`),
7. pumps records until the tree is gone, then writes `EXIT` and exits.

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
`unsafe_code` (`Cargo.toml:84`), which rules out `pre_exec` hooks and raw descriptors above 2
(`crates/sandcrab-guest/src/hardening.rs:10-12` makes the same point). The design needs neither:

- `crabd serve` authenticates the connection and reads the `process.open` frame. Frames are length
  prefixed, so it reads exactly that frame and nothing after it.
- It writes the spec to an anonymous in-memory file (`memfd_create`) and starts `crabd supervise` with
  the **connection as stdin and stdout** and the **memory file as stderr**. All three go through
  `Stdio::from(OwnedFd)`.
- The supervisor reads the spec from descriptor 2, then talks records with the host directly on
  descriptors 0 and 1. `crabd serve` is not in the data path.
- The program gets pipes the supervisor creates (or one pipe for both with `merge`, or a PTY).

`crabd serve` keeps a table `process_id → supervisor pid`. `process.cancel` is two signals from
`crabd serve` to the supervisor: "ask" now, "force" after the grace. The supervisor's exit status tells
`crabd serve` whether the tree was clean. If the connection closes, or `crabd serve` dies, the supervisor
stops the tree by itself.

**On each backend.**

| | Docker | libkrun and Firecracker guest |
| --- | --- | --- |
| Who `crabd serve` is | An unprivileged process in a container with `cap_drop: ALL` and `no-new-privileges`; `docker-init` is PID 1 (`crates/sandcrab-backend-docker/src/spec.rs:7-8`, `:392-395`) | An unprivileged child of `crabd init`, which is PID 1 and root (`crates/sandcrab-guest/src/hardening.rs:3-8`) |
| Privilege needed | None. `PR_SET_CHILD_SUBREAPER` and `/proc` reads need no capability and are not affected by `no_new_privs` | None |
| Where an orphan would go without the supervisor | To `docker-init` | To `crabd init` |
| Backstop | `crabd serve` is a subreaper too. If a supervisor dies without finishing, `crabd serve` kills the processes it adopts as a result. After that: sandbox destroy | The same |

**Rejected.**

| Mechanism | Why not |
| --- | --- |
| A cgroup per exec with `cgroup.kill` | Docker mounts the container's cgroup filesystem read-only, and the container has no capability to change that. In the microVM guests `crabd init` seals the cgroup mount read-only on purpose once `crabd serve` is placed (both in `crates/sandcrab-guest/src/init/sys.rs:286-293`). Opening a writable subtree would weaken the pids cap for a uid-0 sandbox user |
| A pid namespace per exec | Needs `CAP_SYS_ADMIN` or a user namespace. The sandbox has neither, and the self-test asserts that it has neither (§9) |
| The process group alone | A `setsid` escapes it. This is the documented limit of today's `bash` (`docs/protocol.md:197-201`) |

**Fallback, named.** Where `PR_SET_CHILD_SUBREAPER` is refused (a non-Linux development build), the
supervisor falls back to the process group. `hello.features` then says `tree_kill:process_group`, the
server's capabilities repeat it, and the self-test line `tree-kill` fails. It is never silent.

**Known limits.**

- The program and its supervisor run as the same uid, so a program can kill its supervisor. The host
  then sees the connection close without `EXIT`; the SDK reports the process as lost; `crabd serve` kills
  what it adopted. The sandbox stays the boundary.
- `RLIMIT_NPROC` counts per real uid. On Docker, containers share the host kernel's accounting, so the
  count covers every process of that uid on the host, in every sandbox. The per-sandbox `pids_max`
  (a cgroup limit) is the dependable cap on Docker. See §16, question 4.

### 3.6 Timeout and output cap, in the guest

Both are enforced by the supervisor, so they hold when the host, the server or the client is gone.

| | Behaviour |
| --- | --- |
| `timeout_ms` | At the bound: stop the tree (ask, `kill_grace_ms`, force). `EXIT` has `timed_out: true` |
| `output_cap`, `kill` | stdout is streamed as usual. When the count would pass `max_bytes`, the record is cut at the cap, the tree is stopped, `EXIT` has `overflow: true`. The host receives at most `max_bytes` |
| `output_cap`, `tail` | stdout is not streamed. The supervisor keeps the last `max_bytes` in a ring (at most 8 MiB) and sends them as `STDOUT` records when the program has ended. `overflow` is true when bytes were dropped. The program is not stopped |

Only stdout counts against the cap; with `merge` that includes stderr.

### 3.7 Errors: could not start vs. unavailable

| Situation | Where it shows | Shape |
| --- | --- | --- |
| The program could not be started | Response to `process.open` | `-32010 SPAWN_FAILED`, `data: {errno: "ENOENT" \| "EACCES" \| "ENOEXEC" \| "ENOTDIR" \| ..., what: "program" \| "cwd", detail}` |
| Too many processes in this sandbox | Response to `process.open` | `-32011 LIMIT_EXCEEDED`, `data: {what: "processes", limit}` |
| Bad params | Response to `process.open` | `-32602` |
| The sandbox or the backend cannot be reached | Not a `crabd` answer. The host fails to connect or loses the connection | `SandboxError::Unavailable` in `sandcrab-core` (new variant), `UNAVAILABLE` in the server API (§5.6) |

`SPAWN_FAILED` means: the sandbox works, this program does not start, trying again will not help.
Unavailable means: nothing ran, trying again may work. A connection that closes after the ack without an
`EXIT` record is a third case, **lost**: the program ran and its end is unknown.

### 3.8 Limits

| Limit | Value | Flag |
| --- | --- | --- |
| Processes open at once per sandbox (`process.open`) | 64 | `--max-processes` |
| Record payload | 64 KiB data, 256 KiB `EXIT` | fixed |
| Drain after the leader exited | 2 s | fixed |
| Force loop | 5 s | fixed |

A process connection gives back its `--max-connections` slot after the ack, as a tunnel does
(`crates/sandcrab-guest/src/server.rs:26-27`). The per-connection `--max-inflight` of 64
(`crates/sandcrab-guest/src/server.rs:127-128`) does not apply to it.

## 4. Forwards

A forward makes one loopback port inside a sandbox reach a target that the **client** chose and dials.
The sandbox keeps network `disabled`.

### 4.1 Path of one connection

```text
program in the sandbox ── connect 127.0.0.1:P ──► crabd serve (listener)
crabd ── notification forward.incoming {conn_id, port} ──► host      (on the forward.listen connection)
host  ── new connection, first frame forward.accept {conn_id} ──► crabd   (then records, both ways)
server ── notification forward.incoming {conn_id, sandbox_id, guest_port} ──► client   (on its control connection)
client ── dials its target (127.0.0.1:port or a Unix socket)
client ── new connection, first frame forward.attach {conn_id} ──► server   (then raw bytes, both ways)
```

Every connection is opened from the more trusted side towards the less trusted one: host to guest,
client to server. The guest and the server only announce. That keeps what exists today: every guest
connection is opened by the host and starts with the `auth` frame (`docs/protocol.md:71-75`), and the
server never connects to anything.

It also means a forward needs no new transport: no vsock port for guest-initiated connections, no
change to the libkrun helper config, no extra Docker mount. It works over the Docker relay transport as
well.

### 4.2 Guest side (`crabd`)

- **A fixed port list, set at spawn.** `crabd serve --forward-port <p>` (repeatable), from the Docker
  command line or from `init.configure` (`forward_ports`). `crabd` binds `127.0.0.1:<p>` (and `[::1]:<p>`
  when the guest has IPv6 loopback) before it answers `hello`. A port that cannot be bound fails the
  start, so the spawn fails. Ports cannot be added later.
- **`forward.listen`** (connection-level, first frame, no params) → `{ports}`. The connection then stays
  in frame mode. `crabd` sends `forward.incoming {conn_id: u64, port}` notifications on it; the host may
  send `forward.reject {conn_id}`. One listen connection at a time; a second is refused with `-32001`.
- **`forward.accept {conn_id}`** (connection-level, first frame of a new connection) → `{port}`, then
  records until either side closes:

  | Kind | Direction | Payload |
  | --- | --- | --- |
  | `0x21 DATA` | both | bytes, at most 64 KiB |
  | `0x22 EOF` | both | empty: this side will send no more |

  A `conn_id` is used once. An unknown one is `-32004`.

Records again, for the reason in §3.3: an explicit `EOF` gives a correct half-close in both directions
on every transport. The limits of `tunnel.open` over vsock (`docs/protocol.md:457-468`) do not apply to
forwards. Upload-until-EOF works.

**Fail closed.**

| Situation | What the program in the sandbox sees |
| --- | --- |
| No `forward.listen` connection | The connection is accepted and closed at once |
| Announced, but not accepted within 10 s | Closed |
| `forward.reject` (the target could not be dialled) | Closed |
| Over a cap (below) | Closed at accept. Nothing queues |
| The host connection of a forwarded connection drops | Closed |
| A port that is not in the list | `ECONNREFUSED` from the kernel: nothing listens |

**Limits.**

| Limit | Default | Where |
| --- | --- | --- |
| Forwards per sandbox | 16 | `sandcrab-core` validation |
| Connections per forward | 64 | per forward, `max_connections` |
| Connections per sandbox, all forwards | 256 | `crabd --max-forward-conns` |
| Announced and not yet accepted | 32 | `crabd` |
| Buffer per direction | 32 KiB guest, 64 KiB host | fixed, as for tunnels (`docs/protocol.md:435-436`) |

Forward connections do not take `--max-connections` slots.

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
drops while the sandbox lives, it reconnects with backoff; until then the guest refuses connections.

Targets shipped in `sandcrab-core`:

| Target | Used by | Dials |
| --- | --- | --- |
| `HostTcpTarget` | Rust SDK, direct | `127.0.0.1:<port>` of the embedding process's host, per a map it was given |
| `HostUnixTarget` | Rust SDK, direct | a Unix socket path, per a map it was given |
| (in `sandcrab-server`) `ClientRelayTarget` | the server | nothing: it passes the connection to the client (§4.4) |

The **Rust SDK's direct-dial variant**: a program that embeds the SDK is the host itself, so naming a
target is its own decision. `SandboxConfig::forward(guest_port, ForwardTo::HostTcp(port))` or
`ForwardTo::Unix(path)`. The rule "never open a socket on a client's say-so" is a rule for the server.

**Room for a server-side target.** `ForwardTarget` is the seam. In the server API a forward has a
`target` field whose only value is `"client"`. A later value can name a service the operator configured
(Sandcrab's own egress proxy, Phase 3). It will never carry an address.

### 4.4 Server and client

- `sandbox.create` takes `forwards: [{guest_port, max_connections?, target: "client"}]`. The target
  address is **not** part of the API. It exists only inside the SDK.
- Forwards of a sandbox are **served** by one control connection: the one that created the sandbox, or
  the last one that called `forward.serve {sandbox_id}`. After a client restart the SDK calls
  `forward.serve` again.
- The server sends `forward.incoming {conn_id, sandbox_id, guest_port}` on the serving connection.
  `conn_id` is 128 random bits, hex.
- The client dials its target **first**. Then it opens a connection to the server socket with the first
  frame `forward.attach {conn_id}`. After the ack the connection carries raw bytes both ways; a
  half-close on the Unix socket is an end-of-file. When the dial failed, the client sends
  `forward.reject {conn_id}` on the control connection.
- Not attached within 10 s, no serving connection, or the serving connection is gone: rejected. Fail
  closed.

### 4.5 Backends

| Backend | Forwards |
| --- | --- |
| Docker | Supported, on both transports. Network stays `none`; the container's own loopback carries the listener. No bridge, no published port |
| libkrun | Supported, over the existing tools socket. No TSI, no netstack, no helper change |
| Firecracker | **Refused.** `BackendCapabilities::supports_forwards` is false; the manager fails the spawn with `PolicyUnenforceable("forwards are not supported by the firecracker backend")`. The mechanism would work there, but it is untested and out of scope |

## 5. Server API

### 5.1 Socket and framing

- A Unix stream socket. No TCP listener.
- **Frames:** u32 big-endian length, then one JSON-RPC 2.0 object, at most 16 MiB. The same codec as
  `crabd` (`docs/protocol.md:26-45`, `sandcrab_protocol::framing`).

Why the length prefix and not newline-delimited JSON:

| | Length prefix | Newline-delimited |
| --- | --- | --- |
| Switch from frames to raw bytes on a stream connection | Exact: the reader knows where the ack ends | A buffering line reader has usually read past the newline into the raw bytes and must hand them back. A classic bug |
| Size check | Before any allocation | After scanning |
| Code | The codec exists and is tested; ADR 0001 §2 chose it as the one framing | A second codec |
| Debugging by hand | Needs a few lines of script | `socat` is enough |
| TypeScript, no native code | About thirty lines | A few lines fewer |

The exact switch decides it.

### 5.2 Connections

Every connection starts with one frame. Its method decides the kind of connection.

| First frame | Kind | After the answer |
| --- | --- | --- |
| `server.hello` | **control** | Frames: requests, responses, notifications. Many per client are allowed; the SDK uses one |
| `process.spawn` | **stdio** of one process | Raw. Client → server is stdin, a half-close ends it. Server → client is stdout, EOF ends it |
| `stream.open` | **sink** for stderr chunks | Raw, server → client only |
| `forward.attach` | one **forwarded connection** | Raw both ways |

A connection that sends no first frame within 2 s is closed. Any other first method is `-32600`.

One control connection carries any number of sandboxes. A sandbox does not belong to a connection; the
only per-connection state is "serves the forwards of these sandboxes".

### 5.3 Methods on a control connection

Sketches; the real types live in `sandcrab_protocol::api` (§12). Params reject unknown fields, results
accept them, as in `crabd`.

| Method | Params | Result |
| --- | --- | --- |
| `server.hello` | `client: string`, `api_version: "1"` | `server_version`, `api_version`, `backend`, `limits`, `capabilities` |
| `server.capabilities` | none | `Capabilities` (below) |
| `server.selftest` | `template?`, `max_age_secs?` | `SelftestReport` (§9). Runs a fresh one unless a report younger than `max_age_secs` exists |
| `template.list` | none | `[{name, version, image, pinned: "host-local" \| "legacy-manifest" \| "no", external}]` |
| `template.verify` | `name` | `{status: "match" \| "unpinned" \| "mismatch", digest?}` |
| `sandbox.create` | `SandboxCreate` (below) | `SandboxInfo` |
| `sandbox.get` | `sandbox_id` | `SandboxInfo` |
| `sandbox.list` | `labels?: map` (all must match), `created_before?`, `min_age_secs?`, `state?` | `SandboxInfo[]` |
| `sandbox.set_ttl` | `sandbox_id`, `ttl_secs: u64 \| null` | `SandboxInfo` |
| `sandbox.destroy` | `sandbox_id` | `{destroyed: bool}` (false: it was already gone) |
| `sandbox.destroy_by_label` | `labels: map` (not empty) | `{matched, destroyed, remaining: SandboxId[], empty: bool}` (§8.3) |
| `sandbox.exec` | `ExecParams` (below) | `ExecResult` |
| `process.wait` | `process_id` | `ProcessExit` (§3.3), once the process ended and its stdout was written to the stdio connection |
| `process.cancel` | `process_id`, `grace_ms?` | `{was_running, tree_clean}` |
| `process.signal` | `process_id`, `signal` | `{delivered}` |
| `process.list` | `sandbox_id` | `[{process_id, argv0, started_at, running}]` |
| `tool.call` | `sandbox_id`, `name`, `params` | The tool's result. The sixteen LLM tools, passed through with `crabd`'s error codes |
| `file.upload` | `sandbox_id`, `path`, `content_base64`, `mode?`, `overwrite?` | as `upload` (10 MiB, `docs/protocol.md:351-361`) |
| `file.download` | `sandbox_id`, `path`, `max_bytes?` | as `download` |
| `forward.serve` | `sandbox_id` | `{ports}` |
| `forward.reject` | `conn_id`, `reason?` | `{}` |

Larger files go through an exec with stdin or stdout streams, or through a mounted folder.

Notifications from the server: `forward.incoming {conn_id, sandbox_id, guest_port}`,
`sandbox.destroyed {sandbox_id, reason: "destroyed" | "ttl" | "backend_exit"}`,
`server.shutdown {grace_ms}`.

```rust
pub struct SandboxCreate {
    pub template: String,
    /// At most 16. Key [a-z0-9][a-z0-9._-]{0,62}, not starting with "sandcrab.". Value at most 256 bytes.
    pub labels: BTreeMap<String, String>,
    /// None: the server's default. Explicit null: no TTL.
    pub ttl_secs: Option<Option<u64>>,
    /// Ad-hoc mounts: the path is the client's path and the guest path (§6).
    pub mounts: Vec<MountSpec>,                    // { path, mode: "ro" | "rw" }
    /// Registered workspaces by name, at a chosen guest path (unchanged model).
    pub workspaces: Vec<WorkspaceMountSpec>,       // { workspace, guest_path, mode }
    pub network: NetworkMode,                      // "disabled" (default) | "unrestricted"
    pub forwards: Vec<ForwardSpec>,
    /// "uid:gid". Must be allowed by the server config (§6.5).
    pub user: Option<String>,
    pub resources: Resources,                      // vcpu, memory_mb, disk_mb, pids_max, nofile
    pub tmp_exec: bool,
    pub tmp_size_mb: Option<u64>,
    /// Environment of the LLM tools only. `process.spawn` and `sandbox.exec` never see it.
    pub env: BTreeMap<String, String>,
    pub default_cwd: Option<String>,
}

pub struct SandboxInfo {
    pub sandbox_id: String,
    pub template: String,
    pub labels: BTreeMap<String, String>,
    pub state: SandboxState,                       // "running" | "orphaned" | "destroying"
    pub created_at: String,                        // RFC 3339
    pub expires_at: Option<String>,
    pub backend: String,
    pub mounts: Vec<MountSpec>,                    // client paths, never host paths
    pub forwards: Vec<u16>,
    pub forwards_served: bool,
}

pub struct Capabilities {
    pub backend: String,
    pub hardware_isolated: bool,
    pub multi_tenant_safe: bool,
    pub supports_host_dir_mounts: bool,
    pub supports_forwards: bool,
    pub persistent_labels: bool,
    pub tree_kill: String,                         // "subreaper" | "process_group"
    pub mount_identity_checked: bool,              // §6.4
    pub selftest: Option<SelftestSummary>,         // { ok, reason, age_secs }
}
```

### 5.4 `sandbox.exec`: the unary bounded exec

One request, one response, no stream connection. For the many short commands of a run.

```rust
pub struct ExecParams {
    pub sandbox_id: String,
    pub process_id: String,                // client-chosen UUID, so `process.cancel` can abort it
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub input_base64: Option<String>,      // at most 1 MiB decoded; written to stdin, then closed
    pub timeout_ms: u64,                   // required, at most the server's max_exec_timeout
    pub kill_grace_ms: Option<u64>,
    pub max_output_bytes: Option<u64>,     // default and maximum 8 MiB
    pub overflow: Overflow,                // "kill" (default) | "tail"
    pub stderr: StderrMode,                // "tail" | "merge"
    pub stderr_tail_bytes: Option<u32>,
    pub limits: Option<ExecLimits>,
    pub oom_score_adj: Option<u16>,
}

pub struct ExecResult {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_base64: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub overflow: bool,
    pub tree_clean: bool,
    pub duration_ms: u64,
}
```

The server opens `process.open` with the same values and an `output_cap`, collects the `STDOUT` records
and answers. Memory in the server is bounded by `max_output_bytes`. The SDK uses this path when the
call has no output stream, no per-chunk callback, an input that is not a stream and at most 1 MiB, and a
cap of at most 8 MiB. Everything else takes the stream path.

### 5.5 `process.spawn`: the stream path

```text
client ── new connection ── {"method":"process.spawn","params":{...}} ──► server
client ◄── {"result":{"process_id":"..."}}                       the program runs
client ── stdin bytes ──► ... half-close = end of stdin
client ◄── stdout bytes ... EOF = the output has ended
client ── on the control connection: process.wait {process_id} ──► ProcessExit
```

Params: `process_id` (client-chosen), `sandbox_id`, then the fields of `ProcessOpenParams` (§3.2), with
`stderr` extended: `"tail"`, `"merge"`, or `{"stream": stream_id}`.

- The answer comes after the guest's ack. An error answer (`SPAWN_FAILED`, `UNAVAILABLE`, ...) closes the
  connection.
- With `stdin: "null"` any byte from the client closes the connection.
- **stderr chunks.** The client first opens a sink: a new connection with `stream.open {stream_id}`
  (client-chosen UUID). Then it names that id in `process.spawn`. The server writes the `STDERR` records'
  bytes to the sink and closes it at the end. Opening the sink first means no chunk can be lost and
  nothing needs to be held back. A sink not used within 30 s is closed.
- **Backpressure** is the sockets' own. The server reads the next record from the guest only after it
  wrote the previous one to the client. A client that does not read stops the program.
- **The process lives as long as its stdio connection.** If the client closes or loses it, the server
  closes the guest connection and the tree is stopped (§3.4). A lost control connection does not stop a
  process.
- `process.wait` may be sent right after the answer; it resolves at the end. Exits are kept for 5 minutes
  (at most 1024) for a late `process.wait`.

### 5.6 Error codes

`crabd`'s codes pass through unchanged for `tool.call`, `file.*`, `process.*` and `sandbox.exec`
(`docs/protocol.md:121-135`, plus §3.1). The server adds:

| Code | Name | Meaning | `data` |
| --- | --- | --- | --- |
| -32101 | `UNAVAILABLE` | The backend could not start or reach the sandbox: daemon down, VM failed to boot, `crabd` connection lost before anything ran. Trying again may work | `{detail}` |
| -32102 | `SANDBOX_NOT_FOUND` | No such sandbox | `{sandbox_id}` |
| -32103 | `MOUNT_NOT_ALLOWED` | A mount outside the table, nested, a symlink on the way, or one that contains the server's own files | `{path, reason}` |
| -32104 | `LIMIT_EXCEEDED` | A server limit (sandboxes, processes, resources, connections) | `{what, limit}` |
| -32105 | `POLICY_UNENFORCEABLE` | The backend cannot enforce what was asked (forwards on Firecracker, a user that is not allowed) | `{detail}` |
| -32106 | `TEMPLATE_ERROR` | Template not found, unpinned, or its pin does not match | `{kind, template}` |
| -32107 | `SHUTTING_DOWN` | The server no longer accepts this | `{}` |
| -32108 | `CONFLICT` | A `process_id` used twice, a sandbox that is orphaned or being destroyed | `{detail}` |
| -32008 | `UNAUTHORIZED` | The peer's uid is not allowed. One frame, then close | `{}` |

### 5.7 Limits

Server config, with defaults:

| Limit | Default |
| --- | --- |
| Connections, all kinds | 1024 |
| Requests in flight per control connection | 64; beyond that the server stops reading |
| Sandboxes | 64 |
| Processes per sandbox | 64 (the guest's `--max-processes`) |
| Unary execs in flight, server-wide | 256 |
| Resources per sandbox | `max_vcpu` 8, `max_memory_mb` 16384, `max_pids` 8192, `max_nofile` 1048576 |
| `max_exec_timeout` | 1 h |
| Default TTL, maximum TTL | 30 min (today's default, `crates/sandcrab-core/src/sandbox.rs:37-38`), 24 h |

Over a limit is `LIMIT_EXCEEDED`. Nothing queues.

### 5.8 Client disconnect

| What is lost | Effect |
| --- | --- |
| A control connection | Its requests in flight are dropped. A unary exec in flight is cancelled with its kill grace. Forwards it served become unserved (fail closed). **No sandbox is destroyed** |
| A stdio connection | That process's tree is stopped |
| A forward connection | That one forwarded connection ends |

### 5.9 Server restart

The server keeps no database of sandboxes. The backend objects are the record (§8).

| | Docker | libkrun |
| --- | --- | --- |
| Do sandboxes survive the server? | Yes. Containers keep running | No. The helper's lifeline ends the VM when its owner dies (`crates/sandcrab-backend-libkrun/src/vm.rs:12`, `reap.rs:3-4`) |
| What the new server finds | Containers with Sandcrab's labels whose owner process is gone (the existing owner labels, `crates/sandcrab-backend-docker/src/spec.rs:105-110`) | Run directories whose owner is gone |
| What it does | Lists them as `state: "orphaned"`, with their labels and creation time. They can be listed and destroyed. They cannot run anything: `CONFLICT` | Removes them at start, as `reap_orphans` does today. Nothing is left to list |

An orphaned container cannot be used again because the per-sandbox token existed only in the old
server's memory (`crates/sandcrab-backend-docker/src/backend.rs:531`), and `crabd` deleted its token file
(`docs/protocol.md:85-86`). Its processes are already gone: their connections closed with the old server.

Orphans are kept for `orphan_grace` (default 15 min) after the server started, so a client can list them
by label and destroy them itself. After that the server removes them. **Reattaching** to an orphan is not
part of this milestone (§16, question 2).

### 5.10 Graceful shutdown

On `SIGTERM` or `SIGINT`:

1. Stop accepting connections. New requests get `SHUTTING_DOWN`. Send `server.shutdown {grace_ms}`.
2. Cancel every process with the configured grace (default 5 s) and wait for the `EXIT` records.
3. `--on-shutdown destroy` (default): destroy every sandbox. `keep`: leave Docker containers running;
   the next server lists them as orphans. On libkrun `keep` is refused at start, because the VMs cannot
   outlive the server.
4. Remove the socket and exit.

### 5.11 Access control

All of it is checked before the first frame is read.

| Guard | Rule |
| --- | --- |
| Allowed peer uids | `--allow-uid <uid>` (repeatable) or `allow_uids` in the config. **Required**: the server refuses to start without at least one. Checked on every accepted connection with `SO_PEERCRED` on Linux and `getpeereid` on macOS. A peer whose uid is not in the list gets one `UNAUTHORIZED` frame and a close |
| Socket mode | `0600` by default, owner = the server's user. `socket_mode` and `socket_group` can widen it for a client in another uid. This is the second layer; the uid list is the first |
| Socket directory | The server refuses to start when the directory, or any ancestor, is writable by a user other than root and the server's own, unless the sticky bit is set on that ancestor. Reuses `sandcrab_core::private_dir` (`crates/sandcrab-core/src/private_dir.rs:125`) |
| Stale socket | Replaced only when it is a socket owned by the server's user and nothing answers on it |
| Own files in a mount | Refused (§6.3) |

There is no token. A process that can open the socket and passes the uid check has full control of the
server. A per-client token comes only with a TCP listener, which this milestone does not add. The
per-sandbox token between host and `crabd` is unchanged.

The uid in `SO_PEERCRED` is the uid as the host kernel sees it. For a containerised client without a
user-namespace remap that is the container's numeric uid.

### 5.12 Configuration

`sandcrab server --config <file>`, TOML. Flags override.

```toml
socket = "/run/sandcrab/server.sock"
socket_mode = "0660"            # default "0600"
socket_group = "sandcrab"       # optional
allow_uids = [1500]             # required
backend = "docker"              # required: "docker" | "libkrun" | "firecracker". Never a fallback
state_dir = "/var/lib/sandcrab"
templates_dirs = ["/etc/sandcrab/templates"]
on_shutdown = "destroy"
orphan_grace_secs = 900
selftest = { on_start = true, template = "base" }

[limits]
max_sandboxes = 64

[users]
default = "peer"                # the connecting peer's uid:gid, or an explicit "uid:gid"
allow = ["1500:1500"]           # uid:gid pairs a client may ask for; 0 only when listed

[[mounts]]                      # the mount table (§6.2)
client = "/data"
host = "/var/lib/docker/volumes/appdata/_data"
modes = "rw"                    # "rw" (ro and rw allowed) | "ro"
```

`sandcrab doctor` gains checks: the config parses, the socket directory passes, every `host` root exists
and passes its ancestor check, the backend is reachable, `allow_uids` is not empty.

## 6. Mounts

### 6.1 Model

An ad-hoc mount is `{path, mode}`. `path` is absolute, is the path the client sees, and is the path the
sandbox sees. There is no registry entry and no lease. The client may change the directory while a
sandbox uses it; that is the point.

Registered workspaces (`WorkspaceRegistry`) stay as they are, with their leases, their generation
counter and a guest path chosen per mount. A sandbox can have both kinds. The non-nesting rule covers
all of a sandbox's guest paths together.

In `sandcrab-core`:

```rust
pub struct HostMount { pub path: PathBuf, pub mode: MountMode }          // on SandboxConfig.host_mounts

/// Who may mount what. None on the manager: every ad-hoc mount is refused.
pub struct MountPolicy { pub entries: Vec<MountRoot>, pub protected: Vec<PathBuf> }
pub struct MountRoot { pub client: PathBuf, pub host: PathBuf, pub max_mode: MountMode }
```

A resolved ad-hoc mount reaches the backend as a `ResolvedMount` with a new `MountSource::Adhoc` beside
the workspace fields, carrying `host_path`, `guest_path`, `mode` and `root_identity`
(`crates/sandcrab-core/src/mounts.rs:21-38`). Backends treat it as they treat a host-dir workspace.

### 6.2 Resolution

For each `{path, mode}`:

1. **Normalise** `path` lexically: absolute, UTF-8, no NUL, no `..` (`normalize_guest_path`,
   `crates/sandcrab-core/src/mounts.rs:49-69`). The result is the guest path. It must pass
   `validate_mount_target`: not `/`, not under `/proc`, `/sys`, `/dev`, `/run/sandcrab`, `/opt/sandcrab`
   (`mounts.rs:17-18`, `:72-88`).
2. **Map.** Find the table entry whose `client` is the longest component-wise prefix of the path. None:
   `MOUNT_NOT_ALLOWED`. `mode` above the entry's `max_mode`: `MOUNT_NOT_ALLOWED`. The host path is the
   entry's `host` plus the remainder. With the identity map, `client` equals `host`.
3. **Open, fd-verified.** The server opened each `host` root once at start (symlink-free walk from `/`,
   `crates/sandcrab-core/src/fsx.rs:104-129`) and holds the descriptor. The remainder is opened beneath
   it with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`, or component by
   component with `O_NOFOLLOW` where `openat2` is missing (`fsx.rs:10-13`, `:180-215`). A symlink
   anywhere, or something that is not a directory: `MOUNT_NOT_ALLOWED`. The directory must exist; nothing
   is created. `fstat` gives the root identity `(dev, ino)`.
4. **Non-nesting.** No two guest paths of the sandbox are equal or nested (the rule of
   `resolve_mounts`, `mounts.rs:92-97`, `:115-121`). No two host identities are equal unless both mounts
   are read-only.
5. **Protected paths** (§6.3).
6. Hand `host_path`, `guest_path`, `mode` and the identity to the backend.

At most 16 mounts per sandbox (the libkrun helper's share limit,
`crates/sandcrab-backend-libkrun/src/helper_proto.rs:53`), on every backend.

### 6.3 The server never mounts itself

A mount is refused when its directory is, contains, or lies inside any of: the server socket's
directory, the server's state directory, the backend's state and run directories, the template pin
store. The check compares `(dev, ino)` identities, not path strings: the identity of every ancestor of
each protected directory is collected at start, and a mount root whose identity is in that set, or whose
own ancestors include a protected directory, is refused. A bind mount or a second name for the same
directory does not get around it.

Without this, a client could mount the socket into a sandbox, and anything in the sandbox running as an
allowed uid would control the server.

### 6.4 Paths resolved after verification

Both backends finally hand a **path** to something that resolves it again:

- Docker: the bind source is a string (`crates/sandcrab-backend-docker/src/spec.rs:243-260`), resolved by
  the Docker daemon after `verify_mount_roots` ran (`backend.rs:757-764`).
- libkrun: `krun_add_virtiofs3` takes a path; the helper checks the identity immediately before the call
  (`crates/sandcrab-backend-libkrun/src/workspace.rs:16-21`), and a residual window is a known item
  (`docs/design/libkrun-backend.md:342`, `:950`).

Someone who can write to a parent directory in that window can swap a component for a symlink. For
registered workspaces the writer has to be another local user. For ad-hoc mounts the writer can be a
**sandbox** that has a read-write mount on a directory above the new mount, or a hostile client. So this
milestone closes it in two ways:

1. **No nesting across live sandboxes.** A mount is refused when its directory lies inside a directory
   that another live sandbox has mounted read-write, or when it is read-write and contains a directory
   another live sandbox has mounted. Equal directories are allowed. This removes the sandbox as the
   writer, on every backend. Switch: `mounts.cross_sandbox_nesting = "refuse"` (default) or `"allow"`.
2. **Check after start, through `crabd`** (Docker). Before the sandbox is handed to the caller, the
   manager asks `crabd` for `(dev, ino)` of every mount point and compares it with the verified
   identity. A bind mount shows the host's device and inode numbers, because the kernel is shared. A
   mismatch destroys the sandbox and fails the create. This is the "re-check after start" item of the
   review backlog (AGENTS.md §5, item 3). It needs a small new tool-level method, `fs.identity {path}`
   → `{dev, ino}`, which is not an LLM tool.

Where the numbers cannot be compared (Docker Desktop's file sharing, the libkrun virtio-fs share), the
check is skipped and `capabilities.mount_identity_checked` is false. Whether libkrun's virtio-fs reports
host inode numbers to the guest is **not verified** (§16, question 5).

### 6.5 Users and ownership

| | Docker bind mount | libkrun virtio-fs |
| --- | --- | --- |
| Whose uid touches the host files | The container process's uid, unmapped | The helper's uid, whatever the guest uid is |
| Default sandbox user today | The spec's user, else the server's uid:gid (`crates/sandcrab-backend-docker/src/spec.rs:237-241`) | The spec's user, else the configured default, else the host uid:gid (`crates/sandcrab-backend-libkrun/src/backend.rs:384-387`) |
| What the guest sees as owner | The host's numeric uid | The host's uid; guest-only changes live in xattrs (`LinuxComplete`, `helper_proto.rs:240-247`) |

Rules for the server:

- `user` on `sandbox.create` must be in `[users] allow`. uid 0 only when listed.
- Default `"peer"`: the sandbox runs as the uid:gid of the connecting peer. On Linux with a
  containerised client that is the client's own uid, so files written in the sandbox are owned exactly
  as the client's own writes on the shared volume. On the Mac, client, server and helper are the same
  user, so it is the identity.
- On libkrun a `user` other than the server's own uid does not change host ownership (the helper writes
  as itself). The server accepts it and says so in the docs; it does not pretend.
- A read-write mount exposes the existing setuid-bit risk of review item R9 (`docs/reviews/libkrun-backend-review.md:61`).
  The mount table should only name directories that other local users cannot traverse; `doctor` warns
  when a `host` root is group- or world-accessible.

Whether the Docker `unix` transport starts cleanly when the sandbox user differs from the server's user
(the run directory and socket are created for the host user, `docs/protocol.md:379`) is **not
verified**; the `docker-backend` step has a test for it.

### 6.6 `/tmp`, pids, nofile

| Option | Today | New |
| --- | --- | --- |
| `/tmp` | tmpfs, `nosuid,nodev,noexec`, 256 MiB (`crates/sandcrab-backend-docker/src/spec.rs:402-405`, `crates/sandcrab-protocol/src/init.rs:48-50`) | `tmp_exec: true` drops `noexec` only. `tmp_size_mb` sets the size (counts against the sandbox's memory) |
| pids | 1024, fixed (`init.rs:47`; `crates/sandcrab-backend-docker/src/config.rs:104`) | `resources.pids_max`, up to the server's `max_pids`. Docker `pids_limit`; microVM: the pids cgroup `crabd init` creates (`crates/sandcrab-guest/src/init/sys.rs:254-278`) |
| nofile | Docker's default; only `core` is set (`spec.rs:409`) | `resources.nofile`: a Docker `nofile` ulimit; microVM: `RLIMIT_NOFILE` on `crabd serve` |

In `sandcrab-core`: `ResourceOverrides` gains `pids_max` and `nofile`
(`crates/sandcrab-core/src/policy.rs:219-228`); `SandboxConfig` gains `tmp_exec`, `tmp_size_mb`,
`host_mounts`, `forwards` and `labels` (`crates/sandcrab-core/src/sandbox.rs:47-78`). The microVM guests get
the values through new `init.configure` fields. A backend that does not implement an option refuses the
spawn; it never ignores it.

## 7. Sandbox reuse and warm exec

One sandbox per run, many short execs into it. Sandcrab's part is that an exec into a warm sandbox is
cheap. Pooling and the choice of which sandbox an exec goes to are the adapter's.

Cost of one unary exec, by construction: one request on the control connection; one new `crabd`
connection (`auth`, `process.open`); two process starts in the guest (the supervisor, then the program);
one `/proc` listing at the end.

**Targets** (not measured yet; the `ts-e2e` step measures them):

| Case | p50 | p95 |
| --- | --- | --- |
| `sandbox.exec ["/bin/true"]`, Docker, `unix` transport, sequential | 15 ms | 40 ms |
| The same, libkrun | 25 ms | 60 ms |
| The same, 8 at once in one sandbox | 2× the sequential p50 | |

**How to measure.** In the TypeScript e2e: one sandbox, 20 warm-up execs, then 200 sequential and
8 × 50 concurrent, timed at the client with `performance.now()`. Print p50, p95 and max. Assert only a
loose bound (p95 under 150 ms) so CI on shared runners does not flake; record the real numbers in the
step's notes. `sandcrab-bench` gets no new scenario in this milestone.

**Are the in-flight limits enough?** Yes.

- `--max-inflight 64` is per tool connection (`crates/sandcrab-guest/src/server.rs:127-128`). Execs no
  longer use the tool connection; each has its own connection and counts against `--max-processes` (64).
- `--max-connections 64` is not consumed by process or forward connections after their first frame.
- A run has a handful of commands at once. 64 per sandbox leaves a wide margin. The server answers
  `LIMIT_EXCEEDED` beyond it; it does not queue.
- If the handshake per exec turns out to dominate, the fix is a small pool of pre-authenticated guest
  connections in `sandcrab-core`. Not built unless the numbers ask for it.

## 8. Labels, TTL and reaping

### 8.1 Storage

Labels are immutable after create.

| Backend | Where | Fields |
| --- | --- | --- |
| Docker | Container labels, beside the existing ones (`crates/sandcrab-backend-docker/src/spec.rs:96-112`) | `sandcrab.label.<key> = <value>`, `sandcrab.created_at` (RFC 3339), `sandcrab.ttl_secs` |
| libkrun | `meta.json` in the run directory (`RunMeta`, `crates/sandcrab-backend-libkrun/src/reap.rs:167-181`) | `labels`, `ttl_secs`, beside the existing `created_unix_ms` |
| Firecracker | Not stored. `persistent_labels: false` in the capabilities | |

The existing owner labels (uid, pid, boot id, state directory) stay. They are how a new server tells
its predecessor's containers from those of another Sandcrab process on the same host
(`crates/sandcrab-backend-docker/src/reap.rs:9-12`).

In `sandcrab-core`, `RuntimeBackend` gains two methods with default implementations:

```rust
/// Backend objects that carry Sandcrab's marks, whether this process tracks them or not.
async fn list_objects(&self) -> Result<Vec<BackendObject>> { Ok(Vec::new()) }
/// Remove one object this process does not track. Ok(false): it was not there.
async fn destroy_object(&self, id: SandboxId) -> Result<bool> { Ok(false) }

pub struct BackendObject {
    pub id: SandboxId,
    pub template: String,
    pub labels: BTreeMap<String, String>,
    pub created_at: SystemTime,
    pub ttl: Option<Duration>,
    /// The owning process is gone.
    pub orphaned: bool,
}
```

### 8.2 List

`sandbox.list` returns the manager's live sandboxes (`running`) plus the backend objects it does not
track and whose owner is gone (`orphaned`). Objects owned by another live process are never listed and
never touched. Filters: every given label must match exactly; `created_before` and `min_age_secs` go by
`created_at`.

### 8.3 Destroy by label

1. Select as `sandbox.list` does.
2. Destroy each: live ones through the manager, orphans through `destroy_object`. Destroying a sandbox
   closes its process connections, so every tree in it is stopped first.
3. **List the backend again**, fresh, with the same selector.
4. `empty` is true only when step 3 found nothing and no destroy failed. Otherwise `remaining` names
   what is left.

A sandbox created with the same labels while the call runs can make `empty` false. That is correct: it
is not empty.

### 8.4 TTL, the reaper, `reap_orphans`

- Live sandboxes: the manager's reaper, unchanged (`crates/sandcrab-core/src/manager.rs:357-403`).
  `sandbox.set_ttl` changes the expiry in memory. A long run renews its TTL; the SDK offers a keep-alive
  helper.
- A disconnect is not an end. Only TTL, `destroy` and `destroy_by_label` are.
- Orphans (Docker): kept for `orphan_grace`, then removed by the server with the logic of
  `reap_orphans` in its default scope (same uid, same state directory, owner gone,
  `crates/sandcrab-backend-docker/src/reap.rs:38-49`). The host-wide scope is never used by the server.
- `sandcrab doctor --fix` keeps working. It removes the same set, at once.
- A changed TTL is not written back to the container (labels cannot be changed), so it does not survive
  a server restart. It does not need to: an orphan cannot run anything and is removed after the grace.

## 9. Self-test

The isolation claim is earned by running probes in a real sandbox, not by reading the backend's static
capabilities.

**Where.** `sandcrab_sdk::selftest::run(&Sandcrab, SelftestConfig) -> SelftestReport`. It spawns two fresh
sandboxes of the chosen template on the chosen backend: A with network `disabled`, one forward, one
read-only and one read-write ad-hoc mount in a temporary directory; B plain.

**No dependency on the image.** Probes run as `crabd probe <name>`, started through `process.open` from
the `crabd` binary that every sandbox already contains (`/opt/sandcrab/crabd` on Docker,
`/sbin/crabd` in a microVM, `helper_proto.rs:64`). A bring-your-own image without a shell can be tested.
A probe prints one JSON line: what it tried and the `errno` it got.

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
| `mount-identity` | (host side) reads the capability | the check of §6.4 is active on this host |

With `network: "unrestricted"` templates the network lines do not apply; the self-test always runs its
own sandboxes with `disabled`.

**A timeout is never a pass.** Every probe has a deadline (5 s). A line ends in one of three states:
`pass`, `fail`, `inconclusive`. A deadline gives `inconclusive`. The reasons:

- A silently dropped packet and a wall look the same from inside, but a drop can also mean "routed, and
  filtered somewhere else today". Only a refusal with a specific `errno` shows that there is no route.
- A probe that hung, or never started, also ends in a timeout. Counting that as a pass would turn a
  broken self-test into a green one.

`ok` is true only when every required line is `pass`.

```rust
pub struct SelftestReport {
    pub ok: bool,
    /// One sentence naming the first line that did not pass. None when ok.
    pub reason: Option<String>,
    pub backend: String,
    pub template: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub lines: Vec<SelftestLine>,
}
pub struct SelftestLine {
    pub id: String,
    pub status: LineStatus,          // "pass" | "fail" | "inconclusive" | "skipped"
    pub required: bool,
    pub detail: String,
    pub evidence: serde_json::Value, // what was tried, the errno
}
```

**How it is exposed.**

| Surface | |
| --- | --- |
| `sandcrab selftest [--backend ...] [--template ...]` | Prints the lines; exit status 1 unless `ok` |
| Server start | Runs it once (`selftest.on_start`). A failure does not stop the server; the capabilities then say isolation is not proven, and why |
| `server.selftest` | Runs a fresh one, or returns the last when it is younger than `max_age_secs`. One runs at a time |
| `server.capabilities` | `selftest: {ok, reason, age_secs}` |
| TypeScript SDK | `client.selftest()` and a synchronous `client.capabilitiesSnapshot()` (§11.3) |

A client turns this into "isolation proven: `ok`; reason: `reason`".

## 10. Bring-your-own-image templates

A template is already a manifest that names an image (`[template] image`, required,
`crates/sandcrab-core/src/template.rs:118-120`). What is new is a template with **no Dockerfile**, whose
image somebody else built.

```toml
# <templates dir>/myapp/sandcrab.toml
[template]
name = "myapp"
version = "1.4.0"
description = "The application's own runtime image"
image = "registry.example/myapp:1.4.0"     # a tag, or name@sha256:<digest>
external = true                            # new: never built by templates/build.sh
user = "1000:1000"

[resources]
vcpu = 2
memory_mb = 4096
disk_mb = 8192

[workspace]
workdir = "/"

[network]
default_policy = "disabled"

[tools]
tiers = ["core"]                           # leave out "process" when the image has no bash
```

**`crabd` injection.**

| Backend | How | Change |
| --- | --- | --- |
| Docker | As today: the host's static `crabd` is bind-mounted read-only at `/opt/sandcrab/crabd` and is the pinned entrypoint, so an image `ENTRYPOINT` cannot wrap it (`crates/sandcrab-backend-docker/src/spec.rs:65-66`, `:306`, `:368-369`) | None |
| libkrun | Through `sandcrab-rootfs`: `docker export` of the image, `crabd` and the mount points added, a read-only ext4 image, cached by image id, arch and `crabd` digest (AGENTS.md, crate map) | The pipeline must accept an image without a Dockerfile in the template directory and must not assume a shell in it. Whether it does today is **not verified** |

**Pinning: the same rule as built templates.** An external image is never trusted on first use.

1. The operator pulls the image.
2. `sandcrab template pin myapp` records the local image id host-locally, with `source: "adopt"`
   (`docs/template-pins.md:55-60`).
3. When the manifest's reference carries a digest (`name@sha256:...`), `pin` first checks that the local
   image has that repository digest and refuses otherwise. This adds provenance for free, and such a
   digest may be committed, unlike a local image id.
4. A spawn from an unpinned template is refused, as today
   (`crates/sandcrab-backend-docker/src/backend.rs:185-194`). Sandcrab does not pull at spawn.

`templates/build.sh` skips `external = true` templates. `sandcrab template list` shows them.

**What the image must provide.**

| Requirement | Why |
| --- | --- |
| Linux, the host's architecture (`linux/amd64` or `linux/arm64`) | `crabd` and the kernel |
| Nothing else for `process.open` and `sandbox.exec` | `crabd` is a static binary (`crates/sandcrab-backend-docker/src/config.rs:178`); it needs no libc and no shell from the image |
| `bash` on `PATH`, only for the LLM tools `bash` and `process_*` | They run `bash -c` (`crates/sandcrab-guest/src/tools/exec.rs:68-70`) |
| The template's `workdir` exists, or is a mount target | It is the default cwd |
| A world-writable or user-owned `HOME`, if the programs need one | The sandbox user is an arbitrary uid |
| No reliance on `ENTRYPOINT`, `CMD` or an init system | None of them runs |
| Whatever the client's programs need, at the paths the client uses | Program files are not mounts (§14, point 6) |

`sandcrab template verify myapp --probe` (new flag) starts one sandbox from the template and runs the
image-level checks: `crabd` answers, the workdir exists, the architecture matches, and whether `bash`
is there. The test image for the step is a public Node, git and Python image.

Image `ENV` and `WORKDIR` reach a Docker container from the image itself. Whether the rootfs pipeline
carries them into a microVM is **not verified**. It only matters for the LLM tools' default environment;
`process.open` takes an exact env either way.

## 11. TypeScript SDK

### 11.1 Package

`sdk/typescript`, published as `@sandcrab/sdk`. Node 26 or later, ESM only, no native code, no runtime
dependency: `node:net`, `node:stream`, `node:crypto`, `node:events`.

```text
sdk/typescript/
  package.json  tsconfig.json  README.md
  schema/server-api.schema.json     copied from the Rust snapshot; CI compares the two
  scripts/generate-types.mjs
  src/
    index.ts
    client.ts          SandcrabClient: connect, reconnect, requests, notifications
    framing.ts         frame encoder; a reader that returns the bytes after a frame
    connection.ts      control connection; opening stream connections
    sandbox.ts         Sandbox
    process.ts         ProcessHandle, exec
    forwards.ts        dials targets, attaches forwarded connections
    selftest.ts        capabilities snapshot, self-test
    errors.ts          SandcrabError, error codes, InvalidSpecError
    generated/api.ts   generated; committed
    testing/fake-server.ts   exported as "@sandcrab/sdk/testing"
  test/unit/  test/e2e/
```

### 11.2 Types from the schemas

- The server API types live in `sandcrab_protocol::api` and derive `JsonSchema` (`schemars`), like the
  tool types do today (`crates/sandcrab-protocol/src/schema.rs:27-43`).
- `sandcrab server schema` prints one JSON Schema document: `$defs` for every params, result, error data
  and notification type, and a `methods` table (`name → {params, result, kind: "control" | "connection"}`).
- A snapshot test in `crates/sandcrab-protocol/tests/snapshots/` pins that document.
- `scripts/generate-types.mjs` turns `schema/server-api.schema.json` into `src/generated/api.ts` with
  `json-schema-to-typescript` (a dev dependency), and emits a typed `Methods` map so
  `client.request("sandbox.create", params)` is checked.
- CI fails when the copied schema differs from the Rust snapshot, or when regenerating changes the
  committed file.

### 11.3 Public API

```ts
export function connect(options: ConnectOptions): Promise<SandcrabClient>;

export interface ConnectOptions {
  socketPath: string;
  clientName?: string;
  /** Default: reconnect with backoff from 50 ms to 2 s. */
  reconnect?: boolean | { minMs: number; maxMs: number };
  signal?: AbortSignal;
}

export interface SandcrabClient {
  createSandbox(spec: SandboxSpec, o?: { signal?: AbortSignal }): Promise<Sandbox>;
  /** A handle for a sandbox that exists: after a client restart, or from another process. */
  sandbox(id: string): Sandbox;
  listSandboxes(filter?: { labels?: Labels; minAgeMs?: number; createdBefore?: Date; state?: SandboxState }): Promise<SandboxInfo[]>;
  destroyByLabel(labels: Labels): Promise<{ matched: number; destroyed: number; remaining: string[]; empty: boolean }>;
  capabilities(): Promise<Capabilities>;
  /** The last capabilities the client has seen, refreshed in the background. Never throws. */
  capabilitiesSnapshot(): { backend: string | null; isolation: { ok: boolean; reason: string | null }; ageMs: number | null };
  selftest(o?: { template?: string; maxAgeMs?: number }): Promise<SelftestReport>;
  templates(): Promise<TemplateInfo[]>;
  verifyTemplate(name: string): Promise<TemplatePinStatus>;
  readonly connected: boolean;
  on(event: "connect" | "disconnect", listener: () => void): this;
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
}

/** The target stays in the client. The server only learns `guestPort`. */
export interface ForwardSpec {
  guestPort: number;
  target: { port: number; host?: "127.0.0.1" | "::1" } | { socketPath: string };
  maxConnections?: number;
}

export interface Sandbox {
  readonly id: string;
  info(): Promise<SandboxInfo>;
  /** Returns at once. Throws InvalidSpecError for a malformed spec, before anything starts; never otherwise. */
  spawn(spec: SpawnOptions): ProcessHandle;
  /** Rejects only with InvalidSpecError. Every other failure is in the outcome. */
  exec(spec: ExecOptions): Promise<ExecOutput>;
  /** After a client restart: dial these targets for the sandbox's existing forwards. */
  serveForwards(forwards: readonly ForwardSpec[]): Promise<void>;
  setTtl(ttlMs: number | null): Promise<void>;
  /** Renews the TTL on an interval until the returned function is called. */
  keepAlive(ttlMs: number): () => void;
  upload(path: string, data: Uint8Array, o?: { mode?: number; overwrite?: boolean }): Promise<void>;
  download(path: string): Promise<Uint8Array>;
  tool(name: string, params: unknown): Promise<unknown>;
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
}

export interface SpawnOptions extends ProcessBase {
  /** Written to stdin, which is then closed. Not with `stdin: "open"`. */
  input?: string | Uint8Array;
  stdin?: "end" | "open";
  pty?: { rows: number; cols: number };
  timeoutMs?: number;
  killGraceMs?: number;
}

export interface ProcessHandle {
  /** Ordered bytes, one consumer. Ends when the output ends. */
  readonly stdout: AsyncIterable<Uint8Array>;
  /** Does nothing without `stdin: "open"`, and after the process has gone. Never throws. */
  write(data: string | Uint8Array): void;
  end(): void;
  /** Ask, wait `graceMs` (default 5000), force. Resolves when nothing is left. Idempotent. Never rejects. */
  cancel(graceMs?: number): Promise<void>;
  cancelled(): boolean;
  /** True once the program runs, false when it could not be started. Never rejects. */
  readonly started: Promise<boolean>;
  /** Resolves after the process ended and stdout was fully delivered. Never rejects. */
  readonly exit: Promise<ProcessExit>;
}

export interface ProcessExit {
  code: number | null;
  signal: string | null;
  stderrTail: string;
  /** The program could not be started, e.g. { errno: "ENOENT", what: "program" }. Null when it ran. */
  spawnError: { errno: string; what: "program" | "cwd"; message: string } | null;
  /** The server or the backend could not start anything. Nothing ran. Trying again may work. */
  unavailable: boolean;
  /** The connection was lost after the program had started. Its end is unknown. */
  lost: boolean;
  timedOut: boolean;
  cancelled: boolean;
  overflow: boolean;
  /** False: something could not be stopped. Destroy the sandbox. */
  treeClean: boolean;
}

export interface ExecOptions extends ProcessBase {
  timeoutMs: number;
  killGraceMs?: number;
  input?: string | Uint8Array | Readable;
  /** stdout goes here and is not captured. `exec` ends it and resolves after it was flushed. */
  output?: Writable;
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

Behaviour that the types do not show:

- **`spawn` is synchronous.** It validates, creates the handle, and starts connecting in the background.
  `write` before the connection is up is buffered, at most 1 MiB; beyond that `write` drops the data, as it does
  after the process has gone.
- **`exit` never rejects.** Connect failure or `UNAVAILABLE` → `unavailable: true`. `SPAWN_FAILED` →
  `spawnError`. A stdio connection that ends without a `process.wait` answer → `lost: true`.
- **`exec` chooses the path** (§5.4): unary when it can, streams otherwise. With `output`, the stdio
  connection is piped into it with `stream.pipeline`, so `drain` is honoured.
- **`overflow: "tail"` above 8 MiB** is computed in the SDK from the stream with a ring buffer.
- **Abort** calls `process.cancel` with `killGraceMs` and sets `aborted`.
- **Forwards.** For every `forward.incoming` the SDK looks up the sandbox's `ForwardSpec` by guest port,
  dials the target, then attaches (§4.4). An unknown port is rejected. It never dials anything else.

### 11.4 Connections and reconnect

- One control connection per client, opened by `connect`. Stream connections are opened per process,
  per stderr sink and per forwarded connection, and closed with them.
- When the control connection drops: requests in flight fail as unavailable (`sandbox.get`,
  `sandbox.list` and `server.capabilities` are retried once after reconnect; nothing else is). The SDK
  reconnects with backoff and jitter, sends `server.hello`, then `forward.serve` for every sandbox whose
  forwards it holds.
- Processes do not depend on the control connection. If the server itself went away, their stdio
  connections end and their exits say `lost`.
- `capabilitiesSnapshot()` answers from memory. While disconnected it answers
  `{ ok: false, reason: "not connected to the sandcrab server" }`.

### 11.5 Tests

| Layer | What |
| --- | --- |
| Unit, against `testing/fake-server.ts` (a TypeScript server on a temporary Unix socket, scripted per test) | Frames split at every byte boundary; an ack and raw bytes in one chunk; responses out of order; a connection dropped at each stage of spawn; a reader that stops reading (backpressure reaches the fake's writer); `cancel` twice; `write` after `end`; `exit` never rejects in any of these; reconnect and `forward.serve` again; `exec` path choice; the tail ring; abort |
| Contract, `test/e2e/process-contract.test.ts`, against a real server on Docker | The list in the flow's `ts-e2e` step: one test per conformance check of §2.5, plus forwards to a TCP port and to a Unix socket, destroy by label, orphans after a client and after a server restart, 16 sandboxes at once, the warm-exec numbers (§7) |
| Types | `tsc --noEmit`; the schema and generated-file checks of §11.2 |

The e2e uses the `ts-dev` template, because the contract tests use Node as the test program.

## 12. Crate boundaries

| Piece | Crate | Note |
| --- | --- | --- |
| 0.3 params, results, records, error codes | `sandcrab-protocol` | Pure data |
| Server API types (`api` module), their schemas | `sandcrab-protocol` | Pure data. The TypeScript SDK depends on nothing else |
| `init.configure` additions | `sandcrab-protocol` (`init.rs`) | |
| `crabd supervise`, `crabd probe`, forward listeners, `fs.identity` | `sandcrab-guest` | More `rustix` features (`pipe`, `stdio`); whether a new crate is needed depends on §16, question 1 |
| Labels, `host_mounts`, `MountPolicy`, forwards config, `ForwardTarget`, `ForwardAcceptor`, the process-stream client, `list_objects` | `sandcrab-core` | No backend crate |
| Container labels and listing, `/tmp` and limit options, ad-hoc binds | `sandcrab-backend-docker` | |
| Run-record labels, ad-hoc shares, `init.configure` options | `sandcrab-backend-libkrun` | Helper config version unchanged |
| The explicit refusal of forwards and of the new options | `sandcrab-backend-firecracker` | A few lines |
| Process handle, bounded exec, forwards with direct targets, self-test | `sandcrab-sdk` | |
| The daemon | `sandcrab-server` (new) | |
| `sandcrab server`, `sandcrab selftest`, `template pin`/`verify` changes, `doctor` checks | `sandcrab-cli` | |

New row for the crate map in AGENTS.md:

| Crate | Role | Allowed Dependencies |
| :--- | :--- | :--- |
| `sandcrab-server` | Host daemon (lib): Unix-socket listener, peer-uid check, JSON-RPC control connections and raw stream connections, mount table, relaying of processes and forwards, orphan handling, graceful shutdown. One explicitly chosen backend, never a fallback | `sandcrab-sdk` (backends only through its features, like `sandcrab-cli`), `sandcrab-protocol`, `sandcrab-core` (types re-exported by the SDK where possible), Tokio, `rustix` (`net`: peer credentials; `fs`), `serde`/`serde_json`, `toml`, `tracing`, `thiserror`, `uuid`. **No** backend crate directly, **no** `bollard`, **no** HTTP stack, **no** TCP listener |

And one change to an existing row: `sandcrab-cli` may depend on `sandcrab-server` (for the `server`
command).

`getpeereid` on macOS: whether `rustix` exposes peer credentials there without `unsafe` is **not
verified** (§16, question 6).

## 13. Security analysis

### 13.1 Trust boundaries

```text
client ──(1)── server ──(2)── backend (Docker daemon / libkrun helper) ──(3)── guest
```

| Boundary | Who is trusted with what |
| --- | --- |
| (1) client ↔ server | The client is trusted **within the server's config**: the uid list admits it, the mount table and the user list bound it. It is not trusted beyond them |
| (2) server ↔ backend | The server holds the backend's full power: Docker access is root on the host; the libkrun helper runs as the server's user. Unchanged from today's embedded SDK |
| (3) host ↔ guest | The guest is not trusted. Unchanged: every connection is opened by the host; on Docker each starts with the per-sandbox token |

### 13.2 A client that passes the uid check

| Can | Cannot |
| --- | --- |
| Create, list and destroy **every** sandbox on this server, including other clients'. There is no per-client separation without a token. One server serves one owner | Reach a server whose uid list does not name it, even when the socket file is world-writable |
| Mount any directory under the mount table, in the allowed mode | Mount anything else; mount through a symlink; mount the server's socket, state or pins; get a host path back from the API |
| Run programs as any uid:gid in `[users] allow` | Run as another uid, or as 0 unless listed |
| Use up to the configured limits | Exceed them: sandboxes, processes, resources, connections are capped |
| Choose where its own forwards go, inside its own process | Make the server open any socket or port |
| Ask for `network: "unrestricted"` | Get it where the backend cannot enforce it without a silent downgrade (libkrun without its pinned netstack refuses, `crates/sandcrab-backend-libkrun/src/net.rs:6-17`) |

A deployment that must keep clients apart runs one server per client, each with its own socket, uid list
and mount table.

### 13.3 A compromised sandbox

| Through | Can | Cannot |
| --- | --- | --- |
| Forwards | Connect to the declared loopback ports, up to the caps. Whatever the client's target then allows (the adopter's proxies ask for a token per run) | Reach a port that was not declared; choose the target; make the server or the host dial anything; open a connection towards the host (announcements only); hold more than 32 announced connections |
| Process streams | Send any bytes as stdout; block by not reading stdin; kill its own supervisor, which the client sees as a lost process | Change the record framing of another process (one connection each); make the host allocate more than one buffer per stream; forge an `EXIT` for another process |
| Mounts | Read and write its mounts in the given mode | Leave them; become the writer in the swap of §6.4 while cross-sandbox nesting is refused |
| The `crabd` it shares a uid with | Disturb its own sandbox | Authenticate as the host: the token file is gone (`docs/protocol.md:85-86`) and the connections are opened from outside |
| Resources | Use its limits | Fork past `pids_max`; survive `destroy` |

`crabd`, the supervisor and the program share one uid inside the sandbox. That is as today, and the
sandbox boundary does not depend on it.

### 13.4 Resource bounds

| Resource | Bound |
| --- | --- |
| Server memory per process stream | One 64 KiB record at a time |
| Server memory per unary exec | `max_output_bytes`, at most 8 MiB, times the in-flight cap |
| Server memory per forwarded connection | 64 KiB per direction |
| Guest memory per process | One 64 KiB buffer per stream; a tail ring of at most 8 MiB, charged to the sandbox |
| Frames | 16 MiB, checked before allocation |
| Connections without a first frame | Closed after 2 s |
| Finished exits kept for `process.wait` | 1024, 5 min |

### 13.5 No silent downgrades

- The backend is named in the config. A missing one is a start error.
- Firecracker refuses forwards, ad-hoc mounts and the new options it does not implement.
- The process-group fallback of the tree kill is reported in `hello`, in the capabilities and by a
  failing self-test line.
- A mount identity check that cannot run is reported as `mount_identity_checked: false`.
- No backend reports `multi_tenant_safe`; the capabilities pass that on unchanged.

## 14. Contract points for the adopter

These shape the adapter. None blocks Sandcrab work. Each has a recommendation and what Sandcrab needs
either way.

| # | Point | Recommendation | Sandcrab needs |
| --- | --- | --- | --- |
| 1 | **`started` is read synchronously, right after `spawn` returns.** A remote launcher cannot know it yet | The adapter reports "started" as true at once and delivers a failed start in the exit. The adopter's own suite already accepts this for a launcher that starts a wrapper. The alternative, making the field known together with the exit, changes the adopter's interface | Either way: `started: Promise<boolean>` and `exit.spawnError` in the SDK. Nothing more |
| 2 | **Most short execs as a run identity declare no folders** | The adapter keeps one sandbox per run and sends folder-less execs of that run into it; its mounts are the view. For an exec with no run to attach to, the adopter either declares folders or uses the `host` identity. An exec must carry something the adapter can map to a run: the label already does | Exec into an existing sandbox by id. Mounts fixed at create. Both exist in this design. If folders can grow during a run, the adapter needs a new sandbox: Sandcrab does not add mounts to a live sandbox |
| 3 | **`sweep` names an OS user, which a sandbox launcher ignores** | The adopter adds the optional grouping key its spec foresees. The adapter writes it as a label on every sandbox and sweeps with `destroy_by_label`. Without a key the adapter rejects, as it does for `host` | Labels and `destroy_by_label` with a confirmed-empty answer (§8.3) |
| 4 | **The recovery record is null for a sandbox launcher** | Recover by label only. The adopter's two stored values stay unused for sandboxed runs | `sandbox.list` by label after a client restart and after a server restart (§8.4). On libkrun a server restart leaves nothing, which the adapter reports as "gone" |
| 5 | **The test database is not among a run's endpoints** | The adopter adds its port to the run's endpoints when a run may use it | Nothing new: one more forward |
| 6 | **Program files are not folders of the spec** | The image holds the programs at the paths the adopter uses, through a bring-your-own-image template. Programs that live on a volume are added by the adapter as extra read-only mounts. Note for the conformance run: its test program is the client's own interpreter path, which must exist at that path in the sandbox | External templates (§10), read-only ad-hoc mounts |
| 7 | **One sandbox per run, or one per exec** | One per run, reused. One per batch for check steps | A cheap warm exec (§7), `keepAlive` for the TTL |
| 8 | **File ownership during the transition** | The sandbox runs as the uid:gid the local launcher would have used; the server's `[users] allow` lists those. On Docker that gives identical ownership on the shared volume | `user` on create (exists), the allow list (§6.5) |
| 9 | **Two conformance checks look up a guest process id in the client's process table** (new) | The adopter skips those two assertions for a launcher whose recovery record is null, as it already does for the other pid assertions. As written they usually pass by accident and can fail by coincidence | Nothing. `tree_clean` in the exit is the real evidence, and the self-test line `tree-kill` checks it |
| 10 | **stderr limits are counted in characters** (new) | Keep the interface; the adapter asks for four bytes per character and trims | `stderr_tail_bytes` up to 64 KiB |
| 11 | **Mount layout** (new) | The adopter confirms that no folder of one run lies inside a read-write folder of another live run. If one does, set `cross_sandbox_nesting = "allow"` and accept §6.4 | The switch |

## 15. Per flow step

Every implement step ends with `cargo fmt`, `clippy -D warnings` and the unit tests green. Builds and
Docker e2e run on the remote hosts.

### `protocol-03`

- **Files:** `crates/sandcrab-protocol/src/{lib.rs,process.rs (new),forward.rs (new),record.rs (new),api.rs (new),init.rs,error.rs,results.rs,rpc.rs,schema.rs}`, `tests/`, `tests/snapshots/`, `docs/protocol.md`.
- **Contains:** everything in §3.1 to §3.4 and §3.7 as types; the record codec (encode, incremental decode, limits); forward verbs and records (§4.2); `fs.identity`; `init.configure` fields (§6.6); the server API types and the schema document (§5, §11.2).
- **Acceptance:** `PROTOCOL_VERSION == "0.3"`. Params reject unknown fields. The existing sixteen tool schemas are byte-identical to their snapshot. `process.open`, `forward.*` and `fs.identity` are not in `TOOL_NAMES`. The API schema snapshot exists.
- **Tests:** round trips for every new type; record codec with split input, an oversized length, an unknown kind; error code mapping; `init.configure` without the new fields still parses.

### `guest-exec`

- **Files:** `crates/sandcrab-guest/src/{supervise.rs (new),proctree.rs (new),probe.rs (new, stub),server.rs,main.rs,context.rs,lib.rs}`, `Cargo.toml` (`rustix` features). `tools/process.rs` and `tools/exec.rs` are **not** changed.
- **Contains:** `crabd supervise` (§3.5, §3.6); the `process.open` branch in `serve_stream` (`server.rs:505-584`); `process.cancel`, `process.signal`, `fs.identity`; `crabd serve` as a subreaper with the stray rule; the `hello` changes; `--max-processes`.
- **Acceptance:** the program's environment is exactly the given map. 1 MiB of mixed bytes arrives identical. `EXIT` comes after the last byte. A grandchild in a new session that ignores `SIGTERM` is gone when `process.cancel` answers, and `tree_clean` is true. A leader that exits leaving a grandchild on its pipes ends within the drain bound. Timeout and both overflow modes behave as §3.6. `ENOENT` and `EACCES` give `SPAWN_FAILED`. A closed connection stops the tree. `bash` and `process_*` behave as before.
- **Tests:** unit tests for the tree listing on a fixture `/proc`; integration tests that run `crabd serve` on a Unix socket on Linux and drive it (the pattern the tunnel tests use); the cases above, each as one test; a fork-heavy program against the force loop.

### `guest-forwards`

- **Files:** `crates/sandcrab-guest/src/{forward.rs (new),server.rs,main.rs}`.
- **Contains:** §4.2.
- **Acceptance:** listeners are bound before `hello` answers. Loopback only. Every row of the fail-closed table. The caps. Half-close in both directions.
- **Tests:** against a local `crabd serve`: echo through a forward; upload-until-EOF; no listen connection; reject; unclaimed timeout; cap; a port not in the list refuses.

### `core-config`

- **Files:** `crates/sandcrab-core/src/{sandbox.rs,policy.rs,workspace.rs,mounts.rs,adhoc.rs (new),backend.rs,manager.rs,error.rs,testing.rs,lib.rs}`; the capability and `ResolvedSpec` literals in the three backend crates and `sandcrab-bench`; `crates/sandcrab-backend-firecracker/src/backend.rs` (refusals).
- **Contains:** the new `SandboxConfig` and `ResourceOverrides` fields; labels and their validation; `MountPolicy` and §6.2 to §6.4 (the manager side of the identity check); `BackendCapabilities::{supports_forwards, persistent_labels}`; `list_objects`, `destroy_object`; manager `list`, `destroy_by_label`; `SandboxError::Unavailable`.
- **Acceptance:** the whole rule matrix of §6.2 and §6.3 is unit-tested without a backend. A sandbox with forwards on a backend without `supports_forwards` fails with `PolicyUnenforceable`. Labels round-trip through `FakeBackend`. `destroy_by_label` answers `empty: false` when a destroy fails.
- **Tests:** mount table (longest prefix, mode cap, symlink at each depth, `..`, the protected set through a second name, nesting within and across sandboxes); label validation; list filters; old serialized configs still parse.

### `core-forwards`

- **Files:** `crates/sandcrab-core/src/{forward.rs (new),process.rs (new),client.rs,manager.rs,testing.rs,lib.rs}`.
- **Contains:** `ForwardTarget`, `ForwardAcceptor`, the two host targets (§4.3); `ProcessStream` (opens `process.open`, record reader and writer, cancel); `FakeBackend` support for both.
- **Acceptance:** a forward relays both ways with half-close. A failing target rejects. The acceptor reconnects. A process stream delivers bytes in order and the exit last; dropping it closes the connection.
- **Tests:** against the fake `crabd` in `testing.rs` and against a real local `crabd serve`.

### `docker-backend`

- **Files:** `crates/sandcrab-backend-docker/src/{spec.rs,backend.rs,reap.rs,config.rs}`.
- **Contains:** label and time labels (§8.1); `list_objects`, `destroy_object`; `--forward-port` in the command; `tmp_exec`, `tmp_size_mb`, `pids_max`, `nofile`; ad-hoc binds; the identity check after start.
- **Acceptance (e2e, ignored tests):** a container with network `none` reaches its forward and nothing else. Labels are on the container and listed by a second backend instance. A `user` other than the server's works on the `unix` transport. `/tmp` runs a binary only with `tmp_exec`. A swapped mount root fails the create.
- **Tests:** spec unit tests for every new field; the e2e above.

### `libkrun-backend`

- **Files:** `crates/sandcrab-backend-libkrun/src/{backend.rs,workspace.rs,reap.rs,vm.rs}`. `helper_proto.rs` is **not** changed.
- **Contains:** labels in `RunMeta`; `list_objects`; ad-hoc shares through the existing `VirtioFsShare` with identity; the new `init.configure` fields; forwards need only the port list.
- **Acceptance (e2e on macOS and Linux KVM):** forwards work with network `disabled`. A folder at the same path is live from both sides. Labels are listed while the server lives. After the owner dies nothing is left.
- **Tests:** unit tests for the plan and the record; the e2e above.

### `byo-image`

- **Files:** `crates/sandcrab-core/src/template.rs`, `crates/sandcrab-rootfs/src`, `crates/sandcrab-cli/src` (`template pin`, `template verify --probe`), `templates/build.sh`, `docs/template-pins.md`.
- **Contains:** §10.
- **Acceptance:** an external template with a public Node, git and Python image spawns on Docker, and through the rootfs pipeline on libkrun, after `template pin`. Unpinned is refused. A digest in the reference that does not match is refused at `pin`. `build.sh` skips it.
- **Tests:** manifest parsing; pin with and without a digest; the two spawns.

### `rust-sdk`

- **Files:** `crates/sandcrab-sdk/src/{process.rs (new),exec.rs (new),forward.rs (new),sandbox.rs,lib.rs}`.
- **Contains:** `Sandbox::spawn_process` (a handle: stdout as a byte stream, `write`, `end`, `cancel`, an `exit` future that does not error), `Sandbox::exec` (bounded, caps, abort through a cancellation token, input and output as `AsyncRead` and `AsyncWrite`), forwards with direct targets, labels, `list`, `destroy_by_label`, the mount policy on the builder.
- **Acceptance:** the Rust mirror of the process-contract list passes against Docker. Firecracker with a forward is refused with the explicit error.
- **Tests:** unit tests on `FakeBackend`; ignored e2e on Docker.

### `selftest`

- **Files:** `crates/sandcrab-sdk/src/selftest.rs`, `crates/sandcrab-guest/src/probe.rs`, `crates/sandcrab-cli/src` (`selftest` command).
- **Contains:** §9.
- **Acceptance:** green on Docker. Each line fails when its wall is removed in a test double: a probe result with a different `errno`, a timeout, a missing answer. A timeout gives `inconclusive` and `ok: false`.
- **Tests:** the evaluation of each line from recorded probe output (pass, fail, timeout); the e2e run.

### `server`

- **Files:** `crates/sandcrab-server/` (new: `lib.rs`, `config.rs`, `listener.rs`, `peer.rs`, `control.rs`, `streams.rs`, `forwards.rs`, `orphans.rs`, `shutdown.rs`), `crates/sandcrab-cli/src/{args.rs,main.rs,server.rs (new),doctor.rs}`, `Cargo.toml` (workspace member), `docs/hosting.md` (a systemd unit example).
- **Contains:** §5, §4.4, §8, and the server half of §6.
- **Acceptance:** refuses to start without `allow_uids`, on a bad socket directory, with `keep` on libkrun. A peer outside the list is refused with the socket at mode `0666`. A mount that contains the socket is refused, also through a second path. A client disconnect destroys nothing. After a restart on Docker the old sandboxes are listed as orphaned and can be destroyed. Shutdown leaves nothing with `destroy`.
- **Tests:** unit tests for config, the peer check and the first-frame dispatch; integration tests with `FakeBackend` over a real Unix socket for every method; e2e on Docker for restart and shutdown.

### `ts-sdk`

- **Files:** `sdk/typescript/**`.
- **Contains:** §11.
- **Acceptance:** the unit list of §11.5 passes on Node 26. No runtime dependency. `tsc` is clean. The schema checks pass.
- **Tests:** §11.5, unit layer.

### `ts-e2e`

- **Files:** `sdk/typescript/test/e2e/**`, `.github/workflows/ci.yml`, `scripts/package-release.sh`.
- **Acceptance:** the contract suite of §11.5 passes against a real server on Docker. The warm-exec numbers are printed and under the loose bound. The server binary and the SDK tarball are in the release artifacts.

## 16. Open questions

| # | Question | Recommendation |
| --- | --- | --- |
| 1 | Does the pinned `rustix` expose `set_child_subreaper`, `set_parent_process_death_signal`, the dumpable switch, `memfd_create` and `setrlimit` as safe functions? **Not verified.** | They are part of `rustix`'s `process` and `fs` APIs in the 1.x line. If one is missing, add the smallest vetted crate that wraps it. Do not relax `unsafe_code = "forbid"` in `sandcrab-guest` |
| 2 | Should a restarted server be able to use a Docker sandbox again (reattach)? | Not now. It needs the per-sandbox tokens on disk, which is a new secret store. Orphans are listed and destroyed. Revisit if a run must survive a server upgrade |
| 3 | Should the server relay a forwarded connection as raw bytes to the client, or keep records end to end? | Raw to the client. A Unix socket keeps half-close, and the owner decided on raw streams. Records stay between host and guest |
| 4 | `RLIMIT_NPROC` on Docker counts all processes of the uid on the host. Should `nproc` be refused there? | Accept it and document it. It matches what the adopter's local launcher does today. `pids_max` is the per-sandbox cap. The adapter may leave `nproc` out on Docker |
| 5 | Does libkrun's virtio-fs show host inode numbers in the guest, so the check of §6.4 can run there too? **Not verified.** | Measure it in the `libkrun-backend` step. If yes, enable the check. If no, keep `mount_identity_checked: false` and the cross-sandbox rule |
| 6 | Peer credentials on macOS without `unsafe`? **Not verified.** | Use `rustix::net::sockopt` if it has `LOCAL_PEERCRED`; else the `nix` crate's `getpeereid`, which `sandcrab-backend-libkrun` already depends on for another call |
| 7 | Can the user-namespace probe be written without `unsafe`? **Not verified.** | Try `rustix::thread::unshare`. If it is not safe to call, the probe runs the attempt in a helper it starts with `process_group` and reads the result; evidence read from a sysctl alone is not a pass on Docker |
| 8 | Is a mount point under the guest's tmpfs `/tmp` handled in the right order by both backends? **Not verified.** | Sort mounts by depth after the system mounts. Add an e2e case: on the Mac, test directories live under a temporary path |
| 9 | Should the legacy `bash` and `process_start` move onto the supervisor to lose their `setsid` limit? | Later. It would change the behaviour of a daemon started under a live `process_start`. Keep them untouched in this milestone |
| 10 | Should `sandbox.create` be idempotent on a client-chosen key? | Not now. The adapter lists by label before it creates |

## 17. Changes to the flow

The DAG is sound in its order. These points need a change or a note. The flow file was not edited.

1. **`protocol-03` also carries the server API types and their schema** (`sandcrab_protocol::api`).
   `ts-sdk` depends only on `protocol-03`, and it generates its types from that schema. Add this to the
   step's title; add `sdk/typescript/schema` to nobody's artifacts but note the copy.
2. **`guest-exec` and `guest-forwards` collide** on `crates/sandcrab-guest/src/server.rs` (the first-frame
   switch in `serve_stream`) and on `main.rs` (flags). Add the edge `guest-exec → guest-forwards`, or run
   them in separate worktrees and merge `guest-exec` first.
3. **`core-config` and `core-forwards` collide** on `manager.rs`, `testing.rs` and `lib.rs` in
   `sandcrab-core`. Add the edge `core-config → core-forwards`.
4. **`core-config` touches every backend crate.** New fields on `BackendCapabilities` and `ResolvedSpec`
   break the struct literals in `sandcrab-backend-docker`, `-libkrun`, `-firecracker` and `sandcrab-bench`.
   The step must fix them, and add Firecracker's explicit refusals. The flow gives Firecracker's refusal
   to `rust-sdk`; it belongs in `core-config` (the manager gate) and the Firecracker backend.
5. **A missing step: guest init options.** `tmp_exec`, `tmp_size_mb`, `pids_max`, `nofile` and
   `forward_ports` reach a microVM through `init.configure` and need changes in
   `crates/sandcrab-guest/src/init/`. No step lists them. Add `guest-init` (depends on `protocol-03`;
   artifacts `crates/sandcrab-guest/src/init`), and make `libkrun-backend` depend on it. It touches no
   file of `guest-exec` or `guest-forwards`.
6. **`guest-exec` artifacts are wrong.** The step adds `supervise.rs`, `proctree.rs` and changes
   `server.rs`; it does not change `tools/process.rs` or `tools/exec.rs`.
7. **`libkrun-backend` needs no helper config change** for forwards. Drop "helper config v-next" from its
   title. Labels go into the run record, not the helper config.
8. **`selftest` needs `crabd probe`**, a change in `sandcrab-guest`. Add
   `crates/sandcrab-guest/src/probe.rs` to its artifacts. `guest-exec` creates the file as a stub, so the
   two steps do not collide.
9. **`byo-image` and `server` both edit `crates/sandcrab-cli/src`** (`args.rs`, `main.rs`). They are far
   apart in the DAG, so no edge is needed; `byo-image` must be merged before `server` starts.
10. **`docs` mentions "socket and token setup".** There is no token. It should say "socket, allowed uids
    and mount table".
11. **`live-mac` should also depend on `selftest` being green on libkrun.** `selftest` depends only on
    `docker-backend`. Either add `libkrun-backend` to `selftest`'s dependencies or state in `live-mac`
    that a failing line there goes back to `selftest`.
