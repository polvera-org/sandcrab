// An in-process stand-in for `sandcrab-server`, for unit tests of code that uses this SDK.
// It speaks the real framing and records on a temporary Unix socket and keeps sandboxes in
// memory. It runs no program: a test scripts each process with `onSpawn`.

import { randomBytes, randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";

import { encodeFrame, encodeRecord, FrameDecoder, RECORD_EXIT, RECORD_STDERR, RECORD_STDIN, RECORD_STDIN_EOF, RECORD_STDOUT, RecordDecoder } from "../framing.ts";
import { ERROR_CODES } from "../generated/api.ts";
import type { Capabilities, ProcessExit, ProcessSpawnParams, SandboxCreate, SandboxInfo, SelftestReport } from "../generated/api.ts";

/** Thrown by a method handler to answer with a JSON-RPC error. */
export class FakeRpcError extends Error {
  readonly code: number;
  readonly data: unknown;
  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.code = code;
    this.data = data;
  }
}

export interface FakeServerOptions {
  /** Default: a socket in a fresh temporary directory. */
  socketPath?: string;
  /** The API version this server speaks. Default "1.0". Use another to play an older or newer server. */
  apiVersion?: string;
  serverVersion?: string;
  features?: string[];
  backend?: string;
  /** Answer every connection with `UNAUTHORIZED` and close. */
  unauthorized?: boolean;
}

/** A control method handler. `next()` runs the built-in behaviour. */
export type FakeHandler = (params: any, next: () => unknown) => unknown;

const CODE_INVALID_REQUEST = -32600;
const CODE_METHOD_NOT_FOUND = -32601;
const CODE_CANCELLED = -32012;

function parseVersion(v: unknown): [number, number] | null {
  const m = typeof v === "string" ? /^(\d+)\.(\d+)$/.exec(v) : null;
  return m ? [Number(m[1]), Number(m[2])] : null;
}

function write(sock: net.Socket, data: Uint8Array): Promise<void> {
  return new Promise((resolve) => {
    if (sock.destroyed || sock.writableEnded) return resolve();
    sock.write(data, () => resolve());
  });
}

/** One `process.spawn` connection, as the server side of it. */
export class FakeProcess {
  readonly params: ProcessSpawnParams;
  readonly socket: net.Socket;
  /** The bytes of every `STDIN` record so far. */
  readonly stdinChunks: Buffer[] = [];
  /** Resolves at `STDIN_EOF`. */
  readonly stdinEnded: Promise<void>;
  /** Resolves with the grace when `process.cancel` names this process. */
  readonly cancelRequested: Promise<number>;
  /** Resolves when the connection is closed, by either side. */
  readonly closed: Promise<void>;
  /** Bytes of stdout and stderr records flushed to the socket so far. */
  flushedBytes = 0;
  acked = false;
  finished = false;
  _resolveStdinEnded!: () => void;
  _resolveCancel!: (grace: number) => void;
  readonly #requestId: unknown;

  constructor(socket: net.Socket, requestId: unknown, params: ProcessSpawnParams) {
    this.socket = socket;
    this.params = params;
    this.#requestId = requestId;
    this.stdinEnded = new Promise((resolve) => (this._resolveStdinEnded = resolve));
    this.cancelRequested = new Promise((resolve) => (this._resolveCancel = resolve));
    this.closed = new Promise((resolve) =>
      socket.once("close", () => {
        this.finished = true;
        resolve();
      }),
    );
  }

  get stdin(): Buffer {
    return Buffer.concat(this.stdinChunks);
  }

  /** The answer to `process.spawn`: the program runs. */
  ack(): void {
    if (this.acked || this.finished) return;
    this.acked = true;
    this.socket.write(encodeFrame({ jsonrpc: "2.0", id: this.#requestId, result: { process_id: this.params.process_id } }));
  }

  /** An error answer to `process.spawn`, then close. */
  fail(code: number, message: string, data?: unknown): void {
    if (this.acked || this.finished) return;
    this.finished = true;
    this.socket.end(encodeFrame({ jsonrpc: "2.0", id: this.#requestId, error: { code, message, data } }));
  }

  async #data(kind: number, data: string | Uint8Array): Promise<void> {
    const bytes = typeof data === "string" ? Buffer.from(data, "utf8") : data;
    for (let off = 0; off < bytes.byteLength; off += 65536) {
      const part = bytes.subarray(off, off + 65536);
      await write(this.socket, encodeRecord(kind, part));
      if (this.socket.destroyed) return;
      this.flushedBytes += part.byteLength;
    }
  }

  /** `STDOUT` records. Resolves when the socket took them: a client that does not read holds this up. */
  stdout(data: string | Uint8Array): Promise<void> {
    return this.#data(RECORD_STDOUT, data);
  }

  stderr(data: string | Uint8Array): Promise<void> {
    return this.#data(RECORD_STDERR, data);
  }

  /** The `EXIT` record, then close. Default: code 0, a clean tree. */
  exit(exit: Partial<ProcessExit> = {}): void {
    if (this.finished) return;
    this.finished = true;
    const full: ProcessExit = { code: 0, signal: null, stderr_tail: "", timed_out: false, cancelled: false, overflow: false, tree_clean: true, duration_ms: 1, ...exit };
    this.socket.end(encodeRecord(RECORD_EXIT, Buffer.from(JSON.stringify(full), "utf8")));
  }

  /** Any bytes, as they are: for split and merged writes. */
  raw(bytes: Uint8Array): Promise<void> {
    return write(this.socket, bytes);
  }

  /** Close without an `EXIT` record, as a server that went away would. */
  drop(): void {
    this.finished = true;
    this.socket.destroy();
  }
}

interface Control {
  socket: net.Socket;
}

interface PendingForward {
  sandboxId: string;
  guestPort: number;
  resolve(socket: net.Socket): void;
  reject(err: Error): void;
}

export class FakeServer {
  readonly socketPath: string;
  apiVersion: string;
  serverVersion: string;
  features: string[];
  backend: string;
  unauthorized: boolean;
  /** Every control request received, in order. */
  readonly calls: { method: string; params: any }[] = [];
  readonly sandboxes = new Map<string, SandboxInfo>();
  /** Every process ever spawned, by `process_id`. */
  readonly processes = new Map<string, FakeProcess>();
  /** What `server.selftest` reports for every template. */
  selftestResult: { ok: boolean; reason: string | null } = { ok: true, reason: null };
  readonly #reports = new Map<string, { report: SelftestReport; at: number }>();
  readonly #handlers = new Map<string, FakeHandler>();
  readonly #sockets = new Set<net.Socket>();
  readonly #controls = new Set<Control>();
  readonly #serving = new Map<string, Control>();
  readonly #forwards = new Map<string, PendingForward>();
  readonly #tmpDir: string | null;
  /** `process.cancel` for ids no spawn has named yet (the real server keeps them 60 s). */
  readonly #earlyCancels = new Set<string>();
  #spawn: (p: FakeProcess) => unknown = (p) => {
    p.ack();
    p.exit();
  };
  #server: net.Server | null = null;

  private constructor(o: FakeServerOptions) {
    if (o.socketPath) {
      this.#tmpDir = null;
      this.socketPath = o.socketPath;
    } else {
      this.#tmpDir = mkdtempSync(path.join(tmpdir(), "sc-"));
      this.socketPath = path.join(this.#tmpDir, "s.sock");
    }
    this.apiVersion = o.apiVersion ?? "1.0";
    this.serverVersion = o.serverVersion ?? "0.0.0-fake";
    this.features = o.features ?? ["forwards", "mounts", "selftest"];
    this.backend = o.backend ?? "fake";
    this.unauthorized = o.unauthorized ?? false;
  }

  static async start(o: FakeServerOptions = {}): Promise<FakeServer> {
    const server = new FakeServer(o);
    await server.listen();
    return server;
  }

  /** Starts listening (again, after `stop`). */
  listen(): Promise<void> {
    return new Promise((resolve, reject) => {
      const server = net.createServer({ allowHalfOpen: true }, (sock) => this.#accept(sock));
      server.once("error", reject);
      server.listen(this.socketPath, () => {
        this.#server = server;
        resolve();
      });
    });
  }

  /** Stops like a server shutting down: `server.shutdown {grace_ms}` on every control connection, then `stop()`. */
  async shutdown(graceMs = 0): Promise<void> {
    this.notify("server.shutdown", { grace_ms: graceMs });
    await new Promise((resolve) => setImmediate(resolve));
    await this.stop();
  }

  /** Plays a sandbox whose TTL ran out: it is gone, and `sandbox.destroyed {reason: "ttl"}` is sent. */
  expire(sandboxId: string): boolean {
    return this.#destroy(sandboxId, "ttl");
  }

  /** Stops like a server that died: every connection is cut, sandboxes are forgotten. `listen()` restarts it. */
  stop(): Promise<void> {
    return new Promise((resolve) => {
      const server = this.#server;
      this.#server = null;
      for (const sock of this.#sockets) sock.destroy();
      this.sandboxes.clear();
      this.#serving.clear();
      this.#reports.clear();
      this.#earlyCancels.clear();
      for (const f of this.#forwards.values()) f.reject(new Error("the fake server stopped"));
      this.#forwards.clear();
      if (server) server.close(() => resolve());
      else resolve();
    });
  }

  async close(): Promise<void> {
    await this.stop();
    if (this.#tmpDir) rmSync(this.#tmpDir, { recursive: true, force: true });
  }

  /** Replaces the behaviour of one control method. */
  handle(method: string, handler: FakeHandler): void {
    this.#handlers.set(method, handler);
  }

  /** Scripts every later process. Default: ack, then exit with code 0. */
  onSpawn(handler: (p: FakeProcess) => unknown): void {
    this.#spawn = handler;
  }

  get controlConnections(): number {
    return this.#controls.size;
  }

  /** Cuts every control connection; process and forward connections stay. */
  dropControlConnections(): void {
    for (const c of this.#controls) c.socket.destroy();
    this.#controls.clear();
  }

  /** A notification on every control connection. */
  notify(method: string, params: unknown): void {
    for (const c of this.#controls) c.socket.write(encodeFrame({ jsonrpc: "2.0", method, params }));
  }

  /**
   * Plays a program in the sandbox connecting to a forwarded port. Resolves with the server
   * end of the forwarded connection once the client attached; rejects when the forward is
   * not served or the client rejected it.
   */
  forwardIncoming(sandboxId: string, guestPort: number): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
      const control = this.#serving.get(sandboxId);
      const counters = this.sandboxes.get(sandboxId)?.forwards.find((f) => f.guest_port === guestPort);
      if (!control || !this.#controls.has(control) || !counters) {
        if (counters) counters.refused.rejected += 1;
        return reject(new Error("the forward is not served"));
      }
      const connId = randomBytes(16).toString("hex");
      this.#forwards.set(connId, { sandboxId, guestPort, resolve, reject });
      control.socket.write(encodeFrame({ jsonrpc: "2.0", method: "forward.incoming", params: { conn_id: connId, sandbox_id: sandboxId, guest_port: guestPort } }));
    });
  }

  #accept(sock: net.Socket): void {
    this.#sockets.add(sock);
    sock.on("error", () => {});
    sock.once("close", () => this.#sockets.delete(sock));
    if (this.unauthorized) {
      sock.end(encodeFrame({ jsonrpc: "2.0", id: null, error: { code: ERROR_CODES.UNAUTHORIZED, message: "unauthorized", data: {} } }));
      return;
    }
    const decoder = new FrameDecoder();
    // The first frame is read with `read()`, so the socket is still an unread stream when a
    // forwarded connection is handed to the test.
    const first = (): void => {
      let msg: any;
      try {
        for (let chunk = sock.read() as Buffer | null; chunk !== null && msg === undefined; chunk = msg === undefined ? (sock.read() as Buffer | null) : null) {
          decoder.push(chunk);
          msg = decoder.next();
        }
      } catch {
        return void sock.destroy();
      }
      if (msg === undefined) return;
      sock.off("readable", first);
      const rest = decoder.rest();
      if (msg.method === "server.hello") this.#control(sock, msg, rest);
      else if (msg.method === "process.spawn") this.#process(sock, msg, rest);
      else if (msg.method === "forward.attach") this.#attach(sock, msg, rest);
      else sock.end(encodeFrame({ jsonrpc: "2.0", id: msg.id ?? null, error: { code: CODE_INVALID_REQUEST, message: "not a first frame", data: { detail: "not a first frame" } } }));
    };
    sock.on("readable", first);
  }

  #control(sock: net.Socket, hello: any, rest: Buffer): void {
    const server = parseVersion(this.apiVersion);
    const client = parseVersion(hello.params?.api_version);
    this.calls.push({ method: "server.hello", params: hello.params });
    // The server's rule: a server at M.N serves clients of major M and minor N - 1 or later.
    if (!server || !client || client[0] !== server[0] || client[1] < server[1] - 1) {
      const data = { server: this.apiVersion, client: String(hello.params?.api_version) };
      sock.end(encodeFrame({ jsonrpc: "2.0", id: hello.id, error: { code: ERROR_CODES.UNSUPPORTED_VERSION, message: "unsupported API version", data } }));
      return;
    }
    const control: Control = { socket: sock };
    this.#controls.add(control);
    sock.once("close", () => this.#controls.delete(control));
    sock.on("end", () => sock.end());
    sock.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: hello.id,
        result: {
          server_version: this.serverVersion,
          api_version: this.apiVersion,
          features: this.features,
          backend: this.backend,
          limits: { max_connections: 1024, max_requests_in_flight: 64, max_sandboxes: 64, max_processes_per_sandbox: 64, max_mounts: 16, max_vcpu: 8, max_memory_mb: 16384, max_pids: 8192, max_nofile: 1048576, default_ttl_secs: 1800, max_ttl_secs: 86400 },
        },
      }),
    );
    const decoder = new FrameDecoder();
    const onData = (chunk: Buffer): void => {
      try {
        decoder.push(chunk);
        for (let msg: any = decoder.next(); msg !== undefined; msg = decoder.next()) void this.#request(control, msg);
      } catch {
        sock.destroy();
      }
    };
    sock.on("data", onData);
    if (rest.length > 0) onData(rest);
  }

  async #request(control: Control, msg: any): Promise<void> {
    const { id, method, params } = msg;
    this.calls.push({ method, params });
    let reply: object;
    try {
      const builtin = (): unknown => this.#builtin(control, method, params ?? {});
      const custom = this.#handlers.get(method);
      const result = await (custom ? custom(params ?? {}, builtin) : builtin());
      reply = { jsonrpc: "2.0", id, result: result ?? {} };
    } catch (e) {
      const err = e instanceof FakeRpcError ? { code: e.code, message: e.message, data: e.data } : { code: -32603, message: String(e), data: { detail: String(e) } };
      reply = { jsonrpc: "2.0", id, error: err };
    }
    if (!control.socket.destroyed) control.socket.write(encodeFrame(reply));
  }

  #sandbox(id: unknown): SandboxInfo {
    const sb = typeof id === "string" ? this.sandboxes.get(id) : undefined;
    if (!sb) throw new FakeRpcError(ERROR_CODES.SANDBOX_NOT_FOUND, `no such sandbox: ${String(id)}`, { sandbox_id: id });
    return sb;
  }

  #matching(p: { labels?: Record<string, string>; created_after?: string | null; created_before?: string | null }): SandboxInfo[] {
    return [...this.sandboxes.values()].filter(
      (sb) =>
        Object.entries(p.labels ?? {}).every(([k, v]) => sb.labels[k] === v) &&
        (!p.created_after || Date.parse(sb.created_at) > Date.parse(p.created_after)) &&
        (!p.created_before || Date.parse(sb.created_at) < Date.parse(p.created_before)),
    );
  }

  #destroy(id: string, reason: "destroyed" | "ttl" = "destroyed"): boolean {
    if (!this.sandboxes.delete(id)) return false;
    this.#serving.delete(id);
    this.notify("sandbox.destroyed", { sandbox_id: id, reason });
    return true;
  }

  #builtin(control: Control, method: string, p: any): unknown {
    switch (method) {
      case "server.capabilities": {
        const caps: Capabilities = { backend: this.backend, hardware_isolated: false, multi_tenant_safe: false, supports_host_dir_mounts: true, supports_forwards: true, tree_kill: "subreaper", nproc_scope: "sandbox", mount_identity_checked: false, max_mounts: 16, selftest: null };
        const last = typeof p.template === "string" ? this.#reports.get(p.template) : undefined;
        if (last) caps.selftest = { template: last.report.template, ok: last.report.ok, reason: last.report.reason ?? null, age_secs: Math.floor((Date.now() - last.at) / 1000) };
        return caps;
      }
      case "server.selftest": {
        const report: SelftestReport = { ...this.selftestResult, backend: this.backend, template: p.template, started_at: new Date().toISOString(), duration_ms: 1, lines: [] };
        this.#reports.set(p.template, { report, at: Date.now() });
        return report;
      }
      case "sandbox.create": {
        const c = p as SandboxCreate;
        const id = randomUUID();
        const ttl = c.ttl_secs === undefined ? 1800 : c.ttl_secs;
        const info: SandboxInfo = {
          sandbox_id: id,
          template: c.template,
          labels: c.labels ?? {},
          state: "running",
          created_at: new Date().toISOString(),
          expires_at: ttl === null ? null : new Date(Date.now() + ttl * 1000).toISOString(),
          backend: this.backend,
          mounts: c.mounts ?? [],
          forwards: (c.forwards ?? []).map((f) => ({ guest_port: f.guest_port, accepted: 0, refused: { no_listener: 0, cap: 0, unclaimed: 0, rejected: 0 } })),
          forwards_served: (c.forwards ?? []).length > 0,
        };
        this.sandboxes.set(id, info);
        if (info.forwards_served) this.#serving.set(id, control);
        return info;
      }
      case "sandbox.get":
        return this.#sandbox(p.sandbox_id);
      case "sandbox.list":
        return this.#matching(p);
      case "sandbox.set_ttl": {
        const sb = this.#sandbox(p.sandbox_id);
        sb.expires_at = p.ttl_secs === null ? null : new Date(Date.now() + p.ttl_secs * 1000).toISOString();
        return sb;
      }
      case "sandbox.destroy":
        return { destroyed: this.#destroy(p.sandbox_id) };
      case "sandbox.destroy_by_label": {
        const matched = this.#matching(p);
        for (const sb of matched) this.#destroy(sb.sandbox_id);
        const remaining = this.#matching(p).map((sb) => sb.sandbox_id);
        return { matched: matched.length, destroyed: matched.length, remaining, empty: remaining.length === 0 };
      }
      case "process.cancel": {
        const proc = this.processes.get(p.process_id);
        if (!proc) {
          // Not seen yet: remembered, so a spawn of this id still on its way is answered CANCELLED.
          this.#earlyCancels.add(p.process_id);
          return { was_running: false, tree_clean: true };
        }
        if (proc.finished) return { was_running: false, tree_clean: true };
        proc._resolveCancel(p.grace_ms ?? 5000);
        if (!proc.acked) {
          // Registered but not open yet: the cancel is remembered and the spawn answers CANCELLED.
          proc.fail(CODE_CANCELLED, "cancelled before the program started", { detail: "cancelled before the program started" });
          return { was_running: false, tree_clean: true };
        }
        proc.exit({ code: null, signal: p.grace_ms === 0 ? 9 : 15, cancelled: true });
        return { was_running: true, tree_clean: true };
      }
      case "forward.serve": {
        const sb = this.#sandbox(p.sandbox_id);
        this.#serving.set(sb.sandbox_id, control);
        sb.forwards_served = true;
        return { ports: sb.forwards.map((f) => f.guest_port) };
      }
      case "forward.reject": {
        const f = this.#forwards.get(p.conn_id);
        if (f) {
          this.#forwards.delete(p.conn_id);
          const counters = this.sandboxes.get(f.sandboxId)?.forwards.find((x) => x.guest_port === f.guestPort);
          if (counters) counters.refused.rejected += 1;
          f.reject(new Error(p.reason ?? "rejected"));
        }
        return {};
      }
      default:
        throw new FakeRpcError(CODE_METHOD_NOT_FOUND, `no such method: ${method}`, { detail: method });
    }
  }

  #process(sock: net.Socket, msg: any, rest: Buffer): void {
    const params = msg.params as ProcessSpawnParams;
    const proc = new FakeProcess(sock, msg.id, params);
    // As the real server: an id is in conflict only while the first process with it lives.
    if (this.processes.get(params.process_id)?.finished === false) return proc.fail(ERROR_CODES.CONFLICT, "process_id used twice", { detail: `process_id ${params.process_id} is already in use` });
    if (this.#earlyCancels.delete(params.process_id)) return proc.fail(CODE_CANCELLED, "cancelled before the program started", { detail: "cancelled before the program started" });
    this.processes.set(params.process_id, proc);
    const records = new RecordDecoder();
    const onData = (chunk: Buffer): void => {
      try {
        records.push(chunk);
        for (let r = records.next(); r !== undefined; r = records.next()) {
          if (r.kind === RECORD_STDIN && params.stdin === "pipe") proc.stdinChunks.push(Buffer.from(r.payload));
          else if (r.kind === RECORD_STDIN_EOF && params.stdin === "pipe") proc._resolveStdinEnded();
          else throw new Error("unexpected record");
        }
      } catch {
        sock.destroy();
      }
    };
    sock.on("data", onData);
    sock.on("end", () => sock.destroy());
    if (rest.length > 0) onData(rest);
    if (!this.sandboxes.has(params.sandbox_id)) {
      return proc.fail(ERROR_CODES.SANDBOX_NOT_FOUND, `no such sandbox: ${params.sandbox_id}`, { sandbox_id: params.sandbox_id });
    }
    void (async () => {
      try {
        // One tick later, when the socket has settled into flowing mode: a script may pause it.
        await new Promise<void>((resolve) => process.nextTick(resolve));
        await this.#spawn(proc);
      } catch {
        proc.drop();
      }
    })();
  }

  #attach(sock: net.Socket, msg: any, rest: Buffer): void {
    const connId = msg.params?.conn_id;
    const f = this.#forwards.get(connId);
    if (!f) {
      sock.end(encodeFrame({ jsonrpc: "2.0", id: msg.id, error: { code: -32004, message: "unknown conn_id", data: { detail: "no announced connection with this conn_id" } } }));
      return;
    }
    this.#forwards.delete(connId);
    const counters = this.sandboxes.get(f.sandboxId)?.forwards.find((x) => x.guest_port === f.guestPort);
    if (counters) counters.accepted += 1;
    sock.write(encodeFrame({ jsonrpc: "2.0", id: msg.id, result: {} }));
    if (rest.length > 0) sock.unshift(rest);
    f.resolve(sock);
  }
}
