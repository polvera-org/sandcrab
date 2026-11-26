import type { TestContext } from "node:test";

import { connectClient } from "../../src/client.ts";
import type { ClientImpl, ConnectInternals, ConnectOptions } from "../../src/client.ts";
import type { Sandbox } from "../../src/index.ts";
import { FakeServer } from "../../src/testing/fake-server.ts";
import type { FakeProcess, FakeServerOptions } from "../../src/testing/fake-server.ts";

/** A fake server and a client connected to it, both closed when the test ends. */
export async function setup(t: TestContext, o: { server?: FakeServerOptions; client?: Partial<ConnectOptions>; internals?: ConnectInternals } = {}): Promise<{ server: FakeServer; client: ClientImpl }> {
  const server = await FakeServer.start(o.server);
  t.after(() => server.close());
  const client = await connectClient({ socketPath: server.socketPath, reconnect: { minMs: 5, maxMs: 20 }, ...o.client }, o.internals);
  t.after(() => client.close());
  return { server, client };
}

export async function withSandbox(t: TestContext): Promise<{ server: FakeServer; client: ClientImpl; sandbox: Sandbox }> {
  const { server, client } = await setup(t);
  const sandbox = await client.createSandbox({ template: "base" });
  return { server, client, sandbox };
}

/** The next process the fake server sees, with the script paused until the test drives it. */
export function nextProcess(server: FakeServer): Promise<FakeProcess> {
  return new Promise((resolve) => server.onSpawn((p) => resolve(p)));
}

export async function collect(stream: AsyncIterable<Uint8Array>): Promise<Buffer> {
  const chunks: Uint8Array[] = [];
  for await (const chunk of stream) chunks.push(chunk);
  return Buffer.concat(chunks);
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function until(cond: () => boolean, ms = 2000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > deadline) throw new Error("condition not reached in time");
    await sleep(2);
  }
}

/** Bytes that make reordering or loss visible. */
export function pattern(n: number): Buffer {
  const b = Buffer.allocUnsafe(n);
  for (let i = 0; i < n; i++) b[i] = (i * 31 + (i >>> 8)) & 0xff;
  return b;
}
