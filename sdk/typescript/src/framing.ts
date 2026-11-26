// The two wire formats of the server socket.
//
// Frames: u32 big-endian length, then one JSON-RPC 2.0 object (at most 16 MiB).
// Records, on a process connection after the answer: u8 kind, u32 big-endian length, payload.

export const FRAME_HEADER_LEN = 4;
export const MAX_FRAME_LEN = 16 * 1024 * 1024;

export const RECORD_HEADER_LEN = 5;
export const MAX_RECORD_DATA_LEN = 64 * 1024;
export const MAX_EXIT_RECORD_LEN = 256 * 1024;

export const RECORD_STDIN = 0x01;
export const RECORD_STDIN_EOF = 0x02;
export const RECORD_STDOUT = 0x11;
export const RECORD_STDERR = 0x12;
export const RECORD_EXIT = 0x1f;

/** A frame or record that breaks the format. The connection that sent it is closed. */
export class FramingError extends Error {
  override name = "FramingError";
}

export function encodeFrame(message: unknown): Buffer {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  if (body.length > MAX_FRAME_LEN) throw new FramingError(`frame of ${body.length} bytes is over the limit`);
  const out = Buffer.allocUnsafe(FRAME_HEADER_LEN + body.length);
  out.writeUInt32BE(body.length, 0);
  body.copy(out, FRAME_HEADER_LEN);
  return out;
}

/** Incremental frame decoder. `rest()` hands back the bytes after the last frame taken. */
export class FrameDecoder {
  #buf: Buffer = Buffer.alloc(0);

  push(chunk: Uint8Array): void {
    this.#buf = this.#buf.length === 0 ? Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength) : Buffer.concat([this.#buf, chunk]);
  }

  /** The next complete frame, parsed, or `undefined` when more bytes are needed. */
  next(): unknown {
    if (this.#buf.length < FRAME_HEADER_LEN) return undefined;
    const len = this.#buf.readUInt32BE(0);
    if (len > MAX_FRAME_LEN) throw new FramingError(`frame of ${len} bytes is over the limit`);
    const end = FRAME_HEADER_LEN + len;
    if (this.#buf.length < end) return undefined;
    const body = this.#buf.subarray(FRAME_HEADER_LEN, end);
    this.#buf = this.#buf.subarray(end);
    let value: unknown;
    try {
      value = JSON.parse(body.toString("utf8"));
    } catch {
      throw new FramingError("frame is not JSON");
    }
    if (value === null || typeof value !== "object") throw new FramingError("frame is not a JSON object");
    return value;
  }

  rest(): Buffer {
    const rest = this.#buf;
    this.#buf = Buffer.alloc(0);
    return rest;
  }
}

export function encodeRecord(kind: number, payload?: Uint8Array): Buffer {
  const len = payload?.byteLength ?? 0;
  const out = Buffer.allocUnsafe(RECORD_HEADER_LEN + len);
  out.writeUInt8(kind, 0);
  out.writeUInt32BE(len, 1);
  if (payload) out.set(payload, RECORD_HEADER_LEN);
  return out;
}

export interface WireRecord {
  kind: number;
  payload: Buffer;
}

function maxRecordLen(kind: number): number {
  switch (kind) {
    case RECORD_STDIN:
    case RECORD_STDOUT:
    case RECORD_STDERR:
      return MAX_RECORD_DATA_LEN;
    case RECORD_STDIN_EOF:
      return 0;
    case RECORD_EXIT:
      return MAX_EXIT_RECORD_LEN;
    default:
      throw new FramingError(`unknown record kind 0x${kind.toString(16).padStart(2, "0")}`);
  }
}

/** Incremental decoder for the records of a process connection. */
export class RecordDecoder {
  #buf: Buffer = Buffer.alloc(0);

  push(chunk: Uint8Array): void {
    if (chunk.byteLength === 0) return;
    this.#buf = this.#buf.length === 0 ? Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength) : Buffer.concat([this.#buf, chunk]);
  }

  /** The next complete record, or `undefined`. The payload is a view, not a copy. */
  next(): WireRecord | undefined {
    if (this.#buf.length < RECORD_HEADER_LEN) return undefined;
    const kind = this.#buf.readUInt8(0);
    const len = this.#buf.readUInt32BE(1);
    if (len > maxRecordLen(kind)) throw new FramingError(`record 0x${kind.toString(16)} of ${len} bytes is over the limit`);
    const end = RECORD_HEADER_LEN + len;
    if (this.#buf.length < end) return undefined;
    const payload = this.#buf.subarray(RECORD_HEADER_LEN, end);
    this.#buf = this.#buf.subarray(end);
    return { kind, payload };
  }
}
