# Sandcrab protocol and tool reference

Protocol version **`0.1`** (`sandcrab_protocol::PROTOCOL_VERSION`). This page describes the
wire contract between a host (`sandcrab-core`, the SDK, the CLI) and `crabd`, the guest
daemon inside every sandbox. Every backend speaks it, so Docker and Firecracker behave
the same way.

Source of truth: [`crates/sandcrab-protocol/src/`](../crates/sandcrab-protocol/src/) for
types, and [`crates/sandcrab-guest/src/`](../crates/sandcrab-guest/src/) for limits and
behaviour. `sandcrab tools schema` prints the JSON Schema of every tool's input and output
(see [cli.md](cli.md#tools-schema)).

- [Framing](#framing)
- [Envelope](#envelope-json-rpc-20)
- [Handshake: `hello` and `ping`](#handshake-hello-and-ping)
- [Errors](#errors)
- [Tools](#tools)
- [Transports](#transports)
- [Tunnels: `tunnel.open` (port forwarding)](#tunnels-tunnelopen-port-forwarding)
- [`init.*` control contract (microVMs)](#init-control-contract-microvms)
- [Archive format `sandcrab-tar-v1`](#archive-format-sandcrab-tar-v1)

## Framing

Every message is one frame:

```text
+----------------------+-------------------------------------------+
| u32 length, big-end. | UTF-8 JSON (one JSON-RPC 2.0 object)      |
| 4 bytes (HEADER_LEN) | `length` bytes, 1 ..= 16 MiB (MAX_FRAME_LEN)|
+----------------------+-------------------------------------------+
```

- `HEADER_LEN = 4`, `MAX_FRAME_LEN = 16 * 1024 * 1024` bytes.
- A zero-length frame or a length above `MAX_FRAME_LEN` is a framing error. `crabd` answers
  with an error response (id `null`) and **closes the connection**, because the stream can
  no longer be trusted to be frame-aligned. Oversized frames are rejected before any
  allocation.
- Invalid JSON inside a well-formed frame gets `-32700` and the connection stays open.
- The codec (`encode_frame`, `FrameDecoder`, `read_frame`, `write_frame`) is synchronous and
  runtime-agnostic; async transports read the 4-byte header, validate it with
  `parse_header`, then read the body.

## Envelope (JSON-RPC 2.0)

`method` is a tool name or `hello` / `ping`. `params` is the tool's typed params object.

```text
-> {"jsonrpc":"2.0","id":1,"method":"bash","params":{"command":"ls"}}
<- {"jsonrpc":"2.0","id":1,"result":{"exit_code":0,"stdout":"...","stderr":"","truncated":false,"timed_out":false,"duration_ms":4,"signal":null}}
<- {"jsonrpc":"2.0","id":2,"error":{"code":-32004,"message":"not found: ...","data":{"detail":"..."}}}
```

- **Params reject unknown fields** (`deny_unknown_fields`), so a typo in a field name is a
  `-32602`, never silently ignored.
- **Results accept unknown fields**, so an older host keeps working against a newer
  `crabd`. Optional result values are serialized as explicit `null`.
- **Concurrency.** Each request runs in its own task. Responses are written as they
  complete, possibly out of order; match them by `id`. Per connection at most 64 requests
  run at once (`--max-inflight`); beyond that `crabd` stops reading (backpressure). At most
  64 connections are served at once (`--max-connections`).
- **Relative paths** in any tool resolve against the sandbox's default working directory
  (`default_cwd`, reported by `hello`).
- `stream_event` notifications (`StreamEvent::Output`, `StreamEvent::ProcessExited`) are
  defined but **reserved**: `crabd` 0.1 never emits them. Use `process_poll` for incremental
  output.

## Handshake: `hello` and `ping`

| Method | Params | Result |
| --- | --- | --- |
| `hello` | `protocol_version?: string`, `client?: string` (both optional, diagnostics only) | `crabd_version`, `protocol_version` (`"0.1"`), `tools: string[]`, `default_cwd` |
| `ping` | `{}` | `{}` |

The host calls `hello` once after connecting; `sandcrab-core` treats the sandbox as ready
when it answers. `ping` is a cheap liveness probe.

## Errors

Tool failures are JSON-RPC error objects `{code, message, data?}`. `message` is written for
an LLM reader (what went wrong and, where useful, how to fix the call). `data` carries
structured fields.

| Code | Name (`codes::`) | Meaning | `data` |
| --- | --- | --- | --- |
| -32700 | `PARSE_ERROR` | Invalid JSON | `{detail}` |
| -32600 | `INVALID_REQUEST` | Not a JSON-RPC request object (or a framing error) | `{detail}` |
| -32601 | `METHOD_NOT_FOUND` | Unknown method / tool | `{detail}` |
| -32602 | `INVALID_PARAMS` | Bad params (serde message, unknown field, bad regex, ...) | `{detail}` |
| -32603 | `INTERNAL_ERROR` | Daemon error (e.g. a panicking handler, for that request only) | `{detail}` |
| -32001 | `EXECUTION_FAILED` | The tool ran but failed (I/O, spawn failure, destination exists, ...) | `{detail}` |
| -32002 | `TIMED_OUT` | Time budget exceeded | `{timeout_ms}` |
| -32003 | `PATH_NOT_ALLOWED` | Forbidden path (e.g. `file_delete` of `/`) | `{detail}` |
| -32004 | `NOT_FOUND` | File, directory, process or search string not found | `{detail}` |
| -32005 | `AMBIGUOUS_EDIT` | `file_edit` matched more than once without `replace_all` | `{path, occurrences}` |
| -32006 | `TOO_LARGE` | Input or output over a size limit | `{size, limit}` |
| -32007 | `UNIMPLEMENTED` | Tool not implemented by this daemon / backend | `{detail}` |

Unknown codes are preserved verbatim (`ProtocolError::Other`).

The SDK's LLM bridge (`Sandbox::tool_dispatch`, used by crabagent and by `sandcrab tool`)
reports the same errors as `{"error": {"code": "<snake_case>", "message": "..."}}` with
stable string codes: `parse_error`, `invalid_request`, `unknown_tool`, `invalid_params`,
`internal_error`, `execution_failed`, `timed_out`, `path_not_allowed`, `not_found`,
`ambiguous_edit`, `too_large`, `unimplemented`, and `tool_error` for unknown numeric codes.

Note: a `bash` timeout is **not** an error. It returns a normal result with
`timed_out: true` and the output captured so far.

## Tools

16 tools (`TOOL_NAMES`): `bash`, `file_read`, `file_write`, `file_edit`, `file_delete`,
`file_move`, `list_dir`, `glob`, `grep`, `process_start`, `process_poll`,
`process_stdin`, `process_kill`, `process_list`, `upload`, `download`.

All file and search tools are native Rust inside `crabd` (`ignore`, `globset`,
`grep-searcher`, `similar`); nothing shells out to `sed`, `grep` or `find`.

**Path policy.** `crabd` has no path jail: tools can touch anything the `crabd serve` uid
can reach inside the sandbox. The isolation boundary is the container or microVM, and
host-side workspace guards (traversal, symlinks) live in `sandcrab-core`.

### Default limits

From `sandcrab_guest::Limits::default()` and `sandcrab_protocol::limits`:

| Limit | Value |
| --- | --- |
| `bash` default timeout | 120 000 ms (2 min) |
| `bash` max timeout | 3 600 000 ms (1 h); larger values are capped |
| `bash` output kept per stream | 32 KiB (16 KiB head + 16 KiB tail, with a `[... N bytes truncated ...]` marker) |
| Timeout teardown | SIGTERM to the process group, 500 ms grace, then SIGKILL |
| Background processes tracked | 64 |
| Unpolled output buffered per process stream | 1 MiB (oldest bytes dropped, `truncated` set) |
| `file_read` default lines | 2 000 |
| `file_read` max line length | 2 000 chars (cut lines end with `… [line truncated]`) |
| Binary sniff window | first 8 KiB (NUL byte or invalid UTF-8 → binary) |
| `upload` / `download` / `file_edit` file size | 10 MiB (`MAX_TRANSFER_BYTES`) |
| `file_edit` diff | 16 KiB |
| `list_dir` entries | 1 000; `depth` capped at 32 |
| `glob` results | 1 000 |
| `grep` matches | 500 total, 100 per file; lines cut at 500 chars; `context_lines` capped at 20 |

### `bash`

Run `bash -c <command>` to completion. stdin is `/dev/null`.

| Param | Type | Default | |
| --- | --- | --- | --- |
| `command` | string | required | Pipes, redirects, `&&`, multi-line scripts allowed |
| `cwd` | string | `default_cwd` | Missing directory → `NOT_FOUND` |
| `env` | map string→string | `{}` | Extra variables for this command only |
| `timeout_ms` | u64 | 120000 | Capped at 1 h |

Result: `exit_code` (`null` if killed by a signal), `signal`, `stdout`, `stderr`,
`truncated`, `timed_out`, `duration_ms`.

The command leads its own process group. On timeout the whole group gets SIGTERM, then
SIGKILL after 500 ms (`killpg`), so grandchildren die too. When `bash` exits normally,
anything it left running in its group (`server &`) is SIGKILLed: use `process_start` for
long-lived work. Known limitation: a descendant that leaves the group (`setsid`) escapes
the group kill; sandbox teardown is the backstop.

### `file_read`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required |
| `offset` | u64, 1-based | 1 |
| `limit` | u64 | 2000 |

Result: `content` (lines formatted `{n:>6}\t{line}` like `cat -n`, joined with `\n`),
`total_lines`, `start_line`, `end_line`, `truncated` (more lines after `end_line`, or a line
was cut), `is_binary` (then `content` is empty).

Streams the file line by line, so there is no file-size cap: any page of a huge log can be
read. CRLF is shown without the `\r`. `offset` or `limit` of 0 → `INVALID_PARAMS`. A
directory → `EXECUTION_FAILED` (use `list_dir`).

### `file_write`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required; parent directories are created |
| `content` | string | required |
| `append` | bool | false |

Result: `path` (absolute), `bytes_written`, `created`.

Overwrites are atomic: a temp file `.<name>.<random>.crabd-tmp` in the same directory is
written, fsynced, given the old file's mode (new files get 0644) and renamed over the
target. Symlinks are followed; the link itself is kept.

### `file_edit`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required |
| `old_string` | string | required, exact text including whitespace |
| `new_string` | string | required, must differ from `old_string` |
| `replace_all` | bool | false |

Result: `replacements`, `diff` (compact unified diff, about 3 context lines, capped at
16 KiB).

Zero matches → `NOT_FOUND` (re-read the file). More than one match without `replace_all` →
`AMBIGUOUS_EDIT {path, occurrences}`, file untouched. Empty or identical strings →
`INVALID_PARAMS`. On CRLF files, an `old_string` with bare `\n` is retried as `\r\n`. Files
over 10 MiB → `TOO_LARGE`; non-UTF-8 → `EXECUTION_FAILED`. Writes are atomic.

### `file_delete`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required |
| `recursive` | bool | false; required for directories |

Result: `path`, `kind` (`file` \| `dir` \| `symlink` \| `other`). Symlinks are removed, never
followed. `/` → `PATH_NOT_ALLOWED`.

### `file_move`

| Param | Type | Default |
| --- | --- | --- |
| `from` | string | required |
| `to` | string | required; parent directories are created |
| `overwrite` | bool | false |

Result: `from`, `to` (absolute). Existing destination without `overwrite` →
`EXECUTION_FAILED`. Cross-device moves fall back to copy + delete. Moving a directory into
itself → `INVALID_PARAMS`.

### `list_dir`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | `default_cwd` |
| `depth` | u64 | 1 (direct children); max 32 |
| `respect_gitignore` | bool | true |

Result: `entries[]` of `{path (relative), kind, size (0 for dirs), mode}`, depth-first,
directories first then by name; `truncated` when the 1 000-entry cap is hit.

### `glob`

| Param | Type | Default |
| --- | --- | --- |
| `pattern` | string | required, e.g. `**/*.rs` |
| `root` | string | `default_cwd` |
| `respect_gitignore` | bool | true |

Result: `paths[]` (relative to `root`, files only, sorted), `truncated`. `*` never crosses
`/`: `*.rs` matches top-level files only, `**/*.rs` is recursive. Bad pattern →
`INVALID_PARAMS`.

### `grep`

| Param | Type | Default |
| --- | --- | --- |
| `pattern` | string | required; Rust regex syntax |
| `root` | string | `default_cwd`; may be a single file |
| `glob` | string | none; without `/` it matches file names at any depth (`*.ts`), else root-relative paths |
| `literal` | bool | false |
| `case_insensitive` | bool | false |
| `context_lines` | u64 | 0; max 20 |

Result: `matches[]` of `{path, line_number, line, context_before[], context_after[]}`,
grouped by file (sorted), in line order; `truncated`. Binary and unreadable files are
skipped. Bad regex → `INVALID_PARAMS`.

**Walk rules for `list_dir`, `glob` and `grep`:** symlinks are not followed, `.git` is
always skipped, other hidden files are included. `respect_gitignore` applies `.gitignore`
(nested and in parent directories), `.ignore` and `.git/info/exclude`, inside or outside a
git repo; the user's global git excludes are never read.

### `process_start`

| Param | Type | Default |
| --- | --- | --- |
| `command` | string | required, run with `bash -c` in the background |
| `cwd` | string | `default_cwd` |
| `env` | map | `{}` |
| `pty` | bool | false; attach a pseudo-terminal (24x80, `TERM=xterm-256color` unless set) |

Result: `process_id` (opaque, e.g. `p-1`), `pid`. Returns immediately. With 64 tracked
processes, exited ones are evicted first; if all 64 are running → `EXECUTION_FAILED`.

### `process_poll`

Params: `process_id`. Result: `running`, `exit_code`, `signal`, `stdout` and `stderr`
produced **since the previous poll** (PTY output arrives on `stdout`), `truncated` (the
1 MiB buffer overflowed between polls). The poll that first reports `running: false` also
carries the last output. Unknown id → `NOT_FOUND`.

### `process_stdin`

Params: `process_id`, `data` (end with `\n` to submit a line). Result: `bytes_written`.
Exited process or closed stdin → `EXECUTION_FAILED`; a write blocked for 10 s fails the
same way. There is no EOF/close operation in protocol 0.1.

### `process_kill`

Params: `process_id`, `signal` (`TERM` default, `INT`, `KILL`), sent to the whole process
group. Result: `was_running` (killing an exited process is not an error). Use `TERM` or
`KILL` to stop a process reliably: non-interactive bash ignores SIGINT in background jobs.

### `process_list`

Params: `{}`. Result: `processes[]` of `{process_id, pid, command, running, exit_code, pty}`,
oldest first.

### `upload`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required; parent directories are created |
| `content_base64` | string | required; standard alphabet, padded; at most 10 MiB decoded |
| `mode` | u32 (decimal, e.g. 493 for 0755) | 420 (0644); above 0o7777 → `INVALID_PARAMS` |
| `overwrite` | bool | false |

Result: `path`, `bytes_written`, `created`. Atomic write. Existing file without `overwrite`
→ `EXECUTION_FAILED`.

### `download`

| Param | Type | Default |
| --- | --- | --- |
| `path` | string | required |
| `max_bytes` | u64 | 10485760 (10 MiB), also the cap |

Result: `path`, `content_base64`, `size`, `mode`. The size is checked before reading;
over the limit → `TOO_LARGE`. Single files only; `tar.zst` transfer is not implemented yet.

## Transports

The same frames run over every transport.

| Transport | Used by | Details |
| --- | --- | --- |
| Unix socket | Docker | `crabd serve --unix /run/sandcrab/crabd.sock`. The directory is a per-sandbox host directory bind-mounted into the container; the socket is chmod 0660 and `crabd` runs as the host user's uid. A stale socket is replaced on start. |
| TCP | local testing | `crabd serve --tcp 127.0.0.1:5000`. **Unauthenticated:** bind to loopback or a private network only. |
| vsock | Firecracker | `crabd serve --vsock 1024` (any CID). The host side uses Firecracker's vsock-over-UDS: connect to the VM's vsock Unix socket, write `CONNECT <port>\n`, read one line `OK <host_port>\n`; after that the stream carries frames. Any other reply, or EOF (no guest listener yet), means retry. |

`crabd serve` flags: `--default-cwd` (default `/workspace`), `--max-connections` (64),
`--max-inflight` (64), `--max-tunnels` (64), `--shutdown-grace-ms` (10 000). On SIGTERM / SIGINT it stops
accepting, lets in-flight requests finish within the grace period, kills background
process groups and removes the Unix socket. Logs go to stderr (`RUST_LOG`, default `info`).

## Tunnels: `tunnel.open` (port forwarding)

microVMs have no Docker-style published ports: Firecracker cannot publish ports, and
libkrun's TSI is disabled because it exposes the host's loopback to the guest. Instead the
host forwards TCP connections to a guest **loopback** port through `crabd`, over the same
transport as tool calls (vsock, vsock-over-UDS, or a Unix socket). The same primitive works
on every backend.

**Wire design: one dedicated connection per tunnel.** The host opens a *new* transport
connection for every forwarded TCP connection. Its first frame is `tunnel.open`; after the
ack the connection leaves frame mode and carries raw bytes in both directions:

```text
-> {"jsonrpc":"2.0","id":1,"method":"tunnel.open","params":{"port":3000}}
<- {"jsonrpc":"2.0","id":1,"result":{"port":3000}}
   ... raw bytes both ways, no more frames, until either side closes ...
```

Multiplexed data frames on the tool connection were rejected: they would add a flow-control
layer to the protocol, let one busy tunnel delay tool responses (head-of-line blocking), and
add per-chunk JSON overhead. vsock and Unix sockets both allow many cheap connections, and
`init.export` already uses the same "ack, then raw bytes" switch.

| | |
| --- | --- |
| Params | `port: u16` (1..=65535). Nothing else: unknown fields such as `host` are `-32602` |
| Target | `127.0.0.1:<port>` inside the guest, then `[::1]:<port>`. Never any other address |
| Result | `{port}`, sent only **after** the guest TCP connect succeeded, so an ack means the stream is live |
| Errors (then `crabd` closes) | `-32602` bad port; `-32004` nothing listening; `-32001` tunnel limit reached or connect failed; `-32002` connect timeout (5 s) |
| Position | must be the **first** frame. Later on a connection it is `-32600` and the connection stays in frame mode |
| Close | EOF on one side is propagated as a half-close (`shutdown(SHUT_WR)`) to the other; the tunnel ends when both directions are done or either side errors. **vsock listeners** (Firecracker, libkrun): when the guest target reaches EOF, `crabd` flushes its bytes and closes the whole tunnel instead (see below) |

It is a connection-level verb, not a tool: it is not in `TOOL_NAMES` and not in `hello`. A
`crabd` that predates it answers `-32601`.

**Limits and safety**

- **Guest:** at most `--max-tunnels` (64) open tunnels; the next `tunnel.open` fails at
  once with `-32001` (no queueing). Tunnel connections give back their
  `--max-connections` slot, so port forwards and tool connections cannot starve each other.
- **Host:** `sandcrab_core::PortForwarder` binds **`127.0.0.1` only** (the address is not
  configurable; port `0` = ephemeral). At most 64 tunnels per forwarder
  (`ForwarderConfig::max_tunnels`); excess TCP connections are accepted and closed at once.
  The connect + `tunnel.open` handshake is bounded (10 s). A refused tunnel closes the host
  client's TCP connection.
- **No amplification:** both relays copy bytes 1:1 with fixed buffers (32 KiB per
  direction in the guest, 64 KiB on the host); nothing is generated or repeated.
- **Teardown:** `PortForwarder::shutdown` (or drop) closes the listener and every open
  tunnel. When `crabd` shuts down (sandbox destroy) it closes all tunnels without waiting for
  the grace period, and the host side sees EOF.
- `tunnel.open` is served on the tools port only, never on the `init.*` control port, and
  runs as the `crabd serve` user.

Host API (`sandcrab-core`): `open_tunnel(&dyn Connector, port, timeout) -> BoxedStream`
for a single stream, and `PortForwarder::bind(connector, guest_port, host_port, config)`
for a listener. Backends wire `SandboxConfig.ports` onto `PortForwards::bind` (one forwarder
per port, `127.0.0.1:<ephemeral>`, reported by `SandboxHandle::published_ports`, closed on
destroy, drop and VMM exit). Docker keeps its own port mapping.

**Half-close over vsock (measured).** Neither microVM vsock bridge preserves half-close in
both directions:

| | host `SHUT_WR` on the vsock UDS | guest `SHUT_WR` |
| --- | --- | --- |
| Firecracker (KVM) | becomes a **full close** (the guest's reply is lost) | **never forwarded** (no EOF on the host) |
| libkrun 1.19.8 (HVF) | **breaks the stream** (reply lost, the host never sees EOF) | forwarded as EOF |

So both backends set `ForwarderConfig::forward_half_close = false`: a client's `SHUT_WR` is
not sent into the tunnel, and the tunnel ends when the guest side closes, the client closes,
or the guest is silent for 30 s after the client's EOF. And on vsock listeners `crabd` closes
the tunnel when its loopback target reaches EOF (after flushing the target's bytes;
`TunnelConfig::close_on_target_eof`), so the host sees the server's FIN on both backends.

What works: HTTP/1.1 (keep-alive or `Connection: close`, with or without `Content-Length`),
HTTP/1.0 (also when the client half-closes after the request), read-until-close clients, and
any protocol framed by length. What does not: protocols that need the guest to see the
client's EOF (upload-until-EOF gets no reply), and a guest server that half-closes first and
then keeps reading (host bytes after the guest's FIN are dropped). Unix-socket transports
(Docker) keep full half-close semantics in both directions.

## `init.*` control contract (microVMs)

In a Firecracker VM, `crabd` is PID 1 (`crabd init`, also selected automatically when its
PID is 1). It serves a separate **control port** (vsock port `1`, `CONTROL_PORT`) that is
never exposed to agents. The tools port defaults to `1024` (`DEFAULT_TOOLS_PORT`). Same
framing and envelope as tool calls; `ping` is also answered on the control port as a
readiness probe.

| Method | Params | Result |
| --- | --- | --- |
| `init.configure` | `InitConfig`: `sandbox_id`, `hostname`, `user {uid, gid, home?, name?}`, `env`, `default_cwd`, `mounts[]` (see below), `network`, `dns[]`, `tools_port`, `host_time_unix_ms?` | `{serve_pid, boot_ms}`; once per boot |
| `init.export` | `{drive_id}` (a writable drive) | `{format: "sandcrab-tar-v1", guest_path}`, then **raw tar bytes until EOF** on the same connection |
| `init.shutdown` | `{grace_ms}` (default 500) | `{}`; then processes get SIGTERM, SIGKILL after the grace, and the guest reboots so the VMM exits |

Configuration travels over vsock rather than the kernel command line because `env` may
carry secrets and `/proc/cmdline` is world-readable. Drive ids are at most 20 bytes (the
virtio-blk serial limit); ext4 labels at most 16. After an `init.export` ack the
connection carries no more frames; open a new control connection for further requests.

Each `mounts[]` entry (`GuestMount`) is a flat object with a `source` discriminator:

| `source` | Fields | Used by |
| --- | --- | --- |
| `"block"` (default when absent) | `drive_id`, `label?`, `guest_path`, `read_only?` | Firecracker workspace drives, libkrun block volumes |
| `"virtio_fs"` | `tag` (at most 36 bytes), `guest_path`, `read_only?` | libkrun host-dir workspaces (`mount -t virtiofs <tag>`) |

Fields of the other kind are rejected. Hosts serialize block mounts **without** `source`,
so configs stay byte-compatible with guests that predate it; configs without `source`
parse as block.

Full design: [design/firecracker-backend.md](design/firecracker-backend.md).

## Archive format `sandcrab-tar-v1`

Used to copy a workspace out of a guest (`init.export`) and by any backend that moves a
workspace in or out. A POSIX/GNU tar stream (GNU long-name / long-link allowed) with the
standard end-of-archive marker.

Producers (`crabd`):

- entry paths relative to the workspace root, `/`-separated, never absolute, no `..` or
  `.`; the root itself is not an entry;
- only directories, regular files and symlinks (stored, never followed); sockets, FIFOs and
  devices are skipped; hard links become independent files;
- a directory before its contents, siblings in byte order (deterministic);
- permission bits kept (`0o777` for files and symlinks, `0o1777` for directories), setuid /
  setgid dropped; mtime kept; uid/gid informational only;
- one filesystem only (nested mounts skipped); `lost+found` at the root is skipped.

Consumers (`sandcrab_core::archive::unpack_safe`) reject absolute or `..` paths, hard
links, devices / FIFOs, setuid / setgid bits, writes through a symlink, duplicate entries,
a missing end-of-archive marker, and archives over the size or entry limits.
