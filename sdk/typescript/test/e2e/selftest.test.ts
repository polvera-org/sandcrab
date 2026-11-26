// The self-test through the SDK (design §8, §10.3, §15 `ts-e2e`): every line passes on this
// backend, the capabilities snapshot reflects it, a template never tested reads as not proven,
// and a run leaves no scratch directory behind.

import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import path from "node:path";
import { describe, test } from "node:test";

import { BACKEND, SKIP, TEMPLATE, TestServer, until } from "./harness.ts";

const LINES = [
  "caps-empty",
  "no-new-privs",
  "userns-refused",
  "net-loopback-only",
  "net-external-refused",
  "dns-refused",
  "gateway-refused",
  "metadata-refused",
  "forward-reaches-target",
  "forward-only-declared",
  "forward-fails-closed",
  "fs-outside-mounts",
  "other-sandbox-unreachable",
  "exec-env-exact",
  "tree-kill",
];

describe(`self-test through the SDK (${BACKEND})`, { skip: SKIP, timeout: 600_000 }, () => {
  test("every line passes; the snapshot says proven; a template never tested is not proven; no scratch left", async (t) => {
    const root = mkdtempSync(path.join(process.env.SANDCRAB_E2E_DATA ?? process.cwd(), ".e2e-selftest-"));
    t.after(() => rmSync(root, { recursive: true, force: true }));
    const server = await TestServer.start({ mountRoots: [`${root}:rw`] });
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());

    const untested = "base";
    const before = await client.capabilities(TEMPLATE);
    assert.equal(before.selftest, null, "never run yet");
    assert.equal(client.capabilitiesSnapshot(TEMPLATE).isolation.ok, false);

    const t0 = Date.now();
    const report = await client.selftest(TEMPLATE);
    const took = Date.now() - t0;
    console.log(`selftest (${BACKEND}, ${TEMPLATE}) through the server: ${took} ms at the client, ${report.duration_ms} ms in the report`);
    for (const l of report.lines) if (l.status !== "pass") console.log(`  ${l.status} ${l.id}: ${l.detail} ${JSON.stringify(l.evidence)}`);
    assert.equal(report.backend, BACKEND);
    assert.equal(report.template, TEMPLATE);
    assert.deepEqual(
      report.lines.map((l) => [l.id, l.status]),
      LINES.map((id) => [id, "pass"]),
    );
    assert.equal(report.ok, true, report.reason ?? "");
    assert.ok(took < 30_000, `the self-test took ${took} ms`);
    assert.deepEqual(readdirSync(root), [], "the self-test left its scratch directory");

    // The snapshot reflects it: proven, with the backend, and young.
    await until(() => client.capabilitiesSnapshot(TEMPLATE).isolation.ok, 10_000, "the snapshot says proven");
    const snap = client.capabilitiesSnapshot(TEMPLATE);
    assert.deepEqual(snap.isolation, { ok: true, reason: null });
    assert.equal(snap.backend, BACKEND);
    const caps = await client.capabilities(TEMPLATE);
    assert.equal(caps.selftest?.ok, true);
    assert.equal(caps.selftest?.template, TEMPLATE);

    // Asked again within its age: the kept report, not a new run.
    const again = await client.selftest(TEMPLATE, { maxAgeMs: 600_000 });
    assert.equal(again.started_at, report.started_at);

    // Never tested: not proven, and says so.
    const other = await client.capabilities(untested);
    assert.equal(other.selftest, null);
    const otherSnap = client.capabilitiesSnapshot(untested);
    assert.equal(otherSnap.isolation.ok, false);
    assert.match(otherSnap.isolation.reason ?? "", /not proven|never|self-test/i);
  });
});
