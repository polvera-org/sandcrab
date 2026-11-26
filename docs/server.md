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

### Upgrade

A server restart ends every run: sandboxes live only in the server's memory, and a stop destroys
them all (a new server also removes what a crashed one left on Docker). So:

1. Clients drain: stop starting runs, let the running ones finish (or cancel them).
2. `sudo systemctl restart sandcrab-server` (or install the new binary first, then restart).
3. Clients reconnect; `server.hello` tells them the new version.

## macOS: a child of the client

No launchd service. The client makes a private directory (mode `0700`) and starts the server as
its child, on libkrun, with a pipe on its stdin:

```bash
sandcrab --backend libkrun server --socket "$DIR/server.sock" --lifeline-stdin
```

`--lifeline-stdin` stops the server when its stdin reaches end-of-file. The client holds the
pipe open, so a client that dies takes its server and its sandboxes with it. Client, server and
the libkrun helper run as one user, so the default allow list (the server's own uid) is right.

## Processes that outlive their supervisor

Each process runs under its own supervisor in the sandbox, which kills the whole tree on exit,
timeout or cancel and reports `tree_clean`. The program runs as the same user as its
supervisor, so it can kill it. Then nothing is left to stop the tree: the client gets `lost`
(or an exit) with `tree_clean: false`, and whatever the program started keeps running in the
sandbox until the sandbox is destroyed. **A client must destroy a sandbox whose process
reported `tree_clean: false`** before it runs anything else there.

## Self-test

`server.selftest {template}` runs the self-test ([design §8](design/server-and-sdk.md#8-self-test))
for that template on the server's backend, one run at a time per template, and keeps the last
report; `server.capabilities {template}` carries its summary and age. A self-test that cannot
start (no read-write mount root, the template is missing or unpinned) is reported as
`ok: false` with the reason; a run that does not finish in time is inconclusive. Neither is
ever `ok`. It needs a read-write mount root for its scratch directory.

The server sends `sandbox.destroyed` with reason `destroyed` on a destroy and `ttl` when the TTL
reaper ended a sandbox. `backend_exit` is not sent yet: a sandbox whose container or VM died stays
listed until it is destroyed.
