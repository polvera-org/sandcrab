import assert from "node:assert/strict";
import { PassThrough, Readable, Writable } from "node:stream";
import { test } from "node:test";

import type { ExecOutput } from "../../src/index.ts";
import type { FakeProcess, FakeServer } from "../../src/testing/fake-server.ts";
import { pattern, sleep, until, withSandbox } from "./helpers.ts";

const BASE: ExecOutput = { code: 0, signal: null, spawnError: null, rejected: null, unavailable: false, lost: false, timedOut: false, overflow: false, treeClean: true, stdout: Buffer.alloc(0), stderr: "", aborted: false };
const last = (server: FakeServer): FakeProcess => [...server.processes.values()].at(-1)!;

test("exec collects stdout and the stderr tail, and asks the guest for the cap", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout("out 1\n");
    await p.stdout("out 2\n");
    p.exit({ code: 2, stderr_tail: "err\n" });
  });
  const out = await sandbox.exec({ argv: ["sh"], env: { A: "1" }, timeoutMs: 5000, killGraceMs: 0 });
  assert.deepEqual(out, { ...BASE, code: 2, stdout: Buffer.from("out 1\nout 2\n"), stderr: "err\n" });
  const { process_id: _, ...params } = last(server).params;
  assert.deepEqual(params, { sandbox_id: sandbox.id, argv: ["sh"], env: { A: "1" }, kill_grace_ms: 0, timeout_ms: 5000, output_cap: { max_bytes: 8 * 1024 * 1024, on_overflow: "kill" } });
});

test("exec outcomes: timeout, overflow, could not start, unavailable, lost", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const spec = { argv: ["x"], env: {}, timeoutMs: 100 };
  server.onSpawn((p) => (p.ack(), p.exit({ code: null, signal: 9, timed_out: true })));
  assert.deepEqual(await sandbox.exec(spec), { ...BASE, code: null, signal: "SIGKILL", timedOut: true });
  server.onSpawn(async (p) => (p.ack(), await p.stdout("12345"), p.exit({ code: null, signal: 15, overflow: true })));
  assert.deepEqual(await sandbox.exec({ ...spec, maxOutputBytes: 5 }), { ...BASE, code: null, signal: "SIGTERM", overflow: true, stdout: Buffer.from("12345") });
  server.onSpawn((p) => p.fail(-32010, "denied", { errno: "EACCES", what: "program" }));
  assert.deepEqual(await sandbox.exec(spec), { ...BASE, code: null, spawnError: { errno: "EACCES", what: "program", message: "denied" } });
  server.onSpawn((p) => p.fail(-32101, "down", { detail: "down" }));
  assert.deepEqual(await sandbox.exec(spec), { ...BASE, code: null, unavailable: true });
  server.onSpawn((p) => (p.ack(), p.drop()));
  assert.deepEqual(await sandbox.exec(spec), { ...BASE, code: null, lost: true, treeClean: false });
});

test("a signal that is already aborted starts nothing", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 100, signal: AbortSignal.abort() });
  assert.deepEqual(out, { ...BASE, code: null, stdout: new Uint8Array(0), aborted: true });
  await sleep(20);
  assert.equal(server.processes.size, 0);
});

test("an abort during exec cancels with the kill grace and says aborted, never timedOut", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.handle("process.cancel", (p) => {
    server.processes.get(p.process_id)?.exit({ code: null, signal: 15, cancelled: true, timed_out: true });
    return { was_running: true, tree_clean: true };
  });
  server.onSpawn(async (p) => (p.ack(), await p.stdout("so far")));
  const ac = new AbortController();
  const outP = sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 60_000, killGraceMs: 123, signal: ac.signal, onOutput: () => ac.abort() });
  const out = await outP;
  assert.deepEqual(out, { ...BASE, code: null, signal: "SIGTERM", stdout: Buffer.from("so far"), aborted: true });
  assert.equal(server.calls.find((c) => c.method === "process.cancel")?.params.grace_ms, 123);

  // Aborted right after the call: nothing is written, the outcome is the same kind.
  const ac2 = new AbortController();
  const p2 = sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 60_000, signal: ac2.signal });
  ac2.abort();
  assert.equal((await p2).aborted, true);
});

test("input as a string, as bytes and as a Readable", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdinEnded;
    await p.stdout(p.stdin);
    p.exit();
  });
  const spec = { argv: ["cat"], env: {}, timeoutMs: 5000 };
  assert.equal(Buffer.from((await sandbox.exec({ ...spec, input: "text ✓" })).stdout).toString(), "text ✓");
  assert.deepEqual((await sandbox.exec({ ...spec, input: new Uint8Array([9, 0, 255]) })).stdout, Buffer.from([9, 0, 255]));
  const big = pattern(3 * 1024 * 1024);
  const chunks = Array.from({ length: 30 }, (_, i) => big.subarray(i * 100_000, (i + 1) * 100_000)).concat(big.subarray(3_000_000));
  assert.deepEqual((await sandbox.exec({ ...spec, input: Readable.from(chunks) })).stdout, big);
  assert.deepEqual((await sandbox.exec({ ...spec, input: Readable.from([]) })).stdout, Buffer.alloc(0));
});

test("a Readable input is read with backpressure, and dropped when the program ends first", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  let pulled = 0;
  const endless = new Readable({
    highWaterMark: 64 * 1024,
    read() {
      pulled += 64 * 1024;
      this.push(Buffer.alloc(64 * 1024));
    },
  });
  server.onSpawn(async (p) => {
    p.ack();
    p.socket.pause(); // A program that does not read its stdin.
    await sleep(200);
    p.exit({ code: 1 });
  });
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, input: endless });
  assert.equal(out.code, 1);
  assert.ok(pulled < 8 * 1024 * 1024, `read ${pulled} bytes from a stream nobody consumed`);
  assert.equal(endless.destroyed, true);
});

test("a failing input stream stops the program and is reported", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn((p) => p.ack());
  let reads = 0;
  const input = new Readable({
    read() {
      if (reads++ === 0) this.push("first");
      else void until(() => server.processes.size === 1 && last(server).stdin.length > 0).then(() => this.destroy(new Error("disk gone")));
    },
  });
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, input });
  assert.deepEqual(out.rejected, { code: "INPUT_FAILED", message: "disk gone" });
  assert.equal(server.calls.filter((c) => c.method === "process.cancel").length, 1);
});

test("output into a Writable: not captured, backpressure reaches the server, resolves after finish", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const data = pattern(6 * 1024 * 1024);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(data);
    p.exit();
  });
  const got: Buffer[] = [];
  let release: (() => void) | null = null;
  let hold = true;
  let finished = false;
  const output = new Writable({
    highWaterMark: 16 * 1024,
    write(chunk: Buffer, _enc, cb) {
      got.push(chunk);
      if (hold) release = cb;
      else cb();
    },
    final(cb) {
      setTimeout(() => ((finished = true), cb()), 20);
    },
  });
  const outP = sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, output });
  await until(() => release !== null);
  const proc = last(server);
  let flushed = -1;
  while (flushed !== proc.flushedBytes) {
    flushed = proc.flushedBytes;
    await sleep(100);
  }
  assert.ok(flushed < 4 * 1024 * 1024, `the writer was not stopped: ${flushed} bytes flushed`);
  hold = false;
  (release as unknown as () => void)();
  const out = await outP;
  assert.equal(finished, true);
  assert.deepEqual(Buffer.concat(got), data);
  assert.deepEqual(out, BASE);
  assert.equal(output.listenerCount("error"), 0, "exec left a listener on the caller's output");
});

test("a failing output stream stops the program and is reported", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => (p.ack(), await p.stdout("x")));
  const output = new Writable({ write: (_c, _e, cb) => cb(new Error("pipe closed")) });
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, output });
  assert.deepEqual(out.rejected, { code: "OUTPUT_FAILED", message: "pipe closed" });
});

test("overflow tail without a live consumer: the guest keeps the tail", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => (p.ack(), await p.stdout("tail!"), p.exit({ overflow: true })));
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, maxOutputBytes: 5, overflow: "tail" });
  assert.deepEqual(last(server).params.output_cap, { max_bytes: 5, on_overflow: "tail" });
  assert.deepEqual(out, { ...BASE, overflow: true, stdout: Buffer.from("tail!") });
});

test("overflow tail with onOutput: every byte arrives live, the SDK keeps the tail, the guest is not asked", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const data = pattern(200_000);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(data.subarray(0, 70_000));
    await p.stderr("e1");
    await p.stdout(data.subarray(70_000));
    await p.stderr("e2");
    p.exit({ stderr_tail: "e1e2" });
  });
  const live: Buffer[] = [];
  const err: string[] = [];
  const out = await sandbox.exec({
    argv: ["x"],
    env: {},
    timeoutMs: 5000,
    maxOutputBytes: 100,
    overflow: "tail",
    onOutput: (chunk, stream) => (stream === "stdout" ? live.push(Buffer.from(chunk)) : err.push(Buffer.from(chunk).toString())),
  });
  assert.equal(last(server).params.output_cap, undefined);
  assert.equal(last(server).params.stderr, "stream");
  assert.deepEqual(Buffer.concat(live), data);
  assert.deepEqual(err, ["e1", "e2"]);
  assert.deepEqual(out, { ...BASE, overflow: true, stdout: data.subarray(data.length - 100), stderr: "e1e2" });

  // Under the cap nothing is dropped and nothing overflows.
  server.onSpawn(async (p) => (p.ack(), await p.stdout("short"), p.exit()));
  const small = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, maxOutputBytes: 100, overflow: "tail", onOutput: () => {} });
  assert.deepEqual(small, { ...BASE, stdout: Buffer.from("short") });
});

test("which cap and stderr mode each option set asks for", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const spec = { argv: ["x"], env: {}, timeoutMs: 5000 };
  const asked = async (o: object): Promise<[unknown, unknown]> => {
    await sandbox.exec({ ...spec, ...o });
    return [last(server).params.output_cap, last(server).params.stderr];
  };
  assert.deepEqual(await asked({ overflow: "tail", maxOutputBytes: 9 * 1024 * 1024 }), [undefined, undefined]);
  assert.deepEqual(await asked({ overflow: "kill", maxOutputBytes: 7, onOutput: () => {} }), [{ max_bytes: 7, on_overflow: "kill" }, "stream"]);
  assert.deepEqual(await asked({ overflow: "tail", maxOutputBytes: 7, output: new PassThrough().resume() }), [undefined, undefined]);
  assert.deepEqual(await asked({ stderr: "merge", onOutput: () => {} }), [{ max_bytes: 8 * 1024 * 1024, on_overflow: "kill" }, "merge"]);
  assert.deepEqual(await asked({ stderr: "tail", stderrTailBytes: 65536 }), [{ max_bytes: 8 * 1024 * 1024, on_overflow: "kill" }, "tail"]);
});

test("a throwing onOutput does not break the exec", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => (p.ack(), await p.stdout("a"), await p.stderr("b"), p.exit()));
  const out = await sandbox.exec({ argv: ["x"], env: {}, timeoutMs: 5000, onOutput: () => { throw new Error("mine"); } }); // prettier-ignore
  assert.deepEqual(out, { ...BASE, stdout: Buffer.from("a") });
});
