import { InvalidSpecError, NOT_CONNECTED } from "./errors.ts";
import type { ClientImpl } from "./client.ts";
import type { Capabilities, SelftestReport } from "./generated/api.ts";

export interface CapabilitiesSnapshot {
  backend: string | null;
  /** `ok` only when the last self-test of this template passed every required line. */
  isolation: { ok: boolean; reason: string | null };
  /** Age of the self-test behind `isolation`. `null`: none is known. */
  ageMs: number | null;
}

interface Entry {
  backend: string;
  ok: boolean;
  reason: string | null;
  /** When the self-test ran, on this clock. `null`: never. */
  testedAt: number | null;
  fetchedAt: number;
}

/** A snapshot older than this is refreshed in the background when it is read. */
const REFRESH_AFTER_MS = 30_000;

function checkTemplate(template: unknown): asserts template is string {
  if (typeof template !== "string" || template === "") throw new InvalidSpecError("`template` must be a non-empty string");
}

/** `server.capabilities` and `server.selftest`, and the last answer per template. */
export class CapabilityCache {
  readonly #client: ClientImpl;
  readonly #entries = new Map<string, Entry>();
  readonly #inflight = new Set<string>();

  constructor(client: ClientImpl) {
    this.#client = client;
  }

  async capabilities(template?: string): Promise<Capabilities> {
    if (template !== undefined) checkTemplate(template);
    const caps = await this.#client._request("server.capabilities", template === undefined ? {} : { template }, { idempotent: true });
    if (template !== undefined) {
      const now = Date.now();
      const s = caps.selftest ?? null;
      this.#entries.set(template, {
        backend: caps.backend,
        ok: s?.ok === true,
        reason: s ? (s.reason ?? null) : `no self-test has run for template "${template}"`,
        testedAt: s ? now - s.age_secs * 1000 : null,
        fetchedAt: now,
      });
    }
    return caps;
  }

  async selftest(template: string, o: { maxAgeMs?: number } = {}): Promise<SelftestReport> {
    checkTemplate(template);
    const params: { template: string; max_age_secs?: number } = { template };
    if (o.maxAgeMs !== undefined) {
      if (!(Number.isFinite(o.maxAgeMs) && o.maxAgeMs >= 0)) throw new InvalidSpecError("`maxAgeMs` must be a non-negative number");
      params.max_age_secs = Math.floor(o.maxAgeMs / 1000);
    }
    const report = await this.#client._request("server.selftest", params);
    const started = Date.parse(report.started_at);
    const now = Date.now();
    this.#entries.set(template, {
      backend: report.backend,
      ok: report.ok === true,
      reason: report.reason ?? null,
      testedAt: Number.isNaN(started) ? now : Math.min(now, started),
      fetchedAt: now,
    });
    return report;
  }

  snapshot(template: string): CapabilitiesSnapshot {
    const client = this.#client;
    const down = client._incompatible ?? (client.connected ? null : NOT_CONNECTED);
    if (down !== null) return { backend: client._backend, isolation: { ok: false, reason: down }, ageMs: null };
    const now = Date.now();
    const e = typeof template === "string" ? this.#entries.get(template) : undefined;
    if (typeof template === "string" && template !== "" && (!e || now - e.fetchedAt > REFRESH_AFTER_MS)) this.#refresh(template);
    if (!e) {
      return { backend: client._backend, isolation: { ok: false, reason: `the capabilities for template "${template}" are not known yet` }, ageMs: null };
    }
    return { backend: e.backend, isolation: { ok: e.ok, reason: e.reason }, ageMs: e.testedAt === null ? null : Math.max(0, now - e.testedAt) };
  }

  /** After a reconnect: the server may be another one, so what was known is dropped and asked again. */
  refreshAll(): void {
    const templates = [...this.#entries.keys()];
    this.#entries.clear();
    for (const template of templates) this.#refresh(template);
  }

  #refresh(template: string): void {
    if (this.#inflight.has(template)) return;
    this.#inflight.add(template);
    this.capabilities(template)
      .catch(() => {})
      .finally(() => this.#inflight.delete(template));
  }
}
