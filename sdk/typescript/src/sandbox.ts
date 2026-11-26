import { realpath } from "node:fs/promises";
import path from "node:path";

import { AbortedError, InvalidSpecError, SandcrabError } from "./errors.ts";
import { exec } from "./exec.ts";
import type { ExecOptions, ExecOutput } from "./exec.ts";
import { validateForwards } from "./forwards.ts";
import type { ForwardSpec } from "./forwards.ts";
import type { ClientImpl } from "./client.ts";
import type { SandboxCreate, SandboxInfo } from "./generated/api.ts";
import { spawnProcess } from "./process.ts";
import type { ProcessHandle, SpawnOptions } from "./process.ts";

export type Labels = Readonly<Record<string, string>>;

export interface SandboxSpec {
  template: string;
  labels?: Labels;
  /** Absent: the server's default. `null`: no TTL. */
  ttlMs?: number | null;
  mounts?: readonly { path: string; mode: "ro" | "rw" }[];
  network?: "disabled" | "unrestricted";
  forwards?: readonly ForwardSpec[];
  user?: { uid: number; gid: number };
  resources?: { vcpu?: number; memoryMb?: number; diskMb?: number; pidsMax?: number; nofile?: number };
  tmpExec?: boolean;
  /** Size of the sandbox's `/tmp` in MiB. Absent: the template's or backend's default. */
  tmpSizeMb?: number;
}

export interface Sandbox {
  readonly id: string;
  /** `path` is what was asked, `resolvedPath` is what the sandbox sees. Empty for `client.sandbox(id)`. */
  readonly mounts: readonly { path: string; resolvedPath: string; mode: "ro" | "rw" }[];
  /** The sandbox as the server sees it, with fresh forward counters. */
  info(): Promise<SandboxInfo>;
  /** Returns at once. Throws InvalidSpecError for a malformed spec, before anything starts; never otherwise. */
  spawn(spec: SpawnOptions): ProcessHandle;
  /** Rejects only with InvalidSpecError. Every other failure is in the outcome. */
  exec(spec: ExecOptions): Promise<ExecOutput>;
  /** After a client restart: serve the sandbox's existing forwards with these targets. */
  serveForwards(forwards: readonly ForwardSpec[]): Promise<void>;
  setTtl(ttlMs: number | null): Promise<void>;
  /** Renews the TTL on an interval until the returned function is called. */
  keepAlive(ttlMs: number): () => void;
  /** False: it was already gone. */
  destroy(): Promise<boolean>;
}

const MAX_LABELS = 16;
const MAX_MOUNTS = 16;
const LABEL_KEY = /^[a-z0-9][a-z0-9._-]{0,62}$/;

/** Throws `InvalidSpecError`; returns a plain copy. */
export function validateLabels(labels: Labels): Record<string, string> {
  if (labels === null || typeof labels !== "object" || Array.isArray(labels)) throw new InvalidSpecError("`labels` must be an object of strings");
  const entries = Object.entries(labels);
  if (entries.length > MAX_LABELS) throw new InvalidSpecError(`at most ${MAX_LABELS} labels`);
  for (const [k, v] of entries) {
    if (!LABEL_KEY.test(k) || k.startsWith("sandcrab.")) throw new InvalidSpecError(`invalid label key ${JSON.stringify(k)}`);
    if (typeof v !== "string" || Buffer.byteLength(v, "utf8") > 256) throw new InvalidSpecError(`the value of label ${k} must be a string of at most 256 bytes`);
  }
  return Object.fromEntries(entries);
}

function ttlSecs(ttlMs: number | null): number | null {
  if (ttlMs === null) return null;
  if (typeof ttlMs !== "number" || !Number.isFinite(ttlMs) || ttlMs <= 0) throw new InvalidSpecError("`ttlMs` must be a positive number, or null for no TTL");
  return Math.ceil(ttlMs / 1000);
}

function positiveInt(v: unknown, name: string): number {
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 1) throw new InvalidSpecError(`\`${name}\` must be a positive integer`);
  return v;
}

type ResolvedMount = { path: string; resolvedPath: string; mode: "ro" | "rw" };

async function prepareCreate(spec: SandboxSpec): Promise<{ params: SandboxCreate; mounts: ResolvedMount[] }> {
  if (spec === null || typeof spec !== "object") throw new InvalidSpecError("a sandbox spec must be an object");
  if (typeof spec.template !== "string" || spec.template === "") throw new InvalidSpecError("`template` must be a non-empty string");
  const params: SandboxCreate = { template: spec.template };
  if (spec.labels !== undefined) params.labels = validateLabels(spec.labels);
  if (spec.ttlMs !== undefined) params.ttl_secs = ttlSecs(spec.ttlMs);
  if (spec.network !== undefined) {
    if (spec.network !== "disabled" && spec.network !== "unrestricted") throw new InvalidSpecError('`network` must be "disabled" or "unrestricted"');
    params.network = spec.network;
  }
  if (spec.user !== undefined) {
    const { uid, gid } = spec.user ?? {};
    if (!Number.isSafeInteger(uid) || uid < 0 || !Number.isSafeInteger(gid) || gid < 0) throw new InvalidSpecError("`user` needs a non-negative integer uid and gid");
    params.user = `${uid}:${gid}`;
  }
  if (spec.resources !== undefined) {
    const r = spec.resources ?? {};
    params.resources = {};
    if (r.vcpu !== undefined) params.resources.vcpu = positiveInt(r.vcpu, "resources.vcpu");
    if (r.memoryMb !== undefined) params.resources.memory_mb = positiveInt(r.memoryMb, "resources.memoryMb");
    if (r.diskMb !== undefined) params.resources.disk_mb = positiveInt(r.diskMb, "resources.diskMb");
    if (r.pidsMax !== undefined) params.resources.pids_max = positiveInt(r.pidsMax, "resources.pidsMax");
    if (r.nofile !== undefined) params.resources.nofile = positiveInt(r.nofile, "resources.nofile");
  }
  if (spec.tmpExec !== undefined) {
    if (typeof spec.tmpExec !== "boolean") throw new InvalidSpecError("`tmpExec` must be a boolean");
    params.tmp_exec = spec.tmpExec;
  }
  if (spec.tmpSizeMb !== undefined) params.tmp_size_mb = positiveInt(spec.tmpSizeMb, "tmpSizeMb");
  if (spec.forwards !== undefined && spec.forwards.length > 0) {
    validateForwards(spec.forwards);
    params.forwards = spec.forwards.map((f) => (f.maxConnections === undefined ? { guest_port: f.guestPort } : { guest_port: f.guestPort, max_connections: f.maxConnections }));
  }
  const mounts: ResolvedMount[] = [];
  if (spec.mounts !== undefined) {
    if (!Array.isArray(spec.mounts)) throw new InvalidSpecError("`mounts` must be an array");
    if (spec.mounts.length > MAX_MOUNTS) throw new InvalidSpecError(`at most ${MAX_MOUNTS} mounts per sandbox`);
    for (const m of spec.mounts as readonly { path: string; mode: "ro" | "rw" }[]) {
      if (m === null || typeof m !== "object" || typeof m.path !== "string" || !path.isAbsolute(m.path) || m.path.includes("\0")) {
        throw new InvalidSpecError("a mount needs an absolute `path`");
      }
      if (m.mode !== "ro" && m.mode !== "rw") throw new InvalidSpecError(`the mode of mount ${m.path} must be "ro" or "rw"`);
      let resolvedPath: string;
      try {
        resolvedPath = await realpath(m.path);
      } catch {
        throw new InvalidSpecError(`mount path does not exist: ${m.path}`);
      }
      mounts.push({ path: m.path, resolvedPath, mode: m.mode });
    }
    if (mounts.length > 0) params.mounts = mounts.map((m) => ({ path: m.resolvedPath, mode: m.mode }));
  }
  return { params, mounts };
}

export async function createSandbox(client: ClientImpl, spec: SandboxSpec, signal?: AbortSignal): Promise<Sandbox> {
  const { params, mounts } = await prepareCreate(spec);
  for (const [feature, used] of [["mounts", mounts.length > 0], ["forwards", (spec.forwards?.length ?? 0) > 0]] as const) {
    const missing = used ? client._missingFeature(feature) : null;
    if (missing !== null) throw new SandcrabError("UNSUPPORTED_FEATURE", missing);
  }
  if (signal?.aborted) throw new AbortedError();
  let sandbox: SandboxImpl | undefined;
  let aborted = false;
  const request = client._request("sandbox.create", params, {
    // In the tick the answer is read: a forwarded connection may be announced in the very next frame.
    onResult: (info) => {
      if (aborted) {
        client._request("sandbox.destroy", { sandbox_id: info.sandbox_id }).catch(() => {});
        return;
      }
      if (spec.forwards && spec.forwards.length > 0) client._forwards.set(info.sandbox_id, spec.forwards);
      sandbox = new SandboxImpl(client, info.sandbox_id, mounts);
    },
  });
  if (!signal) {
    await request;
    return sandbox as SandboxImpl;
  }
  return new Promise<Sandbox>((resolve, reject) => {
    const onAbort = (): void => {
      // The create may still succeed on the server; its answer then destroys the sandbox.
      aborted = true;
      reject(new AbortedError());
    };
    signal.addEventListener("abort", onAbort, { once: true });
    request.then(
      () => {
        signal.removeEventListener("abort", onAbort);
        if (!aborted) resolve(sandbox as SandboxImpl);
      },
      (e: unknown) => {
        signal.removeEventListener("abort", onAbort);
        if (!aborted) reject(e as Error);
      },
    );
  });
}

export class SandboxImpl implements Sandbox {
  readonly id: string;
  readonly mounts: readonly ResolvedMount[];
  readonly #client: ClientImpl;

  constructor(client: ClientImpl, id: string, mounts: readonly ResolvedMount[]) {
    this.#client = client;
    this.id = id;
    this.mounts = mounts;
  }

  info(): Promise<SandboxInfo> {
    return this.#client._request("sandbox.get", { sandbox_id: this.id }, { idempotent: true });
  }

  spawn(spec: SpawnOptions): ProcessHandle {
    return spawnProcess(this.#client, this.id, spec);
  }

  exec(spec: ExecOptions): Promise<ExecOutput> {
    return exec(this.#client, this.id, spec);
  }

  async serveForwards(forwards: readonly ForwardSpec[]): Promise<void> {
    validateForwards(forwards);
    this.#client._forwards.set(this.id, forwards);
    await this.#client._request("forward.serve", { sandbox_id: this.id });
  }

  async setTtl(ttlMs: number | null): Promise<void> {
    await this.#client._request("sandbox.set_ttl", { sandbox_id: this.id, ttl_secs: ttlSecs(ttlMs) });
  }

  keepAlive(ttlMs: number): () => void {
    if (ttlSecs(ttlMs) === null) throw new InvalidSpecError("`keepAlive` needs a TTL");
    const renew = (): void => {
      this.setTtl(ttlMs).catch(() => {});
    };
    renew();
    const timer = setInterval(renew, Math.max(250, ttlMs / 3));
    timer.unref();
    return () => clearInterval(timer);
  }

  async destroy(): Promise<boolean> {
    const r = await this.#client._request("sandbox.destroy", { sandbox_id: this.id });
    this.#client._forwards.drop(this.id);
    return r.destroyed;
  }
}
