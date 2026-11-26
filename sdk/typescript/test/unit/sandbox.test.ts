import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";

import { AbortedError, InvalidSpecError } from "../../src/errors.ts";
import type { SandboxSpec } from "../../src/index.ts";
import { setup, sleep, until } from "./helpers.ts";

test("create sends the spec in wire form and resolves mount paths", async (t) => {
  const { server, client } = await setup(t);
  const dir = mkdtempSync(path.join(tmpdir(), "sc-m-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  mkdirSync(path.join(dir, "real"));
  symlinkSync(path.join(dir, "real"), path.join(dir, "link"));
  const asked = path.join(dir, "link");
  const resolved = realpathSync(asked);

  const sandbox = await client.createSandbox({
    template: "base",
    labels: { run: "r1", "app.kind": "check" },
    ttlMs: 1500,
    mounts: [{ path: asked, mode: "rw" }],
    network: "disabled",
    user: { uid: 1500, gid: 1500 },
    resources: { vcpu: 2, memoryMb: 512, pidsMax: 256, nofile: 1024 },
    tmpExec: true,
    tmpSizeMb: 64,
  });
  assert.deepEqual(sandbox.mounts, [{ path: asked, resolvedPath: resolved, mode: "rw" }]);
  assert.deepEqual(server.calls.at(-1), {
    method: "sandbox.create",
    params: {
      template: "base",
      labels: { run: "r1", "app.kind": "check" },
      ttl_secs: 2,
      network: "disabled",
      user: "1500:1500",
      resources: { vcpu: 2, memory_mb: 512, pids_max: 256, nofile: 1024 },
      tmp_exec: true,
      tmp_size_mb: 64,
      mounts: [{ path: resolved, mode: "rw" }],
    },
  });
  const info = await sandbox.info();
  assert.equal(info.sandbox_id, sandbox.id);
  assert.deepEqual(info.labels, { run: "r1", "app.kind": "check" });

  await client.createSandbox({ template: "base", ttlMs: null });
  assert.deepEqual(server.calls.at(-1)?.params, { template: "base", ttl_secs: null });
});

test("a malformed spec is an InvalidSpecError and nothing is sent", async (t) => {
  const { server, client } = await setup(t);
  const bad: SandboxSpec[] = [
    { template: "" },
    { template: "base", labels: { "Bad Key": "x" } },
    { template: "base", labels: { "sandcrab.owner": "x" } },
    { template: "base", labels: { k: "x".repeat(257) } },
    { template: "base", ttlMs: 0 },
    { template: "base", mounts: [{ path: "relative/dir", mode: "ro" }] },
    { template: "base", mounts: [{ path: "/nonexistent/sandcrab-mount", mode: "ro" }] },
    { template: "base", mounts: [{ path: tmpdir(), mode: "wr" as "rw" }] },
    { template: "base", network: "open" as "disabled" },
    { template: "base", user: { uid: -1, gid: 0 } },
    { template: "base", resources: { vcpu: 0 } },
    { template: "base", tmpSizeMb: 0 },
    { template: "base", forwards: [{ guestPort: 0, target: { port: 80 } }] },
    { template: "base", forwards: [{ guestPort: 80, target: { port: 80, host: "10.0.0.1" as "::1" } }] },
    { template: "base", forwards: [{ guestPort: 80, target: { port: 80 } }, { guestPort: 80, target: { port: 81 } }] },
    { template: "base", forwards: [{ guestPort: 80, target: {} as { port: number } }] },
  ];
  for (const spec of bad) await assert.rejects(client.createSandbox(spec), InvalidSpecError, JSON.stringify(spec));
  assert.equal(server.calls.filter((c) => c.method === "sandbox.create").length, 0);
  await assert.rejects(client.destroyByLabel({}), InvalidSpecError);
  assert.throws(() => client.sandbox(""), InvalidSpecError);
});

test("list by label and time, destroy, destroy by label", async (t) => {
  const { server, client } = await setup(t);
  const a = await client.createSandbox({ template: "base", labels: { run: "1", kind: "a" } });
  await sleep(5);
  const mid = new Date();
  await sleep(5);
  const b = await client.createSandbox({ template: "base", labels: { run: "1", kind: "b" } });
  await client.createSandbox({ template: "base", labels: { run: "2" } });

  assert.deepEqual((await client.listSandboxes({ labels: { run: "1" } })).map((s) => s.sandbox_id), [a.id, b.id]);
  assert.deepEqual((await client.listSandboxes({ labels: { run: "1" }, createdAfter: mid })).map((s) => s.sandbox_id), [b.id]);
  assert.deepEqual((await client.listSandboxes({ createdBefore: mid })).map((s) => s.sandbox_id), [a.id]);
  assert.equal((await client.listSandboxes()).length, 3);
  assert.equal(server.calls.at(-1)?.method, "sandbox.list");
  assert.deepEqual(server.calls.at(-1)?.params, {});

  assert.equal(await a.destroy(), true);
  assert.equal(await a.destroy(), false);
  assert.equal(await client.sandbox(a.id).destroy(), false);
  assert.deepEqual(await client.destroyByLabel({ run: "1" }), { matched: 1, destroyed: 1, remaining: [], empty: true });
  assert.deepEqual((await client.listSandboxes()).map((s) => s.labels.run), ["2"]);

  server.handle("sandbox.destroy_by_label", () => ({ matched: 2, destroyed: 1, remaining: ["sb-9"], empty: false }));
  assert.deepEqual(await client.destroyByLabel({ run: "2" }), { matched: 2, destroyed: 1, remaining: ["sb-9"], empty: false });
});

test("setTtl and keepAlive", async (t) => {
  const { server, client } = await setup(t);
  const sandbox = await client.createSandbox({ template: "base" });
  await sandbox.setTtl(null);
  assert.deepEqual(server.calls.at(-1), { method: "sandbox.set_ttl", params: { sandbox_id: sandbox.id, ttl_secs: null } });
  assert.equal((await sandbox.info()).expires_at, null);
  await sandbox.setTtl(90_000);
  assert.deepEqual(server.calls.at(-1)?.params, { sandbox_id: sandbox.id, ttl_secs: 90 });

  const renewals = (): number => server.calls.filter((c) => c.method === "sandbox.set_ttl" && c.params.ttl_secs === 1).length;
  const stop = sandbox.keepAlive(750);
  await until(() => renewals() >= 3, 3000);
  stop();
  await sleep(20);
  const n = renewals();
  await sleep(350);
  assert.equal(renewals(), n, "stopped");
  assert.throws(() => sandbox.keepAlive(0), InvalidSpecError);
});

test("an aborted create rejects and destroys the sandbox that was still made", async (t) => {
  const { server, client } = await setup(t);
  const ac = new AbortController();
  let release!: () => void;
  server.handle("sandbox.create", async (_p, next) => {
    await new Promise<void>((resolve) => (release = resolve));
    return next();
  });
  const p = client.createSandbox({ template: "base" }, { signal: ac.signal });
  await until(() => server.calls.some((c) => c.method === "sandbox.create"));
  ac.abort();
  await assert.rejects(p, AbortedError);
  release();
  await until(() => server.calls.some((c) => c.method === "sandbox.destroy"));
  await until(() => server.sandboxes.size === 0);
  await assert.rejects(client.createSandbox({ template: "base" }, { signal: ac.signal }), AbortedError);
});
