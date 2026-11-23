# crabagent

`crabagent` is a minimal coding agent: Claude (Anthropic Messages API) whose only tools are
Sandcrab's 16 tools, running inside one ephemeral sandbox per session, on persistent
workspaces that survive across sessions. It is the V1 test harness. It drives the SDK the
way a real agent product would. Crate: [`crates/sandcrab-agent`](../crates/sandcrab-agent/README.md).

Backends: **DockerDev only** in V1 (`--backend docker --allow-insecure`). `firecracker`,
`cloud-hypervisor` and `libkrun` are rejected with "not available in crabagent yet".

## Setup

Prerequisites are the same as the [README quickstart](../README.md#quickstart-linux--docker):
Docker, a static `crabd` (`scripts/build-crabd.sh`) and pinned template images
(`templates/build.sh --pin`).

```sh
cargo build --release -p sandcrab-agent      # binary: target/release/crabagent
```

The API key comes from the environment or a `.env` file. Without `--env-file`, crabagent
uses the first `.env` from the current directory up to the repository root.

```sh
echo 'ANTHROPIC_API_KEY=sk-ant-...' >> .env
# Optional: the ssh target printed in port-forward hints
echo 'CRABAGENT_SSH_HOST=user@your-dev-host' >> .env
```

**Never commit `.env`.** It is in `.gitignore`. Debug output redacts its values. The
key never enters the sandbox: crabagent calls the API from the host. `ANTHROPIC_BASE_URL`
in the environment or `.env` overrides the API endpoint.

## Usage

```sh
# Interactive REPL on a persistent workspace
crabagent -w myproj --backend docker --allow-insecure

# One-shot task (exit 0 = done, 1 = not done, 130 = Ctrl-C)
crabagent -w myproj --backend docker --allow-insecure -p "add a footer with a GitHub link"
echo "run the tests and fix failures" | crabagent -w myproj --backend docker --allow-insecure -p -

# The live-check run: Vite site, read-only kit from the repo, a published dev port
crabagent --workspace landing --template ts-dev \
  --mount-host docs/site:/kit:ro --network unrestricted --port 5173 \
  --backend docker --allow-insecure
```

`SANDCRAB_BACKEND=docker` and `SANDCRAB_ALLOW_INSECURE=1` can replace the two flags.

### REPL vs `-p`

| | REPL (no `-p`) | One-shot (`-p <task>` or `-p -` for stdin) |
| --- | --- | --- |
| Sandbox | Lives until `/exit` or Ctrl-D | Destroyed when the task ends, unless `--keep-alive` |
| Published ports | Reachable for the whole session | Reachable only while the task runs (the banner says so, and the model is told not to present the URL as live) |
| Exit code | 0 | 0 done, 1 not done (turn limit, refusal, API error, lost sandbox), 130 Ctrl-C |

REPL commands: `/exit` (or Ctrl-D) quits, destroys the sandbox and keeps the workspace;
`/reset` forgets the conversation and keeps the sandbox; `/ports` lists published ports;
`/cost` shows token usage and estimated cost; `/help`. Ctrl-C interrupts the current turn
(model call or running tools). REPL history is saved at `<state root>/crabagent_history`.

### `--keep-alive SECS`

Only with `-p`. After the task finishes, the sandbox and its published ports stay up for
`SECS` seconds so a preview URL is reachable. Ctrl-C ends the hold early. The model is
told the sandbox stays up and to leave its server running. Default `0` (destroy at once).
The hold is skipped when the run was interrupted or the sandbox was lost.

```sh
crabagent -w landing --port 5173 --network unrestricted --backend docker --allow-insecure \
  -p "start the dev server on 5173" --keep-alive 600
```

## Flags

| Flag | Default | |
| --- | --- | --- |
| `-w, --workspace <name>` | none | Persistent workspace, created if missing, mounted read-write at `--cwd` (default `/workspace`) |
| `-t, --template <name>` | `ts-dev` | Sandbox template |
| `-m, --mount <ws>:<path>[:rw\|ro]` | none | Mount an existing workspace (default `rw`). Repeatable |
| `--mount-host <dir>:<path>[:ro\|rw]` | `ro` | Register a host directory as a workspace and mount it. Repeatable |
| `--cwd <path>` | `/workspace` with `-w`, else the first rw mount, else the template workdir | Working directory in the sandbox |
| `--network disabled\|unrestricted` | template's (`disabled` for all four) | Network policy |
| `--port <n>` | none | Publish a guest port on host `127.0.0.1:<random>`. Repeatable |
| `-e, --env KEY=VALUE` | none | Sandbox environment variable. Repeatable |
| `--backend docker` | none (required) | Also `SANDCRAB_BACKEND` |
| `--allow-insecure` | off | Required for docker. Also `SANDCRAB_ALLOW_INSECURE=1` |
| `--state-dir <dir>` | `$SANDCRAB_HOME`, else `~/.sandcrab` | State root |
| `--templates-dir <dir>` | `./templates`, then `<state root>/templates` | Repeatable, first wins |
| `--crabd <path>` | `$SANDCRAB_CRABD`, else `target/<arch>-unknown-linux-musl/release/crabd` | Static `crabd` |
| `--model <id>` | `claude-sonnet-5-5` | Also `CRABAGENT_MODEL` |
| `--effort low\|medium\|high\|xhigh\|max` | model default | `output_config.effort`. Also `CRABAGENT_EFFORT` |
| `--max-tokens <n>` | `64000` | Per model response |
| `--max-turns <n>` | `100` | Model calls per user turn |
| `--no-fallback` | off | Do not send the server-side refusal fallback |
| `--transcript <path>` | `<state root>/transcripts/<session>.jsonl` | JSONL transcript |
| `-p, --prompt <task>` | REPL | One task; `-` reads it from stdin |
| `--keep-alive <secs>` | `0` | Requires `-p`; see above |
| `--env-file <path>` | first `.env` from cwd up to the repo root | Where `ANTHROPIC_API_KEY` is read |
| `-v` / `-vv` | off | Debug logs to stderr |

## Mounts

- `-w <name>` mounts the workspace read-write at `/workspace` (or `--cwd`).
- `--mount-host dir:/path[:ro|rw]` registers `dir` as a host-dir workspace named
  `host-<basename>` (suffix `-2`, `-3`... on name clashes) and mounts it, **read-only by
  default**. Running it again for the same canonical directory reuses the registration.
  Sandcrab never deletes a registered host directory.
- **Overlap is refused.** A `--mount-host` directory that is inside another workspace's
  directory, or that contains one, is an error (nested workspaces are unsafe). The message
  says what to do instead: mount the parent workspace with `--mount <ws>:/parent:ro` and
  use the sub-path, or remove the inner workspace with `sandcrab workspace rm`.
- Read-only mounts are enforced by the backend (`Read-only file system`), and the system
  prompt marks them `READ-ONLY`.
- Between sessions you can change workspace files from the host, for example
  `sandcrab workspace put landing NOTES.md ./NOTES.md`. The next session sees the file.
  Host writes are refused while another process holds a read-write lease on the workspace.

## Ports and SSH tunnels

`--port 5173` publishes guest port 5173 on `127.0.0.1:<random host port>`. The banner
prints the address. Servers must listen on `0.0.0.0` and on that exact port (the system
prompt tells the model, e.g. `vite --host 0.0.0.0 --port 5173 --strictPort`). The host
port changes every session.

When you work on a remote host, the banner also prints a tunnel command:

```text
   port 127.0.0.1:32789 → 5173  (http://127.0.0.1:32789)
     tunnel: ssh -L 32789:127.0.0.1:32789 user@your-dev-host
```

The ssh target is `CRABAGENT_SSH_HOST` (environment or `.env`), else `$USER@<hostname>`.

## Caches

`$HOME` and everything outside the read-write mounts is reset every session. So that
`npm i`, `uv sync` or a Playwright browser download survive, crabagent points the package
caches into the workspace when the cwd is a read-write workspace mount:

- `XDG_CACHE_HOME=<cwd>/.cache` (pip / uv, Playwright on Linux)
- `npm_config_cache=<cwd>/.cache/npm`

The redirect applies to **Sandcrab-managed workspaces** (`-w`), not to registered host
directories (your own repos mounted with `--mount-host ... :rw`), so crabagent does not
write cache folders into them. `--env` overrides both variables. Templates that set an
explicit path in the image keep it (`render` sets `PLAYWRIGHT_BROWSERS_PATH`). The model is
told to keep `.cache` out of git and out of build output.

Other sandbox facts the system prompt states:

- `/tmp` may be `noexec` (it is on DockerDev): binaries and shared libraries cannot run
  from it. Use `$HOME` or the workspace (e.g. `export TMPDIR=$HOME/.tmp`).
- Core dumps are disabled on DockerDev (`ulimit -c` is 0), so crashing processes do not
  leave `core.*` files in the workspace.

## Images

The model can look at images. When `download` (or a `file_read` of a binary image file)
returns a PNG, JPEG, GIF or WebP (detected by magic bytes), crabagent sends it to the
model as an `image` content block inside the `tool_result`, next to a short text note.
The model never receives base64 as text:

- An image is attached only if it is intact and within the API's limits (5 MiB of base64
  per image, and a maximum pixel size per side). Otherwise the model gets a text summary.
- Other text files up to 64 KiB come back as text; any other file is summarised as size,
  sha256 and MIME type.
- Only the most recent few images stay in the conversation history. Older ones are
  replaced with placeholders, which keeps requests small.
- If the API rejects an image (HTTP 400), crabagent removes it and retries the request
  once.

Browsers: only the **`render`** template ships headless Chromium + Playwright
(`require('playwright')`, `chromium` on PATH). `ts-dev` has no browser. Use `-t render`
for tasks that need screenshots, or expect a Playwright download into the workspace cache
on the first run.

## Templates

| Template | Contents | Browser |
| --- | --- | --- |
| `base` | Debian 13 trixie-slim + core CLI utilities | no |
| `python-dev` | Python 3.15 + uv + ruff + pytest + mypy | no |
| `ts-dev` (default) | Node.js 26 + pnpm + bun + TypeScript + vitest + biome, plus Debian `python3` (no pip/venv) | no |
| `render` | Node + Playwright with bundled Chromium, ffmpeg, ImageMagick, Typst | **yes** |

## Transcripts

Every session writes JSONL to `--transcript`, default
`<state root>/transcripts/<session>.jsonl`. Events: `session_start` (system prompt, tools,
mounts, ports, model), `user`, `request` (new messages only), `response` (content, usage,
timing), `tool` (input, output, timing), `interrupted`, `error`, `session_end` (exit
code, outcome, usage, cost). Transcripts contain everything the model saw and wrote:
treat them as sensitive and scan before sharing.

## How it works

- Each tool definition maps to `{name, description, input_schema}`. A `tool_use` goes to
  `Sandbox::tool_dispatch(name, input)`; a reply of `{"error": {...}}` is marked
  `is_error`. Inputs that are not valid JSON, or cut off by `max_tokens`, get an error
  result and the tool is not run.
- All `tool_use` blocks in one response run concurrently; results return in call order.
- Requests stream over SSE with adaptive thinking. Prompt caching covers the system
  prompt, tools and the append-only history. Retries with backoff on 408, 409, 429, 5xx
  and `overloaded_error`.
- Each call prints one line (`▸ bash  npm run build  (1.2s, exit 0)`) and at most 8 output
  lines. The model receives the full, crabd-bounded output.

## What the live check found

From [bench/live-check.md](bench/live-check.md) (DockerDev on a 2-vCPU VPS, model
`claude-sonnet-5-5`, three one-shot sessions building [the landing page](site/landing/index.html)):

- **Cost and time:** 36m36s, 170 model calls, 188 tool calls, **$4.51** total. Prompt
  caching carried it: cache reads were about 98 % of input tokens. Model time and tool
  time were about equal (1 094 s vs 1 100 s).
- **Reliability:** 0 tool errors, 0 timeouts, 0 API retries, no lost sandbox. Read-only
  mount, host mutation between turns and persistence across sessions all passed.
- **Fluidity:** startup and teardown under 1 s each (`spawn→ready` 351 ms, teardown 130 ms
  for a `ts-dev` sandbox with two mounts). Review feedback converged in one round.
- **Friction (most of it fixed since):** most of turn 1 went into installing a browser in
  `ts-dev` so the agent could check its own work, because images reached the model as
  base64 text. Images are now image blocks, caches persist in the workspace, `render`
  ships Chromium, `ts-dev` has `python3`, core dumps are off, `/tmp noexec` is documented,
  and `-p` no longer advertises a preview URL as live (`--keep-alive` exists for that).
- `bash` dominated (160 of 188 calls): the model prefers heredocs and `sed -i` over
  `file_write` / `file_edit`, and shell `grep` over the `grep` tool.

## Tests

```sh
cargo test -p sandcrab-agent                                  # unit tests + fake LLM on core's fake backend
cargo test -p sandcrab-agent --test docker_e2e -- --ignored   # fake LLM against a real DockerDev sandbox
```
