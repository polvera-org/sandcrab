// One smoke case each for mounts and forwards against a real server. The full
// matrix (ro/rw, symlinked roots, TCP and Unix targets, throughput) is the `ts-e2e` step.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { describe, test } from "node:test";

import { BACKEND, NODE, PATH_ENV, SKIP, TEMPLATE, TestServer } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

describe(`mounts and forwards smoke on a real server (${BACKEND})`, { skip: SKIP, timeout: 300_000 }, () => {
  test("a mount under an allowed root is at the same path; one forward reaches an in-process handler", async (t) => {
    // Resolved: on macOS /tmp is a symlink to /private/tmp, and the sandbox sees the resolved path.
    const base = realpathSync(mkdtempSync(path.join(process.env.SANDCRAB_E2E_DATA ?? process.cwd(), ".e2e-data-")));
    t.after(() => rmSync(base, { recursive: true, force: true }));
    const root = path.join(base, "root");
    const run = path.join(root, "run-1");
    mkdirSync(run, { recursive: true });
    writeFileSync(path.join(run, "hello.txt"), "from the host\n");

    const server = await TestServer.start({ mountRoots: [`${root}:rw`] });
    t.after(() => server.close());
    const client = await server.connect();
    t.after(() => client.close());

    const served: string[] = [];
    const sb = await client.createSandbox({
      template: TEMPLATE,
      mounts: [{ path: run, mode: "rw" }],
      forwards: [
        {
          guestPort: 15001,
          target: {
            handler(socket) {
              let got = "";
              socket.on("data", (d: Buffer) => {
                got += d.toString();
                if (got.endsWith("\n")) {
                  served.push(got);
                  socket.end(`handler saw ${got}`);
                }
              });
              socket.on("error", () => {});
            },
          },
        },
      ],
    });
    t.after(() => sb.destroy().catch(() => {}));
    assert.equal(sb.mounts[0]?.resolvedPath, run);

    const cat = await sb.exec({ argv: ["/bin/cat", path.join(run, "hello.txt")], env: ENV, cwd: run, timeoutMs: 20_000 });
    assert.equal(Buffer.from(cat.stdout).toString(), "from the host\n", JSON.stringify({ ...cat, stdout: Buffer.from(cat.stdout).toString() }));
    // rw: the sandbox writes, the host reads.
    const w = await sb.exec({ argv: ["/bin/sh", "-c", "echo from the sandbox > out.txt"], env: ENV, cwd: run, timeoutMs: 20_000 });
    assert.equal(w.code, 0, JSON.stringify(w));
    assert.equal(readFileSync(path.join(run, "out.txt"), "utf8"), "from the sandbox\n");

    const script = "const s = require('net').connect(15001, '127.0.0.1', () => s.write('ping\\n')); let b = ''; s.on('data', (d) => b += d); s.on('end', () => process.stdout.write(b)); s.on('error', (e) => { console.error(e.message); process.exit(3); });";
    const fwd = await sb.exec({ argv: [NODE, "-e", script], env: ENV, timeoutMs: 20_000 });
    assert.equal(Buffer.from(fwd.stdout).toString(), "handler saw ping\n", JSON.stringify({ ...fwd, stdout: Buffer.from(fwd.stdout).toString() }));
    assert.deepEqual(served, ["ping\n"]);
    const info = await sb.info();
    assert.equal(info.forwards[0]?.accepted, 1, JSON.stringify(info));
    assert.equal(info.forwards_served, true);
  });
});
