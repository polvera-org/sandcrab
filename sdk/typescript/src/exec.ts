import { Readable } from "node:stream";
import type { Writable } from "node:stream";
import { finished } from "node:stream/promises";

import { InvalidSpecError } from "./errors.ts";
import type { ClientImpl } from "./client.ts";
import type { OutputCap } from "./generated/api.ts";
import { checkInput, processParams, spawnProcess } from "./process.ts";
import type { ProcessBase, ProcessExit, SpawnInternals } from "./process.ts";

export interface ExecOptions extends ProcessBase {
  /** Wall-clock bound, timed in the guest. */
  timeoutMs: number;
  input?: string | Uint8Array | Readable;
  /** stdout goes here and is not captured. `exec` ends the stream and resolves after its `finish`. */
  output?: Writable;
  /** Default 8 MiB. */
  maxOutputBytes?: number;
  /** Over `maxOutputBytes`: "kill" (default) stops the program, "tail" keeps the last bytes. */
  overflow?: "kill" | "tail";
  /** Every chunk as it arrives. With it, stderr is streamed too (unless merged). */
  onOutput?(chunk: Uint8Array, stream: "stdout" | "stderr"): void;
  signal?: AbortSignal;
}

export interface ExecOutput extends Omit<ProcessExit, "stderrTail" | "cancelled"> {
  /** Empty with `output`. */
  stdout: Uint8Array;
  /** The stderr tail. Empty with `stderr: "merge"`. */
  stderr: string;
  aborted: boolean;
}

export const DEFAULT_MAX_OUTPUT_BYTES = 8 * 1024 * 1024;
/** The guest keeps a tail of at most this; a larger one is computed here from the stream. */
const MAX_GUEST_TAIL_BYTES = 8 * 1024 * 1024;
const DEFAULT_KILL_GRACE_MS = 500;

/** Keeps the last `max` bytes of what is pushed. */
class Tail {
  readonly #max: number;
  #chunks: Uint8Array[] = [];
  #size = 0;
  dropped = false;

  constructor(max: number) {
    this.#max = max;
  }

  push(chunk: Uint8Array): void {
    this.#chunks.push(chunk);
    this.#size += chunk.byteLength;
    while (this.#size > this.#max) {
      this.dropped = true;
      const head = this.#chunks[0] as Uint8Array;
      const over = this.#size - this.#max;
      if (head.byteLength <= over) {
        this.#chunks.shift();
        this.#size -= head.byteLength;
      } else {
        this.#chunks[0] = head.subarray(over);
        this.#size -= over;
      }
    }
  }

  bytes(): Uint8Array {
    return Buffer.concat(this.#chunks, this.#size);
  }
}

function writeOut(output: Writable, chunk: Uint8Array): Promise<void> | void {
  if (output.write(chunk)) return;
  return new Promise((resolve) => {
    const done = (): void => {
      output.off("drain", done);
      output.off("close", done);
      output.off("error", done);
      resolve();
    };
    output.on("drain", done);
    output.on("close", done);
    output.on("error", done);
  });
}

/** `sandbox.exec`: a spawn plus collection. Rejects only with `InvalidSpecError`. */
export async function exec(client: ClientImpl, sandboxId: string, spec: ExecOptions): Promise<ExecOutput> {
  processParams(spec);
  const { input, output, onOutput, signal } = spec;
  if (typeof spec.timeoutMs !== "number" || !Number.isSafeInteger(spec.timeoutMs) || spec.timeoutMs < 1) throw new InvalidSpecError("`timeoutMs` is required: a positive integer");
  const max = spec.maxOutputBytes ?? DEFAULT_MAX_OUTPUT_BYTES;
  if (!Number.isSafeInteger(max) || max < 1) throw new InvalidSpecError("`maxOutputBytes` must be a positive integer");
  const overflow = spec.overflow ?? "kill";
  if (overflow !== "kill" && overflow !== "tail") throw new InvalidSpecError('`overflow` must be "kill" or "tail"');
  if (onOutput !== undefined && typeof onOutput !== "function") throw new InvalidSpecError("`onOutput` must be a function");
  if (output !== undefined && typeof output?.write !== "function") throw new InvalidSpecError("`output` must be a Writable");
  const streamIn = input instanceof Readable ? input : undefined;
  if (input !== undefined && !streamIn) checkInput(input);

  const nothingRan = { code: null, signal: null, spawnError: null, rejected: null, unavailable: false, lost: false, timedOut: false, overflow: false, treeClean: true, stdout: new Uint8Array(0), stderr: "" };
  if (signal?.aborted) return { ...nothingRan, aborted: true };

  // With a live consumer the guest is never asked for the tail: it would hold everything back
  // until the end. The SDK streams and keeps the tail itself.
  const live = output !== undefined || onOutput !== undefined;
  let outputCap: OutputCap | undefined;
  if (overflow === "kill") outputCap = { max_bytes: max, on_overflow: "kill" };
  else if (!live && max <= MAX_GUEST_TAIL_BYTES) outputCap = { max_bytes: max, on_overflow: "tail" };

  const internals: SpawnInternals = { outputCap, stdinPipe: streamIn !== undefined };
  if (onOutput && spec.stderr !== "merge") {
    internals.stderrStream = (chunk) => onOutput(chunk, "stderr");
  }
  const handle = spawnProcess(client, sandboxId, { ...spec, input: streamIn ? undefined : (input as string | Uint8Array | undefined), stdin: "end" }, internals);

  let aborted = false;
  const onAbort = (): void => {
    aborted = true;
    void handle.cancel(spec.killGraceMs ?? DEFAULT_KILL_GRACE_MS);
  };
  signal?.addEventListener("abort", onAbort, { once: true });

  // A broken input or output stream stops the program: its result would not be what was asked for.
  let streamFailure: { code: string; message: string } | null = null;
  const failStream = (code: string, e: unknown): void => {
    streamFailure ??= { code, message: e instanceof Error ? e.message : String(e) };
    void handle.cancel(spec.killGraceMs ?? DEFAULT_KILL_GRACE_MS);
  };

  let gone = false;
  const pumpInput = (async (): Promise<void> => {
    if (!streamIn) return;
    try {
      for await (const chunk of streamIn) {
        if (gone) break;
        await handle._write(typeof chunk === "string" ? chunk : (chunk as Uint8Array));
      }
    } catch (e) {
      if (!gone) failStream("INPUT_FAILED", e);
    }
    handle.end();
  })();

  const onOutputError = (e: Error): void => failStream("OUTPUT_FAILED", e);
  output?.on("error", onOutputError);

  const tail = new Tail(max);
  let total = 0;
  for await (const chunk of handle.stdout) {
    total += chunk.byteLength;
    if (onOutput) {
      try {
        onOutput(chunk, "stdout");
      } catch {
        // A throwing callback does not break the stream.
      }
    }
    if (!output) tail.push(chunk);
    else if (streamFailure === null && !output.destroyed) await writeOut(output, chunk);
  }
  const exit = await handle.exit;
  gone = true;
  signal?.removeEventListener("abort", onAbort);
  if (streamIn) {
    // Stop reading the caller's stream; it is not needed any more.
    if (!streamIn.readableEnded) streamIn.destroy();
    await pumpInput;
  }
  if (output) {
    if (!output.destroyed && !output.writableEnded) output.end();
    await finished(output).catch(() => {});
    output.off("error", onOutputError);
  }

  return {
    code: exit.code,
    signal: exit.signal,
    spawnError: exit.spawnError,
    rejected: exit.rejected ?? streamFailure,
    unavailable: exit.unavailable,
    lost: exit.lost,
    timedOut: aborted ? false : exit.timedOut,
    overflow: exit.overflow || tail.dropped || (overflow === "tail" && total > max),
    treeClean: exit.treeClean,
    stdout: tail.bytes(),
    stderr: exit.stderrTail,
    aborted,
  };
}
