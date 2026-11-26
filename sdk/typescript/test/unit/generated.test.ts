import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// @ts-ignore: a plain script without declarations.
import { generate, OUTPUT_PATH, SCHEMA_PATH } from "../../scripts/generate-types.mjs";

test("src/generated/api.ts is what the schema generates", () => {
  const schema = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));
  assert.equal(readFileSync(OUTPUT_PATH, "utf8"), generate(schema), "stale: run `pnpm generate`");
});

test("the copied schema equals the Rust snapshot", (t) => {
  const snapshot = fileURLToPath(new URL("../../../../crates/sandcrab-protocol/tests/snapshots/server-api.schema.json", import.meta.url));
  if (!existsSync(snapshot)) return t.skip("not in the repository");
  assert.equal(readFileSync(SCHEMA_PATH, "utf8"), readFileSync(snapshot, "utf8"), "stale: run `pnpm sync-schema && pnpm generate`");
});
