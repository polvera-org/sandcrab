import { EventEmitter } from "node:events";
import net from "node:net";

import { AbortedError, CODE_UNSUPPORTED_VERSION, ConnectionError, InvalidSpecError, isRpcError, NOT_CONNECTED, serverError } from "./errors.ts";
import { ForwardRegistry } from "./forwards.ts";
import { encodeFrame, FrameDecoder } from "./framing.ts";
import { API_VERSION } from "./generated/api.ts";
import type { Capabilities, Methods, SandboxInfo, SelftestReport, ServerHelloResult, UnsupportedVersionData } from "./generated/api.ts";
import { createSandbox, SandboxImpl, validateLabels } from "./sandbox.ts";
import type { Labels, Sandbox, SandboxSpec } from "./sandbox.ts";
import { CapabilityCache } from "./selftest.ts";
import type { CapabilitiesSnapshot } from "./selftest.ts";

/** The oldest server API version this SDK works with. */
export const MIN_SERVER_API_VERSION = "1.0";

/** How long a retried read (`sandbox.get`, `sandbox.list`, `server.capabilities`) waits for the reconnect. */
const RETRY_WAIT_MS = 5000;

export interface ConnectOptions {
  socketPath: string;
  /** Sent in `server.hello`, for the server's log. */
  clientName?: string;
  /** Aborts the first connect. */
  signal?: AbortSignal;
  /** Default: backoff from 50 ms to 2 s, with jitter. `false`: a lost connection stays lost. */
  reconnect?: boolean | { minMs: number; maxMs: number };
}

/** Not part of the public API: lets the tests play an SDK that needs a newer server. */
export interface ConnectInternals {
  apiVersion?: string;
  minServerVersion?: string;
}

export interface SandcrabClient {
  /** What `server.hello` answered. `null` until a hello was answered. */
  readonly server: { version: string; apiVersion: string; features: readonly string[] } | null;
  /** True while the control connection is up and the server's version fits. */
  readonly connected: boolean;
  createSandbox(spec: SandboxSpec, o?: { signal?: AbortSignal }): Promise<Sandbox>;
  /** A handle for a sandbox that exists: after a client restart, or from another process. */
  sandbox(id: string): Sandbox;
  listSandboxes(filter?: { labels?: Labels; createdAfter?: Date; createdBefore?: Date }): Promise<SandboxInfo[]>;
  destroyByLabel(labels: Labels): Promise<{ matched: number; destroyed: number; remaining: string[]; empty: boolean }>;
  capabilities(template?: string): Promise<Capabilities>;
  /** The last capabilities seen for this template, refreshed in the background. Never throws. */
  capabilitiesSnapshot(template: string): CapabilitiesSnapshot;
  selftest(template: string, o?: { maxAgeMs?: number }): Promise<SelftestReport>;
  on(event: "connect" | "disconnect", listener: () => void): this;
  /**
   * Closes the control connection and every forwarded connection, and stops reconnecting.
   * Processes keep their own connections and run on until they exit or are cancelled.
   */
  close(): Promise<void>;
}

type ControlMethod = { [M in keyof Methods]: Methods[M]["kind"] extends "control" ? M : never }[keyof Methods];

interface Pending {
  resolve(value: unknown): void;
  reject(err: Error): void;
  onResult?: (value: never) => void;
}

function parseVersion(v: unknown): [number, number] | null {
  if (typeof v !== "string") return null;
  const m = /^(\d+)\.(\d+)$/.exec(v);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

/** `null` when a server at `server` fits an SDK that needs `min`; otherwise the reason, naming both. */
export function versionMismatch(server: string, min: string): string | null {
  const s = parseVersion(server);
  const m = parseVersion(min);
  if (s && m && s[0] === m[0] && s[1] >= m[1]) return null;
  if (s && m && s[0] > m[0]) return versionRefused(server, min);
  return `sandcrab server speaks API ${server}, this SDK needs ${min} or later: upgrade the server`;
}

/** The reason when the server answered `UNSUPPORTED_VERSION` to an SDK that speaks `client`. */
export function versionRefused(server: string, client: string): string {
  const s = parseVersion(server);
  const c = parseVersion(client);
  const serverIsNewer = s !== null && c !== null && (s[0] > c[0] || (s[0] === c[0] && s[1] > c[1]));
  return `sandcrab server speaks API ${server}, this SDK speaks ${client}: upgrade the ${serverIsNewer ? "SDK" : "server"}`;
}

export class ClientImpl extends EventEmitter implements SandcrabClient {
  readonly _socketPath: string;
  readonly _forwards: ForwardRegistry;
  readonly #caps: CapabilityCache;
  readonly #clientName: string;
  readonly #apiVersion: string;
  readonly #minVersion: string;
  readonly #backoff: { minMs: number; maxMs: number } | null;
  #sock: net.Socket | null = null;
  #ready = false;
  #closed = false;
  #hello: ServerHelloResult | null = null;
  #incompatible: string | null = null;
  #pending = new Map<number, Pending>();
  #nextId = 1;
  #attempt = 0;
  #timer: NodeJS.Timeout | null = null;
  #closeWaiters = new Set<() => void>();

  constructor(o: ConnectOptions, internals: ConnectInternals = {}) {
    super();
    if (typeof o?.socketPath !== "string" || o.socketPath === "") throw new InvalidSpecError("`socketPath` must be a path");
    this._socketPath = o.socketPath;
    this.#clientName = o.clientName ?? "@sandcrab/sdk";
    this.#apiVersion = internals.apiVersion ?? API_VERSION;
    this.#minVersion = internals.minServerVersion ?? MIN_SERVER_API_VERSION;
    const r = o.reconnect ?? true;
    this.#backoff = r === false ? null : r === true ? { minMs: 50, maxMs: 2000 } : { minMs: Math.max(1, r.minMs), maxMs: Math.max(r.minMs, r.maxMs) };
    this._forwards = new ForwardRegistry(this);
    this.#caps = new CapabilityCache(this);
  }

  get server(): { version: string; apiVersion: string; features: readonly string[] } | null {
    const h = this.#hello;
    return h ? { version: h.server_version, apiVersion: h.api_version, features: h.features } : null;
  }

  get connected(): boolean {
    return this.#ready;
  }

  /** The backend `server.hello` named, if any hello was answered. */
  get _backend(): string | null {
    return this.#hello?.backend ?? null;
  }

  /** Why nothing can be started on this server (its API version), or `null`. */
  get _incompatible(): string | null {
    return this.#incompatible;
  }

  get _closed(): boolean {
    return this.#closed;
  }

  /** Connects and says hello. Rejects when the socket cannot be reached or the server refuses the peer. */
  _open(signal?: AbortSignal): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      if (signal?.aborted) return reject(new AbortedError());
      const sock = net.connect(this._socketPath);
      const decoder = new FrameDecoder();
      let settled = false;
      const settle = (): boolean => {
        if (settled) return false;
        settled = true;
        signal?.removeEventListener("abort", onAbort);
        return true;
      };
      const fail = (err: Error): void => {
        if (!settle()) return;
        sock.destroy();
        reject(err);
      };
      const onAbort = (): void => fail(new AbortedError());
      signal?.addEventListener("abort", onAbort, { once: true });

      const onHello = (msg: { result?: unknown; error?: unknown }): void => {
        if (isRpcError(msg.error)) {
          if (msg.error.code !== CODE_UNSUPPORTED_VERSION) return fail(serverError(msg.error));
          // The server refuses this SDK's version. `connect` still resolves; nothing can be started.
          const d = (msg.error.data ?? {}) as Partial<UnsupportedVersionData>;
          this.#hello = null;
          this.#incompatible = versionRefused(String(d.server ?? "unknown"), this.#apiVersion);
          settle();
          sock.destroy();
          resolve();
          this.#scheduleReconnect();
          return;
        }
        const hello = msg.result as ServerHelloResult;
        if (!settle()) return;
        this.#hello = hello;
        this.#incompatible = versionMismatch(hello.api_version, this.#minVersion);
        this.#sock = sock;
        this.#attempt = 0;
        if (this.#incompatible === null) {
          this.#ready = true;
          this._forwards.serveAll();
          this.#caps.refreshAll();
        }
        resolve();
        if (this.#ready) this.emit("connect");
      };

      sock.on("connect", () => {
        sock.write(encodeFrame({ jsonrpc: "2.0", id: 0, method: "server.hello", params: { client: this.#clientName, api_version: this.#apiVersion } }));
      });
      sock.on("data", (chunk: Buffer) => {
        try {
          decoder.push(chunk);
          for (let msg = decoder.next(); msg !== undefined; msg = decoder.next()) {
            if (settled) this.#dispatch(msg as Record<string, unknown>);
            else onHello(msg as { result?: unknown; error?: unknown });
          }
        } catch {
          sock.destroy();
        }
      });
      sock.on("error", (e) => fail(new ConnectionError("NOT_CONNECTED", `cannot reach the sandcrab server at ${this._socketPath}: ${e.message}`, { cause: e })));
      sock.on("close", () => {
        fail(new ConnectionError("NOT_CONNECTED", "the sandcrab server closed the connection before it answered hello"));
        if (this.#sock === sock) this.#down();
      });
    });
  }

  #down(): void {
    const wasReady = this.#ready;
    this.#ready = false;
    this.#sock = null;
    const pending = [...this.#pending.values()];
    this.#pending.clear();
    const err = this.#closed ? new ConnectionError("CLOSED", "the client is closed") : new ConnectionError("CONNECTION_LOST", "the connection to the sandcrab server was lost");
    for (const p of pending) p.reject(err);
    if (wasReady) this.emit("disconnect");
    this.#scheduleReconnect();
  }

  #scheduleReconnect(): void {
    if (this.#closed || !this.#backoff || this.#timer) return;
    const base = Math.min(this.#backoff.maxMs, this.#backoff.minMs * 2 ** Math.min(this.#attempt, 20));
    this.#attempt += 1;
    this.#timer = setTimeout(() => {
      this.#timer = null;
      if (this.#closed) return;
      this._open().catch(() => this.#scheduleReconnect());
    }, base * (0.5 + Math.random() / 2));
  }

  #dispatch(msg: Record<string, unknown>): void {
    if (typeof msg.id === "number" && ("result" in msg || "error" in msg)) {
      const p = this.#pending.get(msg.id);
      if (!p) return;
      this.#pending.delete(msg.id);
      if (isRpcError(msg.error)) return p.reject(serverError(msg.error));
      try {
        p.onResult?.(msg.result as never);
      } catch (e) {
        return p.reject(e as Error);
      }
      return p.resolve(msg.result);
    }
    if (typeof msg.method !== "string" || "id" in msg) return;
    const params = (msg.params ?? {}) as Record<string, unknown>;
    if (msg.method === "forward.incoming") this._forwards.incoming(params);
    else if (msg.method === "sandbox.destroyed" && typeof params.sandbox_id === "string") this._forwards.drop(params.sandbox_id);
  }

  #send(method: string, params: unknown, onResult?: (value: never) => void): Promise<unknown> {
    return new Promise((resolve, reject) => {
      if (this.#closed) return reject(new ConnectionError("CLOSED", "the client is closed"));
      if (this.#incompatible !== null) return reject(new ConnectionError("UNSUPPORTED_VERSION", this.#incompatible));
      if (!this.#ready || !this.#sock) return reject(new ConnectionError("NOT_CONNECTED", NOT_CONNECTED));
      const id = this.#nextId++;
      this.#pending.set(id, { resolve, reject, onResult });
      this.#sock.write(encodeFrame({ jsonrpc: "2.0", id, method, params }));
    });
  }

  #waitReady(ms: number): Promise<void> {
    return new Promise((resolve, reject) => {
      if (this.#ready) return resolve();
      const done = (err?: Error): void => {
        clearTimeout(timer);
        this.off("connect", onConnect);
        this.#closeWaiters.delete(onClose);
        if (err) reject(err);
        else resolve();
      };
      const onConnect = (): void => done();
      const onClose = (): void => done(new ConnectionError("CLOSED", "the client is closed"));
      const timer = setTimeout(() => done(new ConnectionError("CONNECTION_LOST", "the connection to the sandcrab server was lost")), ms);
      this.on("connect", onConnect);
      this.#closeWaiters.add(onClose);
    });
  }

  /**
   * One request on the control connection. `idempotent` requests whose connection dropped are sent
   * once more after the reconnect. `onResult` runs in the same tick the answer is read, before
   * any later frame is looked at.
   */
  async _request<M extends ControlMethod>(
    method: M,
    params: Methods[M]["params"],
    o: { idempotent?: boolean; onResult?: (result: Methods[M]["result"]) => void } = {},
  ): Promise<Methods[M]["result"]> {
    try {
      return (await this.#send(method, params, o.onResult)) as Methods[M]["result"];
    } catch (e) {
      const lost = e instanceof ConnectionError && e.code === "CONNECTION_LOST";
      if (!o.idempotent || !lost || !this.#backoff || this.#closed) throw e;
    }
    await this.#waitReady(RETRY_WAIT_MS);
    return (await this.#send(method, params, o.onResult)) as Methods[M]["result"];
  }

  createSandbox(spec: SandboxSpec, o?: { signal?: AbortSignal }): Promise<Sandbox> {
    return createSandbox(this, spec, o?.signal);
  }

  sandbox(id: string): Sandbox {
    if (typeof id !== "string" || id === "") throw new InvalidSpecError("a sandbox id must be a non-empty string");
    return new SandboxImpl(this, id, []);
  }

  async listSandboxes(filter: { labels?: Labels; createdAfter?: Date; createdBefore?: Date } = {}): Promise<SandboxInfo[]> {
    const params: Methods["sandbox.list"]["params"] = {};
    if (filter.labels && Object.keys(filter.labels).length > 0) params.labels = validateLabels(filter.labels);
    if (filter.createdAfter) params.created_after = isoDate(filter.createdAfter, "createdAfter");
    if (filter.createdBefore) params.created_before = isoDate(filter.createdBefore, "createdBefore");
    return this._request("sandbox.list", params, { idempotent: true });
  }

  async destroyByLabel(labels: Labels): Promise<{ matched: number; destroyed: number; remaining: string[]; empty: boolean }> {
    const checked = validateLabels(labels);
    if (Object.keys(checked).length === 0) throw new InvalidSpecError("`destroyByLabel` needs at least one label");
    const r = await this._request("sandbox.destroy_by_label", { labels: checked });
    return { matched: r.matched, destroyed: r.destroyed, remaining: r.remaining, empty: r.empty };
  }

  capabilities(template?: string): Promise<Capabilities> {
    return this.#caps.capabilities(template);
  }

  capabilitiesSnapshot(template: string): CapabilitiesSnapshot {
    return this.#caps.snapshot(template);
  }

  selftest(template: string, o?: { maxAgeMs?: number }): Promise<SelftestReport> {
    return this.#caps.selftest(template, o);
  }

  close(): Promise<void> {
    if (!this.#closed) {
      this.#closed = true;
      if (this.#timer) clearTimeout(this.#timer);
      this.#timer = null;
      this._forwards.closeAll();
      const sock = this.#sock;
      if (sock) sock.destroy();
      else this.#down();
      for (const w of [...this.#closeWaiters]) w();
    }
    return Promise.resolve();
  }
}

function isoDate(d: Date, name: string): string {
  if (!(d instanceof Date) || Number.isNaN(d.getTime())) throw new InvalidSpecError(`\`${name}\` must be a valid Date`);
  return d.toISOString();
}

/** See `connect` in the package index. `internals` is for this package's own tests. */
export async function connectClient(o: ConnectOptions, internals?: ConnectInternals): Promise<ClientImpl> {
  const client = new ClientImpl(o, internals);
  await client._open(o.signal);
  return client;
}
