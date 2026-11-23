# crabagent

`crabagent` is a small coding-agent CLI in the style of pi-coding-agent. Claude, through the
Anthropic Messages API, works inside a Sandcrab sandbox. Its only tools are Sandcrab's 16
sandbox tools (`bash`, `file_read`, `file_write`, `file_edit`, `glob`, `grep`,
`process_start`, `process_poll`, ...), passed through 1:1 from `Sandbox::tool_definitions()`
and run with `Sandbox::tool_dispatch()`. Use it to see how smoothly an agent does real
development work in a sandbox.

- One ephemeral sandbox per session, destroyed on exit, on Ctrl-C and on panic.
- Persistent workspaces outlive sessions, so a second session sees the first one's work.
- Every request, response and tool call goes to a JSONL transcript, and the session ends
  with a token and cost summary.

## Quickstart

```sh
# Build crabd (musl) and the template images once; see the repo README / templates/build.sh
cargo build -p sandcrab-agent --release

# API key: environment variable or .env (searched from the current directory up to the repo root)
echo 'ANTHROPIC_API_KEY=sk-ant-...' >> .env
# Optional: the ssh target printed in port-forward hints
echo 'CRABAGENT_SSH_HOST=user@your-dev-host' >> .env
```

### Landing-page run

```sh
crabagent --workspace landing --template ts-dev \
  --mount-host docs/site:/kit:ro --network unrestricted --port 5173 \
  --backend docker --allow-insecure
```

What this command sets up:

- The workspace `landing` is created if it does not exist and mounted read-write at
  `/workspace`, which is also the working directory.
- `docs/site` is registered as a host-dir workspace (`host-site`, reused on later runs) and
  mounted read-only at `/kit`. The 3D kit lives at `/kit/3d`; see `docs/site/3d/README.md`.
- Guest port 5173 is published on `127.0.0.1:<random>`. The banner prints the exact address
  and an `ssh -L` command for reaching it from your laptop.

Example task: *"Build a landing page for Sandcrab in /workspace with Vite. Use the 3D kit at
/kit/3d (copy what you need, it is read-only). Start the dev server on port 5173."*

Run it again later with `--workspace landing` and the new session continues from the
previous session's files.

### One-shot

```sh
crabagent -w landing --backend docker --allow-insecure -p "add a footer with a GitHub link"
echo "run the tests and fix failures" | crabagent -w proj --backend docker --allow-insecure -p -
```

The exit code is `0` when the model finished, `1` when it did not (turn limit, refusal, API
error or lost sandbox) and `130` on Ctrl-C.

### REPL commands

| Command | Effect |
| --- | --- |
| `/exit` (or Ctrl-D) | Quit. The sandbox is destroyed; the workspace stays. |
| `/reset` | Forget the conversation and keep the same sandbox. |
| `/ports` | List published ports. |
| `/cost` | Show token usage and estimated cost so far. |
| Ctrl-C | Interrupt the current turn (the model call or the running tools). |

## Options

| Flag | Default | |
| --- | --- | --- |
| `-w, --workspace <name>` | none | Persistent workspace, created if missing, mounted read-write at `--cwd`. |
| `-t, --template <name>` | `ts-dev` | Sandbox template. |
| `-m, --mount ws:/path[:rw\|ro]` | none | Mount an existing workspace. Repeatable. |
| `--mount-host dir:/path[:ro\|rw]` | `ro` | Register a host directory as a workspace and mount it. Repeatable. |
| `--cwd <path>` | `/workspace` | Working directory in the sandbox. |
| `--network disabled\|unrestricted` | template's | Network policy. |
| `--port <n>` | none | Publish a guest port on host loopback. Repeatable. |
| `-e KEY=VALUE` | none | Sandbox environment variable. |
| `--backend docker --allow-insecure` | none | Backend; always explicit. Also `SANDCRAB_BACKEND` and `SANDCRAB_ALLOW_INSECURE=1`. |
| `--model` | `claude-sonnet-5-5` | Also `CRABAGENT_MODEL`. |
| `--effort low\|medium\|high\|xhigh\|max` | model's | `output_config.effort`. Also `CRABAGENT_EFFORT`. |
| `--max-turns <n>` | `100` | Model calls per user turn. |
| `--max-tokens <n>` | `64000` | Per response. |
| `--transcript <path>` | `~/.sandcrab/transcripts/<session>.jsonl` | JSONL transcript. |
| `--no-fallback` | off | Disable the server-side refusal fallback. |
| `-p, --prompt <task>` | REPL | Run a single task (`-` reads it from stdin). |
| `--keep-alive <secs>` | `0` | With `-p`: keep the sandbox and its ports up this long after the task (Ctrl-C ends it early). |
| `--env-file <path>` | first `.env` up to the repo root | Where `ANTHROPIC_API_KEY` is read from. |

## How it works

- **Tools.** Each `ToolDefinition` maps to `{name, description, input_schema}` with
  `eager_input_streaming: true`. A `tool_use` goes to `tool_dispatch(name, input)`, and the
  JSON reply goes back as a `tool_result` with string content. Images from `download` (or a
  binary `file_read` of an image) are attached as `image` blocks instead of base64 text. The result is marked
  `is_error` when the reply is `{"error": {...}}`. Inputs that are not valid JSON, or were
  cut off by `max_tokens`, get an error result and the tool is not run.
- **Parallel calls.** All `tool_use` blocks in one response run concurrently. Their results
  go back in a single user message, in call order.
- **API.** Requests are streamed over SSE with adaptive thinking (`display: "updates"`, so
  the model's short progress notes show up dimmed). Prompt caching covers the system prompt
  plus tools and the growing history. Refusals fall back server-side (`fallbacks:
  "default"`). Failed requests are retried with backoff on 408, 409, 429, 5xx and
  `overloaded_error`. The history is append-only, so cached prefixes and thinking blocks
  stay valid between requests.
- **Display.** Each call prints one line, for example `▸ bash  npm run build  (1.2s, exit 0)`,
  followed by at most 8 lines of output. The model still receives the full output, which
  crabd has already bounded.
- **Transcript.** The JSONL file records `session_start` (system prompt, tools, mounts),
  `user`, `request` (new messages only), `response` (content, usage, timing), `tool` (input,
  output, timing), `interrupted`, `error` and `session_end` (usage and cost).

## Tests

```sh
cargo test -p sandcrab-agent                     # unit tests + fake LLM against core's fake backend
cargo test -p sandcrab-agent --test docker_e2e -- --ignored   # fake LLM against a real DockerDev sandbox
```

Full guide (mounts, ports, caches, images, live-check findings): [docs/crabagent.md](../../docs/crabagent.md).
