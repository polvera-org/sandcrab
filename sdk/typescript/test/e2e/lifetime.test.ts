// Sandbox lifetime through the public SDK (design §5.7, §7, §15 `ts-e2e`): labels, list by
// label and time, destroy by label and its `empty`, TTL expiry with the `sandboxDestroyed`
// event, a sandbox that outlives a client killed with SIGKILL, and a server restart that ends
// every run and leaves nothing of the old server.

import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";

import { ServerError } from "../../src/index.ts";
import type { SandboxDestroyedEvent, SandcrabClient, ServerShutdownEvent } from "../../src/index.ts";
import { BACKEND, deadline, PATH_ENV, SKIP, sleep, TEMPLATE, TestServer, until } from "./harness.ts";

const ENV = { PATH: PATH_ENV };
const HERE = path.dirname(fileURLToPath(import.meta.url));

/** What the backend still runs on the host for these sandbox ids (containers, or libkrun helpers). */
function hostLeftovers(ids: string[]): string[] {
  if (BACKEND === "docker") {
    return ids.flatMap((id) => {
      const out = execFileSync("docker", ["ps", "-aq", "--filter", `label=sandcrab.sandbox_id=${id}`], { encoding: "utf8" }).trim();
      return out === "" ? [] : [`container ${out} of ${id}`];
    });
  }
  return krunHelpers();
}

/** `pid` of every `sandcrab-krun` helper of this uid. */
function krunHelpers(): string[] {
  const uid = process.getuid?.();
  const out: string[] = [];
  for (const p of readdirSync("/proc")) {
    if (!/^\d+$/.test(p)) continue;
    try {
      const cmd = readFileSync(`/proc/${p}/cmdline`, "utf8").split("\0")[0] ?? "";
      const status = readFileSync(`/proc/${p}/status`, "utf8");
      const owner = /^Uid:\s+(\d+)/m.exec(status)?.[1];
      if (path.basename(cmd) === "sandcrab-krun" && Number(owner) === uid) out.push(`helper pid ${p}`);
    } catch {
      // gone meanwhile
    }
  }
  return out;
}

describe(`sandbox lifetime on a real server (${BACKEND})`, { skip: SKIP, timeout: 600_000 }, () => {
  test("labels; list by label and createdAfter; destroyByLabel answers empty only when nothing is left", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());

    const a1 = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime", group: "g1", n: "1" } });
    await sleep(50);
    const t0 = new Date();
    await sleep(50);
    const a2 = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime", group: "g1", n: "2" } });
    const b = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime", group: "g2" } });
    t.after(() => b.destroy().catch(() => {}));

    const g1 = await client.listSandboxes({ labels: { group: "g1" } });
    assert.deepEqual(g1.map((i) => i.sandbox_id).sort(), [a1.id, a2.id].sort());
    assert.deepEqual(g1.find((i) => i.sandbox_id === a1.id)?.labels, { e2e: "lifetime", group: "g1", n: "1" });
    assert.deepEqual(
      (await client.listSandboxes({ labels: { group: "g1" }, createdAfter: t0 })).map((i) => i.sandbox_id),
      [a2.id],
    );
    assert.deepEqual(
      (await client.listSandboxes({ labels: { e2e: "lifetime" }, createdBefore: t0 })).map((i) => i.sandbox_id),
      [a1.id],
    );
    assert.deepEqual(await client.listSandboxes({ labels: { group: "g1", n: "3" } }), []);

    const r = await client.destroyByLabel({ group: "g1" });
    assert.deepEqual(r, { matched: 2, destroyed: 2, remaining: [], empty: true });
    assert.deepEqual(await client.listSandboxes({ labels: { group: "g1" } }), [], "empty means a list finds nothing");
    assert.deepEqual((await client.listSandboxes({ labels: { e2e: "lifetime" } })).map((i) => i.sandbox_id), [b.id], "other labels untouched");
    assert.deepEqual(await client.destroyByLabel({ group: "g1" }), { matched: 0, destroyed: 0, remaining: [], empty: true });
    await assert.rejects(a1.info(), (e: unknown) => e instanceof ServerError && e.code === "SANDBOX_NOT_FOUND");

    // A create that races the destroy: `empty` is true only if the second look found nothing.
    const racing = await Promise.all([client.destroyByLabel({ group: "g2" }), client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime", group: "g2" } })]);
    const left = await client.listSandboxes({ labels: { group: "g2" } });
    if (racing[0].empty) assert.ok(left.every((i) => i.sandbox_id === racing[1].id), "empty, yet an older sandbox is left");
    else assert.ok(racing[0].remaining.length > 0);
    assert.deepEqual((await client.destroyByLabel({ group: "g2" })).empty, true);
    assert.deepEqual(await client.listSandboxes({ labels: { e2e: "lifetime" } }), []);
  });

  test("TTL: expiry sends sandboxDestroyed {reason: ttl}; keepAlive holds a sandbox past its TTL", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const events: SandboxDestroyedEvent[] = [];
    client.on("sandboxDestroyed", (e) => events.push(e));

    const kept = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime" }, ttlMs: 2000 });
    const stop = kept.keepAlive(2000);
    const short = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "lifetime" }, ttlMs: 2000 });
    const t0 = Date.now();
    await until(() => events.some((e) => e.sandboxId === short.id), 15_000, "the TTL expiry event");
    console.log(`lifetime (${BACKEND}): TTL 2 s, sandboxDestroyed after ${Date.now() - t0} ms`);
    assert.deepEqual(
      events.find((e) => e.sandboxId === short.id),
      { sandboxId: short.id, reason: "ttl" },
    );
    await assert.rejects(short.info(), (e: unknown) => e instanceof ServerError && e.code === "SANDBOX_NOT_FOUND");
    await sleep(Math.max(0, 5000 - (Date.now() - t0)));
    assert.ok((await kept.info()).sandbox_id === kept.id, "keepAlive renewed it past its TTL");
    stop();
    assert.equal(await kept.destroy(), true);
    await until(() => events.some((e) => e.sandboxId === kept.id), 5000, "the destroy event");
    assert.equal(events.find((e) => e.sandboxId === kept.id)?.reason, "destroyed");
  });

  test("a client killed with SIGKILL mid-run: its sandbox lives on, a new client finds it by label and reaps it", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const run = `run-${process.pid}-${Date.now()}`;
    const child = spawn(process.execPath, [path.join(HERE, "fixtures", "doomed-client.ts"), server.socketPath, run, TEMPLATE], { stdio: ["ignore", "pipe", "inherit"] });
    t.after(() => child.kill("SIGKILL"));
    const line = await deadline(
      new Promise<string>((resolve, reject) => {
        let buf = "";
        child.stdout.on("data", (d: Buffer) => {
          buf += d.toString();
          if (buf.includes("\n")) resolve(buf.split("\n")[0] ?? "");
        });
        child.once("exit", (code) => reject(new Error(`the doomed client exited early (${code})`)));
      }),
      60_000,
      "the doomed client's sandbox",
    );
    const { id } = JSON.parse(line) as { id: string };
    child.kill("SIGKILL");
    await new Promise((r) => child.once("exit", r));

    const client = await server.connect();
    t.after(() => client.close());
    const found = await client.listSandboxes({ labels: { run } });
    assert.deepEqual(
      found.map((i) => i.sandbox_id),
      [id],
    );
    const r = await client.sandbox(id).exec({ argv: ["/bin/echo", "still here"], env: ENV, timeoutMs: 20_000 });
    assert.equal(Buffer.from(r.stdout).toString(), "still here\n", JSON.stringify(r));
    assert.deepEqual(await client.destroyByLabel({ run }), { matched: 1, destroyed: 1, remaining: [], empty: true });
    assert.deepEqual(hostLeftovers([id]).filter((l) => l.startsWith("container")), []);
  });

  test("16 sandboxes at once: all run, one destroyByLabel ends them all", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    // 512 MiB each: 16 microVMs of the template's 1.5 GiB would not fit a shared test host.
    const t0 = Date.now();
    const sbs = await Promise.all(Array.from({ length: 16 }, (_, i) => client.createSandbox({ template: TEMPLATE, labels: { e2e: "sixteen", n: String(i) }, resources: { memoryMb: 512 } })));
    const created = Date.now() - t0;
    const outs = await Promise.all(sbs.map((sb, i) => sb.exec({ argv: ["/bin/echo", String(i)], env: ENV, timeoutMs: 30_000 })));
    assert.deepEqual(
      outs.map((r) => Buffer.from(r.stdout).toString().trim()),
      sbs.map((_, i) => String(i)),
    );
    console.log(`lifetime (${BACKEND}): 16 sandboxes created at once in ${created} ms`);
    assert.equal(new Set(sbs.map((s) => s.id)).size, 16);
    assert.deepEqual(await client.destroyByLabel({ e2e: "sixteen" }), { matched: 16, destroyed: 16, remaining: [], empty: true });
    assert.deepEqual(await client.listSandboxes(), []);
  });

  test("a second uid cannot see the sandbox", { skip: "needs a second local user (sudo); covered by sandcrab-server's unit test each_uid_sees_and_touches_only_its_own_sandboxes" }, () => {});

  test("a server restart: shutdown event, the run ends, the client reconnects, and the new server leaves nothing of the old", async (t) => {
    const helpersBefore = BACKEND === "libkrun" ? krunHelpers().length : 0;
    let server = await TestServer.start();
    const dir = server.dir;
    t.after(() => server.close());
    const client: SandcrabClient = await server.connect();
    t.after(() => client.close());
    const shutdowns: ServerShutdownEvent[] = [];
    client.on("shutdown", (e) => shutdowns.push(e));
    const sbs = await Promise.all([1, 2].map(() => client.createSandbox({ template: TEMPLATE, labels: { e2e: "restart" } })));
    const p = sbs[0]!.spawn({ argv: ["/bin/sleep", "600"], env: ENV });
    assert.equal(await deadline(p.started, 30_000, "start"), true);
    if (BACKEND === "libkrun") assert.ok(krunHelpers().length >= helpersBefore + 2, "a helper per sandbox while they run");

    const stopped = await server.stop();
    assert.equal(stopped.code, 0, "a clean stop");
    const exit = await deadline(p.exit, 20_000, "the exit after the stop");
    assert.ok(exit.cancelled || exit.lost, JSON.stringify(exit));
    assert.equal(exit.unavailable, false);
    assert.equal(shutdowns.length, 1, "the shutdown event arrived");
    assert.ok(shutdowns[0]!.graceMs >= 0);

    server = await TestServer.start({ dir });
    await until(() => client.connected, 15_000, "the client reconnects to the new server");
    assert.deepEqual(await client.listSandboxes(), [], "the new server lists nothing of the old");
    await until(() => hostLeftovers(sbs.map((s) => s.id)).length <= (BACKEND === "libkrun" ? helpersBefore : 0), 15_000, `host leftovers: ${hostLeftovers(sbs.map((s) => s.id)).join(", ")}`);
    // And it works.
    const fresh = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "restart" } });
    const r = await fresh.exec({ argv: ["/bin/true"], env: ENV, timeoutMs: 20_000 });
    assert.equal(r.code, 0);
    await fresh.destroy();
  });
});
