# @sandcrab/sdk

TypeScript client for `sandcrab-server`: sandboxes, processes with an exact argv and
environment, and forwards whose targets stay in your process. Node 26 or later, ESM only, no
native code, no runtime dependency.

```ts
import { connect } from "@sandcrab/sdk";

const client = await connect({ socketPath: "/run/sandcrab-server/server.sock" });

const sandbox = await client.createSandbox({
  template: "base",
  labels: { run: "42" },
  mounts: [{ path: "/srv/app-data/run-42", mode: "rw" }],
  // A program in the sandbox that connects to 127.0.0.1:3128 reaches this port of yours.
  forwards: [{ guestPort: 3128, target: { port: 3128 } }],
});

// Bounded: resolves with the outcome, never rejects at run time.
const out = await sandbox.exec({
  argv: ["node", "--version"],
  env: { PATH: "/usr/local/bin:/usr/bin:/bin" }, // the exact environment, nothing inherited
  cwd: sandbox.mounts[0].resolvedPath,
  timeoutMs: 10_000,
});
console.log(out.code, Buffer.from(out.stdout).toString(), out.stderr);

// Long-running: the handle is returned at once.
const proc = sandbox.spawn({ argv: ["cat"], env: {}, stdin: "open" });
proc.write("hello\n");
proc.end();
for await (const chunk of proc.stdout) process.stdout.write(chunk);
const exit = await proc.exit; // never rejects: code / signal, spawnError, rejected, unavailable, lost

await client.destroyByLabel({ run: "42" });
await client.close();
```

Things to know:

- Read `stdout` of a spawned process, or use `exec`. The SDK holds at most 1 MiB of unread
  output; after that the program blocks, and `exit` does not resolve until you read.
- `unavailable` means nothing ran and trying again may work. `lost` means the connection
  dropped mid-run: the program may have run, so do not retry as if it had not.
- A mount is at the same absolute path in the sandbox as on the host. The SDK resolves
  symlinks first; use `sandbox.mounts[i].resolvedPath` for `cwd` and env.
- A lost control connection stops nothing. The client reconnects and serves its forwards
  again. A server restart ends every sandbox.
- `sandbox.info()`, `listSandboxes()`, `capabilities()` and `selftest()` return the server's
  own types, with the field names of the wire (`sandbox_id`, `forwards[].refused`, ...).

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
End-to-end tests against a real server on Docker are in `test/e2e` (`pnpm test:e2e`; see its README
for the binaries and variables they need).
