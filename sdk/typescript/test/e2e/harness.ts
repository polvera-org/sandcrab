// Shared setup of the end-to-end tests: a real `sandcrab server` on Docker, started as a child
// process with a private socket and state directory, and driven only through the public SDK.
//
// Needs: `SANDCRAB_BIN` (the `sandcrab` binary), Docker, and the pinned `ts-dev` template
// (`templates/build.sh ts-dev`). Passed through to the server: `SANDCRAB_TEMPLATE_PINS` (default
// `~/.sandcrab/template-pins`, as a private `SANDCRAB_HOME` would otherwise hide the host's pins)
// and `SANDCRAB_CRABD` (the static `crabd` to inject). Without `SANDCRAB_BIN` every test skips.
//
// Hygiene: the server runs with `--lifeline-stdin`, so it stops when this test process dies,
// however it dies; every server is also killed when its test file ends. Every wait has a deadline.

import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { mkdirSync, mkdtempSync, openSync, rmSync } from "node:fs";
import net from "node:net";
import { homedir, tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { connect } from "../../src/index.ts";
import type { Sandbox, SandcrabClient } from "../../src/index.ts";

export const BIN = process.env.SANDCRAB_BIN ?? "";
/** `{ skip }` for `describe`/`test`: the reason the e2e cannot run here, or false. */
export const SKIP: string | false = BIN === "" ? "SANDCRAB_BIN is not set: no sandcrab binary to test against" : false;

export const TEMPLATE = "ts-dev";
export const PATH_ENV = "/usr/local/bin:/usr/bin:/bin";
export const NODE = "/usr/local/bin/node";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../..");
const STARTUP_MS = 60_000;
const STOP_MS = 20_000;

const live = new Set<ChildProcess>();
process.on("exit", () => {
  for (const c of live) c.kill("SIGKILL");
});

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Rejects after `ms` with `what` in the message. */
export function deadline<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  return Promise.race([
    p.finally(() => clearTimeout(timer)),
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what}: no result within ${ms} ms`)), ms);
    }),
  ]);
}

function canConnect(socketPath: string): Promise<boolean> {
  return new Promise((resolve) => {
    const s = net.connect(socketPath);
    s.once("connect", () => {
      s.destroy();
      resolve(true);
    });
    s.once("error", () => resolve(false));
  });
}

export interface ServerOptions {
  /** `--mount-root PATH:MODE` each. */
  mountRoots?: string[];
  /** `--allow-uid` each. */
  allowUids?: number[];
  /** Reuse this directory (state, socket, log): a second server after one was killed. */
  dir?: string;
}

export class TestServer {
  readonly dir: string;
  readonly socketPath: string;
  readonly logPath: string;
  readonly #child: ChildProcess;
  readonly #exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;

  private constructor(dir: string, child: ChildProcess) {
    this.dir = dir;
    this.socketPath = path.join(dir, "sock", "server.sock");
    this.logPath = path.join(dir, "server.log");
    this.#child = child;
    this.#exited = new Promise((resolve) => {
      if (child.exitCode !== null || child.signalCode !== null) resolve({ code: child.exitCode, signal: child.signalCode });
      else child.once("exit", (code, signal) => resolve({ code, signal }));
    });
    void this.#exited.then(() => live.delete(child));
  }

  /** Starts a server and waits until its socket answers. */
  static async start(o: ServerOptions = {}): Promise<TestServer> {
    // Short paths: the Docker backend puts sockets under the state directory (108-byte limit).
    const dir = o.dir ?? mkdtempSync(path.join(tmpdir(), "scts-"));
    mkdirSync(path.join(dir, "home"), { recursive: true });
    const args = ["--backend=docker", "--state-dir", path.join(dir, "state"), "--templates-dir", path.join(REPO, "templates"), "server", "--socket", path.join(dir, "sock", "server.sock"), "--lifeline-stdin"];
    for (const r of o.mountRoots ?? []) args.push("--mount-root", r);
    for (const u of o.allowUids ?? []) args.push("--allow-uid", String(u));
    const env: Record<string, string> = {
      PATH: process.env.PATH ?? "/usr/bin:/bin",
      HOME: process.env.HOME ?? homedir(),
      SANDCRAB_HOME: path.join(dir, "home"),
      SANDCRAB_TEMPLATE_PINS: process.env.SANDCRAB_TEMPLATE_PINS ?? path.join(homedir(), ".sandcrab", "template-pins"),
    };
    if (process.env.SANDCRAB_CRABD) env.SANDCRAB_CRABD = process.env.SANDCRAB_CRABD;
    if (process.env.RUST_LOG) env.RUST_LOG = process.env.RUST_LOG;
    const log = openSync(path.join(dir, "server.log"), "a");
    const child = spawn(BIN, args, { env, stdio: ["pipe", log, log] });
    live.add(child);
    child.stdin?.on("error", () => {});
    const server = new TestServer(dir, child);
    const until = Date.now() + STARTUP_MS;
    for (;;) {
      if (await canConnect(server.socketPath)) return server;
      if (child.exitCode !== null || child.signalCode !== null) throw new Error(`sandcrab server exited at start; see ${server.logPath}`);
      if (Date.now() > until) {
        server.kill();
        throw new Error(`sandcrab server did not listen within ${STARTUP_MS} ms; see ${server.logPath}`);
      }
      await sleep(100);
    }
  }

  get pid(): number {
    return this.#child.pid ?? -1;
  }

  /** SIGKILL, no cleanup by the server: what a crash looks like. */
  kill(): Promise<unknown> {
    this.#child.kill("SIGKILL");
    return deadline(this.#exited, STOP_MS, "server exit after SIGKILL");
  }

  /** A clean stop (SIGTERM: processes cancelled, sandboxes destroyed). SIGKILL if it hangs. */
  async stop(): Promise<{ code: number | null; signal: NodeJS.Signals | null }> {
    if (this.#child.exitCode !== null || this.#child.signalCode !== null) return this.#exited;
    this.#child.kill("SIGTERM");
    try {
      return await deadline(this.#exited, STOP_MS, "server stop");
    } catch (e) {
      this.#child.kill("SIGKILL");
      throw e;
    }
  }

  /** Stops the server and removes its directory. */
  async close(): Promise<void> {
    await this.stop().catch(() => {});
    if (!process.env.SANDCRAB_E2E_KEEP) rmSync(this.dir, { recursive: true, force: true });
  }

  connect(): Promise<SandcrabClient> {
    return connect({ socketPath: this.socketPath, clientName: "sdk-e2e" });
  }
}

/**
 * Processes in the sandbox other than its init (`docker-init` and `crabd serve`), the probe and
 * the probe's own supervisor, as `pid cmdline`.
 */
export async function leftovers(sandbox: Sandbox): Promise<string[]> {
  const script = `
    const fs = require("fs");
    const out = [];
    for (const p of fs.readdirSync("/proc")) {
      if (!/^[0-9]+$/.test(p) || Number(p) === 1 || Number(p) === process.pid || Number(p) === process.ppid) continue;
      let cmd;
      try { cmd = fs.readFileSync("/proc/" + p + "/cmdline", "utf8").split("\\0").filter(Boolean); } catch { continue; }
      if (cmd.length === 0) continue;
      if (require("path").basename(cmd[0]) === "crabd") continue;
      out.push(p + " " + cmd.join(" "));
    }
    process.stdout.write(JSON.stringify(out));
  `;
  const r = await sandbox.exec({ argv: [NODE, "-e", script], env: { PATH: PATH_ENV }, timeoutMs: 20_000 });
  if (r.code !== 0) throw new Error(`leftover probe failed: ${JSON.stringify({ ...r, stdout: Buffer.from(r.stdout).toString() })}`);
  return JSON.parse(Buffer.from(r.stdout).toString("utf8")) as string[];
}

/** Whether `pid` exists in the sandbox. */
export async function pidAlive(sandbox: Sandbox, pid: number): Promise<boolean> {
  const r = await sandbox.exec({ argv: ["/bin/sh", "-c", `test -e /proc/${pid} && echo alive || echo gone`], env: { PATH: PATH_ENV }, timeoutMs: 10_000 });
  return Buffer.from(r.stdout).toString().trim() === "alive";
}

export async function collect(stream: AsyncIterable<Uint8Array>): Promise<Buffer> {
  const chunks: Uint8Array[] = [];
  for await (const c of stream) chunks.push(c);
  return Buffer.concat(chunks);
}

/** `n` bytes of mixed UTF-8 (1 to 4 bytes per character) and CRLF / LF / CR line ends. */
export function mixedText(n: number): Buffer {
  const parts = ["ascii ", "ação ", "日本語 ", "🦀🐚 ", "\r\n", "Ωmega ", "\n", "tab\t", "\r", "zero-width​ "];
  const out: Buffer[] = [];
  let size = 0;
  for (let i = 0; size < n; i++) {
    const b = Buffer.from(`${parts[(i * 7) % parts.length]}${i} `, "utf8");
    out.push(b);
    size += b.length;
  }
  // Cut at a character boundary below `n`, then pad with ASCII to exactly `n`.
  let buf = Buffer.concat(out);
  let cut = n;
  while (cut > 0 && ((buf[cut] ?? 0) & 0xc0) === 0x80) cut--;
  buf = Buffer.concat([buf.subarray(0, cut), Buffer.alloc(n - cut, "x")]);
  return buf;
}

export function pct(sorted: number[], p: number): number {
  return sorted[Math.min(sorted.length - 1, Math.floor((sorted.length * p) / 100))] ?? Number.NaN;
}
