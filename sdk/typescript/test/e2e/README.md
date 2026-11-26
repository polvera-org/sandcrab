End-to-end tests against a real `sandcrab server` on Docker (`*.test.ts`, run with `pnpm test:e2e`).
Unit tests against the in-process fake server live in `../unit`.

Each test file starts its own server as a child process (`--lifeline-stdin`, a private socket and
state directory under the system temp dir), drives it only through the public SDK, and stops it at
the end. The files run one at a time, so the printed timings are not skewed by each other.

Needs Docker, the pinned `ts-dev` template (`templates/build.sh ts-dev`) and:

| Variable | What |
| --- | --- |
| `SANDCRAB_BIN` | The `sandcrab` binary. Unset: every test is skipped |
| `SANDCRAB_CRABD` | The static `crabd` to inject (`scripts/build-crabd.sh`), passed to the server |
| `SANDCRAB_TEMPLATE_PINS` | The host's template pins; default `~/.sandcrab/template-pins` |
| `SANDCRAB_E2E_DATA` | Where the mount smoke test makes its folder; default the current directory |
| `SANDCRAB_E2E_KEEP` | Keep each server's directory (state, `server.log`) after the run |

```sh
cargo build -p sandcrab-cli --bin sandcrab && scripts/build-crabd.sh
cd sdk/typescript
SANDCRAB_BIN=$PWD/../../target/debug/sandcrab \
SANDCRAB_CRABD=$PWD/../../target/x86_64-unknown-linux-musl/release/crabd \
pnpm test:e2e
```
