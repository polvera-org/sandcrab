import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import type { Duplex } from "node:stream";
import { test } from "node:test";
import type { TestContext } from "node:test";

import { InvalidSpecError } from "../../src/errors.ts";
import { collect, pattern, setup, until } from "./helpers.ts";

/** A local server that answers each connection with "got:" + what it received, after end-of-file. */
async function uploadEcho(t: TestContext, listen: number | string): Promise<net.Server> {
  const srv = net.createServer({ allowHalfOpen: true }, (sock) => {
    const chunks: Buffer[] = [];
    sock.on("data", (c: Buffer) => chunks.push(c));
    sock.on("end", () => sock.end(Buffer.concat([Buffer.from("got:"), ...chunks])));
    sock.on("error", () => {});
  });
  if (typeof listen === "number") srv.listen(listen, "127.0.0.1");
  else srv.listen(listen);
  await once(srv, "listening");
  t.after(() => srv.close());
  return srv;
}

/** What a program in the sandbox does: send, half-close, read to the end. */
async function roundTrip(guest: net.Socket, data: Buffer): Promise<Buffer> {
  guest.end(data);
  return collect(guest);
}

test("a forward to a loopback TCP port: both directions, upload until end-of-file", async (t) => {
  const { server, client } = await setup(t);
  const srv = await uploadEcho(t, 0);
  const port = (srv.address() as net.AddressInfo).port;
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 8080, maxConnections: 4, target: { port } }] });
  assert.deepEqual(server.calls.at(-1)?.params, { template: "base", forwards: [{ guest_port: 8080, max_connections: 4 }] });
  const data = pattern(2 * 1024 * 1024);
  const guest = await server.forwardIncoming(sandbox.id, 8080);
  assert.deepEqual(await roundTrip(guest, data), Buffer.concat([Buffer.from("got:"), data]));
  // Several at once.
  const many = await Promise.all([1, 2, 3].map(async (i) => roundTrip(await server.forwardIncoming(sandbox.id, 8080), Buffer.from(`c${i}`))));
  assert.deepEqual(many.map(String), ["got:c1", "got:c2", "got:c3"]);
  assert.equal((await sandbox.info()).forwards[0]?.accepted, 4);
});

test("a forward to a Unix socket", async (t) => {
  const { server, client } = await setup(t);
  const dir = mkdtempSync(path.join(tmpdir(), "sc-f-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const socketPath = path.join(dir, "t.sock");
  await uploadEcho(t, socketPath);
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 5432, target: { socketPath } }] });
  const guest = await server.forwardIncoming(sandbox.id, 5432);
  assert.equal((await roundTrip(guest, Buffer.from("hello"))).toString(), "got:hello");
});

test("a forward to an in-process handler, including bytes sent right behind the ack", async (t) => {
  const { server, client } = await setup(t);
  const seen: Duplex[] = [];
  const handler = (socket: Duplex): void => {
    seen.push(socket);
    const chunks: Buffer[] = [];
    socket.on("data", (c: Buffer) => chunks.push(c));
    socket.on("end", () => socket.end(Buffer.concat(chunks).toString().toUpperCase()));
  };
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 3128, target: { handler } }] });
  const guest = await server.forwardIncoming(sandbox.id, 3128);
  assert.equal((await roundTrip(guest, Buffer.from("connect me"))).toString(), "CONNECT ME");
  assert.equal(seen.length, 1);
});

test("fail closed: an undeclared port, an unreachable target and an unserved sandbox are rejected and counted", async (t) => {
  const { server, client } = await setup(t);
  const dead = path.join(tmpdir(), "sandcrab-no-such-target.sock");
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 80, target: { socketPath: dead } }] });
  await assert.rejects(server.forwardIncoming(sandbox.id, 80), /could not be reached/);
  await assert.rejects(server.forwardIncoming(sandbox.id, 80), /could not be reached/);
  const info = await sandbox.info();
  assert.deepEqual(info.forwards, [{ guest_port: 80, accepted: 0, refused: { no_listener: 0, cap: 0, unclaimed: 0, rejected: 2 } }]);

  // The server announces a port the client holds no target for: rejected, nothing is dialled.
  server.sandboxes.get(sandbox.id)?.forwards.push({ guest_port: 81, accepted: 0, refused: { no_listener: 0, cap: 0, unclaimed: 0, rejected: 0 } });
  await assert.rejects(server.forwardIncoming(sandbox.id, 81), /no target/);

  const plain = await client.createSandbox({ template: "base" });
  await assert.rejects(server.forwardIncoming(plain.id, 80), /not served/);
  await assert.rejects(plain.serveForwards([{ guestPort: 0, target: { port: 1 } }]), InvalidSpecError);
});

test("after a reconnect the client serves its forwards again; a new client can take them over", async (t) => {
  const { server, client } = await setup(t);
  const handler = (socket: Duplex): void => void socket.end("v1");
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 80, target: { handler } }] });
  server.dropControlConnections();
  await assert.rejects(server.forwardIncoming(sandbox.id, 80), /not served/);
  await until(() => server.calls.some((c) => c.method === "forward.serve"));
  await until(() => client.connected);
  const guest = await server.forwardIncoming(sandbox.id, 80);
  assert.equal((await roundTrip(guest, Buffer.alloc(0))).toString(), "v1");

  // Another client, as after a restart: it names the targets again.
  const { connectClient } = await import("../../src/client.ts");
  const second = await connectClient({ socketPath: server.socketPath });
  t.after(() => second.close());
  await second.sandbox(sandbox.id).serveForwards([{ guestPort: 80, target: { handler: (s) => void s.end("v2") } }]);
  assert.equal((await roundTrip(await server.forwardIncoming(sandbox.id, 80), Buffer.alloc(0))).toString(), "v2");
});

test("close ends open forwarded connections", async (t) => {
  const { server, client } = await setup(t);
  const sandbox = await client.createSandbox({ template: "base", forwards: [{ guestPort: 80, target: { handler: () => {} } }] });
  const guest = await server.forwardIncoming(sandbox.id, 80);
  guest.on("error", () => {});
  guest.resume();
  const ended = Promise.race([once(guest, "end"), once(guest, "close")]);
  await client.close();
  await ended;
});
