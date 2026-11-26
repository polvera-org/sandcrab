// A client the lifetime e2e kills with SIGKILL mid-run: it creates a labelled sandbox, starts
// a long program in it, prints `{"id": ...}` and waits for ever.
// Usage: node doomed-client.ts <socket path> <run label> <template>

import { connect } from "../../../src/index.ts";

const [socketPath, run, template] = process.argv.slice(2);
if (!socketPath || !run || !template) throw new Error("usage: doomed-client.ts <socket> <run> <template>");
const client = await connect({ socketPath, clientName: "doomed" });
const sb = await client.createSandbox({ template, labels: { e2e: "lifetime", run } });
const p = sb.spawn({ argv: ["/bin/sleep", "600"], env: { PATH: "/usr/bin:/bin" } });
if (!(await p.started)) throw new Error("the program did not start");
process.stdout.write(`${JSON.stringify({ id: sb.id })}\n`);
setInterval(() => {}, 60_000);
