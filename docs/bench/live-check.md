# Live check: crabagent builds a landing page

Date: 2026-11-23. Host: the project VPS (Docker 29.10, no KVM). Branch `feat/phase-1`
(V1, review PASS). Operator and observer: Claude Code. The page itself was written
entirely by `crabagent` (model `claude-sonnet-5-5`, default effort) with every tool call
running inside a Sandcrab DockerDev sandbox. The operator only wrote the prompts, dropped
one file into the workspace between turns, and copied `dist/` out at the end.

> **Note (later rename):** this run predates the decision to make Docker an officially
> supported backend. `DockerDev` is now `Docker`, and the `--allow-insecure` opt-in used at
> the time is a deprecated no-op; the commands below omit it.

Result: [`docs/site/landing/`](../site/landing/index.html). Cleaned transcript of all three
sessions: [`live-check/transcript.jsonl`](live-check/transcript.jsonl) (1.2 MB; tool outputs
over 4 KB and one base64 blob elided, home paths and the ssh host redacted; scanned
for API-key patterns: none).

## Setup

```sh
cargo build --release -p sandcrab-agent -p sandcrab-cli        # 3m23s, CARGO_BUILD_JOBS=2
target/release/sandcrab doctor                                  # docker + all 4 template pins: ok
crabagent --workspace landing --template ts-dev \
  --mount-host docs/site:/kit:ro --network unrestricted --port 5173 \
  --backend docker --transcript /tmp/live/transcriptN.jsonl -p - < promptN.txt
```

The CLI accepted every flag in the flow notes as written; nothing had to be adapted.
`--mount-host` registered `docs/site` as the host-dir workspace `host-site` and mounted it at
`/kit` read-only. The workspace `landing` (managed dir) was created on the first run and is
mounted read-write at `/workspace`, which is also the cwd. Each run was one `-p` session (one
sandbox), so the three sessions also test workspace persistence across sandboxes.

## The three turns

| Turn | Prompt (user voice) | Wall | Model calls | Tool calls | Cost |
| --- | --- | --- | --- | --- | --- |
| 1 | Product brief: landing page selling Sandcrab, 3D hero from `/kit/3d`, crab assets and palette, responsive, light/dark, static output, preview on 5173 | 20m03s | 88 | 93 | $2.40 |
| 2 | Review: read the `NOTES.md` I dropped in; hero crab is small and off-centre; quickstart code block cut off; inline chips break on 390 px | 14m03s | 54 | 61 | $1.75 |
| 3 | Hero should be the waving `crab` scene, move `molt` into the "Spawn. Work. Molt." band; add a note on the framing trick to `/kit/3d/README.md`; final build | 2m30s | 28 | 34 | $0.35 |
| **Total** | | **36m36s** | **170** | **188** | **$4.51** |

All three sessions ended `outcome: Done`, exit 0.

**Tokens** (from `session_end`): input 346, output 143.1k, cache read 18.84M, cache write
480k. Prompt caching carried the run. Cache reads are about 98 % of input tokens.

**Where the time went**: model time (sum of `response.ms`) was 1,094 s and tool time was
1,100 s. Almost all the tool time is the agent checking its own work in headless Chromium
that it installed itself (see friction). The real development calls (`npm i`, `vite build`,
file writes) took seconds.

**Tool calls by tool**:

| Tool | Calls | Tool | Calls |
| --- | --- | --- | --- |
| `bash` | 160 | `process_start` | 4 |
| `file_edit` | 14 | `process_poll` | 2 |
| `file_write` | 3 | `process_list` | 2 |
| `list_dir` | 1 | `process_kill` | 1 |
| `download` | 1 | | |

`bash` dominates. The model prefers heredocs (`cat > file <<'E'`) and `sed -i` over
`file_write` and `file_edit`. In turn 1 it wrote `index.html` (25 KB) and `main.js` with
`file_write`, and in turns 2 and 3 it switched to `file_edit` for targeted changes. It never
used `grep` or `glob`; it used shell `grep`/`sed -n` inside `bash` instead.

**Errors and retries**: 0 tool replies with `is_error`, 0 timeouts, 0 API retries, no lost
sandbox. 12 `bash` calls exited non-zero and 1 output was truncated by crabd's bound:

- `python3: command not found` (×3 across turns). `ts-dev` has no Python. The agent rewrote
  each one in Node right away.
- Chromium launch failures (×4) and missing shared libraries. See friction #1.
- `grep: ... failed to map segment from shared object` after it put the libraries under
  `/tmp`. `/tmp` is `noexec` (bug B2). The agent moved them to `~` and continued.
- One wrong guessed path (`src/kit3d/src/scenes/crab.js`); it listed the folder and fixed it.
- One `sed -n` dump of `index.html` + `style.css` hit the output bound (truncated flag set).
  The agent re-read the parts it needed with narrower ranges.

Recovery was immediate every time. No turn got stuck in a loop.

**Sandbox lifecycle**: spawn took under 1 s from launch to `session_start` (13:41:35 → 36.15,
including workspace creation). Teardown took under 1 s from `session_end` to process exit.
A separate operator `sandcrab -v run -t ts-dev` with the same two mounts reported
`spawn→ready 351 ms, teardown 130 ms`. After every session `docker ps --filter
label=sandcrab.managed` was empty.

## Multi-mount, read-only and host mutation

- **Multi-mount: PASS.** Each session saw `/workspace` (rw, persistent) and `/kit` (ro, host
  dir) together. The agent copied the kit into `src/kit3d/` (three r186 vendored) and the
  webps into `public/assets/crab/`, as instructed.
- **Read-only: PASS, but the agent never tested it.** The system prompt marks `/kit`
  "READ-ONLY: never write here", and the agent believed it. In turn 3, asked to add a note
  to `/kit/3d/README.md`, it declined without trying. It wrote the note to
  `/workspace/KIT-README-NOTE.md` and said "remount `/kit` read-write and I'll do it". To get
  ground truth, the operator ran a probe sandbox with the same mounts: `touch /kit/3d/PROBE`
  → `Read-only file system`, `/proc/mounts` shows `/kit ext4 ro`. Nothing appeared in
  `docs/site/` on the host, and `git status docs/site/3d docs/site/assets` is clean.
- **Host mutation between turns: PASS.** Between turns 1 and 2 the operator ran
  `sandcrab workspace put landing NOTES.md /tmp/live/NOTES.md`. The put took 16 ms; the file
  held the tagline "Carve safe harbors for your agents", the repo URL and a hint about the
  hero. The turn-2 sandbox saw the file on its first `ls`. The agent used all three items:
  the tagline sits under the brand in the nav and footer, there is a GitHub button, and it
  reframed the hero.
- **Persistence across sessions: PASS.** Turns 2 and 3 resumed from the files left by the
  previous sandbox (source, `node_modules`, `dist/`) with no reinstall of npm dependencies.

## What felt fluid

- Startup is effectively instant. Banner, mounts, published port and ssh tunnel hint all
  show at once.
- The agent oriented quickly. It found and read the kit README, `symbology.html` (it
  stripped the SVGs with a Node one-liner) and the walkthrough, then planned and scaffolded
  Vite in about 10 calls.
- `npm i` over `--network unrestricted` worked first time (11 s for vite + fontsource).
- The accuracy of the copy is striking. Because `/kit` held `walkthrough.html` and
  `bench-report.html` as well, the page states real details: the u32 length-prefixed JSON-RPC
  frame, the 16 MiB frame cap, the 32 KiB per-stream head/tail bound, killpg on timeout,
  Docker labelled dev/CI only and "not multi-tenant safe", Firecracker "built against a fake
  VMM, real-KVM e2e pending".
- `process_start` for `vite preview`, then `process_poll`/`process_kill` behaved exactly as
  intended. Using `process_list` at session start to check for stale servers was a nice touch.
- Review feedback converged in one round each. All four turn-2 items were fixed, and the
  turn-3 hero swap was done in 2.5 minutes for $0.35.

## Friction

1. **The agent cannot see what it builds (biggest time sink).** `ts-dev` has no browser,
   and crabagent cannot pass images to the model. The agent wanted visual checks, so in
   turn 1 it ran `npx playwright install chromium` (126 MB), found missing system libraries,
   used `apt-get download` and `dpkg -x` to put them in `~/cdeps`, and then wrote pixel-bbox
   measurement scripts with `pngjs`. That cost about 8 minutes of tool time and most of the
   turn-1 cost. Because `$HOME` is ephemeral, turn 2 repeated the whole install. In turn 2 it
   also called `download` on a PNG, hoping to view it, and got 170 KB of base64 as text,
   which is useless to the model and expensive. In turn 3 it gave up and checked framing by
   projecting the geometry through the kit's camera maths in Node. Each final reply honestly
   says "I couldn't view screenshots".
2. **The preview dies with the `-p` session.** In every turn the final message says "open
   http://127.0.0.1:327xx", but the sandbox is destroyed right after, so the link is dead
   by the time a user reads it. The operator previewed by serving the workspace's `dist/`
   from the host (`~/.sandcrab/workspaces/<id>/data/dist`). A REPL session keeps the port
   alive; `-p` cannot.
3. **Chromium core dumps land in the persistent workspace.** Every crashed Chromium launch
   wrote a `core.<pid>` file into `/workspace` (cwd), with `ulimit -c` = unlimited. The agent
   noticed and cleaned them up in turns 1 and 2 and added `core.*` to `.gitignore`. See B1.
4. **`ts-dev` has no `python3`.** The model reaches for it by reflex (3 failed calls); it is
   cheap to recover from, but it is a recurring wasted call.
5. **Large heredoc `bash` calls instead of `file_write`.** This works, but the display line
   is `bash cd /workspace && cat > …`, which hides which file changed. Not a bug.
6. The random host port changes every session (32789 → 32790 → 32791). This is expected with
   `--port`, but it means a new ssh tunnel each time.

## Bugs found (not fixed; reported)

| ID | Severity | Component | Bug | Repro |
| --- | --- | --- | --- | --- |
| B1 | Medium | `sandcrab-backend-docker` | No `RLIMIT_CORE` limit. Crashing processes dump unbounded `core.*` files into the cwd, which is usually the persistent workspace. They count against the workspace and need host-side cleanup. | `sandcrab --backend docker run -t ts-dev -- 'ulimit -c'` → `unlimited`. Set `HostConfig.Ulimits` `core=0`. |
| B2 | Low | `sandcrab-backend-docker` (`spec.rs`) | The `/tmp` tmpfs is mounted `noexec` (Docker's tmpfs default) while the module doc says `nosuid,nodev`. Scripts and `.so` files under `/tmp` fail with `Permission denied` / `failed to map segment`. This is arguably good hardening, but it is undocumented and the system prompt doesn't mention it. | `run -t ts-dev -- 'grep " /tmp " /proc/mounts'` → `…nosuid,nodev,noexec…`; a `chmod +x /tmp/x.sh; /tmp/x.sh` → rc 126. Either document it (doc + crabagent prompt) or pass `exec` explicitly. |
| B3 | Medium | `crabagent` | `download` (and `file_read`) of an image returns base64 text to the model, so an agent cannot look at a screenshot. This wastes about 100k+ tokens and pushes agents into slow workarounds (friction #1). | Turn 2 transcript: `download /tmp/h1.png` → `content_base64` 170,088 chars in a `tool_result`. Map image MIME types to an `image` content block. |
| B4 | Low (UX) | `crabagent` | In `-p` mode the published port's URL is advertised (banner and the model's final message) but torn down at exit. | Any `-p` run with `--port`: the URL is dead after the summary prints. Possible fixes: say so in the banner, add `--keep-alive`, or tell the model in `-p` mode that servers die at exit. |
| B5 | Info | `crabagent` / registry | `--mount-host` creates `~/.sandcrab/workspaces/<id>/` (holding only `mount.lock`) for a host-dir workspace. This is harmless, but it surprised me. | `ls ~/.sandcrab/workspaces/<host-site id>` |

No issues with the protocol, crabd, mounts, ports, persistence or teardown. 188 tool calls
ran with zero `is_error` replies.

## Harvest

- Copied `/workspace/dist/` (from `~/.sandcrab/workspaces/<landing id>/data/dist`) unchanged
  into `docs/site/landing/`: `index.html`, `favicon.svg`, `assets/` (JS, CSS, self-hosted
  fonts, 9 crab webps). It is 1.7 MB. **No hand edits.** All paths were already relative.
- Static server (`python3 -m http.server`, `/landing/`): 4 WebGL canvases render (crab hero,
  molt, wire, burrow; SwiftShader). No horizontal overflow at 1440 and 390 px. Light and
  dark both look right. Theme toggle cycles auto/light/dark. No failed requests.
- `file://`: **does not work.** Vite emits `crossorigin` on the module script and the
  stylesheet, so Chromium blocks both from an opaque origin. The page shows unstyled HTML
  with no JS and no 3D. The agent warned about this. I left it as is per the "no hand fixes"
  rule; removing the two `crossorigin` attributes would restore the CSS, but the ES module
  would still need http.
- No-WebGL / reduced-motion: the kit's fallback (`assets/crab/*.webp`, passed as
  `fallbackBase`) and the still frame are wired in. The agent tested both in its own headless
  runs; I did not re-run them.

**Visual verdict** (operator screenshots, desktop 1440×900 and mobile 390×844, light and
dark): a polished, on-brand page. It uses the sand/teal/ink palette with coral kept for the
crab, Space Grotesk / Inter / JetBrains Mono, and the symbology glyphs as feature icons.
Sections: hero with epigraph → Spawn/Work/Molt with the molt scene → four pillars → one
wire (frame diagram plus wire scene) → workspaces (burrow scene plus window-door and
handoff illustrations) → backends (jar vs tide pool, honest capability table) → 16 tools
plus SDK/CLI snippets → crabagent → Apache-2.0 quickstart. It is honest about Docker vs
Firecracker. One nit remains: the turn-3 hero crab is now slightly over-zoomed (the claws
come close to the card edge). Note that Playwright's full-page screenshots showed the hero
repeated near the bottom. That is a capture artifact: viewport screenshots scrolled to the
same offsets are correct, and the DOM has one `<h1>`.

## Transcript excerpt (turn 3, cleaned)

```
▸ bash  ls /kit/3d 2>&1 | head -30; cat /kit/3d/README.md 2>&1 | head -60   (0.1s, exit 0)
▸ bash  cd /workspace; grep -n "meta\|view\|fallback\|still" src/kit3d/src/crab.js …  (0.0s, exit 0)
▸ file_edit  /workspace/src/main.js
▸ bash  cd /workspace; npm run build 2>&1 | tail -1; …                     (0.9s, exit 0)
  ✓ built in 479ms

**2. Kit README**
`/kit` is mounted read-only in this session, so I didn't write to `/kit/3d/README.md`.
The note is ready in `/workspace/KIT-README-NOTE.md` … paste it into `3d/README.md`
under "Composing your own scene". Or remount `/kit` read-write and I'll do it.

usage: 28 requests · in 58 (+858.2k cache read, 46.5k cache write) · out 15.2k · ≈ $0.35
sandbox 35ff5cf1-bedb-449f-9098-4bd7764cb184 destroyed
```
