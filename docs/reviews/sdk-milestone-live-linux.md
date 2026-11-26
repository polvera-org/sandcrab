# SDK milestone: live check on a Linux VPS (Docker)

Date: 2026-11-26. Step `live-linux` of the sdk-milestone flow.

Host: `dev-vps`, Ubuntu 26.04, 2 vCPU, 7.7 GB RAM, Docker 29.10 (containerd image store), no
KVM, other unrelated containers running (load average about 1–3). The operator followed
[`docs/server.md`](../server.md) literally; every place where it was wrong or short is a finding
below. The client user is called `dev` (uid 1000) here.

## What was run

1. Release `sandcrab` and the static musl `crabd` built as `dev` (`CARGO_INCREMENTAL=0`,
   2 jobs); the `base` template built and pinned as root under `SANDCRAB_HOME=/var/lib/sandcrab`;
   `sandcrab doctor` and `sandcrab doctor --server-config`.
2. The packaged unit installed as root with a config: root server, `socket_mode = "0660"`,
   `socket_group = "sandcrab"`, `allow_uids = [1000]`, one `rw` mount root under a scratch
   directory in the client's home (`ReadWritePaths=` for it), `selftest_on_start = ["base"]`.
3. A consumer project: the TypeScript SDK built from this tree with `pnpm pack`, installed from
   the tarball with `npm install ./sandcrab-sdk-0.1.0.tgz`, Node 26.13.1. One script drives the
   running service through the public API only.
   The repo's e2e suite was not pointed at the service: every file starts its own server with its
   own flags (mount roots, allow list) and several kill or restart it, so an "external server"
   switch would cover little and is not small. The script covers the items the step lists.
4. The same script from a `node:26.13.1-trixie-slim` container, `--user 1000:1000`, the socket
   group added, only the socket directory and the data directory mounted (same absolute paths);
   then `systemctl restart` while that client stayed up.
5. Lifetime: client SIGKILL mid-run; `systemctl stop`; server SIGKILL (systemd restarts it).
6. Numbers, below.
7. Cleanup, below.

## Results

| # | Item | Result |
| --- | --- | --- |
| 1 | Build, template pin, `doctor` | **Pass after a doc fix.** The documented doctor command fails its `crabd` check (F3). With `SANDCRAB_CRABD` set: every server check ok. |
| 2 | Service | **Pass after two fixes.** As shipped, the start self-test failed on every root service (F1, F2). Fixed: `active (running)`, socket `0660 root:sandcrab`, directory `/run/sandcrab-server` `0755 root:root`, kept across restarts. Refused: root (`UNAUTHORIZED`), uid 2000 in the socket group (`UNAUTHORIZED`), `nobody` (`EACCES` at the socket). Journal: no env value, argv or stdin (searched for the test's values and the self-test nonce; 0 hits); 19 lines in 10 min, all start/stop. |
| 3 | Consumer script on the host | **Pass**, 8/8, three runs on the final build: exec with an exact env (incl. empty and non-ASCII values; sandbox uid = peer uid); 100 MiB through stdout; cancel of a TERM-ignoring `setsid` grandchild (cancelled, `treeClean`, grandchild gone, SIGKILL after the 1 s grace); a `rw` and a `ro` mount at their host paths (ro write refused, files `1000:1000` on the host); a forward from a `network: "disabled"` sandbox to an HTTP server in the client (small GET, 256 MiB download with matching SHA-256, external address refused); `selftest("base")` 15/15 pass as the peer; labels, `listSandboxes`, `destroyByLabel` (2 of 2, other label kept, two `sandboxDestroyed` events); TTL 3 s → `sandboxDestroyed {reason: "ttl"}` |
| 4 | Containerised client | **Pass**, 8/8. On `systemctl restart`: `shutdown {graceMs: 5000}`, `disconnect`, `connect` 255 ms later through the same directory mount; the old sandbox is `SANDBOX_NOT_FOUND`, a new one runs |
| 5 | Lifetime | **Pass.** Client SIGKILL: the sandbox stays, a new client finds it by label and destroys it (its process ended with its connection, as designed). `systemctl stop`: no sandbox container left, socket removed, directory kept. Server SIGKILL: systemd restarts it after 2 s, the new server removes the predecessor's container |
| 6 | Numbers | Below |
| 7 | Cleanup | **Done**, below |

## Numbers (dev-vps, 2 vCPU, shared with other containers)

| What | Value |
| --- | --- |
| Warm exec (`true`) through the SDK, 200 sequential | p50 8.8 / 9.8 ms, p95 13.2 / 15.2 ms (two runs) |
| Cold create + first exec, 8 sequential | p50 421–449 ms, max 495–637 ms |
| 16 sandboxes created at once | 3.1 s for all; one exec in each, in parallel, 0.2 s; destroy all about 1.5 s |
| Forward, 256 MiB HTTP download in the sandbox (curl) | 150–254 MB/s (host client), 243 MB/s (container client) |
| 100 MiB through stdout | 159–193 MB/s on an idle host (35–95 MB/s while a cargo build ran) |
| Self-test `base` | 1.1–1.4 s |
| TTL expiry event (TTL 3 s) | 3.7–4.2 s after `createSandbox` returned |
| Server RSS | idle 9–11 MB; peak 11.3 MB sampled at 100 ms during the 16-sandbox burst (VmHWM 12.4 MB) |

## Findings

| # | Severity | Finding | Status |
| --- | --- | --- | --- |
| F1 | **High** | The packaged unit and sample config put the socket in `/run/sandcrab`, which is also the sandbox's own run directory (crabd's socket and token) in every template. The self-test's "server socket not reachable" probe finds it, so `fs-outside-mounts` fails and isolation never reads as proven on the packaged service. The design already said `/run/sandcrab-server`. | Fixed: `fix(packaging)`, unit `RuntimeDirectory=sandcrab-server`, sample `socket`, docs |
| F2 | **High** | The self-test's scratch directory is made by the server with its umask. The unit sets `UMask=0077`, so the `ro` canary was root-only: the sandbox user got `EACCES` reading it and `EACCES` (not `EROFS`) writing the ro mount, and `fs-outside-mounts` failed on every root service. Not seen in CI because the e2e servers run as the client's uid. | Fixed: `fix(sdk)`, the scratch `ro`, its canary and `rw` are handed to the sandbox user |
| F3 | Medium | `docs/server.md`: the doctor command lacks `SANDCRAB_CRABD`, so its `crabd` check fails and the Docker daemon check is skipped. | Doc fixed |
| F4 | Medium | `docs/server.md` makes the `sandcrab` group but never adds the client user to it, and makes the mount root root-owned, where a client cannot create its run folders. | Doc fixed (`usermod -aG`, `install -d -o`) |
| F5 | Medium | Building the templates as root for the service moves the shared Docker tag (`sandcrab/base:0.1.0`); a user who already pinned it under their own `SANDCRAB_HOME` has a stale pin and their spawns are refused. With the containerd image store the old image was removed, so it cannot be retagged back. | Doc: adopt with `template pin` or repin for both. On dev-vps the existing user's `base` pin was repinned to the new image |
| F6 | Low | Warnings reached journald wrapped in ANSI colour codes. | Fixed: `fix(cli)`, colour only on a terminal |
| F7 | Low | `ReadWritePaths=` is edited into the installed unit; reinstalling it drops the edit. | Doc: a drop-in via `systemctl edit` (verified) |
| F8 | Low | At the default level the journal has no record of refused connections, created or destroyed sandboxes (they are `info`). Fine for privacy, thin for an operator. `RUST_LOG=info` turns them on. | Open, not changed |
| F9 | Low | `doctor --server-config` for a Docker config still runs the Firecracker and libkrun checks (with their KVM warnings), and the libkrun orphan reaper creates `/var/lib/sandcrab/run/krun`. Noise. | Open |
| F10 | Info | The doctor says the server makes the socket directory `0700`/`0710`; under the unit systemd makes it `0755` (`RuntimeDirectoryMode`), which the server accepts. Socket `0660` is what guards it. | Open |

## Doc fixes

`docs/server.md`: socket directory `/run/sandcrab-server` throughout; client user added to the
`sandcrab` group and owning the mount root; `ReadWritePaths=` through a drop-in, and why
`ProtectHome` matters for roots under `/home`; `SANDCRAB_CRABD` on the doctor command; the
shared template tag and how to adopt; the container example with `--user` and the data mount.

## Cleanup on dev-vps

Removed: the unit, its drop-in, `/etc/sandcrab`, `/var/lib/sandcrab` (state and the service's
pins), `/run/sandcrab-server`, `/usr/local/bin/sandcrab`, `/usr/local/lib/sandcrab`, the
`sandcrab` group, the scratch directories, the build copy with its `target/`, the Node install
and npm/pnpm caches made for this step, the `node` image and every test container. No
`sandcrab-*` container is left. Kept: `sandcrab/base:0.1.0` (`84d42957…`), which this step built
and which is now the pinned `base` of the host's existing user (F5). Free disk back to 21 GB.
