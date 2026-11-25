# `sandcrab` CLI reference

The operator and developer CLI (crate [`sandcrab-cli`](../crates/sandcrab-cli/)). Build it
with `cargo build --release -p sandcrab-cli`; the binary is `target/release/sandcrab`.
Everything below matches `sandcrab --help` for version 0.1.0.

```text
sandcrab [GLOBAL OPTIONS] <COMMAND>

  doctor     Check the host (crabd, Docker, template pins, Firecracker and libkrun prerequisites, state dir)
  workspace  Manage persistent workspaces
  template   List, build or pin templates
  rootfs     Firecracker rootfs images (ext4) built from templates (Linux builds)
  libkrun    libkrun backend: install the pinned libraries, netstack and signed helper
             (macOS Apple Silicon and Linux builds)
  run        Spawn a sandbox, run a command, print its output, destroy the sandbox
  tool       Spawn a sandbox, make one tool call, print the JSON result, destroy the sandbox
  tools      Tool catalog
```

## Global options

Accepted by every command.

| Option | Default | |
| --- | --- | --- |
| `--state-dir <DIR>` | `$SANDCRAB_HOME`, else `~/.sandcrab` | State root: workspace registry, sandbox sockets, caches |
| `--templates-dir <DIR>` | `./templates`, then `<state root>/templates` | Repeatable; first match wins |
| `--backend <docker\|firecracker\|libkrun>` | none (`$SANDCRAB_BACKEND`) | Runtime backend. **Explicit, no fallback.** `firecracker` (alias `fc`) exists only in Linux builds; `libkrun` (alias `krun`) in macOS Apple Silicon and Linux builds. A backend not compiled in is an error naming the ones that are. `docker-dev`, the pre-rename name, is still accepted as an alias of `docker` |
| `--allow-unpinned` | off | `docker` only: run template images that have no host-local pin (refused by default; see [template-pins.md](template-pins.md)) |
| `--crabd <PATH>` | `$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd` (target dir: `$CARGO_TARGET_DIR`, else `target/`) | Host path of the static musl `crabd` that gets injected |
| `--libkrun-tsi` | off | **Deprecated**, `libkrun` only: run `--network unrestricted` through libkrun's TSI socket proxy instead of the pinned userspace netstack. With TSI the guest can reach services on the host's loopback and LAN. An installed netstack wins: the flag is then ignored, with an error-level log on every spawn. It only applies where no netstack is installed. Never used implicitly |
| `--json` | off | Machine-readable JSON output |
| `-v`, `-vv` | off | Lifecycle timings to stderr (`-vv`: debug logs) |
| `-h`, `-V` | | Help, version |

**Deprecated:** `--allow-insecure` and `SANDCRAB_ALLOW_INSECURE` are no-ops kept for one
release. They are hidden from `--help`, still accepted, and print a deprecation warning.
The Docker backend is officially supported and needs no opt-in; it shares the host kernel
and reports `hardware_isolated = false`, `multi_tenant_safe = false`. A backend
that cannot start (for example no `/dev/kvm`) is an error. `sandcrab` never falls back to
another backend.

## `doctor`

```sh
sandcrab doctor [--fix [--all [-y]]]
```

Checks `crabd`, Docker (and the Docker transport in use, `unix` or `tcp`), every template
image against this host's pin (local integrity), the state directory, and, per backend
compiled into the build:

- **Firecracker** (Linux): `/dev/kvm` readable and writable (with a hint for VMs without
  nested virtualization), `kvm` group membership, pinned kernel and `firecracker` binary,
  `mke2fs` (e2fsprogs >= 1.47.1 for tar input), the rootfs cache per template, the `sctapN`
  tap pool, and orphaned run directories.
- **libkrun** (macOS Apple Silicon, Linux): the `lk:*` lines: config, hypervisor, signed
  helper and its install record, pinned libraries, `crabd`, `mke2fs`, case-insensitive
  workspace volumes, the cgroup parent (Linux), the netstack, and orphaned run dirs of dead
  processes, which it always reaps. What each line means and how to fix it:
  [hosting.md §4.2](hosting.md#42-sandcrab-doctor---backend-libkrun).

Exit code 1 if any check fails. Missing libkrun prerequisites fail only when libkrun is the
selected backend (`sandcrab doctor --backend libkrun`); otherwise they are warnings. A libkrun
install that is present but does not match its pins always fails.

| Flag | |
| --- | --- |
| `--fix` | Remove orphaned sandcrab containers (only this user's, for this state dir, whose sandcrab process is gone) and Firecracker run dirs of dead sandcrab processes; download and verify the pinned Firecracker kernel and binary; tighten state dir permissions |
| `--all` | With `--fix`: remove **every** sandcrab Docker container on the host, including live sandboxes of other processes and users. Asks for confirmation |
| `-y, --yes` | With `--all`: skip the confirmation |

## `workspace`

Persistent workspaces, stored in the registry under the state root. A workspace is either
**managed** (`create`: data under `<state root>/workspaces/<id>/data/`) or a **registered
host directory** (`register`: Sandcrab never deletes it). `<WORKSPACE>` accepts a name or
an id.

| Command | |
| --- | --- |
| `workspace create <NAME>` | Create a managed workspace |
| `workspace register <NAME> <PATH>` | Register an existing host directory. Directories inside or containing another workspace are refused |
| `workspace ls` | List workspaces |
| `workspace put <WORKSPACE> <REL_PATH> <SOURCE>` | Write a file into a workspace from a local file, or `-` for stdin |
| `workspace cat <WORKSPACE> <REL_PATH>` | Print a workspace file to stdout |
| `workspace files <WORKSPACE> [DIR]` | List a directory (non-recursive; default: the root) |
| `workspace rm <WORKSPACE> [--delete-data]` | Unregister. `--delete-data` also deletes a managed workspace's data; registered host dirs are never deleted |

Host-side operations refuse paths that escape the workspace (`..`, symlinks). `put` is
refused while another process holds a read-write lease on the workspace (a running
sandbox with it mounted `rw`). Reads are always allowed.

## `template`

| Command | |
| --- | --- |
| `template list` | List templates found in the template directories, with this host's pin (`host-local`, `legacy-manifest` or `no`) |
| `template build [NAMES]... [--all] [--no-pin]` | Build images with `templates/build.sh` and record their image IDs as host-local pins (unless `--no-pin`). `--all` builds every template, `base` first |
| `template pin [NAMES]... [--all]` | Adopt the images already present: record their current IDs as host-local pins without rebuilding (prints a warning) |

The same build is available as `templates/build.sh [--no-pin] [NAMES...]`. Pins live in
`~/.sandcrab/template-pins/` (or `$SANDCRAB_TEMPLATE_PINS`), never in `sandcrab.toml`. A
pin is the *local* image ID: it proves the image is unchanged since this host built it, not
where it came from, because image IDs do not reproduce across hosts yet. See
[template-pins.md](template-pins.md).

## `rootfs`

Firecracker only (Linux builds). No KVM needed to build.

| Command | |
| --- | --- |
| `rootfs build [NAMES]... [--all] [--verify]` | Build (or reuse) the read-only ext4 rootfs of templates: template image → export → inject `crabd` as `/sbin/init` → `mke2fs`. Needs Docker and the built template image. `--verify` re-hashes a cached image against its manifest |
| `rootfs list` | List cached rootfs images |

Images are cached under `<state root>/cache/rootfs/<key>/{rootfs.ext4,manifest.json}`,
keyed by image ID, arch, `crabd` sha256 and pipeline version. Cached images are read-only
(`0444`), and the manifest records a stat stamp next to the SHA-256, so a spawn verifies the
image with an `fstat` (a full re-hash only when the stamp changed). `--backend firecracker run`
and `--backend libkrun run` also build the rootfs on a cache miss; libkrun has no `rootfs`
subcommand, so on macOS the first spawn of a template builds it.

## `libkrun install`

macOS Apple Silicon and Linux builds. Setup guide: [hosting.md](hosting.md#22-libkrun-on-macos)
(macOS), [hosting.md](hosting.md#34-libkrun-on-linux) (Linux).

```sh
sandcrab libkrun install [--from-source] [--fw-from-source] [--force]
                         [--helper <PATH> | --no-helper] [--no-netstack] [--dry-run]
```

Installs, under the state root: the pinned libkrun + libkrunfw into
`artifacts/libkrun/<version>/`, the pinned userspace netstack (gvproxy on macOS, passt on
Linux) into `artifacts/libkrun-net/<kind>-<version>/`, and the `sandcrab-krun` helper into
`bin/`, signed on macOS and recorded in `bin/sandcrab-krun.pin.json` (SHA-256 + stat stamp).
Every directory is created `0700`.

| Flag | |
| --- | --- |
| `--from-source` | Build libkrun and libkrunfw from the pinned git tags (`scripts/fetch-libkrun.sh`) instead of copying the Homebrew bottles. The only option on a Mac without Homebrew. Linux always builds libkrun from source |
| `--fw-from-source` | Linux: also build libkrunfw (the guest kernel) from source instead of the pinned release asset |
| `--force` | Reinstall the libraries and the netstack even if a verified install is present |
| `--helper <PATH>` | Helper binary to install. Default: `sandcrab-krun` next to the `sandcrab` executable |
| `--no-helper` | Install the libraries (and netstack) only |
| `--no-netstack` | Skip the netstack. `--network unrestricted` is then refused on libkrun |
| `--dry-run` | Print the steps without changing anything |

On macOS the default copies the Homebrew bottles (`brew install libkrun libkrunfw` first) and
refuses if they are not the pinned versions. The helper is signed with the
`com.apple.security.hypervisor` entitlement: ad hoc by default, or with
`$SANDCRAB_SIGN_IDENTITY` (a Developer ID also gets the hardened runtime and
`disable-library-validation`; see [hosting.md §5](hosting.md#5-release-signing-macos)).
`SANDCRAB_SIGN_RUNTIME=1` adds the hardened runtime to an ad-hoc signature. Install always
re-signs the helper it copies. With `--json` it prints what it installed.

**Upgrading:** the helper protocol is versioned. A helper and a backend from different builds
refuse each other, so re-run `sandcrab libkrun install --helper <path>` after rebuilding.

## `run`

```sh
sandcrab --backend docker run [OPTIONS] -- <COMMAND>...
```

Spawn a sandbox, run a command with `bash`, print stdout / stderr, destroy the sandbox. One
trailing argument is run as a shell script (`-- 'ls | wc -l'`); several are quoted as an
argv (`-- bash -c 'exit 3'`). Exits with the command's exit code.

| Option | Default | |
| --- | --- | --- |
| `-t, --template <NAME>` | `base` | `base`, `python-dev`, `ts-dev`, `render`, ... |
| `-m, --mount <ws>:<guest_path>[:rw\|ro]` | `rw` | Mount a workspace. Repeatable |
| `--cwd <PATH>` | first rw mount, else the template workdir | Default working directory |
| `-e, --env KEY=VALUE` | | Repeatable |
| `--network disabled\|unrestricted` | template's (all four: `disabled`) | Network policy. On libkrun, `unrestricted` needs the netstack from `sandcrab libkrun install` (refused without it) |
| `-p, --port <PORT>` | | Publish a guest port on host `127.0.0.1:<random>`. Repeatable |
| `--ttl <SECS>` | 1800, none with `--keep` | Sandbox TTL (`0` = none) |
| `--keep` | off | After the command, keep the sandbox and its ports alive until Ctrl-C, then destroy it |

Examples:

```sh
sandcrab --backend docker run -t python-dev -- 'python3 --version'
sandcrab --backend docker run -t ts-dev -m site:/workspace -p 5173 --keep \
  --network unrestricted -- 'npm i && npx vite --host 0.0.0.0 --port 5173'
sandcrab -v --backend docker run -- true     # prints spawn→ready and teardown timings
```

## `tool`

```sh
sandcrab --backend docker tool [OPTIONS] <NAME> [PARAMS]
```

Spawn a sandbox, make one tool call, print the JSON result, destroy the sandbox. `PARAMS`
is a JSON object (`-` reads stdin; default `{}`). Same sandbox options as `run`, except
`--keep`. Exits 1 if the tool returned an error.

```sh
sandcrab --backend docker tool -m site:/workspace grep '{"pattern":"TODO","glob":"*.ts"}'
echo '{"path":"README.md","limit":20}' | sandcrab --backend docker tool -m site:/workspace file_read -
```

## `tools schema`

```sh
sandcrab tools schema
```

Prints the LLM tool definitions: name, description, input and output JSON Schema for all
16 tools. Feed it to any tool-calling LLM or MCP adapter. Reference:
[protocol.md](protocol.md#tools). No backend needed.

## Environment variables

| Variable | Used by | |
| --- | --- | --- |
| `SANDCRAB_HOME` | CLI, SDK, crabagent | State root (default `~/.sandcrab`) |
| `SANDCRAB_BACKEND` | CLI, crabagent | `docker`, `firecracker` (Linux) or `libkrun` (macOS Apple Silicon, Linux) |
| `SANDCRAB_ALLOW_INSECURE` | CLI, crabagent | **Deprecated no-op**: prints a warning, no other effect |
| `SANDCRAB_CRABD` | CLI, SDK, crabagent, bench | Path of the static `crabd` |
| `SANDCRAB_TEMPLATE_PINS` | CLI, SDK, `templates/build.sh` | Host-local template pin directory (default `<state root>/template-pins`; [template-pins.md](template-pins.md)) |
| `SANDCRAB_DOCKER_TRANSPORT` | Docker backend (CLI, SDK, crabagent, bench) | `unix` or `tcp`. Default `tcp` on macOS (Docker Desktop), `unix` elsewhere ([hosting.md](hosting.md#23-docker-on-macos-docker-desktop)) |
| `SANDCRAB_LIBKRUN_CGROUP` | libkrun backend on Linux (CLI, SDK, crabagent, bench) | Delegated cgroup v2 parent for the per-helper leaves. Unset: no host-side cap, with a warning. Set but unusable: the backend refuses to start ([hosting.md](hosting.md#host-side-cap-cgroup-v2-delegation-recommended)) |
| `SANDCRAB_SIGN_IDENTITY` | `sandcrab libkrun install`, `scripts/sign-krun-helper.sh` (macOS) | codesign identity for the helper. Default `-` (ad hoc) |
| `SANDCRAB_SIGN_RUNTIME` | same | `1`: hardened runtime even ad hoc |
| `SANDCRAB_KRUN_ENTITLEMENTS` | `scripts/sign-krun-helper.sh` | Entitlements plist override |
| `SANDCRAB_BENCH_KRUN_CGROUP`, `SANDCRAB_BENCH_FC_CGROUP` | bench | cgroup v2 parent for libkrun / Firecracker VMM leaves in the bench ([bench/README.md](bench/README.md)) |
| `SANDCRAB_E2E_BACKEND` | SDK e2e tests | Backend the ignored SDK e2e suite runs against (`docker`, `firecracker`, `libkrun`) |
| `RUST_LOG` | `crabd` | Log filter (default `info`) |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `CRABAGENT_MODEL`, `CRABAGENT_EFFORT`, `CRABAGENT_SSH_HOST` | crabagent | See [crabagent.md](crabagent.md) |

## State root layout

```text
<state root>/                    # $SANDCRAB_HOME or ~/.sandcrab
  registry.json, registry.lock   # workspace registry (atomic writes under the lock)
  workspaces/<id>/data/          # managed workspace contents
  workspaces/<id>/...            # per-workspace backend state (Firecracker image cache, staging)
  locks/<id>.{mount,rw}.lock     # lease flocks: rw exclusive, ro shared
  run/                           # per-sandbox socket dirs (Docker), run/fc/ (Firecracker), run/krun/ (libkrun)
  cache/                         # kernel/, firecracker/ (Firecracker artifacts), rootfs/ (Firecracker and libkrun)
  artifacts/libkrun/<version>/   # pinned libkrun + libkrunfw, manifest.json (sandcrab libkrun install)
  artifacts/libkrun-net/         # pinned netstack: gvproxy (macOS) or passt (Linux)
  bin/sandcrab-krun              # signed libkrun helper + sandcrab-krun.pin.json
  template-pins/                 # host-local template image pins
  recovery/                      # Firecracker drives whose sync-back failed
  transcripts/                   # crabagent transcripts
```

## Exit codes and signals

| Situation | Exit code |
| --- | --- |
| `run` | The sandboxed command's exit code; `128 + signal` if it was killed by a signal |
| `tool` | 0, or 1 if the tool returned an error |
| Other commands | 0 on success, 1 on error (`doctor`: any failed check) |
| Usage error | 2 |
| `sandcrab` receives SIGINT / SIGTERM / SIGHUP / SIGQUIT during `run` or `tool` | The sandbox is destroyed first, then `128 + signal` (130 on Ctrl-C, 143 on SIGTERM) |

If `sandcrab` is killed hard (SIGKILL), the container is left behind. `sandcrab doctor
--fix` removes it once its owner process is gone, and the SDK's TTL reaper destroys
sandboxes that a live process forgets.
