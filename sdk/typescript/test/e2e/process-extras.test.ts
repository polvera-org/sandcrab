// Process cases beyond the contract files (design §6.5, §13, §15 `ts-e2e`): limits per process,
// `tmpExec`, many short execs in one sandbox (warm exec p50/p95, sequential and 8-way
// concurrent), and a zero-mount sandbox whose uid has no passwd entry.

import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { Readable } from "node:stream";
import { describe, test } from "node:test";

import type { ExecOutput, Sandbox } from "../../src/index.ts";
import { BACKEND, NODE, PATH_ENV, pct, SKIP, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

function out(r: ExecOutput): string {
  return Buffer.from(r.stdout).toString("utf8");
}

/** CFS throttling of the sandbox's cgroup (cgroup v2 `cpu.stat`), or null where there is none. */
async function throttled(sb: Sandbox): Promise<{ periods: number; throttled: number; usec: number } | null> {
  const r = await sb.exec({ argv: ["/bin/cat", "/sys/fs/cgroup/cpu.stat"], env: ENV, timeoutMs: 10_000 });
  const s = out(r);
  const n = (k: string): number | null => {
    const m = new RegExp(`^${k} (\\d+)$`, "m").exec(s);
    return m ? Number(m[1]) : null;
  };
  const periods = n("nr_periods");
  if (r.code !== 0 || periods === null) return null;
  return { periods, throttled: n("nr_throttled") ?? 0, usec: n("throttled_usec") ?? 0 };
}

interface Timing {
  total: number[];
  started: number[];
}

/** One warm exec of /bin/true as `spawn`, timed at the client: to `started`, and to the exit. */
async function once(sb: Sandbox, t: Timing): Promise<void> {
  const t0 = performance.now();
  const p = sb.spawn({ argv: ["/bin/true"], env: ENV });
  void p.started.then(() => t.started.push(performance.now() - t0));
  for await (const _ of p.stdout) {
    // nothing
  }
  const exit = await p.exit;
  t.total.push(performance.now() - t0);
  assert.equal(exit.code, 0, JSON.stringify(exit));
}

/** Resident memory of a host process, in KiB. */
// Asynchronous: on macOS one `ps` takes ~80 ms while VMs run, and a synchronous call every
// 50 ms would stall the client's event loop and with it the pipeline being measured.
async function rssKiB(pid: number): Promise<number> {
  const { stdout } = await promisify(execFile)("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" });
  return Number(stdout.trim());
}

function summary(name: string, xs: number[]): string {
  const s = [...xs].sort((a, b) => a - b);
  return `${name} p50 ${pct(s, 50).toFixed(1)} ms, p95 ${pct(s, 95).toFixed(1)} ms, max ${(s.at(-1) ?? NaN).toFixed(1)} ms`;
}

async function warmNumbers(sb: Sandbox, label: string): Promise<{ seqP95: number; concP95: number }> {
  for (let i = 0; i < 20; i++) await once(sb, { total: [], started: [] });
  const seq: Timing = { total: [], started: [] };
  for (let i = 0; i < 200; i++) await once(sb, seq);
  const before = await throttled(sb);
  const conc: Timing = { total: [], started: [] };
  await Promise.all(
    Array.from({ length: 8 }, async () => {
      for (let i = 0; i < 50; i++) await once(sb, conc);
    }),
  );
  const afterT = await throttled(sb);
  const thr = before && afterT ? `; CFS throttled in ${afterT.throttled - before.throttled} of ${afterT.periods - before.periods} periods, ${((afterT.usec - before.usec) / 1000).toFixed(0)} ms` : "; no cgroup cpu.stat";
  console.log(`warm exec (${BACKEND}, ${label}), 200 sequential: ${summary("total", seq.total)}; ${summary("to started", seq.started)}`);
  console.log(`warm exec (${BACKEND}, ${label}), 8 x 50 concurrent: ${summary("total", conc.total)}; ${summary("to started", conc.started)}${thr}`);
  return { seqP95: pct([...seq.total].sort((a, b) => a - b), 95), concP95: pct([...conc.total].sort((a, b) => a - b), 95) };
}

describe(`process extras on a real server (${BACKEND})`, { skip: SKIP, timeout: 600_000 }, () => {
  test("limits per process: two processes of one sandbox, each with its own", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" } });
    t.after(() => sb.destroy().catch(() => {}));
    const script = "ulimit -n; ulimit -d; ulimit -f; sleep 0.3";
    const [a, b, none] = await Promise.all([
      sb.exec({ argv: ["/bin/sh", "-c", script], env: ENV, timeoutMs: 20_000, limits: { nofile: 64, dataBytes: 512 * 1024 * 1024, fsizeBytes: 1024 * 1024 } }),
      sb.exec({ argv: ["/bin/sh", "-c", script], env: ENV, timeoutMs: 20_000, limits: { nofile: 128, dataBytes: 256 * 1024 * 1024 } }),
      sb.exec({ argv: ["/bin/sh", "-c", script], env: ENV, timeoutMs: 20_000 }),
    ]);
    assert.equal(out(a), "64\n524288\n2048\n", JSON.stringify({ ...a, stdout: out(a) }));
    assert.equal(out(b), "128\n262144\nunlimited\n", JSON.stringify({ ...b, stdout: out(b) }));
    const [n] = out(none).split("\n");
    assert.ok(Number(n) > 128, `without limits nofile is the sandbox's: ${out(none)}`);

    // A process over its own nofile limit fails; its neighbour does not.
    const opens = (limit: number) =>
      sb.exec({
        argv: [NODE, "-e", "const fs=require('fs');let n=0;try{for(;n<200;n++)fs.openSync('/dev/null','r')}catch(e){console.log(e.code,n);process.exit(1)}console.log('ok',n)"],
        env: ENV,
        timeoutMs: 20_000,
        limits: { nofile: limit },
      });
    const [tight, loose] = await Promise.all([opens(64), opens(1024)]);
    assert.equal(tight.code, 1, out(tight));
    assert.match(out(tight), /^EMFILE \d+/);
    assert.equal(out(loose).trim(), "ok 200");
  });

  test("tmpExec: /tmp is noexec by default; tmpExec: true allows it", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const write = "printf '#!/bin/sh\\necho ran\\n' > /tmp/x.sh && chmod +x /tmp/x.sh";
    for (const tmpExec of [undefined, true] as const) {
      const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" }, ...(tmpExec === undefined ? {} : { tmpExec }) });
      t.after(() => sb.destroy().catch(() => {}));
      const w = await sb.exec({ argv: ["/bin/sh", "-c", write], env: ENV, timeoutMs: 20_000 });
      assert.equal(w.code, 0, JSON.stringify(w));
      const r = await sb.exec({ argv: ["/tmp/x.sh"], env: ENV, timeoutMs: 20_000 });
      if (tmpExec) {
        assert.equal(out(r), "ran\n", JSON.stringify(r));
      } else {
        assert.equal(r.code, null, JSON.stringify(r));
        assert.deepEqual(r.spawnError && { errno: r.spawnError.errno, what: r.spawnError.what }, { errno: "EACCES", what: "program" });
      }
    }
  });

  test("many short execs in one sandbox: warm exec p50/p95, sequential and 8-way concurrent", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" } });
    t.after(() => sb.destroy().catch(() => {}));
    const two = await warmNumbers(sb, "template vcpu");
    assert.ok(two.seqP95 < 150 && two.concP95 < 150, JSON.stringify(two));
    // The same with more vCPUs: tells a CPU quota tail from one of ours.
    const wide = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" }, resources: { vcpu: 8 } });
    t.after(() => wide.destroy().catch(() => {}));
    const eight = await warmNumbers(wide, "vcpu 8");
    assert.ok(eight.seqP95 < 150 && eight.concP95 < 150, JSON.stringify(eight));
  });

  test("a zero-mount sandbox as a uid with no passwd entry runs a command; HOME and the rest come only from env", async (t) => {
    // Another uid than the client's needs an allowance in the server config.
    const uid = 54_321;
    const server = await TestServer.start({ configToml: `allow_sandbox_uids = [[${uid}, ${uid}]]\n` });
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    // A non-root server serves a sandbox user that shares its gid (Docker's unix transport).
    const gid = process.getgid?.() ?? 1000;
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" }, user: { uid, gid } });
    t.after(() => sb.destroy().catch(() => {}));
    assert.deepEqual(sb.mounts, []);
    const r = await sb.exec({
      argv: ["/bin/sh", "-c", "id -u; id -g; getent passwd $(id -u) || echo no-passwd-entry; echo HOME=${HOME-unset}; cd /tmp && echo hi > f && cat f; env | sort"],
      env: { PATH: PATH_ENV, HOME: "/tmp", MARK: "x" },
      timeoutMs: 20_000,
    });
    assert.equal(r.code, 0, JSON.stringify({ ...r, stdout: out(r) }));
    const lines = out(r).split("\n");
    assert.deepEqual(lines.slice(0, 5), [String(uid), String(gid), "no-passwd-entry", "HOME=/tmp", "hi"]);
    assert.deepEqual(lines.slice(5).filter((l) => l !== "" && !l.startsWith("PWD=") && !l.startsWith("OLDPWD=") && !l.startsWith("SHLVL=") && !l.startsWith("_=")), [`HOME=/tmp`, "MARK=x", `PATH=${PATH_ENV}`]);
  });

  test("a pipeline between two sandboxes with a reader that pauses: every byte arrives, server memory stays bounded", async (t) => {
    const server = await TestServer.start();
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());
    const [a, b] = await Promise.all([1, 2].map(() => client.createSandbox({ template: TEMPLATE, labels: { e2e: "extras" } })));
    t.after(() => a!.destroy().catch(() => {}));
    t.after(() => b!.destroy().catch(() => {}));
    const bytes = 256 * 1024 * 1024;
    const base = await rssKiB(server.pid);
    let peak = base;
    let sampling = false;
    const sampler = setInterval(() => {
      if (sampling) return;
      sampling = true;
      rssKiB(server.pid)
        .then((kib) => (peak = Math.max(peak, kib)))
        // the server is gone; the assertions below say so
        .catch(() => {})
        .finally(() => (sampling = false));
    }, 50);
    t.after(() => clearInterval(sampler));
    const producer = a!.spawn({ argv: ["/usr/bin/head", "-c", String(bytes), "/dev/zero"], env: ENV });
    // The reader stops for 200 ms after every 16 MiB.
    const reader = "let n=0,next=16<<20;process.stdin.on('data',(d)=>{n+=d.length;if(n>=next){next+=16<<20;process.stdin.pause();setTimeout(()=>process.stdin.resume(),200)}});process.stdin.on('end',()=>console.log(n))";
    const t0 = Date.now();
    const consumer = await b!.exec({ argv: [NODE, "-e", reader], env: ENV, timeoutMs: 300_000, input: Readable.from(producer.stdout) });
    const exit = await producer.exit;
    clearInterval(sampler);
    assert.equal(exit.code, 0, JSON.stringify(exit));
    assert.equal(consumer.code, 0, JSON.stringify({ ...consumer, stdout: out(consumer) }));
    assert.equal(out(consumer).trim(), String(bytes));
    const grew = (peak - base) / 1024;
    console.log(`pipeline (${BACKEND}): ${bytes >> 20} MiB through the client in ${Date.now() - t0} ms; server RSS ${(base / 1024).toFixed(0)} MiB, peak +${grew.toFixed(1)} MiB`);
    assert.ok(grew < 64, `the server grew by ${grew.toFixed(1)} MiB`);
  });
});
