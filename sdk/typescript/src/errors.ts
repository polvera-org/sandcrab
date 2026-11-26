import { ERROR_CODES } from "./generated/api.ts";

/** Codes `crabd` answers; they pass through the server for `process.spawn` and `process.cancel`. */
const GUEST_CODES: Record<number, string> = {
  [-32700]: "PARSE_ERROR",
  [-32600]: "INVALID_REQUEST",
  [-32601]: "METHOD_NOT_FOUND",
  [-32602]: "INVALID_PARAMS",
  [-32603]: "INTERNAL_ERROR",
  [-32001]: "EXECUTION_FAILED",
  [-32002]: "TIMED_OUT",
  [-32003]: "PATH_NOT_ALLOWED",
  [-32004]: "NOT_FOUND",
  [-32006]: "TOO_LARGE",
  [-32007]: "UNIMPLEMENTED",
  [-32010]: "SPAWN_FAILED",
  [-32011]: "LIMIT_EXCEEDED",
  [-32012]: "CANCELLED",
};

const NAMES = new Map<number, string>([
  ...Object.entries(GUEST_CODES).map(([code, name]): [number, string] => [Number(code), name]),
  ...Object.entries(ERROR_CODES).map(([name, code]): [number, string] => [code, name]),
]);

export const CODE_UNAVAILABLE = ERROR_CODES.UNAVAILABLE;
export const CODE_UNSUPPORTED_VERSION = ERROR_CODES.UNSUPPORTED_VERSION;
export const CODE_SANDBOX_NOT_FOUND = ERROR_CODES.SANDBOX_NOT_FOUND;
export const CODE_SPAWN_FAILED = -32010;
export const CODE_CANCELLED = -32012;

export const NOT_CONNECTED = "not connected to the sandcrab server";

/** The name of a JSON-RPC error code (`"SANDBOX_NOT_FOUND"`), or the number as a string. */
export function errorName(code: number): string {
  return NAMES.get(code) ?? String(code);
}

/** Base of every error this package throws or rejects with. */
export class SandcrabError extends Error {
  override name = "SandcrabError";
  /** `"SANDBOX_NOT_FOUND"`, `"MOUNT_NOT_ALLOWED"`, `"INVALID_SPEC"`, `"NOT_CONNECTED"`, `"UNSUPPORTED_FEATURE"` (the server lacks a feature the call needs), ... */
  readonly code: string;
  constructor(code: string, message: string, options?: ErrorOptions) {
    super(message, options);
    this.code = code;
  }
}

/** The server answered a request with an error. `data` is the error's data, as the schema types it. */
export class ServerError extends SandcrabError {
  override name = "ServerError";
  readonly rpcCode: number;
  readonly data: unknown;
  constructor(rpcCode: number, message: string, data?: unknown) {
    super(errorName(rpcCode), message);
    this.rpcCode = rpcCode;
    this.data = data;
  }
}

/** A malformed spec. Thrown before anything is sent or started. */
export class InvalidSpecError extends SandcrabError {
  override name = "InvalidSpecError";
  constructor(message: string) {
    super("INVALID_SPEC", message);
  }
}

/**
 * No usable connection to the server. `code` is `"NOT_CONNECTED"` (nothing was sent),
 * `"CONNECTION_LOST"` (the request was sent and its answer is unknown), `"CLOSED"` (the client
 * was closed) or `"UNSUPPORTED_VERSION"` (the server's API version does not fit this SDK).
 */
export class ConnectionError extends SandcrabError {
  override name = "ConnectionError";
}

/** The call's `AbortSignal` fired. */
export class AbortedError extends SandcrabError {
  override name = "AbortedError";
  constructor(message = "aborted") {
    super("ABORTED", message);
  }
}

export interface RpcErrorObject {
  code: number;
  message: string;
  data?: unknown;
}

export function isRpcError(v: unknown): v is RpcErrorObject {
  return typeof v === "object" && v !== null && typeof (v as RpcErrorObject).code === "number";
}

export function serverError(e: RpcErrorObject): ServerError {
  return new ServerError(e.code, typeof e.message === "string" ? e.message : errorName(e.code), e.data);
}
