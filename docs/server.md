# Running `sandcrab server`

`sandcrab server` is the host daemon behind the SDKs: a JSON-RPC API on a Unix socket, over one
backend you name. The API is in [design/server-and-sdk.md](design/server-and-sdk.md) §5; this
page is for the operator. Host prerequisites per backend are in [hosting.md](hosting.md).

| Host | How it runs |
| --- | --- |
| Linux | A root service under systemd, socket in `/run/sandcrab-server/` ([below](#linux-a-root-service)) |
| macOS | A child process of its client, on libkrun ([below](#macos-a-child-of-the-client)) |

## Linux: a root service

The server runs as root: Docker access is root-equivalent anyway, and root can open every
allowed mount root and reach the `crabd` socket of a sandbox that runs as another user.

### Install

```bash
# The binary and the static crabd (scripts/build-crabd.sh builds it).
sudo install -m 0755 target/release/sandcrab /usr/local/bin/sandcrab
sudo install -D -m 0755 target/x86_64-unknown-linux-musl/release/crabd /usr/local/lib/sandcrab/crabd

# Templates, built and pinned in the service's state root.
sudo mkdir -p /etc/sandcrab && sudo cp -r templates /etc/sandcrab/templates
sudo SANDCRAB_HOME=/var/lib/sandcrab /etc/sandcrab/templates/build.sh base

# A group for the clients (log in again after joining it), and a directory clients may
# mount, owned by the client so it can make its run folders there.
sudo groupadd --system sandcrab
sudo usermod -aG sandcrab <client user>
sudo install -d -o <client user> -g <client user> /srv/sandcrab

# Config and unit.
sudo install -m 0644 packaging/systemd/server.toml /etc/sandcrab/server.toml   # then edit it
sudo install -m 0644 packaging/systemd/sandcrab-server.service /etc/systemd/system/
sudo systemctl edit sandcrab-server   # [Service] ReadWritePaths=/srv/sandcrab (see below)
sudo SANDCRAB_HOME=/var/lib/sandcrab SANDCRAB_CRABD=/usr/local/lib/sandcrab/crabd \
  sandcrab doctor --server-config /etc/sandcrab/server.toml
sudo systemctl daemon-reload && sudo systemctl enable --now sandcrab-server
```

The unit ([`packaging/systemd/sandcrab-server.service`](../packaging/systemd/sandcrab-server.service))
restarts the server on failure, keeps `/run/sandcrab-server` across restarts
(`RuntimeDirectoryPreserve=yes`), and turns on ordinary hardening (`ProtectSystem=strict`,
`NoNewPrivileges`, `AF_UNIX` only, kernel tunables and modules protected). It is written for the
Docker backend. **Add every read-write mount root to `ReadWritePaths=`**: with
`ProtectSystem=strict` (and `ProtectHome=read-only` for roots under `/home`) the server cannot
write there otherwise, and the self-test makes its scratch directory in one. Put it in a drop-in
(`systemctl edit`), so reinstalling the unit does not drop it.

The doctor needs the same `SANDCRAB_HOME` and `SANDCRAB_CRABD` as the unit; without
`SANDCRAB_CRABD` its `crabd` check fails.

Templates are Docker images with shared tags (`sandcrab/base:0.1.0`). If a user on the host
already built and pinned them under their own `SANDCRAB_HOME`, rebuilding as root moves the tag
and that user's pin no longer matches (their spawns are refused until they repin). Either
adopt the existing image for the service (`sudo SANDCRAB_HOME=/var/lib/sandcrab sandcrab
--templates-dir /etc/sandcrab/templates template pin base`) or rebuild and repin for both.

The server raises its soft `RLIMIT_NOFILE` to the hard limit (at most 65536; the unit sets
`LimitNOFILE=65536`) and sizes its caps from it, so a busy server refuses with
`LIMIT_EXCEEDED` instead of running out of descriptors: connections of all kinds (at most 1024,
a quarter of the limit, the `max_connections` in `server.hello`) and forwarded connections
across all sandboxes (at most 4096, an eighth of the limit; one past the cap is refused, so the
program in the sandbox sees its connection closed).

### Config keys

`sandcrab server --config FILE` reads TOML; flags override the file. Unknown keys are refused.

| Key | Default | Meaning |
| --- | --- | --- |
| `socket` | (required) | Socket path. Its directory holds the socket and nothing else |
| `socket_mode` | `"0600"` | Socket mode, octal |
| `socket_group` | none | Socket group (name or gid), for a client in another uid with `0660` |
| `allow_uids` | the server's own uid | Peer uids admitted, checked on every connection. A root server must list the client's uid |
| `backend` | (required) | `docker`, `libkrun` or `firecracker`. Never a fallback |
| `state_dir` | `$SANDCRAB_HOME`, else `~/.sandcrab` | The backend's state |
| `templates_dirs` | `./templates`, `<state>/templates` | Template directories, first wins |
| `default_template` | none | Used when a create names no template |
| `selftest_on_start` | none | Templates whose self-test runs once at start, in the background |
| `allow_root_user` | `false` | May a client ask for uid or gid 0 in a sandbox |
| `allow_sandbox_uids` | none | Inclusive `[first, last]` uid ranges a client may name as its sandbox's uid besides its own, e.g. `[[60000, 62999]]` |
| `allow_sandbox_gids` | none | The same for the sandbox's gid besides the client's own gid |
| `[limits] max_sandboxes` | `64` | Sandboxes alive at once, all clients together. Raise it for many concurrent runs |
| `[limits] default_ttl_secs` | `1800` | TTL of a sandbox created without one |
| `[limits] max_ttl_secs` | `86400` | Longest TTL a client may ask for |
| `[limits] max_disk_mb` | `32768` | Largest `resources.disk_mb` a client may ask for (the scratch drive on libkrun and Firecracker; Docker does not enforce `disk_mb`) |
| `[[mount_roots]] path`, `mode` | none | Host directories clients may mount (`rw`: ro and rw; `ro`: ro only). No roots: every mount is refused |

Flags of `sandcrab server`: `--config`, `--socket`, `--socket-mode`, `--socket-group`,
`--allow-uid` (repeatable), `--mount-root PATH[:ro|:rw]` (repeatable), `--default-template`,
`--max-sandboxes`, `--allow-root-user`, `--lifeline-stdin`. The global flags `--backend`,
`--state-dir` and `--templates-dir` apply too.

Limits that are not config keys:

| Limit | Value |
| --- | --- |
| Connections of all kinds | 1024, or a quarter of the soft fd limit if that is less |
| Forwarded connections, all sandboxes | 4096, or an eighth of the soft fd limit if that is less |
| Requests in flight per control connection | 64 (then the server stops reading) |
| Processes per sandbox | 64 |
| Mounts per sandbox | 16; 4 on libkrun x86_64 ([Known limits](#known-limits)) |
| Forwards per sandbox | 16; 64 connections per forward, 256 per sandbox |
| `resources` per sandbox | `vcpu` 8, `memory_mb` 16384, `pids_max` 8192, `nofile` 1048576 |
| Labels per sandbox | 16 |
| First frame of a connection | within 2 s |

Over any limit is `LIMIT_EXCEEDED {what, limit}`; nothing queues. `server.hello` reports the
limits in force.

`sandcrab doctor --server-config FILE` (run it as the server's user, with the same
`SANDCRAB_HOME`) checks that the file parses, the socket directory would pass the start check,
the allow list, that every mount root exists and lies outside the state directory, the backend
preflight, and that the templates the config names are present and pinned.

### The sandbox user

A sandbox runs as its client's uid and gid (the peer credentials of the connection). A
`sandbox.create` that names another `user` is refused with `POLICY_UNENFORCEABLE` unless the
uid lies in an `allow_sandbox_uids` range and the gid is the client's own or lies in an
`allow_sandbox_gids` range; uid or gid 0 always needs `allow_root_user`. The reason: a sandbox
can leave a setuid or setgid file in a read-write mount, and the host honours it as that user
(Docker cannot mount a bind mount `nosuid`). Allow only ranges of throwaway users that own
nothing and are in no privileged group, and keep the mount roots on a filesystem mounted
`nosuid` where you can. A root client may name any user.

### A client in a container

Bind-mount the socket's **directory** into the client's container, not the socket file:

```bash
docker run --user <uid>:<gid> -v /run/sandcrab-server:/run/sandcrab-server \
  --group-add "$(getent group sandcrab | cut -d: -f3)" -v /srv/sandcrab:/srv/sandcrab ...
```

A bind mount of the file pins its inode, and the server replaces the socket at every start.
The directory survives restarts, so the client's mount stays valid. The client's uid must be in
`allow_uids` (the uid as the host kernel sees it), and its numeric gid list must hold the
socket's group. Directories it passes as mounts must be host directories bound into its
container at the same absolute path, under a mount root.

### Upgrade and restart

A server restart ends every run. Sandboxes, labels and TTLs live only in the server's memory, a
stop destroys every sandbox, and a new server removes what a crashed one left on Docker (owned
containers only; libkrun VMs die with their helper's lifeline). A new server cannot pick up an
old sandbox and does not try.

What a client sees during `systemctl restart`:

1. `server.shutdown {grace_ms: 5000}` on its control connection (the SDK's `shutdown` event).
   New requests get `SHUTTING_DOWN`.
2. Every running process is cancelled with a 5 s grace; its exit arrives if the relay can pass it
   on, otherwise the SDK reports it as `lost`.
3. Every sandbox is destroyed, the socket is removed, the control connection closes (`disconnect`).
4. The SDK reconnects with backoff (50 ms to 2 s). Measured on `dev-vps`: `connect` again 255 ms
   after `disconnect`, through a container's directory mount. The old sandbox ids now answer
   `SANDBOX_NOT_FOUND`.

So, for an upgrade:

1. Clients drain: stop starting runs, let the running ones finish (or cancel them).
2. Install the new binary, then `sudo systemctl restart sandcrab-server`.
3. Clients reconnect; `server.hello` tells them the new version. A client older than the server's
   previous minor version is refused with `UNSUPPORTED_VERSION`.

If the server crashes, the unit restarts it after 2 s (`Restart=on-failure`), and the new server
removes the predecessor's containers before it listens. A client killed mid-run leaves its
sandboxes running until their TTL; a new client of the same uid finds them with
`listSandboxes({labels})` and destroys them.

## macOS: a child of the client

No launchd service. The client makes a private directory (mode `0700`) and starts the server as
its child, on libkrun, with a pipe on its stdin:

```bash
sandcrab --backend libkrun server --socket "$DIR/server.sock" --lifeline-stdin \
  --mount-root /Users/dev/work:rw
```

`--lifeline-stdin` stops the server when its stdin reaches end-of-file. The client holds the
pipe open, so a client that dies takes its server and its sandboxes with it. Client, server and
the libkrun helper run as one user, so the default allow list (the server's own uid) is right,
and so is the default sandbox user. Measured on `mac-m2`: after a `SIGKILL` to the client, the
server and every `sandcrab-krun` helper were gone 0.14 s later, with nothing left behind.

Before the first start: install the helper (`sandcrab libkrun install`, [hosting.md](hosting.md#22-libkrun-on-macos)),
build and pin the templates with Docker Desktop (`templates/build.sh ts-dev`), and run one
sandbox of each template once (`sandcrab --backend libkrun run -t ts-dev -- true`) to fill the
rootfs cache; the first build takes 7 to 20 s. Keep `SANDCRAB_HOME` and the socket directory
short: Unix socket paths are limited to about 100 bytes.

A Node client:

```ts
import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { connect } from "@sandcrab/sdk";

const dir = mkdtempSync(path.join(tmpdir(), "sc-")); // created 0700
const socketPath = path.join(dir, "server.sock");
const server = spawn(
  "sandcrab",
  ["--backend", "libkrun", "server", "--socket", socketPath, "--lifeline-stdin",
   "--mount-root", "/Users/dev/work:rw"],
  { stdio: ["pipe", "inherit", "inherit"] }, // keep server.stdin open: it is the lifeline
);

let client;
for (let i = 0; !client; i++) {
  try {
    client = await connect({ socketPath });
  } catch (e) {
    if (i === 100 || server.exitCode !== null) throw e;
    await sleep(50);
  }
}
// ... use the client. To stop: await client.close(); server.stdin.end();
```

Mount paths under `/tmp` arrive in the sandbox as `/private/tmp/...`; use the SDK's
`resolvedPath` for `cwd` and env.

## Processes that outlive their supervisor

Each process runs under its own supervisor in the sandbox, which kills the whole tree on exit,
timeout or cancel and reports `tree_clean`. The program runs as the same user as its
supervisor, so it can kill it. Then nothing is left to stop the tree: the client gets `lost`
(or an exit) with `tree_clean: false`, and whatever the program started keeps running in the
sandbox until the sandbox is destroyed. **A client must destroy a sandbox whose process
reported `tree_clean: false`** before it runs anything else there.

## Self-test

`server.selftest {template}` runs the self-test ([design §8](design/server-and-sdk.md#8-self-test))
for that template on the server's backend: fifteen named lines (no capabilities, no new
privileges, no user namespace, loopback only, external addresses, DNS, gateway and metadata
refused, a forward reaches its target and nothing else and fails closed, nothing visible outside
the mounts, another sandbox unreachable, an exact env, a real tree kill). A line passes only on
a specific refusal; a timeout is `inconclusive`, never a pass.

- **It runs as the calling client.** The two self-test sandboxes run as the caller's uid:gid,
  the user its own sandboxes get by default. A root server therefore tests what its clients get,
  not 0:0. Reports are kept per template and user.
- **`server.capabilities {template}`** carries the summary of the last report for that template
  and the caller's user, with its age. `selftest: null` means it never ran for that user: treat
  it as **not proven**. The TypeScript SDK's `capabilitiesSnapshot()` does.
- **When it cannot run**, it is still a report: `ok: false`, no lines, and a reason that starts
  "isolation not proven" (no read-write mount root, the template is missing or unpinned, the
  backend refused, uid or gid 0 without `allow_root_user`). A run longer than 180 s is
  inconclusive. Neither is ever `ok`.
- **Scratch.** It makes a directory `sandcrab-selftest-<id>` in the first read-write mount root
  and removes it when the run ends. The directory and everything in it are made through
  descriptors, never followed through a symlink, so a client that can write the mount root
  cannot redirect what a root server writes or chowns. Its `ro` and `rw` parts belong to the
  sandbox user (a unit with `UMask=0077` would otherwise make them unreadable to it).
- One run at a time per template. `max_age_secs` returns the last report when it is young
  enough. `selftest_on_start` runs as the server's own uid, or on a root server as the first
  admitted non-root uid with its primary group.
- Measured: 0.5 s on `mac-m2` (libkrun), 1.1 to 1.4 s on `dev-vps` (Docker).

## Notifications

The server sends `sandbox.destroyed` with reason `destroyed` on a destroy and `ttl` when the TTL
reaper ended a sandbox (noticed within about a second), to the owner uid's control connections.
`backend_exit` is not sent: a sandbox whose container or VM died stays listed until it is
destroyed. `server.shutdown {grace_ms}` goes to every control connection when the server stops.

## Many concurrent execs

Every exec in a sandbox shares that sandbox's vCPUs. The templates give 1 (`base`) or 2 (`ts-dev`,
`python-dev`). On `kvm-dedicated`, 8 × 50 concurrent `true` execs in one Docker sandbox with 2 vCPUs
had p95 48 ms against a p50 of 6.5 ms: the container's CPU quota, not the server. For many
concurrent execs in one sandbox, raise `resources.vcpu` (at most 8). For many concurrent runs, use
more sandboxes and raise `[limits] max_sandboxes` (default 64). Creating a sandbox costs about
0.2 s on libkrun on `mac-m2` and 0.4 s on Docker on `dev-vps` (1.5 to 2.7 s on a busy HDD host),
against 5 to 10 ms for a warm exec, so keep sandboxes warm and reuse them.

## Known limits

| Limit | Detail |
| --- | --- |
| A program can kill its supervisor | Its tree then lives until the sandbox is destroyed (above) |
| A backend object that dies by itself | No notification (`backend_exit` is never sent). The sandbox stays listed until a destroy, its TTL or a server stop; calls into it fail |
| A restart ends every run | See [Upgrade and restart](#upgrade-and-restart) |
| libkrun on Linux x86_64: at most 4 devices for mounts | Directory mounts, block-volume workspaces and a NIC share four devices, so 4 mounts with network `disabled`, 3 with `unrestricted`. A fifth is refused before boot with `MOUNT_NOT_ALLOWED {cap}`. `max_mounts` reports it. Apple Silicon is not limited this way (64 measured); the cap there is 16 |
| libkrun on Linux: host deletes show late | A file the guest already looked up still shows in the guest for about 5 s after the host deletes it (virtio-fs caching, measured on `kvm-dedicated`). On macOS the lag is 4 to 5 ms |
| libkrun: file ownership | Files the guest writes appear on the host as the server's uid:gid. A sandbox user other than the server's can read them but not write. On a Mac run sandboxes as the server's uid (the default) |
| macOS: `/tmp` is `/private/tmp` | A mount under `/tmp` appears in the sandbox at `/private/tmp/...`, and new files there get group `wheel` |
| macOS: a root clock worker in every guest | libkrun's `init.krun` keeps a root process for time sync after host sleep. It belongs to libkrun, not to the sandbox user; tools that list guest processes should ignore it |
| libkrun: system directories are not mount targets | Exactly `/bin`, `/sbin`, `/lib*`, `/usr`, `/etc`, `/var`, `/opt`, `/mnt`, `/run`, `/tmp`, `/boot`, `/root`, any direct child of `/usr`, and anything inside `/run` or `/mnt/scratch` are refused. Directories below the others are fine |
| Docker: `disk_mb` | Not enforced (overlay2 needs a quota-capable filesystem). `max_disk_mb` still bounds the request |
| Docker: `nproc` is per host uid | `RLIMIT_NPROC` counts every process of that uid on the host (`capabilities.nproc_scope = "host_uid"`). Leave `limits.nproc` out on Docker and use `resources.pids_max`. `nproc: 1` fails the spawn with `EAGAIN` |
| Templates built as root move the shared tag | Templates are Docker images with shared tags. Rebuilding as root for the service moves the tag under another user's pin; adopt with `template pin` instead (above) |
| `tree_kill` in capabilities | Says `subreaper` when no live sandbox reported the fallback, even before any evidence. The self-test line `tree-kill` is the evidence |
| Self-test sandboxes | Not counted against `max_sandboxes` (at most two per template at a time) |
| `unrestricted` network through the server | Works in the backends' own tests; not run end to end through the server and the SDK |

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| The client gets `EACCES` connecting | Its user is not in the socket group (`usermod -aG sandcrab <user>`, then log in again), or in a container the numeric gid is missing (`--group-add`) |
| `UNAUTHORIZED` right after connecting | The client's uid (as the host kernel sees it) is not in `allow_uids`. A root server admits only root without that key |
| The server does not start: socket directory | The directory is writable by a user other than root and the server's, or a non-socket file sits at the socket path. Use `/run/sandcrab-server`, never `/run/sandcrab` |
| The server does not start: "another server is running on it" | A live server answers on that socket. Stop it, or choose another path |
| Self-test `fs-outside-mounts` fails on a service | The socket is in `/run/sandcrab` (every sandbox has its own there), or an old build made the scratch with the unit's umask. Move the socket to `/run/sandcrab-server` and update |
| Self-test reason "isolation not proven: ... could not run" | No `rw` mount root, a root that is not in `ReadWritePaths=`, a missing or unpinned template. The reason names it |
| `TEMPLATE_ERROR {kind: "pin_mismatch"}` | The image under the tag changed since the pin was made (often: someone rebuilt the template as another user). Repin with the service's `SANDCRAB_HOME` |
| `POLICY_UNENFORCEABLE` on create naming `user` | The requested uid or gid is not the client's own and not in `allow_sandbox_uids` / `allow_sandbox_gids`, or it is 0 without `allow_root_user` |
| `MOUNT_NOT_ALLOWED` | The path is not under a `[[mount_roots]]` entry, has a symlink in it (send the resolved path; the SDK does), nests with another mount, is over the cap, or touches the server's own directories. `data.reason` says which |
| A forward connection closes at once in the sandbox | No control connection serves the sandbox's forwards (the client disconnected; it calls `forward.serve` again after reconnecting), the client's target refused, or a cap. `sandbox.get` shows the per-forward `refused` counters |
| `sandcrab doctor` fails its `crabd` check | `SANDCRAB_CRABD` is not set to the static `crabd` the unit uses |
| `doctor --backend libkrun` exits 1 although libkrun is ready | Templates the doctor checks are not built (`python-dev`, `render`). Build them or ignore the line |
| Nothing in the journal about refused connections or created sandboxes | They are logged at `info`; the default level is `warn`. Set `Environment=RUST_LOG=info` in a drop-in. Env values, argv beyond `argv[0]` and stdin are never logged at any level |
| Warm execs slow under concurrency | The sandbox's vCPUs ([Many concurrent execs](#many-concurrent-execs)) |
