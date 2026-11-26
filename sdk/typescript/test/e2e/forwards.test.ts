// Forwards through the public SDK with the sandbox's network disabled (design §4, §13, §15
// `ts-e2e`): the three target kinds, an HTTP proxy-like target with a long-lived CONNECT stream,
// 16 parallel connections, one large stream with a throughput number, refusals with their
// counters, and forwards served again by a new client.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { Readable } from "node:stream";
import { after, before, describe, test } from "node:test";

import type { ForwardSpec, Sandbox, SandcrabClient, wire } from "../../src/index.ts";
import { BACKEND, NODE, PATH_ENV, SKIP, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };
const P = { tcp: 15101, unix: 15102, handler: 15103, proxy: 15104, big: 15105, dead: 15106, echo: 15107 } as const;
const BIG_BYTES = 256 * 1024 * 1024;
const PARALLEL = 16;
const PER_CONN = 256 * 1024;

/** Runs `code` with Node in the sandbox; its stdout is one JSON value. */
async function node(sb: Sandbox, code: string, timeoutMs = 60_000): Promise<any> {
  const r = await sb.exec({ argv: [NODE, "-e", code], env: ENV, timeoutMs, maxOutputBytes: 1024 * 1024 });
  const out = Buffer.from(r.stdout).toString("utf8");
  assert.equal(r.code, 0, `in-sandbox program failed: ${JSON.stringify({ ...r, stdout: out })}`);
  return JSON.parse(out);
}

/** In-sandbox: connect to `port`, send `line`, read until the end. JSON {ok, got} or {ok:false, error}. */
const ASK = `
const ask = (port, line) => new Promise((resolve) => {
  const s = require("net").connect(port, "127.0.0.1");
  let got = "";
  s.on("connect", () => s.write(line));
  s.on("data", (d) => (got += d));
  s.on("end", () => resolve({ ok: true, got }));
  s.on("error", (e) => resolve({ ok: false, got, error: e.code || e.message }));
  setTimeout(() => { s.destroy(); resolve({ ok: false, got, error: "timeout" }); }, 15000).unref();
});
`;

function lineServer(kind: string, socket: net.Socket | import("node:stream").Duplex): void {
  let got = "";
  socket.on("data", (d: Buffer) => {
    got += d.toString();
    if (got.endsWith("\n")) socket.end(`${kind}:${got}`);
  });
  socket.on("error", () => {});
}

function listen(server: net.Server | http.Server, where: number | string): Promise<void> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    if (typeof where === "number") server.listen(where, "127.0.0.1", () => resolve());
    else server.listen(where, () => resolve());
  });
}

function sum(r: wire.ForwardRefused): number {
  return r.cap + r.no_listener + r.rejected + r.unclaimed;
}

describe(`forwards on a real server (${BACKEND}), network disabled`, { skip: SKIP, timeout: 600_000 }, () => {
  let server: TestServer;
  let client: SandcrabClient;
  let sb: Sandbox;
  let forwards: ForwardSpec[];
  const closers: (() => void)[] = [];
  let sockDir: string;
  const proxied: string[] = [];

  before(async () => {
    sockDir = mkdtempSync(path.join(tmpdir(), "scfw-"));
    const tcp = net.createServer((s) => lineServer("tcp", s));
    await listen(tcp, 0);
    const unixPath = path.join(sockDir, "target.sock");
    const unix = net.createServer((s) => lineServer("unix", s));
    await listen(unix, unixPath);
    // Raw echo, half-close aware: what comes in goes back, then the end.
    const echo = net.createServer({ allowHalfOpen: true }, (s) => {
      s.on("error", () => {});
      s.pipe(s);
    });
    await listen(echo, 0);
    // Proxy-like: plain requests with an absolute URL, and CONNECT tunnels to an echo upstream.
    const proxy = http.createServer((req, res) => {
      proxied.push(`${req.method} ${req.url}`);
      res.end(`proxied ${req.method} ${req.url}`);
    });
    proxy.on("connect", (req, socket: net.Socket, head: Buffer) => {
      proxied.push(`CONNECT ${req.url}`);
      socket.on("error", () => {});
      socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head.length > 0) socket.write(`echo:${head}`);
      socket.on("data", (d) => socket.write(`echo:${d}`));
      socket.on("end", () => socket.end());
    });
    await listen(proxy, 0);
    // A closed port: listen, take the number, close.
    const dead = net.createServer();
    await listen(dead, 0);
    const deadPort = (dead.address() as net.AddressInfo).port;
    await new Promise((r) => dead.close(r));
    closers.push(
      () => tcp.close(),
      () => unix.close(),
      () => echo.close(),
      () => proxy.close(),
    );
    const port = (s: net.Server | http.Server): number => (s.address() as net.AddressInfo).port;

    forwards = [
      { guestPort: P.tcp, target: { port: port(tcp) } },
      { guestPort: P.unix, target: { socketPath: unixPath } },
      { guestPort: P.handler, target: { handler: (s) => lineServer("handler", s) } },
      { guestPort: P.proxy, target: { port: port(proxy) } },
      {
        guestPort: P.big,
        target: {
          handler(s) {
            s.on("error", () => {});
            const chunk = Buffer.alloc(256 * 1024, 0x61);
            Readable.from(
              (function* () {
                for (let n = 0; n < BIG_BYTES; n += chunk.length) yield chunk;
              })(),
            ).pipe(s);
          },
        },
      },
      { guestPort: P.dead, target: { port: deadPort } },
      { guestPort: P.echo, target: { port: port(echo) } },
    ];
    server = await TestServer.start();
    client = await server.connect();
    sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "forwards" }, network: "disabled", forwards });
  });

  after(async () => {
    // `client` may be the second one by now: the first was closed on purpose.
    if (sb) await client?.sandbox(sb.id).destroy().catch(() => {});
    await client?.close();
    await server?.close();
    for (const c of closers) c();
    if (sockDir) rmSync(sockDir, { recursive: true, force: true });
  });

  async function counters(c: SandcrabClient = client): Promise<Map<number, wire.ForwardInfo>> {
    const info = await c.sandbox(sb.id).info();
    return new Map(info.forwards.map((f) => [f.guest_port, f]));
  }

  test("a loopback TCP port, a Unix socket and an in-process handler", async () => {
    const r = await node(sb, `${ASK} (async () => { process.stdout.write(JSON.stringify(await Promise.all([ask(${P.tcp}, "a\\n"), ask(${P.unix}, "b\\n"), ask(${P.handler}, "c\\n")]))); })();`);
    assert.deepEqual(r, [
      { ok: true, got: "tcp:a\n" },
      { ok: true, got: "unix:b\n" },
      { ok: true, got: "handler:c\n" },
    ]);
    const c = await counters();
    for (const p of [P.tcp, P.unix, P.handler]) assert.equal(c.get(p)?.accepted, 1, JSON.stringify(c.get(p)));
  });

  test("the network stays disabled: nothing but the forwards", async () => {
    const r = await node(sb, `${ASK} (async () => { process.stdout.write(JSON.stringify([await ask(15999, "x\\n"), require("os").networkInterfaces()])); })();`);
    assert.equal(r[0].ok, false, "an undeclared port is refused");
    assert.equal(r[0].error, "ECONNREFUSED");
    assert.deepEqual(Object.keys(r[1]), ["lo"]);
  });

  test("HTTP through a proxy-like target, and a long-lived CONNECT stream", async () => {
    const code = `
      const http = require("http");
      const get = () => new Promise((resolve, reject) => {
        const req = http.request({ host: "127.0.0.1", port: ${P.proxy}, method: "GET", path: "http://example.invalid/some/path?q=1" }, (res) => {
          let b = ""; res.on("data", (d) => (b += d)); res.on("end", () => resolve({ status: res.statusCode, body: b }));
        });
        req.on("error", reject); req.end();
      });
      const tunnel = () => new Promise((resolve, reject) => {
        const req = http.request({ host: "127.0.0.1", port: ${P.proxy}, method: "CONNECT", path: "upstream.invalid:443" });
        req.on("connect", (res, socket) => {
          const got = []; let buf = ""; let i = 0; const t0 = Date.now();
          socket.on("data", (d) => {
            buf += d;
            while (buf.includes("\\n")) { const k = buf.indexOf("\\n"); got.push(buf.slice(0, k)); buf = buf.slice(k + 1); }
            if (got.length === 5) socket.end();
          });
          socket.on("end", () => resolve({ status: res.statusCode, got, ms: Date.now() - t0 }));
          socket.on("error", reject);
          const tick = () => { socket.write("msg" + i + "\\n"); if (++i < 5) setTimeout(tick, 300); };
          tick();
        });
        req.on("error", reject); req.end();
      });
      (async () => { process.stdout.write(JSON.stringify({ get: await get(), tunnel: await tunnel() })); })().catch((e) => { console.error(e); process.exit(2); });
    `;
    const r = await node(sb, code);
    assert.deepEqual(r.get, { status: 200, body: "proxied GET http://example.invalid/some/path?q=1" });
    assert.equal(r.tunnel.status, 200);
    assert.deepEqual(r.tunnel.got, ["echo:msg0", "echo:msg1", "echo:msg2", "echo:msg3", "echo:msg4"]);
    assert.ok(r.tunnel.ms >= 1000, `the tunnel lived ${r.tunnel.ms} ms`);
    assert.deepEqual(proxied, ["GET http://example.invalid/some/path?q=1", "CONNECT upstream.invalid:443"]);
  });

  test(`${PARALLEL} parallel connections: every byte back, no reset, no refusal`, async () => {
    const before = (await counters()).get(P.echo);
    assert.ok(before);
    const code = `
      const crypto = require("crypto");
      const one = (i) => new Promise((resolve) => {
        const data = crypto.createHash("sha256").update("seed" + i).digest();
        const payload = Buffer.alloc(${PER_CONN});
        for (let k = 0; k < payload.length; k++) payload[k] = data[k % data.length] ^ (k >> 8);
        const s = require("net").connect({ port: ${P.echo}, host: "127.0.0.1", allowHalfOpen: true });
        const parts = [];
        s.on("connect", () => s.end(payload));
        s.on("data", (d) => parts.push(d));
        s.on("end", () => { const got = Buffer.concat(parts); resolve({ ok: got.equals(payload), bytes: got.length }); });
        s.on("error", (e) => resolve({ ok: false, error: e.code }));
      });
      Promise.all(Array.from({ length: ${PARALLEL} }, (_, i) => one(i))).then((r) => process.stdout.write(JSON.stringify(r)));
    `;
    const r = await node(sb, code);
    assert.equal(r.length, PARALLEL);
    for (const one of r) assert.deepEqual(one, { ok: true, bytes: PER_CONN });
    const afterC = (await counters()).get(P.echo);
    assert.ok(afterC);
    assert.equal(afterC.accepted - before.accepted, PARALLEL);
    assert.deepEqual(afterC.refused, before.refused, "no refusal counter moved");
  });

  test("one large stream: throughput, client target to a program in the sandbox", async () => {
    const code = `
      const s = require("net").connect(${P.big}, "127.0.0.1");
      const h = require("crypto").createHash("sha256");
      let n = 0; const t0 = process.hrtime.bigint();
      s.on("data", (d) => { n += d.length; h.update(d); });
      s.on("end", () => process.stdout.write(JSON.stringify({ n, ms: Number(process.hrtime.bigint() - t0) / 1e6, sha: h.digest("hex") })));
      s.on("error", (e) => { console.error(e.code); process.exit(3); });
    `;
    const r = await node(sb, code, 300_000);
    assert.equal(r.n, BIG_BYTES);
    const want = createHash("sha256");
    const chunk = Buffer.alloc(256 * 1024, 0x61);
    for (let n = 0; n < BIG_BYTES; n += chunk.length) want.update(chunk);
    assert.equal(r.sha, want.digest("hex"));
    const mbps = BIG_BYTES / 1e6 / (r.ms / 1000);
    console.log(`forward throughput (${BACKEND}): ${BIG_BYTES / 1024 / 1024} MiB in ${r.ms.toFixed(0)} ms = ${mbps.toFixed(0)} MB/s (target ≥ 50 MB/s)`);
    assert.ok(mbps > 10, `throughput ${mbps.toFixed(1)} MB/s`);
  });

  test("a target that cannot be reached: the connection closes empty, counted as rejected", async () => {
    const before = (await counters()).get(P.dead);
    assert.ok(before);
    const r = await node(sb, `${ASK} ask(${P.dead}, "x\\n").then((r) => process.stdout.write(JSON.stringify(r)));`);
    assert.equal(r.got, "", JSON.stringify(r));
    const afterC = (await counters()).get(P.dead);
    assert.ok(afterC);
    assert.equal(afterC.accepted, before.accepted);
    assert.equal(afterC.refused.rejected - before.refused.rejected, 1, JSON.stringify(afterC));
  });

  test("after client.close(): refused (fail closed) and counted; a new client serves the forwards and they work again", async () => {
    await client.close();
    const other = await server.connect();
    client = other;
    const handle = other.sandbox(sb.id);
    const before = (await counters(other)).get(P.tcp);
    assert.ok(before);
    // The sandbox outlives its creator's connection; nobody serves its forwards now.
    const refused = await node(handle, `${ASK} ask(${P.tcp}, "x\\n").then((r) => process.stdout.write(JSON.stringify(r)));`);
    assert.equal(refused.got, "", JSON.stringify(refused));
    const mid = (await counters(other)).get(P.tcp);
    assert.ok(mid);
    assert.equal(mid.accepted, before.accepted, JSON.stringify(mid));
    assert.equal(sum(mid.refused) - sum(before.refused), 1, JSON.stringify(mid));
    console.log(`forwards (${BACKEND}): with no serving client, refused as ${JSON.stringify(mid.refused)}`);

    await handle.serveForwards(forwards);
    const r = await node(handle, `${ASK} (async () => { process.stdout.write(JSON.stringify(await Promise.all([ask(${P.tcp}, "d\\n"), ask(${P.handler}, "e\\n")]))); })();`);
    assert.deepEqual(r, [
      { ok: true, got: "tcp:d\n" },
      { ok: true, got: "handler:e\n" },
    ]);
    const info = await handle.info();
    assert.equal(info.forwards_served, true);
  });
});
