# @sandcrab/sdk

TypeScript client for `sandcrab server`: sandboxes, processes with an exact argv and
environment, host folders at the same path, and forwards whose targets stay in your process.
Node 26 or later, ESM only, no native code, no runtime dependency. Server API version `1.0`.

Running the server: [docs/server.md](../../docs/server.md). Wire reference:
[docs/protocol.md](../../docs/protocol.md#server-api-client--sandcrab-server).

Until the package is on a registry, install the built tarball (`pnpm pack` in this directory, or
the release artifact): `npm install ./sandcrab-sdk-0.1.0.tgz`.

## Connect

```ts
import { connect } from "@sandcrab/sdk";

const client = await connect({ socketPath: "/run/sandcrab-server/server.sock" });
console.log(client.server); // { version, apiVersion, features }
```

`connect` rejects when the socket cannot be reached or the server refuses your uid
(`ServerError` with code `UNAUTHORIZED`). A server whose API version does not fit still
resolves: nothing can be started on it, and `capabilitiesSnapshot()` says why. A lost control
connection is re-established with backoff (50 ms to 2 s); pass `reconnect: false` to turn that
off.

## Spawn: a long-running process

```ts
const sandbox = await client.createSandbox({ template: "base", labels: { run: "42" } });

const proc = sandbox.spawn({ argv: ["cat"], env: {}, stdin: "open" });
proc.write("hello\n");
proc.end();
for await (const chunk of proc.stdout) process.stdout.write(chunk);
const exit = await proc.exit; // never rejects
```

- `spawn` returns at once and throws only `InvalidSpecError`, for a malformed spec, before
  anything starts. Every later failure is in `exit`.
- `argv[0]` is looked up in the `PATH` of `env` when it has no `/`. There is no shell.
- `env` is the exact environment. Nothing is added and nothing is inherited, not even `PATH`
  or `HOME`.
- `stdin`: `"end"` (default; `input` is written, then stdin closes) or `"open"` (until `end()`).
  `write()` never throws; data written before the connection is up is kept and sent in order.
- **Read `stdout`.** The SDK holds at most 1 MiB of unread output; after that the program blocks
  and `exit` does not resolve until you read. Leaving the loop early (`break`) is fine: the SDK
  then discards the rest so `exit` still resolves.
- `cancel(graceMs = 5000)`: ask (`SIGTERM` to the whole tree), wait, force. Resolves when nothing
  is left. Idempotent, never rejects.
- `started` resolves `true` once the program runs, `false` if it could not start.
- `stderr`: `"tail"` (default; the last `stderrTailBytes` bytes, default 4096, at most 65536,
  cut at a UTF-8 boundary, in `exit.stderrTail`) or `"merge"` (into stdout, in order).
- Other options: `cwd` (absolute), `timeoutMs` (timed in the guest), `killGraceMs` (default
  500), `limits` (`nproc`, `dataBytes` at least 32 MiB, `fsizeBytes`, `nofile` at least 64),
  `oomScoreAdj` (0 to 1000).

## Exec: bounded, with the outcome

```ts
const out = await sandbox.exec({
  argv: ["node", "--version"],
  env: { PATH: "/usr/local/bin:/usr/bin:/bin" },
  timeoutMs: 10_000, // required
  maxOutputBytes: 1024 * 1024, // default 8 MiB
  overflow: "kill", // or "tail": keep the last bytes and let the program run on
  onOutput: (chunk, stream) => log(stream, chunk), // optional, every chunk as it arrives
  signal: AbortSignal.timeout(30_000), // optional
});
console.log(out.code, Buffer.from(out.stdout).toString(), out.stderr);
```

`exec` rejects only with `InvalidSpecError`. It resolves with the exit fields plus `stdout`
(empty when you pass `output`), `stderr` (the tail) and `aborted`. `input` may be a string,
bytes or a `Readable`; `output` may be a `Writable`, which gets every chunk with its
backpressure and is ended before `exec` resolves. With `onOutput`, stderr is streamed too (unless
merged), and a `"tail"` cap never holds live output back. A signal that is already aborted
starts nothing (`aborted: true`); an abort during the run cancels it with `killGraceMs` and sets
`aborted`, never `timedOut`.

## What an exit means, and what to do

`exit` (and the result of `exec`) never rejects. Exactly one of these describes how it ended:

| Outcome | Fields | What it means | What to do |
| --- | --- | --- | --- |
| Exited | `code` or `signal` (Node's name, e.g. `"SIGKILL"`), with `timedOut`, `cancelled`, `overflow` as flags | The program ran and ended. `timedOut`: the guest stopped it at `timeoutMs`. `overflow`: stdout passed the cap | Use the result. If `treeClean` is `false`, something the program started could not be stopped: destroy the sandbox |
| Could not start | `spawnError {errno, what, message}` | The sandbox works, this program does not start (`ENOENT`, `EACCES`, `ENOEXEC`; `what` is `"program"` or `"cwd"`) | Fix the argv, `PATH` or `cwd`. Retrying will not help |
| Rejected | `rejected {code, message}` | The server refused the request: `SANDBOX_NOT_FOUND`, `LIMIT_EXCEEDED`, `CONFLICT`, `INVALID_PARAMS`, `SHUTTING_DOWN`, ... With `exec`, also `INPUT_FAILED` / `OUTPUT_FAILED` when your stream broke | Read `code`. Usually a bug or a limit; retrying unchanged will not help |
| Unavailable | `unavailable: true` | Nothing ran: the request was never written (not connected, server version does not fit, client closed), or the server could not reach the sandbox | Safe to retry, perhaps after a reconnect |
| Lost | `lost: true`, `treeClean: false` | The connection dropped after the request was written: the server went away mid-run, or the program killed its supervisor. **The program may have run** | Do not retry as if nothing ran. Treat it as failed, and destroy the sandbox before running anything else there |

`cancelled: true` without `code` means the cancel came before the program started; nothing ran.

## Mounts: host folders at the same path

```ts
const sandbox = await client.createSandbox({
  template: "ts-dev",
  mounts: [
    { path: "/srv/sandcrab/run-42", mode: "rw" },
    { path: "/srv/sandcrab/tools", mode: "ro" },
  ],
});
const cwd = sandbox.mounts[0].resolvedPath;
await sandbox.exec({ argv: ["ls"], env: {}, cwd, timeoutMs: 5000 });
```

A mount appears in the sandbox at the same absolute path as on the host. The folder must exist
and lie under one of the server's `[[mount_roots]]`; mounts of one sandbox may not nest. The SDK
resolves symlinks first and sends the resolved path, so use `resolvedPath` for `cwd` and env (on
macOS `/tmp/x` becomes `/private/tmp/x`). At most `max_mounts` mounts (in `capabilities()`): 16,
or 4 on libkrun x86_64. Changes from either side are live. A refusal rejects `createSandbox` with
a `ServerError` of code `MOUNT_NOT_ALLOWED`, whose `data` has `path`, `reason` and `cap`.

## Forwards: a loopback port in the sandbox, a target in your process

With `network: "disabled"` (the default) the sandbox reaches nothing but its forwards. A program
in the sandbox connects to `127.0.0.1:<guestPort>`; the SDK connects the target you named. The
server only learns the port.

```ts
import net from "node:net";

const sandbox = await client.createSandbox({
  template: "base",
  forwards: [
    // A loopback TCP port of yours (host defaults to 127.0.0.1; "::1" also allowed).
    { guestPort: 3128, target: { port: 3128 } },
    // A Unix socket of yours.
    { guestPort: 5432, target: { socketPath: "/run/app/db.sock" } },
    // In-process code, no listener needed.
    { guestPort: 8080, maxConnections: 8, target: { handler: (sock) => sock.end("hello\n") } },
  ],
});
```

- Ports are fixed at create; at most 16 forwards, 64 connections per forward by default.
- A port that is not declared refuses (`ECONNREFUSED`). A target that fails, a client that is
  not connected, or no attach within the deadline closes the program's connection. Nothing
  else is ever dialled. `sandbox.info()` returns per-forward `accepted` and `refused` counters.
- The forwards are served by your control connection. After a reconnect the SDK serves them
  again by itself. After a client restart, take over with
  `client.sandbox(id).serveForwards(forwards)`.
- Measured: 150 to 436 MB/s for one stream, depending on host and backend; 16 parallel
  connections without a reset.

## Labels, lifetime and events

```ts
const found = await client.listSandboxes({ labels: { run: "42" }, createdAfter: since });
const { empty, remaining } = await client.destroyByLabel({ run: "42" });

const stop = sandbox.keepAlive(60_000); // renews the TTL until stop() is called
await sandbox.setTtl(null); // no TTL
await sandbox.destroy(); // false: it was already gone

client.on("shutdown", ({ graceMs }) => drain(graceMs));
client.on("sandboxDestroyed", ({ sandboxId, reason }) => forget(sandboxId, reason));
client.on("disconnect", () => {});
client.on("connect", () => {});
```

- A sandbox belongs to your uid. Another uid's clients cannot see or touch it; clients of the
  same uid share it.
- A sandbox outlives your connection and your process. It ends on `destroy`, `destroyByLabel`,
  its TTL (the server's default unless you set `ttlMs`), or a server stop. A server restart ends
  every sandbox.
- `destroyByLabel` answers `empty: true` only when every destroy succeeded and a second look
  found nothing.
- `shutdown`: the server is stopping and will destroy every sandbox after `graceMs`.
  `sandboxDestroyed` (`reason` `"destroyed"` or `"ttl"`; `"backend_exit"` exists in the types
  but the server does not send it yet). Events arrive only while the control connection is up;
  after a reconnect, `listSandboxes()` tells what is left.
- `sandbox.info()`, `listSandboxes()`, `capabilities()` and `selftest()` return the server's own
  types, with the wire's snake_case field names (`sandbox_id`, `forwards[].refused`, ...).

## Capabilities and the self-test

```ts
const report = await client.selftest("ts-dev", { maxAgeMs: 10 * 60_000 });
if (!report.ok) console.error(report.reason, report.lines.filter((l) => l.status !== "pass"));

const caps = await client.capabilities("ts-dev"); // backend, hardware_isolated, max_mounts, selftest, ...
const snap = client.capabilitiesSnapshot("ts-dev"); // synchronous, never throws
if (!snap.isolation.ok) console.warn(snap.isolation.reason);
```

The self-test runs fifteen named probes in two fresh sandboxes of the template, **as your uid**,
and passes a line only on a specific refusal; a timeout is `inconclusive`, never a pass.
`capabilitiesSnapshot` answers from memory and refreshes in the background; it reports
isolation as not ok while not connected, when the self-test never ran for that template
(`selftest: null` on the wire), or when the last run failed.

## Errors

| Class | `code` | When |
| --- | --- | --- |
| `InvalidSpecError` | `INVALID_SPEC` | A malformed spec, before anything is sent |
| `ServerError` | the server's name (`MOUNT_NOT_ALLOWED`, `TEMPLATE_ERROR`, ...), plus `rpcCode` and `data` | The server answered an error |
| `ConnectionError` | `NOT_CONNECTED`, `CONNECTION_LOST`, `CLOSED`, `UNSUPPORTED_VERSION` | No usable connection. `CONNECTION_LOST`: the request was sent and its answer is unknown |
| `SandcrabError` | `UNSUPPORTED_FEATURE` | The server does not offer `mounts`, `forwards` or `selftest` |
| `AbortedError` | `ABORTED` | The call's `AbortSignal` fired |

Two codes are both named `LIMIT_EXCEEDED`: the server's (`rpcCode` -32104) and the guest's
process limit (-32011). Compare `rpcCode` when the difference matters.

## Testing your own code

`@sandcrab/sdk/testing` exports `FakeServer`, an in-process server on a temporary Unix socket
that speaks the real protocol and lets a test script each process:

```ts
import { connect } from "@sandcrab/sdk";
import { FakeServer } from "@sandcrab/sdk/testing";

const server = await FakeServer.start();
server.onSpawn(async (p) => {
  p.ack();
  await p.stdout("hi\n");
  p.exit({ code: 0 });
});
const client = await connect({ socketPath: server.socketPath });
```

## Developing

```sh
pnpm install
pnpm typecheck
pnpm test        # unit tests against the fake server (test/unit)
pnpm build       # dist/: ESM JavaScript and declarations
pnpm pack        # the tarball to install by file or URL
```

The wire types in `src/generated/api.ts` come from `schema/server-api.schema.json`, a copy of
`crates/sandcrab-protocol/tests/snapshots/server-api.schema.json`. After the Rust snapshot
changes: `pnpm sync-schema && pnpm generate`. A unit test fails when either is stale.

### End-to-end tests

`test/e2e` runs against a real `sandcrab server` on Docker or libkrun. Each file starts its own
server as a child process with a private socket and state directory, drives it through the
public API only, and stops it. Without `SANDCRAB_BIN` every test is skipped.

```sh
# From the repo root: the binary and the static crabd, and the pinned ts-dev template.
cargo build -p sandcrab-cli --bin sandcrab && scripts/build-crabd.sh && templates/build.sh ts-dev
cd sdk/typescript
SANDCRAB_BIN=$PWD/../../target/debug/sandcrab \
SANDCRAB_CRABD=$PWD/../../target/$(uname -m | sed 's/arm64/aarch64/')-unknown-linux-musl/release/crabd \
pnpm test:e2e
```

For libkrun set `SANDCRAB_E2E_BACKEND=libkrun` and `SANDCRAB_E2E_HOME` to a prepared
`SANDCRAB_HOME` with the helper installed. Every variable, and the libkrun steps:
[test/e2e/README.md](test/e2e/README.md). Results at release: 52 pass and 2 skipped on Docker and
on libkrun (Linux KVM and Apple Silicon). The skipped cases are the cross-uid case (it needs a
second local user) and whichever of the two mount-cap cases does not apply to the platform.
