import assert from "node:assert/strict";
import { once } from "node:events";
import { test } from "node:test";

import { connectClient, versionMismatch } from "../../src/client.ts";
import { ConnectionError, InvalidSpecError, ServerError } from "../../src/errors.ts";
import { connect } from "../../src/index.ts";
import { FakeRpcError, FakeServer } from "../../src/testing/fake-server.ts";
import { setup, sleep, until } from "./helpers.ts";

test("connect says hello and reports the server", async (t) => {
  const { server, client } = await setup(t, { client: { clientName: "unit" } });
  assert.equal(client.connected, true);
  assert.deepEqual(client.server, { version: "0.0.0-fake", apiVersion: "1.0", features: ["forwards", "mounts", "selftest"] });
  assert.deepEqual(server.calls[0], { method: "server.hello", params: { client: "unit", api_version: "1.0" } });
});

test("connect rejects when nothing listens, and when the peer is refused", async (t) => {
  await assert.rejects(connect({ socketPath: "/nonexistent/sandcrab.sock" }), (e: unknown) => e instanceof ConnectionError && e.code === "NOT_CONNECTED");
  const server = await FakeServer.start({ unauthorized: true });
  t.after(() => server.close());
  await assert.rejects(connect({ socketPath: server.socketPath }), (e: unknown) => e instanceof ServerError && e.code === "UNAUTHORIZED");
});

test("a pre-aborted signal rejects connect", async (t) => {
  const server = await FakeServer.start();
  t.after(() => server.close());
  await assert.rejects(connect({ socketPath: server.socketPath, signal: AbortSignal.abort() }), { code: "ABORTED" });
});

test("a server one minor behind: connect resolves, nothing starts, the reason names both versions", async (t) => {
  // The SDK plays one that speaks and needs 1.1; the server is at 1.0 and accepts it.
  const { client } = await setup(t, { server: { apiVersion: "1.0" }, internals: { apiVersion: "1.1", minServerVersion: "1.1" } });
  assert.equal(client.connected, false);
  assert.equal(client.server?.apiVersion, "1.0");
  const snap = client.capabilitiesSnapshot("base");
  assert.equal(snap.isolation.ok, false);
  assert.equal(snap.isolation.reason, "sandcrab server speaks API 1.0, this SDK needs 1.1 or later: upgrade the server");
  await assert.rejects(client.createSandbox({ template: "base" }), (e: unknown) => e instanceof ConnectionError && e.code === "UNSUPPORTED_VERSION");
  const exit = await client.sandbox("sb-1").spawn({ argv: ["true"], env: {} }).exit;
  assert.equal(exit.unavailable, true);
  const out = await client.sandbox("sb-1").exec({ argv: ["true"], env: {}, timeoutMs: 1000 });
  assert.equal(out.unavailable, true);
});

test("a server a major ahead refuses the hello: same outcome, the reason says to upgrade the SDK", async (t) => {
  const { server, client } = await setup(t, { server: { apiVersion: "2.0" } });
  assert.equal(client.connected, false);
  assert.equal(client.server, null);
  assert.equal(client.capabilitiesSnapshot("base").isolation.reason, "sandcrab server speaks API 2.0, this SDK speaks 1.0: upgrade the SDK");
  assert.equal((await client.sandbox("x").spawn({ argv: ["true"], env: {} }).exit).unavailable, true);
  // The server is replaced by one that fits: the client finds it by itself.
  server.apiVersion = "1.0";
  await until(() => client.connected);
  assert.equal(client.capabilitiesSnapshot("base").isolation.reason, 'the capabilities for template "base" are not known yet');
});

test("a server a minor ahead is fine", async (t) => {
  const { client } = await setup(t, { server: { apiVersion: "1.1" } });
  assert.equal(client.connected, true);
  assert.equal(versionMismatch("1.4", "1.1"), null);
  assert.match(versionMismatch("0.9", "1.0") ?? "", /0\.9.*1\.0/);
});

test("server errors are typed", async (t) => {
  const { client } = await setup(t);
  await assert.rejects(client.sandbox("nope").info(), (e: unknown) => {
    assert.ok(e instanceof ServerError);
    assert.equal(e.code, "SANDBOX_NOT_FOUND");
    assert.equal(e.rpcCode, -32102);
    assert.deepEqual(e.data, { sandbox_id: "nope" });
    return true;
  });
});

test("reconnect: events, a new hello, and a read in flight is sent once more", async (t) => {
  const { server, client } = await setup(t);
  const sandbox = await client.createSandbox({ template: "base" });
  let dropped = false;
  server.handle("sandbox.get", (_p, next) => {
    if (dropped) return next();
    dropped = true;
    server.dropControlConnections();
    return new Promise(() => {});
  });
  const events: string[] = [];
  client.on("disconnect", () => events.push("disconnect"));
  client.on("connect", () => events.push("connect"));
  const info = await sandbox.info();
  assert.equal(info.sandbox_id, sandbox.id);
  assert.deepEqual(events, ["disconnect", "connect"]);
  assert.equal(server.calls.filter((c) => c.method === "server.hello").length, 2);
});

test("a write in flight is not retried, and a call while disconnected fails at once", async (t) => {
  const { server, client } = await setup(t, { client: { reconnect: false } });
  server.handle("sandbox.create", () => {
    server.dropControlConnections();
    return new Promise(() => {});
  });
  await assert.rejects(client.createSandbox({ template: "base" }), { code: "CONNECTION_LOST" });
  assert.equal(client.connected, false);
  await assert.rejects(client.listSandboxes(), { code: "NOT_CONNECTED" });
  assert.deepEqual(client.capabilitiesSnapshot("base").isolation, { ok: false, reason: "not connected to the sandcrab server" });
  await sleep(30);
  assert.equal(client.connected, false, "reconnect: false stays disconnected");
});

test("the client comes back after a server restart", async (t) => {
  const { server, client } = await setup(t);
  await server.stop();
  await once(client, "disconnect");
  await server.listen();
  await once(client, "connect");
  assert.deepEqual(await client.listSandboxes(), []);
});

test("close rejects what is pending and later calls", async (t) => {
  const { server, client } = await setup(t);
  server.handle("sandbox.list", () => new Promise(() => {}));
  const pending = client.listSandboxes();
  await until(() => server.calls.some((c) => c.method === "sandbox.list"));
  await client.close();
  await assert.rejects(pending, { code: "CLOSED" });
  await assert.rejects(client.listSandboxes(), { code: "CLOSED" });
  assert.equal((await client.sandbox("x").spawn({ argv: ["true"], env: {} }).exit).unavailable, true);
});

test("capabilities, self-test and the synchronous snapshot", async (t) => {
  const { server, client } = await setup(t);
  const caps = await client.capabilities();
  assert.equal(caps.backend, "fake");
  assert.equal(caps.selftest, null);
  assert.deepEqual(client.capabilitiesSnapshot("base"), { backend: "fake", isolation: { ok: false, reason: 'the capabilities for template "base" are not known yet' }, ageMs: null });
  // Reading the snapshot asked the server in the background.
  await until(() => client.capabilitiesSnapshot("base").isolation.reason === 'no self-test has run for template "base"');

  const report = await client.selftest("base", { maxAgeMs: 60_000 });
  assert.equal(report.ok, true);
  assert.deepEqual(server.calls.at(-1), { method: "server.selftest", params: { template: "base", max_age_secs: 60 } });
  const snap = client.capabilitiesSnapshot("base");
  assert.deepEqual(snap.isolation, { ok: true, reason: null });
  assert.equal(typeof snap.ageMs, "number");

  server.selftestResult = { ok: false, reason: "net-external-refused: inconclusive" };
  await client.selftest("other");
  assert.deepEqual(client.capabilitiesSnapshot("other").isolation, { ok: false, reason: "net-external-refused: inconclusive" });
  assert.equal((await client.capabilities("other")).selftest?.ok, false);
  await assert.rejects(client.selftest(""), InvalidSpecError);

  server.handle("server.selftest", () => {
    throw new FakeRpcError(-32106, "template `x`: NotFound", { kind: "not_found", template: "x" });
  });
  await assert.rejects(client.selftest("x"), { code: "TEMPLATE_ERROR" });
});

test("after a reconnect the snapshot forgets what another server proved", async (t) => {
  const { server, client } = await setup(t);
  await client.selftest("base");
  assert.equal(client.capabilitiesSnapshot("base").isolation.ok, true);
  await server.stop();
  await once(client, "disconnect");
  assert.equal(client.capabilitiesSnapshot("base").isolation.ok, false);
  await server.listen();
  await once(client, "connect");
  assert.equal(client.capabilitiesSnapshot("base").isolation.ok, false);
  await until(() => client.capabilitiesSnapshot("base").isolation.reason === 'no self-test has run for template "base"');
});

test("events: sandboxDestroyed for a destroy and a TTL expiry, shutdown with its grace; a throwing listener breaks nothing", async (t) => {
  const { server, client } = await setup(t);
  const a = await client.createSandbox({ template: "base" });
  const b = await client.createSandbox({ template: "base" });
  const destroyed: { sandboxId: string; reason: string }[] = [];
  const shutdowns: { graceMs: number }[] = [];
  client.on("sandboxDestroyed", () => {
    throw new Error("a caller's bug");
  });
  client.on("sandboxDestroyed", (e) => destroyed.push(e));
  client.on("shutdown", (e) => shutdowns.push(e));
  await a.destroy();
  assert.equal(server.expire(b.id), true);
  await until(() => destroyed.length === 2);
  assert.deepEqual(destroyed, [
    { sandboxId: a.id, reason: "destroyed" },
    { sandboxId: b.id, reason: "ttl" },
  ]);
  assert.deepEqual(await client.listSandboxes(), [], "the connection survived the throwing listener");
  await server.shutdown(1500);
  await until(() => shutdowns.length === 1);
  assert.deepEqual(shutdowns, [{ graceMs: 1500 }]);
});
