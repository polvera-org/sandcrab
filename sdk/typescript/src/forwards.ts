import net from "node:net";
import type { Duplex } from "node:stream";

import { CODE_SANDBOX_NOT_FOUND, InvalidSpecError, isRpcError, serverError, ServerError } from "./errors.ts";
import { encodeFrame, FrameDecoder } from "./framing.ts";
import type { ClientImpl } from "./client.ts";

/** The target stays in the client. The server only learns `guestPort`. */
export interface ForwardSpec {
  /** The loopback port inside the sandbox. */
  guestPort: number;
  maxConnections?: number;
  target:
    | { port: number; host?: "127.0.0.1" | "::1" } // a loopback TCP port of the client
    | { socketPath: string } // a Unix socket of the client
    | { handler(socket: Duplex): void }; // in-process code; no listener needed
}

type Target = ForwardSpec["target"];

/** A target that does not answer within this is rejected; the server gives up at 10 s too. */
const DIAL_TIMEOUT_MS = 10_000;
const MAX_FORWARDS = 16;

function isPort(n: unknown): n is number {
  return typeof n === "number" && Number.isInteger(n) && n >= 1 && n <= 65535;
}

/** Throws `InvalidSpecError` for a malformed forward list. */
export function validateForwards(forwards: readonly ForwardSpec[]): void {
  if (!Array.isArray(forwards)) throw new InvalidSpecError("`forwards` must be an array");
  if (forwards.length > MAX_FORWARDS) throw new InvalidSpecError(`at most ${MAX_FORWARDS} forwards per sandbox`);
  const seen = new Set<number>();
  for (const f of forwards) {
    if (f === null || typeof f !== "object") throw new InvalidSpecError("a forward must be an object");
    if (!isPort(f.guestPort)) throw new InvalidSpecError("`guestPort` must be a port number (1 to 65535)");
    if (seen.has(f.guestPort)) throw new InvalidSpecError(`guest port ${f.guestPort} is forwarded twice`);
    seen.add(f.guestPort);
    if (f.maxConnections !== undefined && !(Number.isInteger(f.maxConnections) && f.maxConnections >= 1)) {
      throw new InvalidSpecError("`maxConnections` must be a positive integer");
    }
    const t = f.target as { port?: unknown; host?: unknown; socketPath?: unknown; handler?: unknown } | null;
    if (t === null || typeof t !== "object") throw new InvalidSpecError(`forward ${f.guestPort} needs a target`);
    const kinds = ["port", "socketPath", "handler"].filter((k) => k in t);
    if (kinds.length !== 1) throw new InvalidSpecError(`the target of forward ${f.guestPort} must have exactly one of port, socketPath, handler`);
    if ("port" in t) {
      if (!isPort(t.port)) throw new InvalidSpecError(`the target port of forward ${f.guestPort} must be a port number`);
      if (t.host !== undefined && t.host !== "127.0.0.1" && t.host !== "::1") {
        throw new InvalidSpecError(`the target host of forward ${f.guestPort} must be 127.0.0.1 or ::1`);
      }
    } else if ("socketPath" in t) {
      if (typeof t.socketPath !== "string" || t.socketPath === "") throw new InvalidSpecError(`the target socketPath of forward ${f.guestPort} must be a path`);
    } else if (typeof t.handler !== "function") {
      throw new InvalidSpecError(`the target handler of forward ${f.guestPort} must be a function`);
    }
  }
}

function dial(target: Exclude<Target, { handler: unknown }>): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const sock = "port" in target ? net.connect({ port: target.port, host: target.host ?? "127.0.0.1", allowHalfOpen: true }) : net.connect({ path: target.socketPath, allowHalfOpen: true });
    const timer = setTimeout(() => sock.destroy(new Error("the forward target did not answer in time")), DIAL_TIMEOUT_MS);
    sock.once("connect", () => {
      clearTimeout(timer);
      resolve(sock);
    });
    // Stays on for the socket's life, so a late error never goes unhandled.
    sock.on("error", (e) => {
      clearTimeout(timer);
      sock.destroy();
      reject(e);
    });
  });
}

/**
 * Opens the connection that carries one forwarded connection and resolves after the ack. The
 * ack is read with `read()`, so the socket is handed over as a fresh, unread stream.
 */
function attach(socketPath: string, connId: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const sock = net.connect({ path: socketPath, allowHalfOpen: true });
    const decoder = new FrameDecoder();
    const fail = (e: Error): void => {
      sock.destroy();
      reject(e);
    };
    const onClose = (): void => fail(new Error("the server closed the forward connection"));
    const onReadable = (): void => {
      try {
        let msg: { error?: unknown } | undefined;
        for (let chunk = sock.read() as Buffer | null; chunk !== null; chunk = sock.read() as Buffer | null) {
          decoder.push(chunk);
          msg = decoder.next() as { error?: unknown } | undefined;
          if (msg !== undefined) break;
        }
        if (msg === undefined) return;
        sock.off("readable", onReadable);
        sock.off("close", onClose);
        if (isRpcError(msg.error)) return fail(serverError(msg.error));
        // Bytes that came with the ack belong to the forwarded stream.
        const rest = decoder.rest();
        if (rest.length > 0) sock.unshift(rest);
        resolve(sock);
      } catch (e) {
        fail(e as Error);
      }
    };
    sock.once("connect", () => sock.write(encodeFrame({ jsonrpc: "2.0", id: 1, method: "forward.attach", params: { conn_id: connId } })));
    sock.on("readable", onReadable);
    sock.on("error", fail);
    sock.on("close", onClose);
  });
}

/** The forward targets of this client's sandboxes, and the connections it relays. */
export class ForwardRegistry {
  readonly #client: ClientImpl;
  readonly #targets = new Map<string, Map<number, Target>>();
  readonly #open = new Set<net.Socket>();

  constructor(client: ClientImpl) {
    this.#client = client;
  }

  set(sandboxId: string, forwards: readonly ForwardSpec[]): void {
    this.#targets.set(sandboxId, new Map(forwards.map((f) => [f.guestPort, f.target])));
  }

  drop(sandboxId: string): void {
    this.#targets.delete(sandboxId);
  }

  sandboxIds(): string[] {
    return [...this.#targets.keys()];
  }

  /** After a reconnect: this connection serves the forwards again. */
  serveAll(): void {
    for (const id of this.sandboxIds()) {
      this.#client._request("forward.serve", { sandbox_id: id }).catch((e: unknown) => {
        if (e instanceof ServerError && e.rpcCode === CODE_SANDBOX_NOT_FOUND) this.drop(id);
      });
    }
  }

  /** A `forward.incoming` notification. Never throws. */
  incoming(params: Record<string, unknown>): void {
    const { conn_id: connId, sandbox_id: sandboxId, guest_port: guestPort } = params;
    if (typeof connId !== "string") return;
    const target = typeof sandboxId === "string" && typeof guestPort === "number" ? this.#targets.get(sandboxId)?.get(guestPort) : undefined;
    if (!target) return this.#reject(connId, "no target for this forward");
    void this.#relay(connId, target);
  }

  async #relay(connId: string, target: Target): Promise<void> {
    let local: net.Socket | null = null;
    if (!("handler" in target)) {
      try {
        local = await dial(target);
      } catch (e) {
        return this.#reject(connId, `the forward target could not be reached: ${(e as Error).message}`);
      }
    }
    let remote: net.Socket;
    try {
      remote = await attach(this.#client._socketPath, connId);
    } catch {
      local?.destroy();
      return;
    }
    if (this.#client._closed || local?.destroyed) {
      local?.destroy();
      remote.destroy();
      return;
    }
    this.#track(remote);
    if (local === null) {
      try {
        (target as { handler(socket: Duplex): void }).handler(remote);
      } catch {
        remote.destroy();
      }
      return;
    }
    this.#track(local);
    const a = local;
    const kill = (): void => {
      a.destroy();
      remote.destroy();
    };
    a.on("error", kill);
    remote.on("error", kill);
    // `pipe` ends the other side at end-of-file: a half-close in each direction.
    a.pipe(remote);
    remote.pipe(a);
  }

  #track(sock: net.Socket): void {
    this.#open.add(sock);
    sock.once("close", () => this.#open.delete(sock));
  }

  #reject(connId: string, reason: string): void {
    this.#client._request("forward.reject", { conn_id: connId, reason }).catch(() => {});
  }

  closeAll(): void {
    for (const sock of this.#open) sock.destroy();
    this.#open.clear();
    this.#targets.clear();
  }
}
