// Mounts through the public SDK (design §6, §15 `ts-e2e`): same absolute path, ro and rw,
// ownership of what the sandbox writes, a deep path the image does not have, host-side changes
// seen live, a symlinked root, and every refusal the server owes.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import path from "node:path";
import { after, before, describe, test } from "node:test";

import { InvalidSpecError, ServerError } from "../../src/index.ts";
import type { Sandbox, SandcrabClient } from "../../src/index.ts";
import { BACKEND, PATH_ENV, SKIP, TEMPLATE, TestServer, until } from "./harness.ts";

const ENV = { PATH: PATH_ENV };

function text(b: Uint8Array): string {
  return Buffer.from(b).toString("utf8");
}

async function sh(sb: Sandbox, script: string, cwd?: string): Promise<{ code: number | null; out: string; err: string }> {
  const r = await sb.exec({ argv: ["/bin/sh", "-c", script], env: ENV, cwd, timeoutMs: 20_000 });
  return { code: r.code, out: text(r.stdout), err: r.stderr };
}

const KRUN_X86 = BACKEND === "libkrun" && process.arch === "x64";

function refusedWith(code: string): (e: unknown) => boolean {
  return (e: unknown) => {
    assert.ok(e instanceof ServerError, `expected a ServerError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    return true;
  };
}

describe(`mounts on a real server (${BACKEND})`, { skip: SKIP, timeout: 600_000 }, () => {
  let base: string;
  let rwRoot: string;
  let roRoot: string;
  let outside: string;
  let server: TestServer;
  let client: SandcrabClient;

  before(async () => {
    // Resolved: on macOS /tmp is a symlink to /private/tmp, and the sandbox sees the resolved path.
    base = realpathSync(mkdtempSync(path.join(process.env.SANDCRAB_E2E_DATA ?? process.cwd(), ".e2e-mounts-")));
    rwRoot = path.join(base, "rw-root");
    roRoot = path.join(base, "ro-root");
    outside = path.join(base, "outside");
    for (const d of [rwRoot, roRoot, outside]) mkdirSync(d, { recursive: true });
    // The server directory is made first so it can also be an allowed root: then its socket
    // and state directories lie under a root, and only the server's own rule refuses them.
    // (On libkrun the state directory is the shared prepared home, outside every root.)
    const serverDir = mkdtempSync(path.join("/tmp", "scts-"));
    // The symlinked root: the server is given the link, resolves it once at start.
    symlinkSync(rwRoot, path.join(base, "rw-link"));
    server = await TestServer.start({ dir: serverDir, mountRoots: [`${path.join(base, "rw-link")}:rw`, `${roRoot}:ro`, `${serverDir}:rw`] });
    client = await server.connect();
  });

  after(async () => {
    await client?.destroyByLabel({ e2e: "mounts" }).catch(() => {});
    await client?.close();
    await server?.close();
    if (base) rmSync(base, { recursive: true, force: true });
  });

  test("same absolute path; ro refuses writes; rw writes land on the host with the expected owner; a deep path absent from the image", async (t) => {
    const deep = path.join(rwRoot, "run-1", "a", "b", "c", "deep");
    mkdirSync(deep, { recursive: true });
    const data = path.join(roRoot, "data");
    mkdirSync(data, { recursive: true });
    writeFileSync(path.join(data, "in.txt"), "read only\n");
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: [{ path: deep, mode: "rw" }, { path: data, mode: "ro" }] });
    t.after(() => sb.destroy().catch(() => {}));
    assert.deepEqual(
      sb.mounts.map((m) => [m.resolvedPath, m.mode]),
      [
        [deep, "rw"],
        [data, "ro"],
      ],
    );

    // The same absolute path inside, also as cwd.
    const pwd = await sh(sb, "pwd; cat " + path.join(data, "in.txt"), deep);
    assert.equal(pwd.code, 0, JSON.stringify(pwd));
    assert.equal(pwd.out, `${deep}\nread only\n`);

    // ro: the write is refused, the host file is untouched.
    const ro = await sh(sb, `echo changed > ${path.join(data, "in.txt")}; echo new > ${path.join(data, "new.txt")}`);
    assert.notEqual(ro.code, 0, JSON.stringify(ro));
    assert.match(ro.err, /Read-only file system/);
    assert.equal(readFileSync(path.join(data, "in.txt"), "utf8"), "read only\n");
    assert.throws(() => statSync(path.join(data, "new.txt")));

    // rw: the file lands on the host, owned like the client's own writes.
    const rw = await sh(sb, "echo from the sandbox > out.txt && mkdir -p sub && echo nested > sub/n.txt && id -u && id -g", deep);
    assert.equal(rw.code, 0, JSON.stringify(rw));
    assert.equal(readFileSync(path.join(deep, "out.txt"), "utf8"), "from the sandbox\n");
    assert.equal(readFileSync(path.join(deep, "sub", "n.txt"), "utf8"), "nested\n");
    const st = statSync(path.join(deep, "out.txt"));
    const owner = `${st.uid}:${st.gid}`;
    // The client's own write beside it: macOS gives a new file its directory's group (under
    // /private/tmp that is wheel), so the client's gid is not always the expected one.
    writeFileSync(path.join(deep, "client.txt"), "");
    const mine = statSync(path.join(deep, "client.txt"));
    assert.equal(owner, `${mine.uid}:${mine.gid}`, "owned like the client's own writes");
    assert.equal(st.uid, process.getuid?.());
    console.log(`mounts (${BACKEND}): sandbox user ${rw.out.trim().split("\n").join(":")}, host owner of its write ${owner}`);

    // The deep path does not exist in the image; its parents in the sandbox are only the way
    // to the mount point, not the host's folders: a host file beside the mount is not there.
    const sibling = path.join(rwRoot, "run-1", "a", "sibling.txt");
    writeFileSync(sibling, "host only\n");
    const parent = await sh(sb, `test -e ${sibling} && echo visible || echo absent`);
    assert.equal(parent.out, "absent\n");
  });

  test("a host-side change is seen in the sandbox: create, modify, delete (bounded wait)", async (t) => {
    const dir = path.join(rwRoot, "live");
    mkdirSync(dir, { recursive: true });
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: [{ path: dir, mode: "ro" }] });
    t.after(() => sb.destroy().catch(() => {}));
    const f = path.join(dir, "f.txt");
    const read = async (): Promise<string> => (await sh(sb, `cat ${f} 2>/dev/null || echo ABSENT`)).out;

    writeFileSync(f, "one\n");
    assert.equal(await read(), "one\n", "a file made after create");
    writeFileSync(f, "two, longer\n");
    await until(async () => (await read()) === "two, longer\n", 10_000, "a modified file");
    const t0 = Date.now();
    unlinkSync(f);
    // virtio-fs keeps an entry the guest already looked up for a few seconds (libkrun).
    await until(async () => (await read()) === "ABSENT\n", 15_000, "a deleted file");
    console.log(`mounts (${BACKEND}): host delete seen in the sandbox after ${Date.now() - t0} ms`);
  });

  test("a symlinked root: the client's path and the resolved path differ; the sandbox uses the resolved one", async (t) => {
    const viaLink = path.join(base, "rw-link", "via-link");
    mkdirSync(viaLink, { recursive: true });
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: [{ path: viaLink, mode: "rw" }] });
    t.after(() => sb.destroy().catch(() => {}));
    const m = sb.mounts[0];
    assert.equal(m?.path, viaLink);
    assert.equal(m?.resolvedPath, path.join(rwRoot, "via-link"));
    const r = await sh(sb, "pwd && echo ok > f", m?.resolvedPath);
    assert.equal(r.code, 0, JSON.stringify(r));
    assert.equal(r.out, `${m?.resolvedPath}\n`);
    assert.equal(readFileSync(path.join(viaLink, "f"), "utf8"), "ok\n");
  });

  test("16 mounts are accepted; 17 are refused before anything is sent", { skip: KRUN_X86 && "libkrun on x86_64 boots at most four directory mounts (next test)" }, async (t) => {
    const dirs = Array.from({ length: 17 }, (_, i) => path.join(rwRoot, "many", `m${i}`));
    for (const d of dirs) mkdirSync(d, { recursive: true });
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: dirs.slice(0, 16).map((p) => ({ path: p, mode: "ro" as const })) });
    t.after(() => sb.destroy().catch(() => {}));
    const r = await sh(sb, `ls -d ${dirs.slice(0, 16).join(" ")} | wc -l`);
    assert.equal(r.out.trim(), "16");
    await assert.rejects(
      client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: dirs.map((p) => ({ path: p, mode: "ro" as const })) }),
      (e: unknown) => e instanceof InvalidSpecError && /16/.test(e.message),
    );
    const caps = await client.capabilities();
    assert.equal(caps.max_mounts, 16);
  });

  // libkrun on x86_64 runs out of virtio-mmio interrupts after four directory mounts; the
  // backend refuses a fifth before the VM starts, naming the cap (crates/sandcrab-backend-libkrun
  // `X86_64_DEVICE_ROOM`). Elsewhere the 16 above hold.
  test("libkrun x86_64: four mounts boot; a fifth is refused with its cap before anything starts", { skip: !KRUN_X86 && "only libkrun on x86_64 has this cap" }, async (t) => {
    const dirs = Array.from({ length: 5 }, (_, i) => path.join(rwRoot, "room", `m${i}`));
    for (const d of dirs) mkdirSync(d, { recursive: true });
    const sb = await client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: dirs.slice(0, 4).map((p) => ({ path: p, mode: "ro" as const })) });
    t.after(() => sb.destroy().catch(() => {}));
    const r = await sh(sb, `ls -d ${dirs.slice(0, 4).join(" ")} | wc -l`);
    assert.equal(r.out.trim(), "4");
    await assert.rejects(client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts: dirs.map((p) => ({ path: p, mode: "ro" as const })) }), (e: unknown) => {
      refusedWith("MOUNT_NOT_ALLOWED")(e);
      const data = (e as ServerError).data as { cap?: number | null; reason: string };
      assert.equal(data.cap, 4, JSON.stringify(data));
      assert.match(data.reason, /too many mounts for libkrun/);
      return true;
    });
  });

  test("refused: outside the roots, above the root's mode, a symlink out, nesting in one sandbox, the server's own directories, a missing path", async () => {
    const before = (await client.listSandboxes()).length;
    const create = (mounts: { path: string; mode: "ro" | "rw" }[]): Promise<unknown> => client.createSandbox({ template: TEMPLATE, labels: { e2e: "mounts" }, mounts });

    await assert.rejects(create([{ path: outside, mode: "ro" }]), refusedWith("MOUNT_NOT_ALLOWED"));
    const roData = path.join(roRoot, "rw-asked");
    mkdirSync(roData, { recursive: true });
    await assert.rejects(create([{ path: roData, mode: "rw" }]), refusedWith("MOUNT_NOT_ALLOWED"));

    // A symlink inside a root that leads out: the SDK resolves it, the server sees the target.
    const escape = path.join(rwRoot, "escape");
    symlinkSync(outside, escape);
    await assert.rejects(create([{ path: escape, mode: "rw" }]), refusedWith("MOUNT_NOT_ALLOWED"));

    const outer = path.join(rwRoot, "outer");
    const inner = path.join(outer, "inner");
    mkdirSync(inner, { recursive: true });
    await assert.rejects(
      create([
        { path: outer, mode: "rw" },
        { path: inner, mode: "ro" },
      ]),
      (e: unknown) => refusedWith("MOUNT_NOT_ALLOWED")(e) && /nested/.test((e as Error).message),
    );
    await assert.rejects(
      create([
        { path: outer, mode: "rw" },
        { path: outer, mode: "ro" },
      ]),
      refusedWith("MOUNT_NOT_ALLOWED"),
    );

    // The server's socket directory, its state directory, and a folder that contains both.
    for (const own of [path.dirname(server.socketPath), server.stateDir, server.dir]) {
      await assert.rejects(create([{ path: own, mode: "ro" }]), refusedWith("MOUNT_NOT_ALLOWED"), own);
    }

    await assert.rejects(create([{ path: path.join(rwRoot, "does-not-exist"), mode: "ro" }]), InvalidSpecError);
    assert.equal((await client.listSandboxes()).length, before, "no refused create left a sandbox");
  });
});
