import { connectClient } from "./client.ts";
import type { ConnectOptions, SandcrabClient } from "./client.ts";

/**
 * Connects to a `sandcrab-server` socket and says hello. Rejects when the socket cannot be
 * reached or the server refuses this peer. A server whose API version does not fit still
 * resolves: nothing can be started on it, and `capabilitiesSnapshot()` says why.
 */
export function connect(o: ConnectOptions): Promise<SandcrabClient> {
  return connectClient(o);
}

export { MIN_SERVER_API_VERSION } from "./client.ts";
export type { ConnectOptions, SandcrabClient } from "./client.ts";
export { AbortedError, ConnectionError, errorName, InvalidSpecError, SandcrabError, ServerError } from "./errors.ts";
export type { ExecOptions, ExecOutput } from "./exec.ts";
export type { ForwardSpec } from "./forwards.ts";
export { API_VERSION } from "./generated/api.ts";
export type { Capabilities, ErrorData, SandboxInfo, SelftestReport } from "./generated/api.ts";
export type * as wire from "./generated/api.ts";
export type { ProcessBase, ProcessExit, ProcessHandle, SpawnOptions } from "./process.ts";
export type { Labels, Sandbox, SandboxSpec } from "./sandbox.ts";
export type { CapabilitiesSnapshot } from "./selftest.ts";
