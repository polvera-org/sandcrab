// tsc rewrites `./x.ts` imports to `./x.js` in the emitted JavaScript but leaves them in the
// declarations, where a consumer has no `.ts` file to find. This does the same there.

import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const dist = fileURLToPath(new URL("../dist/", import.meta.url));
for (const name of readdirSync(dist, { recursive: true })) {
  if (!String(name).endsWith(".d.ts")) continue;
  const file = dist + name;
  const text = readFileSync(file, "utf8");
  const fixed = text.replace(/(\bfrom\s+"|\bimport\("|\bimport\s+")(\.{1,2}\/[^"]+)\.ts"/g, '$1$2.js"');
  if (fixed !== text) writeFileSync(file, fixed);
}
