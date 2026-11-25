# `sandcrab` CLI reference

The operator and developer CLI (crate [`sandcrab-cli`](../crates/sandcrab-cli/)). Build it
with `cargo build --release -p sandcrab-cli`; the binary is `target/release/sandcrab`.
Everything below matches `sandcrab --help` for version 0.1.0.

```text
sandcrab [GLOBAL OPTIONS] <COMMAND>

  doctor     Check the host (crabd, Docker, template pins, Firecracker prerequisites, state dir)
  workspace  Manage persistent workspaces
  template   List or build templates
  rootfs     Firecracker rootfs images (ext4) built from templates
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
| `--backend <docker\|firecracker>` | none (`$SANDCRAB_BACKEND`) | Runtime backend. **Explicit, no fallback.** `firecracker` exists only in Linux builds. The pre-rename name `docker-dev` is still accepted as an alias of `docker` |
| `--allow-unpinned` | off | `docker` only: run template images that have no host-local pin (refused by default; see [template-pins.md](template-pins.md)) |
| `--crabd <PATH>` | `$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd` (target dir: `$CARGO_TARGET_DIR`, else `target/`) | Host path of the static musl `crabd` that gets injected |
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

Checks `crabd`, Docker, every template image against this host's pin (local integrity), the state
directory, and the Firecracker prerequisites: `/dev/kvm` readable and writable (with a
hint for VMs without nested virtualization), `kvm` group membership, pinned kernel and
`firecracker` binary, `mke2fs` (e2fsprogs >= 1.47.1 for tar input), the rootfs cache per
template, the `sctapN` tap pool, and orphaned run directories. Exit code 1 if any check
fails.

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
keyed by image ID, `crabd` sha256 and pipeline version. `--backend firecracker run` also
builds the rootfs on a cache miss.

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
| `--network disabled\|unrestricted` | template's (all four: `disabled`) | Network policy |
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
| `RUST_LOG` | `crabd` | Log filter (default `info`) |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `CRABAGENT_MODEL`, `CRABAGENT_EFFORT`, `CRABAGENT_SSH_HOST` | crabagent | See [crabagent.md](crabagent.md) |

## State root layout

```text
<state root>/                    # $SANDCRAB_HOME or ~/.sandcrab
  registry.json, registry.lock   # workspace registry (atomic writes under the lock)
  workspaces/<id>/data/          # managed workspace contents
  workspaces/<id>/...            # per-workspace backend state (Firecracker image cache, staging)
  locks/<id>.{mount,rw}.lock     # lease flocks: rw exclusive, ro shared
  run/                           # per-sandbox socket dirs (Docker), run/fc/ (Firecracker)
  cache/                         # kernel/, firecracker/, rootfs/ (Firecracker artifacts)
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
