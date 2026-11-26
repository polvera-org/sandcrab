// The server's side of the contract (design §5.7, §5.8, §5.11, §10.4, §10.5): the version and
// feature check, a refused peer, `unavailable` vs `lost`, and a clean server stop.

import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { connectClient } from "../../src/client.ts";
import { API_VERSION, ServerError } from "../../src/index.ts";
import { deadline, NODE, PATH_ENV, SKIP, sleep, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

describe("server contract on a real server", { skip: SKIP, timeout: 300_000 }, () => {
  test("hello: the API version and features; a newer or older SDK is told why", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    assert.equal(client.connected, true);
    assert.equal(client.server?.apiVersion, API_VERSION);
    assert.match(client.server?.version ?? "", /^\d+\.\d+\.\d+/);
    for (const f of ["forwards", "mounts"]) assert.ok(client.server?.features.includes(f), `feature ${f} in ${client.server?.features}`);
    const caps = await client.capabilities();
    assert.equal(caps.backend, "docker");
    assert.equal(caps.hardware_isolated, false);
    assert.equal(caps.supports_forwards, true);

    // An SDK that needs a newer server: connect resolves, nothing starts, the reason names both.
    const needsNewer = await connectClient({ socketPath: server.socketPath, reconnect: false }, { minServerVersion: "1.1" });
    t.after(() => needsNewer.close());
    assert.equal(needsNewer.connected, false);
    const snap = needsNewer.capabilitiesSnapshot(TEMPLATE);
    assert.equal(snap.isolation.ok, false);
    assert.match(snap.isolation.reason ?? "", /1\.0.*1\.1.*upgrade the server/);
    const exit = await needsNewer.sandbox("00000000-0000-4000-8000-000000000000").spawn({ argv: ["/bin/true"], env: {} }).exit;
    assert.equal(exit.unavailable, true);

    // An SDK of another major: the server answers UNSUPPORTED_VERSION.
    const otherMajor = await connectClient({ socketPath: server.socketPath, reconnect: false }, { apiVersion: "2.0", minServerVersion: "2.0" });
    t.after(() => otherMajor.close());
    assert.equal(otherMajor.connected, false);
    assert.match(otherMajor.capabilitiesSnapshot(TEMPLATE).isolation.reason ?? "", /speaks API 1\.0, this SDK speaks 2\.0/);
  });

  test("a peer whose uid is not allowed gets UNAUTHORIZED", async (t) => {
    const uid = process.getuid?.() ?? 0;
    const server = await TestServer.start({ allowUids: [uid + 4242] });
    t.after(() => server.close());
    await assert.rejects(server.connect(), (e: unknown) => e instanceof ServerError && e.code === "UNAUTHORIZED");
  });

  test("a server down before the spawn: unavailable, nothing ran", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const sb = await client.createSandbox({ template: TEMPLATE });
    await server.stop();
    const p = sb.spawn({ argv: ["/bin/true"], env: ENV });
    const exit = await deadline(p.exit, 10_000, "exit");
    assert.equal(exit.unavailable, true, JSON.stringify(exit));
    assert.equal(exit.lost, false);
    assert.equal(await p.started, false);
    const r = await sb.exec({ argv: ["/bin/true"], env: ENV, timeoutMs: 5_000 });
    assert.equal(r.unavailable, true);
  });

  test("a server killed mid-process: lost, not unavailable; a new server removes what was left", async (t) => {
    let server = await TestServer.start();
    const dir = server.dir;
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lost" } });
    const p = sb.spawn({ argv: [NODE, "-e", "console.log('up'); setInterval(() => {}, 1000)"], env: ENV });
    const it = p.stdout[Symbol.asyncIterator]();
    const first = await deadline(it.next(), 20_000, "first output");
    assert.equal(Buffer.from(first.value ?? []).toString(), "up\n");
    assert.equal(await p.started, true);
    await server.kill();
    const exit = await deadline(p.exit, 10_000, "exit after the server died");
    assert.equal(exit.lost, true, JSON.stringify(exit));
    assert.equal(exit.unavailable, false);
    assert.equal(exit.treeClean, false, "not known to be stopped");
    // The control connection is down; reads fail rather than hang.
    await sleep(100);
    assert.equal(client.connected, false);

    // A new server on the same state directory reaps the orphaned container at start.
    server = await TestServer.start({ dir });
    const again = await server.connect();
    t.after(() => again.close());
    assert.deepEqual(await again.listSandboxes({ labels: { e2e: "lost" } }), [], "a restarted server lists no old sandbox");
    await deadline(
      (async () => {
        while (!client.connected) await sleep(50);
      })(),
      10_000,
      "the first client reconnects",
    );
  });

  test("a clean server stop mid-process: the process is cancelled, never unavailable", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const sb = await client.createSandbox({ template: TEMPLATE });
    const p = sb.spawn({ argv: ["/bin/sleep", "600"], env: ENV });
    assert.equal(await deadline(p.started, 20_000, "start"), true);
    const stopped = await server.stop();
    assert.equal(stopped.code, 0, "the server stopped cleanly");
    const exit = await deadline(p.exit, 10_000, "exit after stop");
    assert.equal(exit.unavailable, false, JSON.stringify(exit));
    assert.ok(exit.cancelled || exit.lost, JSON.stringify(exit));
  });
});
