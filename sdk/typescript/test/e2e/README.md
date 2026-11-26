End-to-end tests against a real `sandcrab server` (`*.test.ts`, run with `pnpm test:e2e`), on Docker
or libkrun (`SANDCRAB_E2E_BACKEND`).
Unit tests against the in-process fake server live in `../unit`.

Each test file starts its own server as a child process (`--lifeline-stdin`, a private socket and
state directory under the system temp dir), drives it only through the public SDK, and stops it at
the end. The files run one at a time, so the printed timings are not skewed by each other.

Needs the pinned `ts-dev` template (`templates/build.sh ts-dev`), Docker (also for libkrun: the rootfs
is built from the template image) and:

| Variable | What |
| --- | --- |
| `SANDCRAB_BIN` | The `sandcrab` binary. Unset: every test is skipped |
| `SANDCRAB_E2E_BACKEND` | `docker` (default) or `libkrun` |
| `SANDCRAB_E2E_HOME` | libkrun only: a prepared `SANDCRAB_HOME` with the libraries and the helper (`sandcrab libkrun install --helper ...`); every libkrun server uses it as its state directory, so keep the path short (its run directory holds Unix sockets). The rootfs cache fills there (building it ahead saves the first test the wait: below). Unset on libkrun: every test is skipped |
| `SANDCRAB_CRABD` | The static `crabd` to inject (`scripts/build-crabd.sh`), passed to the server |
| `SANDCRAB_TEMPLATE_PINS` | The host's template pins; default `~/.sandcrab/template-pins` |
| `SANDCRAB_E2E_DATA` | Where the mount and self-test cases make their folders; default the current directory |
| `SANDCRAB_E2E_KEEP` | Keep each server's directory (state, `server.log`) after the run |

```sh
cargo build -p sandcrab-cli --bin sandcrab && scripts/build-crabd.sh
cd sdk/typescript
SANDCRAB_BIN=$PWD/../../target/debug/sandcrab \
SANDCRAB_CRABD=$PWD/../../target/$(uname -m | sed 's/arm64/aarch64/')-unknown-linux-musl/release/crabd \
pnpm test:e2e
```

On libkrun (Linux KVM or Apple Silicon):

```sh
export SANDCRAB_HOME=$HOME/.sce2e SANDCRAB_E2E_HOME=$HOME/.sce2e SANDCRAB_E2E_BACKEND=libkrun
cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
sandcrab --backend=libkrun libkrun install --helper target/release/sandcrab-krun
# Fill the rootfs cache ahead. Linux: `sandcrab rootfs build ts-dev base`. macOS has no
# `rootfs` command (Firecracker only); one run per template builds it:
sandcrab --backend=libkrun run -t ts-dev -- true
cd sdk/typescript && SANDCRAB_BIN=... SANDCRAB_CRABD=... pnpm test:e2e
```

Cases that cannot apply to a backend are skipped with the reason in the output.
