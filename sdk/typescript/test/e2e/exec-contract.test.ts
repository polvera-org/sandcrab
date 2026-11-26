// The bounded `exec` against a real server (design §10.4, §13, §15 `ts-contract`):
// timeout vs abort, the output cap, live output, input and output streams, warm exec numbers.

import assert from "node:assert/strict";
import { Readable, Writable } from "node:stream";
import { after, afterEach, before, describe, test } from "node:test";

import type { Sandbox, SandcrabClient } from "../../src/index.ts";
import { BACKEND, leftovers, mixedText, NODE, PATH_ENV, pct, SKIP, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

describe(`exec contract on a real server (${BACKEND})`, { skip: SKIP, timeout: 600_000 }, () => {
  let server: TestServer;
  let client: SandcrabClient;
  let sb: Sandbox;

  before(async () => {
    server = await TestServer.start();
    client = await server.connect();
    sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "exec" } });
  });

  after(async () => {
    await sb?.destroy().catch(() => {});
    await client?.close();
    await server?.close();
  });

  afterEach(async () => {
    assert.deepEqual(await leftovers(sb), [], "a program is still running in the sandbox");
  });

  test("the bound: timed out, not aborted", async () => {
    const t0 = Date.now();
    const r = await sb.exec({ argv: ["/bin/sleep", "60"], env: ENV, timeoutMs: 500, killGraceMs: 100 });
    assert.equal(r.timedOut, true, JSON.stringify(r));
    assert.equal(r.aborted, false);
    assert.equal(r.treeClean, true);
    assert.ok(Date.now() - t0 < 10_000);
  });

  test("a signal aborted before the call starts nothing", async () => {
    const marker = `/tmp/pre-aborted-${process.pid}`;
    const r = await sb.exec({ argv: ["/usr/bin/touch", marker], env: ENV, timeoutMs: 10_000, signal: AbortSignal.abort() });
    assert.equal(r.aborted, true);
    assert.equal(r.timedOut, false);
    assert.equal(r.code, null);
    const check = await sb.exec({ argv: ["/bin/sh", "-c", `test -e ${marker} && echo ran || echo nothing`], env: ENV, timeoutMs: 10_000 });
    assert.equal(Buffer.from(check.stdout).toString().trim(), "nothing");
  });

  test("a signal aborted right after the call, and one aborted mid-run: aborted, never timed out", async () => {
    const ac = new AbortController();
    const p = sb.exec({ argv: ["/bin/sleep", "60"], env: ENV, timeoutMs: 60_000, killGraceMs: 100, signal: ac.signal });
    ac.abort();
    const r = await p;
    assert.equal(r.aborted, true, JSON.stringify(r));
    assert.equal(r.timedOut, false);

    const ac2 = new AbortController();
    const p2 = sb.exec({ argv: ["/bin/sleep", "60"], env: ENV, timeoutMs: 60_000, killGraceMs: 100, signal: ac2.signal });
    setTimeout(() => ac2.abort(), 500);
    const r2 = await p2;
    assert.equal(r2.aborted, true, JSON.stringify(r2));
    assert.equal(r2.timedOut, false);
    assert.equal(r2.treeClean, true);
    assert.equal(r2.lost, false);
  });

  test("the output cap with kill", async () => {
    const r = await sb.exec({ argv: ["/bin/sh", "-c", "head -c 4194304 /dev/zero; sleep 30"], env: ENV, timeoutMs: 30_000, maxOutputBytes: 1000, overflow: "kill", killGraceMs: 100 });
    assert.equal(r.overflow, true, JSON.stringify({ ...r, stdout: r.stdout.length }));
    assert.ok(r.stdout.length <= 1000, `${r.stdout.length} bytes kept`);
    assert.equal(r.timedOut, false);
    assert.equal(r.code, null);
  });

  test("the output cap with tail keeps the last bytes", async () => {
    const r = await sb.exec({ argv: ["/bin/sh", "-c", "seq 1 100000"], env: ENV, timeoutMs: 30_000, maxOutputBytes: 100, overflow: "tail" });
    const full = Array.from({ length: 100000 }, (_, i) => `${i + 1}\n`).join("");
    assert.equal(r.code, 0);
    assert.equal(r.overflow, true);
    assert.equal(Buffer.from(r.stdout).toString(), full.slice(-100));
  });

  test("a tail cap of 100 bytes with onOutput: every byte arrives live", async () => {
    const script = "echo first; sleep 1; seq 1 50000; echo err-line >&2";
    const chunks: { at: number; stream: string; data: Buffer }[] = [];
    const t0 = Date.now();
    const r = await sb.exec({
      argv: ["/bin/sh", "-c", script],
      env: ENV,
      timeoutMs: 30_000,
      maxOutputBytes: 100,
      overflow: "tail",
      onOutput: (chunk, stream) => chunks.push({ at: Date.now(), stream, data: Buffer.from(chunk) }),
    });
    const done = Date.now();
    const out = Buffer.concat(chunks.filter((c) => c.stream === "stdout").map((c) => c.data)).toString();
    const expected = `first\n${Array.from({ length: 50000 }, (_, i) => `${i + 1}\n`).join("")}`;
    assert.equal(out.length, expected.length, "every stdout byte reached onOutput");
    assert.equal(out, expected);
    assert.equal(Buffer.from(r.stdout).toString(), expected.slice(-100));
    assert.equal(r.overflow, true);
    assert.equal(r.code, 0);
    const first = chunks[0];
    assert.ok(first && first.data.toString().startsWith("first"), "the first chunk is the first line");
    assert.ok(done - first.at >= 700, `the first line came ${done - first.at} ms before the end: not live`);
    assert.ok(first.at - t0 < done - t0);
    // stderr is streamed too, tagged, and still in the tail.
    assert.equal(Buffer.concat(chunks.filter((c) => c.stream === "stderr").map((c) => c.data)).toString(), "err-line\n");
    assert.equal(r.stderr, "err-line\n");
  });

  test("input from a string, bytes and a Readable; output to a Writable", async () => {
    const data = mixedText(512 * 1024);
    const s = await sb.exec({ argv: ["/bin/cat"], env: ENV, input: data.toString("utf8"), timeoutMs: 30_000 });
    assert.ok(Buffer.from(s.stdout).equals(data));
    const b = await sb.exec({ argv: ["/bin/cat"], env: ENV, input: new Uint8Array(data), timeoutMs: 30_000 });
    assert.ok(Buffer.from(b.stdout).equals(data));
    const parts = Array.from({ length: 64 }, (_, i) => data.subarray(i * 8192, (i + 1) * 8192));
    const rd = Readable.from(parts);
    const got: Buffer[] = [];
    let finished = false;
    const wr = new Writable({
      highWaterMark: 1024,
      write(chunk: Buffer, _enc, cb) {
        got.push(Buffer.from(chunk));
        // A slow sink: drain is waited for.
        setTimeout(cb, 1);
      },
      final(cb) {
        finished = true;
        cb();
      },
    });
    const r = await sb.exec({ argv: ["/bin/cat"], env: ENV, input: rd, output: wr, timeoutMs: 60_000 });
    assert.equal(r.code, 0, JSON.stringify({ ...r, stdout: r.stdout.length }));
    assert.equal(r.stdout.length, 0, "stdout went to the stream, not the result");
    assert.ok(finished, "exec resolved after the stream finished");
    assert.ok(Buffer.concat(got).equals(data));
  });

  test("stderr merge and tail in exec", async () => {
    const m = await sb.exec({ argv: ["/bin/sh", "-c", "echo a; echo b >&2; echo c"], env: ENV, stderr: "merge", timeoutMs: 10_000 });
    assert.equal(Buffer.from(m.stdout).toString(), "a\nb\nc\n");
    assert.equal(m.stderr, "");
    const t = await sb.exec({ argv: [NODE, "-e", "process.stderr.write('x'.repeat(10000) + 'END')"], env: ENV, stderrTailBytes: 8, timeoutMs: 10_000 });
    assert.equal(t.stderr, "xxxxxEND");
  });

  test("warm exec of /bin/true: p50 and p95 timed at the client", async () => {
    const one = async (): Promise<number> => {
      const t = performance.now();
      const r = await sb.exec({ argv: ["/bin/true"], env: ENV, timeoutMs: 10_000 });
      const d = performance.now() - t;
      assert.equal(r.code, 0, JSON.stringify(r));
      return d;
    };
    for (let i = 0; i < 20; i++) await one();
    const seq: number[] = [];
    for (let i = 0; i < 200; i++) seq.push(await one());
    seq.sort((a, b) => a - b);
    const conc: number[] = [];
    await Promise.all(
      Array.from({ length: 8 }, async () => {
        for (let i = 0; i < 50; i++) conc.push(await one());
      }),
    );
    conc.sort((a, b) => a - b);
    const f = (n: number): string => `${n.toFixed(1)} ms`;
    console.log(`warm exec /bin/true through the SDK, 200 sequential: p50 ${f(pct(seq, 50))}, p95 ${f(pct(seq, 95))}, max ${f(seq[seq.length - 1] ?? 0)}`);
    console.log(`warm exec /bin/true through the SDK, 8 x 50 concurrent: p50 ${f(pct(conc, 50))}, p95 ${f(pct(conc, 95))}, max ${f(conc[conc.length - 1] ?? 0)}`);
    // The loose bound of design §13; the target (p50 at or under 30 ms) is reported, not asserted.
    assert.ok(pct(seq, 95) < 150, `sequential p95 ${f(pct(seq, 95))} is over 150 ms`);
  });
});
