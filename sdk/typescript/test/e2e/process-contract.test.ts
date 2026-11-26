// The process contract of a spawned process, against a real server on Docker (design §10.4,
// §15 `ts-contract`). One server and one `ts-dev` sandbox for the file; after every case no
// program may be left running in the sandbox.

import assert from "node:assert/strict";
import { after, afterEach, before, describe, test } from "node:test";

import type { Sandbox, SandcrabClient } from "../../src/index.ts";
import { collect, deadline, leftovers, mixedText, NODE, PATH_ENV, pidAlive, SKIP, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

describe("process contract (spawn) on a real server", { skip: SKIP, timeout: 300_000 }, () => {
  let server: TestServer;
  let client: SandcrabClient;
  let sb: Sandbox;

  before(async () => {
    server = await TestServer.start();
    client = await server.connect();
    sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "process" } });
  });

  after(async () => {
    await sb?.destroy().catch(() => {});
    await client?.close();
    await server?.close();
  });

  afterEach(async () => {
    assert.deepEqual(await leftovers(sb), [], "a program is still running in the sandbox");
  });

  test("exact argv and env, no shell, nothing added", async () => {
    const argv = [NODE, "-e", "process.stdout.write(JSON.stringify({argv: process.argv.slice(1), env: process.env}))", "", "two words", "$HOME; rm -rf /", "'\"quoted\"'", "ação 🦀", "*", "a\nb"];
    const env = { PATH: PATH_ENV, EMPTY: "", SPACED: " lead and trail ", UNICODE: "日本語", "WITH.DOT": "x", EQUALS: "a=b=c" };
    const p = sb.spawn({ argv, env });
    const out = JSON.parse((await collect(p.stdout)).toString("utf8"));
    const exit = await p.exit;
    assert.equal(exit.code, 0, JSON.stringify(exit));
    assert.deepEqual(out.argv, argv.slice(3));
    assert.deepEqual(out.env, env);
    // The same through `exec`.
    const r = await sb.exec({ argv, env, timeoutMs: 20_000 });
    assert.equal(r.code, 0);
    assert.deepEqual(JSON.parse(Buffer.from(r.stdout).toString("utf8")), out);
  });

  test("1 MiB of mixed UTF-8 and CRLF through spawn: ordered, complete, exit after the last byte", async () => {
    const data = mixedText(1024 * 1024);
    const p = sb.spawn({ argv: ["/bin/cat"], env: ENV, stdin: "open" });
    let atExit = -1;
    let got = 0;
    void p.exit.then(() => (atExit = got));
    const reading = (async () => {
      const chunks: Buffer[] = [];
      for await (const c of p.stdout) {
        chunks.push(Buffer.from(c));
        got += c.byteLength;
      }
      return Buffer.concat(chunks);
    })();
    for (let off = 0; off < data.length; off += 100_000) p.write(data.subarray(off, off + 100_000));
    p.end();
    const out = await reading;
    const exit = await p.exit;
    assert.equal(exit.code, 0, JSON.stringify(exit));
    assert.equal(out.length, data.length);
    assert.ok(out.equals(data), "bytes differ");
    assert.equal(atExit, data.length, "exit resolved before the last byte was read");
    // Through `exec`, with the text as a string input.
    const r = await sb.exec({ argv: ["/bin/cat"], env: ENV, input: data.toString("utf8"), timeoutMs: 30_000 });
    assert.equal(r.code, 0);
    assert.ok(Buffer.from(r.stdout).equals(data), "exec bytes differ");
  });

  test("stdin: `input` written then closed", async () => {
    const p = sb.spawn({ argv: ["/usr/bin/wc", "-c"], env: ENV, input: "12345\r\n" });
    assert.equal((await collect(p.stdout)).toString().trim(), "7");
    assert.equal((await p.exit).code, 0);
    // No input and the default stdin: the program sees end-of-file at once.
    const q = sb.spawn({ argv: ["/usr/bin/wc", "-c"], env: ENV });
    assert.equal((await collect(q.stdout)).toString().trim(), "0");
    assert.equal((await q.exit).code, 0);
  });

  test("input to a program that exits without reading it: the exit, not lost", async () => {
    const big = Buffer.alloc(4 * 1024 * 1024, 0x61);
    for (const argv of [["/bin/true"], ["/usr/bin/head", "-c", "10"]]) {
      const p = sb.spawn({ argv, env: ENV, input: big });
      const out = await collect(p.stdout);
      const exit = await deadline(p.exit, 20_000, `exit of ${argv[0]}`);
      assert.equal(exit.lost, false, `${argv.join(" ")}: ${JSON.stringify(exit)}`);
      assert.equal(exit.code, 0, `${argv.join(" ")}: ${JSON.stringify(exit)}`);
      assert.equal(out.length, argv.length === 1 ? 0 : 10);
    }
    const r = await sb.exec({ argv: ["/bin/true"], env: ENV, input: big, timeoutMs: 20_000 });
    assert.equal(r.lost, false, JSON.stringify({ ...r, stdout: r.stdout.length }));
    assert.equal(r.code, 0);
  });

  test("cancel at any moment of the spawn: the exit always comes, nothing is left", async () => {
    for (const delay of [0, 1, 2, 5, 10, 20, 50]) {
      const p = sb.spawn({ argv: ["/bin/sleep", "60"], env: ENV });
      if (delay > 0) await new Promise((r) => setTimeout(r, delay));
      await deadline(p.cancel(100), 15_000, `cancel after ${delay} ms`);
      const exit = await deadline(p.exit, 5_000, `exit after cancel at ${delay} ms`);
      assert.equal(exit.cancelled, true, `${delay} ms: ${JSON.stringify(exit)}`);
      assert.equal(exit.lost, false, `${delay} ms: ${JSON.stringify(exit)}`);
      assert.equal(exit.unavailable, false);
    }
  });

  test("32 processes at once, each with its own output", async () => {
    const ps = Array.from({ length: 32 }, (_, i) => sb.spawn({ argv: ["/bin/sh", "-c", `printf 'p%s' ${i}; sleep 0.2`], env: ENV }));
    const outs = await Promise.all(ps.map(async (p) => (await collect(p.stdout)).toString()));
    const exits = await Promise.all(ps.map((p) => p.exit));
    assert.deepEqual(outs, ps.map((_, i) => `p${i}`));
    assert.ok(exits.every((e) => e.code === 0), JSON.stringify(exits.filter((e) => e.code !== 0)));
  });

  test("stdin kept open: a conversation, then end()", async () => {
    const script = "const rl = require('readline').createInterface({input: process.stdin}); rl.on('line', (l) => console.log('echo:' + l)); rl.on('close', () => console.log('bye'));";
    const p = sb.spawn({ argv: [NODE, "-e", script], env: ENV, stdin: "open" });
    const it = p.stdout[Symbol.asyncIterator]();
    let buf = "";
    const readUntil = async (s: string): Promise<void> => {
      while (!buf.includes(s)) {
        const r = await deadline(it.next(), 20_000, `waiting for ${s}`);
        if (r.done) throw new Error(`stdout ended before ${s}: ${buf}`);
        buf += Buffer.from(r.value).toString("utf8");
      }
    };
    p.write("one\n");
    await readUntil("echo:one\n");
    p.write(new TextEncoder().encode("two\n"));
    await readUntil("echo:two\n");
    p.end();
    await readUntil("bye\n");
    for (let r = await it.next(); !r.done; r = await it.next()) buf += Buffer.from(r.value).toString("utf8");
    assert.equal(buf, "echo:one\necho:two\nbye\n");
    assert.equal((await p.exit).code, 0);
  });

  test("cancel kills a grandchild that called setsid and ignores SIGTERM; twice is fine", async () => {
    const grandchild = "process.on('SIGTERM', () => {}); setInterval(() => {}, 1000);";
    const parent = `
      process.on('SIGTERM', () => {});
      const c = require('child_process').spawn(process.execPath, ['-e', ${JSON.stringify(grandchild)}], { detached: true, stdio: 'ignore' });
      console.log(c.pid);
      setInterval(() => {}, 1000);
    `;
    const p = sb.spawn({ argv: [NODE, "-e", parent], env: ENV });
    const it = p.stdout[Symbol.asyncIterator]();
    const first = await deadline(it.next(), 20_000, "the grandchild pid");
    assert.ok(!first.done);
    const pid = Number(Buffer.from(first.value).toString().trim());
    assert.ok(pid > 1, `pid ${pid}`);
    assert.ok(await pidAlive(sb, pid), "the grandchild runs");
    // In its own session: setsid moved it out of the program's process group.
    const sid = await sb.exec({ argv: ["/bin/sh", "-c", `cut -d' ' -f6 /proc/${pid}/stat`], env: ENV, timeoutMs: 10_000 });
    assert.equal(Buffer.from(sid.stdout).toString().trim(), String(pid), "the grandchild leads its own session");

    const t0 = Date.now();
    await deadline(Promise.all([p.cancel(300), p.cancel(300)]), 20_000, "cancel");
    await deadline(p.cancel(300), 5_000, "a third cancel");
    assert.ok(p.cancelled());
    const exit = await p.exit;
    assert.equal(exit.cancelled, true, JSON.stringify(exit));
    assert.equal(exit.treeClean, true, JSON.stringify(exit));
    assert.equal(exit.lost, false);
    assert.equal(exit.signal, "SIGKILL", "TERM was ignored, so the grace ran out");
    assert.ok(Date.now() - t0 < 10_000);
    assert.equal(await pidAlive(sb, pid), false, "the grandchild is gone");
  });

  test("a program that cannot start arrives in the exit, distinct from unavailable", async () => {
    const p = sb.spawn({ argv: ["/no/such/program"], env: ENV });
    assert.equal(await p.started, false);
    const exit = await p.exit;
    assert.deepEqual(exit.spawnError && { errno: exit.spawnError.errno, what: exit.spawnError.what }, { errno: "ENOENT", what: "program" });
    assert.equal(exit.unavailable, false);
    assert.equal(exit.lost, false);
    assert.equal(exit.rejected, null);
    assert.equal(exit.code, null);

    // Not on PATH.
    const q = sb.spawn({ argv: ["no-such-program-on-path"], env: ENV });
    assert.equal((await q.exit).spawnError?.errno, "ENOENT");
    // A cwd that does not exist.
    const c = sb.spawn({ argv: ["/bin/true"], env: ENV, cwd: "/no/such/dir" });
    const ce = await c.exit;
    assert.equal(ce.spawnError?.what, "cwd", JSON.stringify(ce));
    // Through exec: the outcome, not a rejection.
    const r = await sb.exec({ argv: ["/no/such/program"], env: ENV, timeoutMs: 10_000 });
    assert.equal(r.spawnError?.errno, "ENOENT");
    assert.equal(r.unavailable, false);
    // Started is true for one that runs.
    const ok = sb.spawn({ argv: ["/bin/true"], env: ENV });
    assert.equal(await ok.started, true);
    assert.equal((await ok.exit).code, 0);
  });

  test("an abandoned stdout reader still gets its exit", async () => {
    const p = sb.spawn({ argv: ["/bin/sh", "-c", "head -c 8388608 /dev/zero; exit 7"], env: ENV });
    for await (const _ of p.stdout) break;
    const exit = await deadline(p.exit, 30_000, "exit after an abandoned reader");
    assert.equal(exit.code, 7, JSON.stringify(exit));
  });

  test("stderr: the tail, and merged into stdout in order", async () => {
    const script = "for i in 1 2 3; do echo out$i; echo err$i >&2; done; printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' >&2";
    const p = sb.spawn({ argv: ["/bin/sh", "-c", script], env: ENV, stderrTailBytes: 16 });
    assert.equal((await collect(p.stdout)).toString(), "out1\nout2\nout3\n");
    const exit = await p.exit;
    assert.equal(exit.stderrTail, "KLMNOPQRSTUVWXYZ");
    const m = sb.spawn({ argv: ["/bin/sh", "-c", script], env: ENV, stderr: "merge" });
    assert.equal((await collect(m.stdout)).toString(), "out1\nerr1\nout2\nerr2\nout3\nerr3\nABCDEFGHIJKLMNOPQRSTUVWXYZ");
    assert.equal((await m.exit).stderrTail, "");
  });

  test("limits: fsize ends the program with SIGXFSZ", async () => {
    const p = sb.spawn({ argv: ["/bin/sh", "-c", "exec head -c 1048576 /dev/zero > /tmp/fsize-test"], env: ENV, limits: { fsizeBytes: 4096 } });
    await collect(p.stdout);
    const exit = await p.exit;
    assert.equal(exit.signal, "SIGXFSZ", JSON.stringify(exit));
    assert.equal(exit.code, null);
    await sb.exec({ argv: ["/bin/rm", "-f", "/tmp/fsize-test"], env: ENV, timeoutMs: 10_000 });
  });

  test("timeoutMs on spawn: timed out, tree clean", async () => {
    const p = sb.spawn({ argv: ["/bin/sleep", "60"], env: ENV, timeoutMs: 300, killGraceMs: 100 });
    await collect(p.stdout);
    const exit = await deadline(p.exit, 10_000, "timeout");
    assert.equal(exit.timedOut, true, JSON.stringify(exit));
    assert.equal(exit.treeClean, true);
  });

  test("an exit code and the signal of a program that killed itself", async () => {
    const r = await sb.exec({ argv: ["/bin/sh", "-c", "exit 42"], env: ENV, timeoutMs: 10_000 });
    assert.equal(r.code, 42);
    const s = await sb.exec({ argv: ["/bin/sh", "-c", "kill -USR1 $$"], env: ENV, timeoutMs: 10_000 });
    assert.equal(s.signal, "SIGUSR1");
    assert.equal(s.code, null);
  });
});
