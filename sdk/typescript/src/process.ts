import { randomUUID } from "node:crypto";
import net from "node:net";

import { CODE_CANCELLED, CODE_SPAWN_FAILED, CODE_UNAVAILABLE, errorName, InvalidSpecError, isRpcError } from "./errors.ts";
import type { RpcErrorObject } from "./errors.ts";
import { encodeFrame, encodeRecord, FrameDecoder, FramingError, MAX_RECORD_DATA_LEN, RECORD_EXIT, RECORD_STDERR, RECORD_STDIN, RECORD_STDIN_EOF, RECORD_STDOUT, RecordDecoder } from "./framing.ts";
import type { ClientImpl } from "./client.ts";
import type { OutputCap, ProcessExit as WireExit, ProcessSpawnParams } from "./generated/api.ts";

export interface ProcessBase {
  /** `argv[0]` is the program; looked up in the `PATH` of `env` when it has no `/`. No shell. */
  argv: readonly string[];
  /** The exact environment. Nothing is added, nothing inherited. */
  env: Readonly<Record<string, string>>;
  /** Absolute. For a mounted folder use its `resolvedPath`. */
  cwd?: string;
  limits?: { nproc?: number; dataBytes?: number; fsizeBytes?: number; nofile?: number };
  oomScoreAdj?: number;
  stderr?: "tail" | "merge";
  /** Default 4096, at most 65536. */
  stderrTailBytes?: number;
  /** Grace between ask and force at the timeout, at the output cap and on a lost connection. Default 500. */
  killGraceMs?: number;
}

export interface SpawnOptions extends ProcessBase {
  /** Written to stdin, which is then closed; not with "open". */
  input?: string | Uint8Array;
  /** "end" (default): stdin is closed after `input`. "open": it stays open until `end()`. */
  stdin?: "end" | "open";
  timeoutMs?: number;
}

export interface ProcessHandle {
  /** Ordered bytes, one consumer. Read it, or `exit` may never resolve (the SDK holds at most 1 MiB). */
  readonly stdout: AsyncIterable<Uint8Array>;
  /** Never throws. Data written before the connection is up is kept and sent in order. */
  write(data: string | Uint8Array): void;
  end(): void;
  /** Ask, wait `graceMs` (default 5000), force. Resolves when nothing is left. Idempotent. Never rejects. */
  cancel(graceMs?: number): Promise<void>;
  cancelled(): boolean;
  /** False: it could not be started. Never rejects. */
  readonly started: Promise<boolean>;
  /** After the last byte. Never rejects. */
  readonly exit: Promise<ProcessExit>;
}

export interface ProcessExit {
  code: number | null;
  /** Node's name of the signal that ended the program, e.g. "SIGKILL". */
  signal: string | null;
  stderrTail: string;
  /** The program could not be started. Trying again will not help. */
  spawnError: { errno: string; what: "program" | "cwd"; message: string } | null;
  /** The server refused the request for another reason. */
  rejected: { code: string; message: string } | null;
  /** Nothing ran: the request was never written, or the server said so. Trying again may work. */
  unavailable: boolean;
  /** The connection dropped after the request was written: the program may have run. Do not retry as if nothing ran. */
  lost: boolean;
  timedOut: boolean;
  cancelled: boolean;
  overflow: boolean;
  /** False: something could not be stopped, or (with `lost`) is not known to be stopped; destroy the sandbox. */
  treeClean: boolean;
}

/** What `exec` adds to a spawn. Not part of the public API. */
export interface SpawnInternals {
  /** stdin is a pipe that stays open until `end()`, whatever `stdin` says. */
  stdinPipe?: boolean;
  /** Overrides `stderr`: `STDERR` records go to `onStderr`. */
  stderrStream?: (chunk: Uint8Array) => void;
  outputCap?: OutputCap;
}

/** The SDK holds at most this much unread stdout per process, then stops reading the socket. */
export const STDOUT_HIGH_WATER = 1024 * 1024;
export const DEFAULT_CANCEL_GRACE_MS = 5000;
/** After a cancel was answered the `EXIT` record must follow; if it does not, the connection is closed. */
const EXIT_AFTER_CANCEL_MS = 5000;

const MAX_STDERR_TAIL_BYTES = 64 * 1024;
const MIN_DATA_BYTES = 32 * 1024 * 1024;
const MIN_NOFILE = 64;

// Linux signal numbers (x86-64 and arm64) under Node's names. The guest is always Linux,
// whatever the client runs on.
const SIGNALS = [
  "SIGHUP", "SIGINT", "SIGQUIT", "SIGILL", "SIGTRAP", "SIGABRT", "SIGBUS", "SIGFPE", "SIGKILL", "SIGUSR1",
  "SIGSEGV", "SIGUSR2", "SIGPIPE", "SIGALRM", "SIGTERM", "SIGSTKFLT", "SIGCHLD", "SIGCONT", "SIGSTOP", "SIGTSTP",
  "SIGTTIN", "SIGTTOU", "SIGURG", "SIGXCPU", "SIGXFSZ", "SIGVTALRM", "SIGPROF", "SIGWINCH", "SIGIO", "SIGPWR", "SIGSYS",
]; // prettier-ignore

export function signalName(n: number): string {
  return SIGNALS[n - 1] ?? `SIG${n}`;
}

const BASE_EXIT: ProcessExit = {
  code: null,
  signal: null,
  stderrTail: "",
  spawnError: null,
  rejected: null,
  unavailable: false,
  lost: false,
  timedOut: false,
  cancelled: false,
  overflow: false,
  treeClean: true,
};

function uint(v: unknown, name: string, min = 0, max = Number.MAX_SAFE_INTEGER): number {
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < min || v > max) {
    throw new InvalidSpecError(`\`${name}\` must be an integer${max === Number.MAX_SAFE_INTEGER ? ` of at least ${min}` : ` between ${min} and ${max}`}`);
  }
  return v;
}

/** Validates the part every process shares and returns it as wire fields. Throws `InvalidSpecError`. */
export function processParams(spec: ProcessBase): Omit<ProcessSpawnParams, "process_id" | "sandbox_id"> {
  if (spec === null || typeof spec !== "object") throw new InvalidSpecError("a process spec must be an object");
  const { argv, env } = spec;
  if (!Array.isArray(argv) || argv.length === 0 || argv.some((a) => typeof a !== "string")) throw new InvalidSpecError("`argv` must be a non-empty array of strings");
  if (argv[0] === "") throw new InvalidSpecError("`argv[0]` must name the program");
  if (argv.some((a: string) => a.includes("\0"))) throw new InvalidSpecError("`argv` elements must not contain NUL");
  if (env === null || typeof env !== "object" || Array.isArray(env)) throw new InvalidSpecError("`env` must be an object of strings: the exact environment");
  for (const [k, v] of Object.entries(env)) {
    // Values are never echoed: they may be secrets.
    if (k === "" || k.includes("=") || k.includes("\0")) throw new InvalidSpecError(`\`env\` has an invalid name ${JSON.stringify(k)}`);
    if (typeof v !== "string" || v.includes("\0")) throw new InvalidSpecError(`\`env.${k}\` must be a string without NUL`);
  }
  const out: Omit<ProcessSpawnParams, "process_id" | "sandbox_id"> = { argv: [...argv], env: { ...env } };
  if (spec.cwd !== undefined) {
    if (typeof spec.cwd !== "string" || !spec.cwd.startsWith("/") || spec.cwd.includes("\0")) throw new InvalidSpecError("`cwd` must be an absolute path");
    out.cwd = spec.cwd;
  }
  if (spec.limits !== undefined) {
    const l = spec.limits ?? {};
    out.limits = {};
    if (l.nproc !== undefined) out.limits.nproc = uint(l.nproc, "limits.nproc");
    if (l.dataBytes !== undefined) out.limits.data_bytes = uint(l.dataBytes, "limits.dataBytes", MIN_DATA_BYTES);
    if (l.fsizeBytes !== undefined) out.limits.fsize_bytes = uint(l.fsizeBytes, "limits.fsizeBytes");
    if (l.nofile !== undefined) out.limits.nofile = uint(l.nofile, "limits.nofile", MIN_NOFILE);
  }
  if (spec.oomScoreAdj !== undefined) out.oom_score_adj = uint(spec.oomScoreAdj, "oomScoreAdj", 0, 1000);
  if (spec.stderr !== undefined) {
    if (spec.stderr !== "tail" && spec.stderr !== "merge") throw new InvalidSpecError('`stderr` must be "tail" or "merge"');
    out.stderr = spec.stderr;
  }
  if (spec.stderrTailBytes !== undefined) out.stderr_tail_bytes = uint(spec.stderrTailBytes, "stderrTailBytes", 0, MAX_STDERR_TAIL_BYTES);
  if (spec.killGraceMs !== undefined) out.kill_grace_ms = uint(spec.killGraceMs, "killGraceMs");
  return out;
}

export function checkInput(input: unknown): void {
  if (typeof input !== "string" && !(input instanceof Uint8Array)) throw new InvalidSpecError("`input` must be a string or bytes");
}

/** `sandbox.spawn`: validates, creates the handle, connects behind it. */
export function spawnProcess(client: ClientImpl, sandboxId: string, spec: SpawnOptions, internals: SpawnInternals = {}): ProcessImpl {
  const params: ProcessSpawnParams = { ...processParams(spec), process_id: randomUUID(), sandbox_id: sandboxId };
  if (spec.stdin !== undefined && spec.stdin !== "end" && spec.stdin !== "open") throw new InvalidSpecError('`stdin` must be "end" or "open"');
  if (spec.input !== undefined) {
    checkInput(spec.input);
    if (spec.stdin === "open") throw new InvalidSpecError('`input` cannot be combined with `stdin: "open"`: use `write`');
  }
  if (spec.timeoutMs !== undefined) params.timeout_ms = uint(spec.timeoutMs, "timeoutMs", 1);
  const open = spec.stdin === "open" || internals.stdinPipe === true;
  const hasInput = spec.input !== undefined && spec.input.length > 0;
  if (open || hasInput) params.stdin = "pipe";
  if (internals.stderrStream) params.stderr = "stream";
  if (internals.outputCap) params.output_cap = internals.outputCap;
  return new ProcessImpl(client, params, open, hasInput ? spec.input : undefined, internals.stderrStream);
}

function exitFromError(e: RpcErrorObject): ProcessExit {
  if (e.code === CODE_UNAVAILABLE) return { ...BASE_EXIT, unavailable: true };
  if (e.code === CODE_CANCELLED) return { ...BASE_EXIT, cancelled: true };
  if (e.code === CODE_SPAWN_FAILED) {
    const d = (e.data ?? {}) as { errno?: unknown; what?: unknown };
    return { ...BASE_EXIT, spawnError: { errno: typeof d.errno === "string" ? d.errno : "UNKNOWN", what: d.what === "cwd" ? "cwd" : "program", message: String(e.message) } };
  }
  return { ...BASE_EXIT, rejected: { code: errorName(e.code), message: String(e.message) } };
}

function exitFromRecord(w: WireExit): ProcessExit {
  return {
    ...BASE_EXIT,
    code: typeof w.code === "number" ? w.code : null,
    signal: typeof w.signal === "number" ? signalName(w.signal) : null,
    stderrTail: typeof w.stderr_tail === "string" ? w.stderr_tail : "",
    timedOut: w.timed_out === true,
    cancelled: w.cancelled === true,
    overflow: w.overflow === true,
    treeClean: w.tree_clean === true,
  };
}

function drained(sock: net.Socket): Promise<void> {
  return new Promise((resolve) => {
    const done = (): void => {
      sock.off("drain", done);
      sock.off("close", done);
      resolve();
    };
    sock.on("drain", done);
    sock.on("close", done);
  });
}

export class ProcessImpl implements ProcessHandle {
  readonly stdout: AsyncIterable<Uint8Array>;
  readonly started: Promise<boolean>;
  readonly exit: Promise<ProcessExit>;
  readonly #client: ClientImpl;
  readonly #id: string;
  readonly #onStderr: ((chunk: Uint8Array) => void) | undefined;
  #sock: net.Socket | null = null;
  /** init: nothing written. sent: the request is written. running: acked. done: `exit` is settled. */
  #state: "init" | "sent" | "running" | "done" = "init";
  #resolveStarted!: (started: boolean) => void;
  #resolveExit!: (exit: ProcessExit) => void;
  // stdout
  #queue: Uint8Array[] = [];
  #queued = 0;
  #paused = false;
  #discard = false;
  #abandoned = false;
  #waiter: (() => void) | null = null;
  // stdin
  readonly #stdinPipe: boolean;
  #stdinEnded = false;
  #stdinChain: Promise<void>;
  // cancel
  #cancelRequested = false;
  #cancelPromise: Promise<void> | null = null;

  constructor(client: ClientImpl, params: ProcessSpawnParams, stdinOpen: boolean, input: string | Uint8Array | undefined, onStderr: ((chunk: Uint8Array) => void) | undefined) {
    this.#client = client;
    this.#id = params.process_id;
    this.#onStderr = onStderr;
    this.started = new Promise((resolve) => (this.#resolveStarted = resolve));
    this.exit = new Promise((resolve) => (this.#resolveExit = resolve));
    const iterator = this.#iterator();
    this.stdout = { [Symbol.asyncIterator]: () => iterator };
    this.#stdinPipe = params.stdin === "pipe";
    // Nothing goes to stdin before the answer: with a failed start there is nobody to read it.
    this.#stdinChain = this.started.then(() => {});
    if (input !== undefined) {
      void this._write(input);
      this.end();
    } else if (!stdinOpen) {
      this.#stdinEnded = true;
    }
    if (client._incompatible !== null || client._closed) {
      // The version check failed, or the client is closed: the request is never written.
      this.#finish({ ...BASE_EXIT, unavailable: true });
    } else {
      this.#connect(params);
    }
  }

  #connect(params: ProcessSpawnParams): void {
    const sock = net.connect(this.#client._socketPath);
    this.#sock = sock;
    const frames = new FrameDecoder();
    const records = new RecordDecoder();
    sock.on("connect", () => {
      if (this.#state !== "init") return;
      this.#state = "sent";
      sock.write(encodeFrame({ jsonrpc: "2.0", id: 1, method: "process.spawn", params }));
    });
    sock.on("data", (chunk: Buffer) => {
      try {
        this.#onData(sock, chunk, frames, records);
      } catch {
        // A broken frame or record: the connection is closed, which stops the tree.
        sock.destroy();
      }
    });
    sock.on("error", () => {});
    sock.on("close", () => {
      if (this.#state === "init") this.#finish({ ...BASE_EXIT, unavailable: true });
      else this.#finish({ ...BASE_EXIT, lost: true, treeClean: false, cancelled: this.#cancelRequested });
    });
  }

  #onData(sock: net.Socket, chunk: Buffer, frames: FrameDecoder, records: RecordDecoder): void {
    if (this.#state === "sent") {
      frames.push(chunk);
      const msg = frames.next() as { error?: unknown } | undefined;
      if (msg === undefined) return;
      chunk = frames.rest();
      if (isRpcError(msg.error)) {
        this.#finish(exitFromError(msg.error));
        sock.destroy();
        return;
      }
      this.#state = "running";
      this.#resolveStarted(true);
    }
    if (this.#state !== "running") return;
    records.push(chunk);
    for (let r = records.next(); r !== undefined; r = records.next()) {
      if (r.kind === RECORD_STDOUT) {
        this.#pushStdout(sock, r.payload);
      } else if (r.kind === RECORD_STDERR) {
        try {
          this.#onStderr?.(r.payload);
        } catch {
          // A throwing callback is the caller's problem, not the stream's.
        }
      } else if (r.kind === RECORD_EXIT) {
        this.#finish(exitFromRecord(JSON.parse(r.payload.toString("utf8")) as WireExit));
        sock.destroy();
        return;
      } else {
        throw new FramingError("a record that only a client sends");
      }
    }
  }

  #pushStdout(sock: net.Socket, payload: Uint8Array): void {
    if (this.#discard || payload.byteLength === 0) return;
    this.#queue.push(payload);
    this.#queued += payload.byteLength;
    this.#wake();
    if (!this.#paused && this.#queued >= STDOUT_HIGH_WATER) {
      // Backpressure: the server stops relaying, the guest stops reading the pipe, the program blocks.
      this.#paused = true;
      sock.pause();
    }
  }

  #wake(): void {
    const w = this.#waiter;
    this.#waiter = null;
    w?.();
  }

  #resume(): void {
    if (!this.#paused) return;
    this.#paused = false;
    this.#sock?.resume();
  }

  /** Reads on and throws away, so `EXIT` is reached whoever stopped reading. */
  #startDiscarding(): void {
    this.#discard = true;
    this.#resume();
  }

  #iterator(): AsyncIterator<Uint8Array, undefined> {
    return {
      next: async () => {
        for (;;) {
          const chunk = this.#abandoned ? undefined : this.#queue.shift();
          if (chunk !== undefined) {
            this.#queued -= chunk.byteLength;
            if (this.#queued < STDOUT_HIGH_WATER) this.#resume();
            return { value: chunk, done: false };
          }
          if (this.#abandoned || this.#state === "done") return { value: undefined, done: true };
          await new Promise<void>((resolve) => (this.#waiter = resolve));
        }
      },
      return: async () => {
        this.#abandoned = true;
        this.#queue = [];
        this.#queued = 0;
        this.#startDiscarding();
        this.#wake();
        return { value: undefined, done: true };
      },
    };
  }

  #finish(exit: ProcessExit): void {
    if (this.#state === "done") return;
    this.#state = "done";
    this.#resolveStarted(false); // No effect after the ack.
    this.#resolveExit(exit);
    this.#wake();
  }

  /** Like `write`, and resolves when the bytes were handed to the socket with room to spare. */
  _write(data: string | Uint8Array): Promise<void> {
    if (this.#stdinEnded || !this.#stdinPipe || this.#state === "done") return Promise.resolve();
    let bytes: Uint8Array;
    if (typeof data === "string") bytes = Buffer.from(data, "utf8");
    else if (data instanceof Uint8Array) bytes = data;
    else return Promise.resolve();
    this.#stdinChain = this.#stdinChain.then(() => this.#sendStdin(bytes));
    return this.#stdinChain;
  }

  async #sendStdin(bytes: Uint8Array): Promise<void> {
    for (let off = 0; off < bytes.byteLength; off += MAX_RECORD_DATA_LEN) {
      const sock = this.#sock;
      if (this.#state !== "running" || !sock || sock.destroyed) return;
      if (!sock.write(encodeRecord(RECORD_STDIN, bytes.subarray(off, off + MAX_RECORD_DATA_LEN)))) await drained(sock);
    }
  }

  write(data: string | Uint8Array): void {
    void this._write(data);
  }

  end(): void {
    if (this.#stdinEnded || !this.#stdinPipe) return;
    this.#stdinEnded = true;
    this.#stdinChain = this.#stdinChain.then(() => {
      const sock = this.#sock;
      if (this.#state === "running" && sock && !sock.destroyed) sock.write(encodeRecord(RECORD_STDIN_EOF));
    });
  }

  #isDone(): boolean {
    return this.#state === "done";
  }

  cancelled(): boolean {
    return this.#cancelRequested;
  }

  cancel(graceMs?: number): Promise<void> {
    if (!this.#cancelPromise) {
      this.#cancelRequested = true;
      const grace = typeof graceMs === "number" && Number.isFinite(graceMs) && graceMs >= 0 ? Math.floor(graceMs) : DEFAULT_CANCEL_GRACE_MS;
      this.#cancelPromise = this.#cancel(grace).catch(() => {});
    }
    return this.#cancelPromise;
  }

  async #cancel(graceMs: number): Promise<void> {
    if (this.#state === "done") return;
    if (this.#state === "init") {
      // Nothing was written, and now nothing will be.
      this.#finish({ ...BASE_EXIT, cancelled: true });
      this.#sock?.destroy();
      return;
    }
    this.#startDiscarding();
    const ask = async (): Promise<boolean | null> => {
      try {
        return (await this.#client._request("process.cancel", { process_id: this.#id, grace_ms: graceMs })).was_running;
      } catch {
        return null;
      }
    };
    const beforeAnswer = this.#state === "sent";
    let wasRunning = await ask();
    // The cancel may have passed the spawn on its way: if the spawn then succeeds, ask once more.
    if (wasRunning === false && beforeAnswer && (await this.started) && !this.#isDone()) wasRunning = await ask();
    if (this.#isDone()) return;
    if (wasRunning === null) {
      // No control connection, or the server refused: a closed process connection stops the tree too.
      this.#sock?.destroy();
    } else {
      const timer = setTimeout(() => this.#sock?.destroy(), EXIT_AFTER_CANCEL_MS);
      timer.unref();
      void this.exit.then(() => clearTimeout(timer));
    }
    await this.exit;
  }
}
