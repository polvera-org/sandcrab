import assert from "node:assert/strict";
import { test } from "node:test";

import { InvalidSpecError } from "../../src/errors.ts";
import { encodeFrame, encodeRecord, RECORD_EXIT, RECORD_STDOUT } from "../../src/framing.ts";
import type { ProcessExit, SpawnOptions } from "../../src/index.ts";
import { signalName, STDOUT_HIGH_WATER } from "../../src/process.ts";
import { FakeRpcError } from "../../src/testing/fake-server.ts";
import { collect, nextProcess, pattern, sleep, until, withSandbox } from "./helpers.ts";

const CLEAN: ProcessExit = { code: 0, signal: null, stderrTail: "", spawnError: null, rejected: null, unavailable: false, lost: false, timedOut: false, cancelled: false, overflow: false, treeClean: true };
const ENV = { PATH: "/usr/bin" };

test("a process: the exact spec on the wire, stdout in order, exit after the last byte", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const data = pattern(300_000);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(data.subarray(0, 100_000));
    await p.stdout(data.subarray(100_000));
    p.exit({ code: 3, stderr_tail: "warn\n" });
  });
  const h = sandbox.spawn({ argv: ["prog", "a b", ""], env: ENV, cwd: "/work", limits: { nproc: 64, dataBytes: 64 << 20, fsizeBytes: 1 << 20, nofile: 256 }, oomScoreAdj: 500, stderrTailBytes: 8192, killGraceMs: 0, timeoutMs: 60_000 });
  assert.equal(await h.started, true);
  assert.deepEqual(await collect(h.stdout), data);
  assert.deepEqual(await h.exit, { ...CLEAN, code: 3, stderrTail: "warn\n" });
  assert.equal(h.cancelled(), false);
  const [proc] = [...server.processes.values()];
  const { process_id, ...rest } = proc!.params;
  assert.match(process_id, /^[0-9a-f-]{36}$/);
  assert.deepEqual(rest, { sandbox_id: sandbox.id, argv: ["prog", "a b", ""], env: ENV, cwd: "/work", limits: { nproc: 64, data_bytes: 64 << 20, fsize_bytes: 1 << 20, nofile: 256 }, oom_score_adj: 500, stderr_tail_bytes: 8192, kill_grace_ms: 0, timeout_ms: 60_000 });
});

test("a malformed spec throws before anything starts", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const bad: SpawnOptions[] = [
    { argv: [], env: {} },
    { argv: [""], env: {} },
    { argv: ["a\0b"], env: {} },
    { argv: ["a"], env: { "A=B": "x" } },
    { argv: ["a"], env: { A: 1 as unknown as string } },
    { argv: ["a"], env: undefined as unknown as Record<string, string> },
    { argv: ["a"], env: {}, cwd: "relative" },
    { argv: ["a"], env: {}, timeoutMs: 0 },
    { argv: ["a"], env: {}, stderrTailBytes: 65537 },
    { argv: ["a"], env: {}, oomScoreAdj: 1001 },
    { argv: ["a"], env: {}, limits: { nofile: 8 } },
    { argv: ["a"], env: {}, stderr: "stream" as "tail" },
    { argv: ["a"], env: {}, stdin: "open", input: "x" },
    { argv: ["a"], env: {}, input: 5 as unknown as string },
  ];
  for (const spec of bad) assert.throws(() => sandbox.spawn(spec), InvalidSpecError, JSON.stringify(spec));
  await assert.rejects(sandbox.exec({ argv: ["a"], env: {} } as never), InvalidSpecError);
  await assert.rejects(sandbox.exec({ argv: ["a"], env: {}, timeoutMs: 10, maxOutputBytes: 0 }), InvalidSpecError);
  assert.equal(server.processes.size, 0);
});

test("signals carry Node's names for Linux numbers", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn((p) => {
    p.ack();
    p.exit({ code: null, signal: 9, timed_out: true, tree_clean: false });
  });
  assert.deepEqual(await sandbox.spawn({ argv: ["x"], env: {} }).exit, { ...CLEAN, code: null, signal: "SIGKILL", timedOut: true, treeClean: false });
  assert.deepEqual([1, 2, 6, 10, 11, 13, 15, 25, 31, 40].map(signalName), ["SIGHUP", "SIGINT", "SIGABRT", "SIGUSR1", "SIGSEGV", "SIGPIPE", "SIGTERM", "SIGXFSZ", "SIGSYS", "SIG40"]);
});

test("could not start, rejected, unavailable: each has its own field", async (t) => {
  const { server, client, sandbox } = await withSandbox(t);
  server.onSpawn((p) => p.fail(-32010, "no such program: nope", { errno: "ENOENT", what: "program" }));
  let h = sandbox.spawn({ argv: ["nope"], env: {} });
  assert.equal(await h.started, false);
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, spawnError: { errno: "ENOENT", what: "program", message: "no such program: nope" } });
  assert.deepEqual(await collect(h.stdout), Buffer.alloc(0));

  server.onSpawn((p) => p.fail(-32010, "cwd", { errno: "ENOTDIR", what: "cwd" }));
  assert.deepEqual((await sandbox.spawn({ argv: ["x"], env: {} }).exit).spawnError, { errno: "ENOTDIR", what: "cwd", message: "cwd" });

  server.onSpawn((p) => p.fail(-32602, "`limits.nofile` is above the sandbox's limit"));
  assert.deepEqual(await sandbox.spawn({ argv: ["x"], env: {} }).exit, { ...CLEAN, code: null, rejected: { code: "INVALID_PARAMS", message: "`limits.nofile` is above the sandbox's limit" } });

  // The fake answers SANDBOX_NOT_FOUND by itself.
  assert.deepEqual((await client.sandbox("gone").spawn({ argv: ["x"], env: {} }).exit).rejected?.code, "SANDBOX_NOT_FOUND");

  server.onSpawn((p) => p.fail(-32101, "docker is down", { detail: "docker is down" }));
  h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, unavailable: true });
  assert.equal(await h.started, false);

  server.onSpawn((p) => p.fail(-32012, "cancelled"));
  assert.deepEqual(await sandbox.spawn({ argv: ["x"], env: {} }).exit, { ...CLEAN, code: null, cancelled: true });
});

test("no server at all: unavailable, because nothing was written", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  await server.stop();
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  h.write("kept, then dropped with the process");
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, unavailable: true });
  assert.equal(await h.started, false);
});

test("a connection dropped at each stage after the write is lost, never unavailable", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const lost = { ...CLEAN, code: null, lost: true, treeClean: false };

  server.onSpawn((p) => p.drop()); // before the answer
  let h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.deepEqual(await h.exit, lost);
  assert.equal(await h.started, false);

  server.onSpawn((p) => {
    p.ack();
    p.drop();
  }); // right after the answer
  h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.deepEqual(await h.exit, lost);
  assert.equal(await h.started, true);

  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout("partial");
    p.drop();
  }); // mid-stream
  h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.equal((await collect(h.stdout)).toString(), "partial");
  assert.deepEqual(await h.exit, lost);

  server.onSpawn(async (p) => {
    p.ack();
    await p.raw(Buffer.from([0x7f, 0, 0, 0, 0])); // an unknown record kind
    await p.closed;
  });
  assert.deepEqual(await sandbox.spawn({ argv: ["x"], env: {} }).exit, lost);

  server.onSpawn(async (p) => {
    p.ack();
    await p.raw(encodeRecord(RECORD_EXIT, Buffer.from("not json")));
  });
  assert.deepEqual(await sandbox.spawn({ argv: ["x"], env: {} }).exit, lost);
});

test("the answer and every record in one chunk, and the same one byte at a time", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const bytes = (id: string): Buffer =>
    Buffer.concat([
      encodeFrame({ jsonrpc: "2.0", id: 1, result: { process_id: id } }),
      encodeRecord(RECORD_STDOUT, Buffer.from("one\r\n")),
      encodeRecord(RECORD_STDOUT, Buffer.from("two")),
      encodeRecord(RECORD_EXIT, Buffer.from(JSON.stringify({ code: 7, stderr_tail: "", timed_out: false, cancelled: false, overflow: false, tree_clean: true, duration_ms: 1 }))),
    ]);
  server.onSpawn((p) => p.socket.end(bytes(p.params.process_id)));
  let h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.equal((await collect(h.stdout)).toString(), "one\r\ntwo");
  assert.equal((await h.exit).code, 7);

  server.onSpawn(async (p) => {
    p.socket.setNoDelay(true);
    for (const b of bytes(p.params.process_id)) {
      await p.raw(Buffer.from([b]));
      await new Promise((resolve) => setImmediate(resolve));
    }
    p.socket.end();
  });
  h = sandbox.spawn({ argv: ["x"], env: {} });
  assert.equal((await collect(h.stdout)).toString(), "one\r\ntwo");
  assert.deepEqual(await h.exit, { ...CLEAN, code: 7 });
});

test("backpressure: a reader that stops reading stops the server's writer", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const data = pattern(8 * 1024 * 1024);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(data);
    p.exit();
  });
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  const [proc] = await Promise.all([until(() => server.processes.size === 1).then(() => [...server.processes.values()][0]!), h.started]);
  // Nobody reads: the writer stalls at the SDK's 1 MiB plus the socket buffers.
  let flushed = -1;
  while (flushed !== proc.flushedBytes) {
    flushed = proc.flushedBytes;
    await sleep(100);
  }
  assert.ok(flushed >= STDOUT_HIGH_WATER, `flushed ${flushed}`);
  assert.ok(flushed < 4 * 1024 * 1024, `the writer was not stopped: ${flushed} bytes flushed`);
  let exited = false;
  void h.exit.then(() => (exited = true));
  await sleep(20);
  assert.equal(exited, false);
  // Reading lets it all through, intact.
  assert.deepEqual(await collect(h.stdout), data);
  assert.deepEqual(await h.exit, CLEAN);
});

test("an abandoned stdout iterator still reaches the exit", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(pattern(6 * 1024 * 1024));
    p.exit({ code: 4 });
  });
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  let seen = 0;
  for await (const chunk of h.stdout) {
    seen += chunk.byteLength;
    break;
  }
  assert.ok(seen > 0);
  assert.equal((await h.exit).code, 4);
  assert.deepEqual(await collect(h.stdout), Buffer.alloc(0), "one consumer: nothing is left for a second loop");
});

test("write before the connection is up is kept in order; write after end is dropped", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const big = pattern(5 * 1024 * 1024);
  const procP = nextProcess(server);
  const h = sandbox.spawn({ argv: ["cat"], env: {}, stdin: "open" });
  h.write("head:");
  h.write(big);
  h.write(new Uint8Array([1, 2, 3]));
  const proc = await procP;
  assert.equal(proc.params.stdin, "pipe");
  await sleep(20);
  assert.equal(proc.stdinChunks.length, 0, "nothing is sent before the answer");
  proc.ack();
  h.write(":tail");
  h.end();
  h.write("dropped");
  h.end();
  await proc.stdinEnded;
  assert.deepEqual(proc.stdin, Buffer.concat([Buffer.from("head:"), big, Buffer.from([1, 2, 3]), Buffer.from(":tail")]));
  assert.ok(proc.stdinChunks.every((c) => c.length <= 65536));
  proc.exit();
  await h.exit;
  h.write("after the exit");
  h.end();
});

test("input is written once and stdin closed; without input stdin is not a pipe", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => {
    p.ack();
    if (p.params.stdin === "pipe") {
      await p.stdinEnded;
      await p.stdout(p.stdin);
    }
    p.exit();
  });
  let h = sandbox.spawn({ argv: ["cat"], env: {}, input: "héllo\r\n" });
  h.write("ignored: stdin ends after the input");
  assert.equal((await collect(h.stdout)).toString(), "héllo\r\n");
  await h.exit;

  h = sandbox.spawn({ argv: ["cat"], env: {}, input: new Uint8Array([0, 255, 10]) });
  assert.deepEqual(await collect(h.stdout), Buffer.from([0, 255, 10]));

  h = sandbox.spawn({ argv: ["true"], env: {} });
  h.write("nowhere to go");
  await h.exit;
  assert.equal([...server.processes.values()].at(-1)?.params.stdin, undefined);
});

test("cancel: asks the server once, twice is the same promise, the exit says cancelled", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout("before");
  });
  const h = sandbox.spawn({ argv: ["sleep", "100"], env: {} });
  await h.started;
  const a = h.cancel(250);
  const b = h.cancel(9999);
  assert.equal(a, b);
  assert.equal(h.cancelled(), true);
  await a;
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, signal: "SIGTERM", cancelled: true });
  const cancels = server.calls.filter((c) => c.method === "process.cancel");
  assert.equal(cancels.length, 1);
  assert.equal(cancels[0]?.params.grace_ms, 250);
  await h.cancel();
  assert.equal(server.calls.filter((c) => c.method === "process.cancel").length, 1);
});

test("cancel with nobody reading: what still arrives is discarded and the exit is reached", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.handle("process.cancel", (p) => {
    const proc = server.processes.get(p.process_id)!;
    // The program writes a lot more while it is being stopped.
    void proc.stdout(pattern(4 * 1024 * 1024)).then(() => proc.exit({ code: null, signal: 9, cancelled: true }));
    return { was_running: true, tree_clean: true };
  });
  server.onSpawn(async (p) => {
    p.ack();
    await p.stdout(pattern(3 * 1024 * 1024));
  });
  const h = sandbox.spawn({ argv: ["yes"], env: {} });
  await h.started;
  await h.cancel(0);
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, signal: "SIGKILL", cancelled: true });
});

test("cancel before the request is written: nothing is written", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  const done = h.cancel();
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, cancelled: true });
  await done;
  assert.equal(await h.started, false);
  await sleep(30);
  assert.equal(server.processes.size, 0);
  assert.equal(server.calls.filter((c) => c.method === "process.cancel").length, 0);
});

test("cancel before the answer: the server remembers it and the spawn answers CANCELLED", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  const procP = nextProcess(server);
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  const proc = await procP; // The request is read; no answer yet.
  await h.cancel(100);
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, cancelled: true });
  assert.equal(await h.started, false);
  assert.equal(await proc.cancelRequested, 100);
  assert.equal(server.calls.filter((c) => c.method === "process.cancel").length, 1);
});

test("a cancel that passed the spawn is sent once more when the spawn succeeds", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  let cancels = 0;
  server.handle("process.cancel", (p, next) => {
    cancels += 1;
    if (cancels > 1) return next();
    // As if the server had not seen the spawn yet. Then the spawn succeeds.
    setImmediate(() => server.processes.get(p.process_id)?.ack());
    return { was_running: false, tree_clean: true };
  });
  const procP = nextProcess(server);
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  await procP;
  await h.cancel();
  assert.equal(cancels, 2);
  assert.equal(await h.started, true);
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, signal: "SIGTERM", cancelled: true });
});

test("cancel without a control connection, or refused by the server, closes the process connection", async (t) => {
  const { server, client, sandbox } = await withSandbox(t);
  server.onSpawn((p) => p.ack());
  let h = sandbox.spawn({ argv: ["x"], env: {} });
  await h.started;
  server.handle("process.cancel", () => {
    throw new FakeRpcError(-32603, "boom");
  });
  await h.cancel();
  assert.deepEqual(await h.exit, { ...CLEAN, code: null, lost: true, cancelled: true, treeClean: false });
  await [...server.processes.values()][0]!.closed;

  h = sandbox.spawn({ argv: ["x"], env: {} });
  await h.started;
  await client.close();
  await h.cancel();
  assert.equal((await h.exit).lost, true);
});

test("a lost control connection stops no process", async (t) => {
  const { server, client, sandbox } = await withSandbox(t);
  const procP = nextProcess(server);
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  const proc = await procP;
  proc.ack();
  server.dropControlConnections();
  await until(() => !client.connected);
  await proc.stdout("still here");
  proc.exit({ code: 0 });
  assert.equal((await collect(h.stdout)).toString(), "still here");
  assert.deepEqual(await h.exit, CLEAN);
});

test("a server that goes away mid-run: the exit says lost", async (t) => {
  const { server, sandbox } = await withSandbox(t);
  server.onSpawn((p) => p.ack());
  const h = sandbox.spawn({ argv: ["x"], env: {} });
  await h.started;
  await server.stop();
  const exit = await h.exit;
  assert.equal(exit.lost, true);
  assert.equal(exit.unavailable, false);
});
