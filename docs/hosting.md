# Host setup

Sandcrab has three backends. Each runs the same templates, the same static `crabd` and the
same protocol; they differ in what isolates the sandbox and in what host they need.

| Backend | Host | Isolation | Section |
| --- | --- | --- | --- |
| **Docker** | Linux with Docker Engine; macOS with Docker Desktop (local development) | Shared host kernel, hardened (`hardware_isolated = false`, `multi_tenant_safe = false`) | [1](#1-docker-host-linux), [2.3](#23-docker-on-macos-docker-desktop) |
| **libkrun** | macOS on Apple Silicon (Hypervisor.framework); Linux x86_64 with KVM | Hardware (own guest kernel), `multi_tenant_safe = false` | [2](#2-macos-apple-silicon), [3.4](#34-libkrun-on-linux), [4](#4-libkrun-reference) |
| **Firecracker** | Linux x86_64 with KVM | Hardware (own guest kernel), `multi_tenant_safe = false` until `jailer` confinement lands | [3](#3-linux-kvm-host) |

The backend is always chosen explicitly (`--backend`, `SANDCRAB_BACKEND`, or the SDK builder).
Sandcrab never falls back from one to another. Docker is for your own agents and trusted code.
No backend reports `multi_tenant_safe = true` yet, so none of them is meant for hostile
multi-tenant code today; Firecracker is the one on the path to it (`jailer`).

Placeholders: `<user>` is the account that runs Sandcrab, `<host>` the machine, `<state root>`
is `$SANDCRAB_HOME` or `~/.sandcrab`.

> **Upgrading an existing host?** Stop every running `sandcrab`, `crabagent` and bench process
> first, then:
>
> - **Rebuild `crabd`** (`scripts/build-crabd.sh`). Protocol 0.2 added a connection-level
>   `auth` frame ([protocol.md](protocol.md#authentication-auth)). The Docker backend always
>   starts `crabd` with `--auth-token-file`, which an older `crabd` does not have. Host and
>   `crabd` should come from the same build.
> - **Reinstall the libkrun helper** (`sandcrab libkrun install --helper <path>`). The
>   helper protocol is now version 2; a helper and a backend from different builds refuse each
>   other.
> - **Rootfs caches** (Firecracker, libkrun) now hold `0444` images with a stat stamp next to
>   the SHA-256. Older entries are made `0444`, hashed and stamped on their first use; nothing
>   to do by hand, unless the cache belongs to another user (then that first use fails: rebuild
>   it as its owner).
> - V1 moved the workspace lease lock files to `<state root>/locks/`. A process running an
>   older build and one running the new build would not see each other's leases.

## 1. Docker host (Linux)

### Packages

- Linux x86_64 or aarch64 (tested: Debian / Ubuntu on a 2-vCPU KVM VPS).
- Docker Engine (rootful; tested with Docker 29) and your user in the `docker` group.
- Rust via rustup (`rust-toolchain.toml` selects the stable channel) plus the musl
  target and `musl-tools`, for the static `crabd`:

```sh
rustup target add x86_64-unknown-linux-musl     # or aarch64-unknown-linux-musl
sudo apt-get install -y musl-tools build-essential
```

On a small host, `CARGO_BUILD_JOBS=2` keeps builds from starving everything else.

### The `docker` group is root-equivalent

Any member of the `docker` group can start a privileged container that mounts `/`, so it
is root on the host. Sandcrab's Docker backend needs that access. Only give it to
accounts you would give `sudo`, and don't use a shared multi-user box as a Docker host
for untrusted users. Rootless Docker is not tested with Sandcrab.

### Docker daemon hardening

`/etc/docker/daemon.json`:

```json
{
  "log-driver": "local",
  "log-opts": { "max-size": "10m", "max-file": "3" },
  "live-restore": true,
  "no-new-privileges": true,
  "userland-proxy": false,
  "icc": false,
  "ip": "127.0.0.1"
}
```

| Setting | Why |
| --- | --- |
| `"ip": "127.0.0.1"` | Published ports bind to loopback by default, never to the public interface. Sandcrab already publishes sandbox ports on `127.0.0.1`; this protects everything else on the host too |
| `"userland-proxy": false` | Port forwarding through iptables instead of a `docker-proxy` process per port |
| `"icc": false` | No traffic between containers on the default bridge |
| `"no-new-privileges": true` | Default for every container (Sandcrab also sets it per sandbox) |
| `"live-restore": true` | Containers survive a daemon restart |
| `log-driver` / `log-opts` | Bounded container logs |

Apply with `sudo systemctl restart docker`.

Docker writes its own iptables rules and bypasses `ufw` for published ports. Binding to
`127.0.0.1` is what actually keeps them private.

### Firewall, SSH, fail2ban

```sh
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH
sudo ufw enable

sudo apt-get install -y fail2ban    # enable the sshd jail, e.g. maxretry = 5, bantime = 1h
```

SSH: keys only (`PasswordAuthentication no`), `PermitRootLogin prohibit-password` or `no`,
`MaxAuthTries 3`, `X11Forwarding no`, `AllowAgentForwarding no`. Reach sandbox preview
ports through an SSH tunnel (`ssh -L <port>:127.0.0.1:<port> <user>@<host>`) rather than
opening them; crabagent prints that command for you ([crabagent.md](crabagent.md#ports-and-ssh-tunnels)).

### Build and check

```sh
scripts/build-crabd.sh            # static musl crabd + its sha256
templates/build.sh                # build base, python-dev, ts-dev, render and pin them on this host
cargo build --release -p sandcrab-cli
target/release/sandcrab doctor
```

What the Docker backend does per sandbox: `cap_drop ALL`, `no-new-privileges`, private IPC, docker
init as PID 1, PID / memory (no swap) / CPU limits, a size-limited
`nosuid,nodev,noexec` tmpfs at `/tmp`, core dumps disabled, the host user's uid:gid,
network `none` unless `unrestricted`, ports on `127.0.0.1` only, `crabd` bind-mounted
read-only, and a per-sandbox 256-bit token that every connection to `crabd` must present
([protocol: `auth`](protocol.md#authentication-auth)). Template `disk_mb` is **not** enforced
(overlay2 would need xfs with pquota). Images must match this host's pin
([template-pins.md](template-pins.md); local integrity, not provenance). It still shares the
host kernel, so it is not hardware isolation.

The host ↔ `crabd` transport on Linux is a Unix socket in a per-sandbox run dir under the
Docker `state_dir` (default `<state root>/run`). The backend requires that directory to be a
real directory (not a symlink) owned by this user, and tightens a looser one to 0700. The
`tcp` transport ([2.3](#23-docker-on-macos-docker-desktop)) also works on Linux, and Docker
Desktop for Linux needs it.

## 2. macOS (Apple Silicon)

A Mac runs two backends: **libkrun** (hardware-isolated microVMs on Hypervisor.framework) and
**Docker** through Docker Desktop. Only `linux/arm64` guests run on a Mac (Hypervisor.framework
does not emulate x86). macOS on Intel is not supported by libkrun ("not compiled in").

### 2.1 Common prerequisites

- Xcode Command Line Tools (`xcode-select --install`), for `codesign` and a linker.
- Rust via rustup plus the musl target. `scripts/build-crabd.sh` links `crabd` with
  `rust-lld` on macOS, so no musl toolchain is needed:

  ```sh
  rustup target add aarch64-unknown-linux-musl
  ```

- [Homebrew](https://brew.sh).
- **Docker Desktop**, even for libkrun: it builds the template images (`templates/build.sh`)
  and exports them into rootfs images. For libkrun it is a build-time prerequisite only (once
  per template version, cached), not needed while sandboxes run.

```sh
scripts/build-crabd.sh                # aarch64 static crabd (rust-lld)
templates/build.sh                    # arm64 template images, pinned on this host
cargo build --release -p sandcrab-cli # docker + libkrun backends are in the default features
```

### 2.2 libkrun on macOS

**1. Packages.** libkrun and libkrunfw (the guest kernel) come from Homebrew; e2fsprogs makes
the ext4 rootfs and scratch images. Homebrew's e2fsprogs is keg-only, so put its `sbin` on
`PATH` (add the line to your shell profile):

```sh
brew install libkrun libkrunfw e2fsprogs
export PATH="$(brew --prefix e2fsprogs)/sbin:$PATH"
mke2fs -V                             # must print a version
```

`sandcrab libkrun install` copies the Homebrew bottles only if they are the pinned versions
(libkrun 1.19.8, libkrunfw 5.6.4). If `brew upgrade` moved them, install refuses; pass
`--from-source` to build the pinned tags instead (needs Rust, clang and lld).

**2. Build the helper.** One `sandcrab-krun` helper process per sandbox becomes the VMM. It is
the only binary that needs the `com.apple.security.hypervisor` entitlement; the CLI, SDK,
crabagent and tests need no signature.

```sh
cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
```

**3. Install.** `install` signs the helper itself (ad hoc by default; see
[5. Release signing](#5-release-signing-macos)). `scripts/sign-krun-helper.sh <path>` signs a
binary in place, for running a helper straight from `target/`.

```sh
sandcrab libkrun install --helper target/release/sandcrab-krun
sandcrab doctor --backend libkrun
```

What `install` does is in [4.1](#41-what-sandcrab-libkrun-install-does), and every `lk:*`
line of `doctor` in [4.2](#42-sandcrab-doctor---backend-libkrun). Then:

```sh
sandcrab --backend libkrun run -t base -- 'uname -a'
sandcrab --backend libkrun run -t base --network unrestricted -- 'curl -sI https://example.com'
```

The first spawn of a template builds its rootfs (seconds for `base`, about a minute for
`ts-dev`); later spawns reuse the cache.

**macOS specifics:**

- **No cgroups.** The guest's RAM bounds the guest, but the helper's own VMM overhead (about
  100 MB idle) is not capped. `doctor` reports this as `lk:cgroup` (info).
- **Case-insensitive volumes.** Workspaces are shared live over virtio-fs, so a workspace on
  the default (case-insensitive) APFS volume is case-insensitive inside the guest too: `Readme`
  and `readme` are the same file. Docker Desktop behaves the same way. `doctor` warns
  (`lk:workspace_case`) for the managed workspace root and every registered host directory on
  such a volume. For projects that need case sensitivity, keep them on a case-sensitive APFS
  volume (Disk Utility → add an APFS volume, format "APFS (Case-sensitive)"). Rootfs images
  are ext4 inside a file and are not affected.
- **Socket path budget.** macOS limits Unix socket paths to 104 bytes. A very long
  `SANDCRAB_HOME` is refused with a hint to shorten it (never truncated).
- **Netstack quarantine.** The netstack (gvproxy) is the upstream release, which is not
  notarized. A file carrying `com.apple.quarantine` would be blocked by Gatekeeper behind a
  dialog. `sandcrab libkrun install`, and the backend when it starts, remove that attribute once
  the netstack matches its pinned SHA-256, through the file descriptor that was verified (its
  downloads are normally not quarantined in the first place). `sandcrab doctor` warns
  (`lk:netstack`) while the attribute is present. A quarantined file that does not match its pin
  is refused.

### 2.3 Docker on macOS (Docker Desktop)

Docker Desktop runs containers in a Linux VM, so a Unix socket in a bind-mounted directory
cannot connect the Mac to `crabd`: the socket lives in the VM's kernel. On macOS the Docker
backend therefore uses its **`tcp` transport** by default:

- `crabd` listens on a Unix socket in a 1 MiB per-sandbox tmpfs volume
  (`sandcrab-<id>-sock`; mode 0700, owned by the container user, `nosuid,nodev,noexec`).
- A second container per sandbox, `sandcrab-<id>-relay`, runs `crabd relay`. It mounts that
  volume read-only and publishes port 7070 on **`127.0.0.1` with an ephemeral host port**,
  never `0.0.0.0`. It runs no user code and is hardened like the sandbox, plus a read-only
  root filesystem, 64 pids and 64 MiB.
- A port on `127.0.0.1` is reachable by every local user and process on the Mac. So every
  connection must first present a per-sandbox 256-bit token
  ([protocol: `auth`](protocol.md#authentication-auth)). Without the token, nothing runs.
  The token goes to `crabd` as a 0400 file in the per-sandbox run dir, never in env or
  labels, so it is not in `docker inspect`. `crabd` deletes the file after reading it.
  The `unix` transport uses the same token. When the sandbox runs as another uid than the
  host process, the run dir has to be 0777 and the token 0444; what keeps other host users
  out is the Docker `state_dir` (default `<state root>/run`), which must be private
  ([1](#build-and-check)).

**`disabled` networking is unchanged.** The sandbox container keeps network mode `none`
(only loopback; no routes, no egress). Only the relay has a network interface, and it has no
user code. The trade-off is one extra small container per sandbox (created and started in
parallel with the sandbox; spawn-to-ready was about 200 ms p50 on an Apple Silicon Mac).

**The relay is not on the default bridge.** Every relay joins one dedicated bridge network,
`sandcrab-relay`, which the backend creates on first use with inter-container traffic off
(`com.docker.network.bridge.enable_icc=false`) and no outbound NAT. A network of that name
without those options is refused, never used (`docker network rm sandcrab-relay` and Sandcrab
recreates it). So:

| Who connects to a relay's port 7070 | Docker Engine (Linux) | Docker Desktop |
| --- | --- | --- |
| the host, through the `127.0.0.1` publish | yes (with the token) | yes (with the token) |
| another relay on `sandcrab-relay` | no (ICC off) | no (ICC off) |
| a container on the default bridge or any other network (an `unrestricted` sandbox, an unrelated container) | no (Docker isolates bridge networks) | **yes**: Desktop routes containers to other networks' published ports, and `host.docker.internal` reaches the host's published ports by design |

Where a neighbour can connect, it still meets `crabd`'s token, and an unauthenticated
connection never holds a connection slot: it waits in a small pre-auth queue that evicts the
oldest when full ([protocol: `auth`](protocol.md#authentication-auth)). Docker's internal
networks were rejected because Docker does not publish ports from them, and putting the
sandbox on a bridge would have given `disabled` sandboxes a network interface. Docker daemons
older than 25 allow duplicate network names; two concurrent first spawns there can create two
`sandcrab-relay` networks, after which spawns fail closed until one is removed.

Choose the transport with `SANDCRAB_DOCKER_TRANSPORT=unix|tcp` (or `DockerConfig::transport`).
`sandcrab doctor` shows the one in use. Linux defaults to `unix`. The Docker backend still
reports `hardware_isolated = false` and `multi_tenant_safe = false` on every host.

## 3. Linux KVM host

A Linux KVM host runs **Firecracker** and **libkrun** side by side (and Docker, if installed),
as the unprivileged `<user>`. Both microVM backends use the same `/dev/kvm` access, templates,
template pins and `crabd`.

### 3.1 Requirements

| Requirement | Check |
| --- | --- |
| Linux x86_64 (the Firecracker kernel and binary and the Linux libkrun pins are x86_64) | `uname -m` |
| `/dev/kvm` readable and writable by `<user>` | `ls -l /dev/kvm`; `sudo usermod -aG kvm <user>`, then log in again |
| VT-x / AMD-V visible to this kernel | `grep -cE 'vmx\|svm' /proc/cpuinfo` > 0 |
| Docker (to export template images into rootfs images) | `docker info` |
| e2fsprogs >= 1.47.1 (`mke2fs -d` with tar input, no root needed). Debian 13 and Proxmox VE 9 ship 1.47.2 | `mke2fs -V` |
| `curl` (artifact downloads) | |
| Firecracker, optional: nftables, for `unrestricted` networking | `nft --version` |
| libkrun: Rust, `make`, `gcc`, `git`, `patchelf`, static glibc (`libc6-dev`) to build libkrun and passt | |

`sandcrab doctor` checks these and says whether the host is bare metal, a VM with nested
virtualization, or a VM without it. The [Docker host](#1-docker-host-linux) section applies
too (packages, daemon hardening, firewall, SSH).

### 3.2 Nested virtualization (Proxmox example)

If the host is itself a VM, the hypervisor must pass virtualization extensions through.

**Finding from the V1 attempt.** The first planned KVM host was a Proxmox VM whose CPU type
was the default **"QEMU Virtual CPU version 2.5+"** (`qemu64`). That model does not expose
`vmx` / `svm`, so the guest had no `/dev/kvm` and Firecracker could not run. The fix is on
the Proxmox host, not in the VM:

1. Enable nested virtualization in the host's KVM module (Intel shown; AMD uses
   `kvm_amd` and is usually on by default):

   ```sh
   cat /sys/module/kvm_intel/parameters/nested           # Y or 1 = enabled
   echo "options kvm_intel nested=1" | sudo tee /etc/modprobe.d/kvm-nested.conf
   # reload kvm_intel with all VMs stopped, or reboot the Proxmox host
   ```

2. Give the VM the host CPU model: in the VM's Hardware → Processors, set Type to `host`,
   or `qm set <vmid> --cpu host`. This is `cpu: host` in `/etc/pve/qemu-server/<vmid>.conf`.
3. Stop and start the VM (a reboot from inside the guest does not apply CPU changes).
4. In the guest: `grep -cE 'vmx|svm' /proc/cpuinfo` is non-zero, `/dev/kvm` exists, and
   `sandcrab doctor` reports "VM with nested virtualization".

For benchmarks, prefer a dedicated VM or bare metal with SSD/NVMe storage, and run every
backend on the same host. Nested virtualization adds overhead to microVM numbers, a busy VM
skews all of them, and HDD storage skews Docker ([bench/README.md](bench/README.md)).

### 3.3 Firecracker

> **Status:** the real-KVM end-to-end suite passes (6/6 on a bare-metal host) and the SDK
> e2e suite runs on Firecracker. The backend reports `multi_tenant_safe = false` until
> `jailer` confinement lands.

#### Pinned artifacts

| Artifact | Pin |
| --- | --- |
| Guest kernel | `vmlinux` 6.18.62 (Firecracker CI build), SHA-256 pinned |
| Firecracker + jailer | v1.18.0 release tarball, tarball and both binaries SHA-256 pinned |

Source of truth: [`crates/sandcrab-backend-firecracker/src/artifacts.rs`](../crates/sandcrab-backend-firecracker/src/artifacts.rs).
Download and verify them into `<state root>/cache/{kernel,firecracker}/<version>/`:

```sh
sandcrab doctor --fix          # or: scripts/fetch-artifacts.sh [--out <cache-root>]
```

Every artifact is verified on download and again on use.

#### Rootfs images

```sh
scripts/build-crabd.sh
templates/build.sh base ts-dev
sandcrab rootfs build base ts-dev     # template image → export → inject crabd → mke2fs
sandcrab rootfs list
```

No KVM is needed for this step. Images are cached by (image ID, arch, `crabd` sha256, pipeline
version), stored read-only (`0444`) with a stat stamp next to their SHA-256. Each spawn opens
the image, checks it through that fd (an `fstat`, or a re-hash if the stamp changed) and hands
Firecracker the same fd (`/proc/<pid>/fd/<n>`). A template image without a host-local pin is
refused. See [cli.md](cli.md#rootfs).

#### Networking: `fc-net-setup.sh`

`--network disabled` (every template's default) needs no setup. For `unrestricted`, run the
one-time root setup. It creates a pool of tap devices owned by `<user>`, so Sandcrab never
needs privileges at runtime:

```sh
sudo scripts/fc-net-setup.sh --user <user> --count 64        # taps sctap0..sctap63
sudo scripts/fc-net-setup.sh --user <user> --persist         # also recreate them at boot
sudo scripts/fc-net-setup.sh --dry-run                       # print the commands only
sudo scripts/fc-net-setup.sh --uninstall                     # remove taps, table and unit
```

| Option | Default | |
| --- | --- | --- |
| `--user USER` | `$SUDO_USER` | Owner of the taps |
| `--count N` | 64 | Pool size (1..256) |
| `--prefix P` | `sctap` | Tap name prefix |
| `--uplink IF` | the default route's interface | Interface used for masquerading |
| `--allow-private` | off | Allow guests to reach private, link-local and CGNAT ranges |
| `--allow-smtp` | off | Allow outbound SMTP (tcp 25, 465, 587). Only for hosts allowed to send mail, see below |
| `--persist` | off | Install a root-owned copy at `/usr/local/sbin/sandcrab-fc-net-setup` plus a systemd oneshot that runs it at boot and again whenever Docker restarts (taps vanish on reboot) |
| `--dry-run` | off | Print instead of executing |
| `--uninstall` | | Remove everything it installed |

What it sets up: tap `sctapN` gets host `172.30.N.1/30` and guest `172.30.N.2`; a dedicated
nftables table `inet sandcrab` (no other tables are touched) that masquerades
`172.30.0.0/16` and enforces the egress policy below; `net.ipv4.ip_forward=1`; and, if
Docker's `DOCKER-USER` chain exists, ACCEPT rules for tap traffic there.

At runtime each sandbox leases a tap with an `flock` on `<lock dir>/<tap>.lock`. The lock dir
is host-wide per user, so every Sandcrab process of that user shares one lease set, whatever
its `SANDCRAB_HOME`: `/run/user/<uid>/sandcrab/taps`, else `$XDG_RUNTIME_DIR/sandcrab/taps`,
else `$TMPDIR/sandcrab-<uid>/taps` (usually `/tmp`). It is created 0700, and a lease is
refused unless the dir is a real directory owned by the user with mode 0700 and its parent is
owned by the user (or root) and not writable by group or others. So a `/tmp/sandcrab-<uid>`
pre-created by someone else is never used. Override it with
`FirecrackerConfig::taps.lock_dir`; all processes sharing the pool must use the same dir.

##### Egress policy

Guests with `unrestricted` networking get IPv4 internet access and nothing else:

| Blocked | Why | Override |
| --- | --- | --- |
| Spoofed source addresses (tap N only accepts `172.30.N.2`), all non-IPv4 traffic (so no IPv6) | A guest must not impersonate another guest or the host | none |
| Guest → host (every host address, incl. the tap gateway, the public IP and `docker0`); only replies to host-initiated traffic | Host services are not part of the sandbox | none |
| Guest → guest | Sandboxes are isolated from each other | none |
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10` | LAN, Docker networks, cloud metadata (`169.254.169.254`), CGNAT and tailnets | `--allow-private` |
| The uplink's own network: its on-link subnet(s) and default gateway, read from the routing table each time the script runs | On a dedicated server these are public addresses (the provider's subnet and router), so the ranges above miss them. They are still the host's LAN: neighbouring servers and the provider's router | `--allow-private` |
| `0.0.0.0/8`, `192.0.0.0/24`, `192.0.2.0/24`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `224.0.0.0/4` (multicast), `240.0.0.0/4` (class E, incl. `255.255.255.255`) | Reserved / special-purpose ranges (RFC 6890): nothing legitimate lives there, and they can reach odd places on some networks | none |
| Outbound SMTP, tcp `25`, `465`, `587` (rejected with a TCP reset, so mailers fail fast) | Spam from a sandbox goes out from the host's IP. That gets the IP blocklisted and dedicated servers suspended by their provider | `--allow-smtp` |

Use `--allow-smtp` only when the host is allowed to send mail and you trust the workloads.
The drop / reject rules have counters: `sudo nft list chain inet sandcrab forward` shows
how often each one hit. The uplink subnets are a snapshot: after a DHCP renumbering or a new
uplink route, re-run the script (with `--persist` it re-runs at every boot).

##### Docker coexistence

All the blocks live in Sandcrab's own nftables table at priority `filter - 1`, so they run
before Docker's (iptables-nft) filter chains, and an nftables drop is final. Docker cannot
re-open anything, even when it rebuilds its rules on restart. The only part that depends on
Docker is connectivity: Docker sets the `FORWARD` policy to DROP, so guests need the two
`DOCKER-USER` ACCEPT rules. If they were lost, guests would lose internet access (fail
closed), not gain access.

To keep those rules in place, the `--persist` unit is tied to `docker.service`:
`After=docker.service`, `PartOf=docker.service` (a `systemctl restart` or `stop` of Docker
is propagated to the unit) and `WantedBy=docker.service` (every start of Docker, including
an automatic restart after a crash, starts the unit again). Each start re-runs the setup,
which is idempotent and safe with running guests: taps are kept, the table is swapped
atomically and conntrack state survives the swap. On hosts without Docker the unit has no
Docker coupling.

Verify:

```sh
ip -br link show type tun | grep sctap ; cat /sys/class/net/sctap0/owner
sudo nft list table inet sandcrab
sudo iptables -S DOCKER-USER                      # only on Docker hosts
systemctl show sandcrab-fc-net -p PartOf -p WantedBy
```

`--persist` installs a root-owned copy on purpose: a root unit that executes a file the
Sandcrab user can edit would be a privilege escalation.

There is no egress allowlist yet. `egress_restricted` policies are refused at spawn time
(Phase 3 adds a proxy). Published ports (`--port`) work on Firecracker without a NIC: each is a
`127.0.0.1:<ephemeral>` listener tunnelled over vsock to the guest's loopback (`tunnel.open`).

#### Host-side cap (optional)

Without a cgroup, the guest's RAM bounds the guest but the Firecracker process itself is not
capped. A delegated cgroup v2 parent, set up as for libkrun ([3.4](#34-libkrun-on-linux)), gives
each VMM a leaf with `memory.max`, `memory.swap.max = 0` and `pids.max`. It is set through the
SDK (`FirecrackerConfig::cgroup_parent`) or, for the bench, `SANDCRAB_BENCH_FC_CGROUP`; the CLI
does not expose it.

#### Run

```sh
sandcrab doctor
sandcrab --backend firecracker run -t base -- 'uname -a'
cargo test -p sandcrab-backend-firecracker --test firecracker_kvm -- --ignored --test-threads=1   # real-KVM tests
SANDCRAB_E2E_BACKEND=firecracker cargo test -p sandcrab-sdk --features firecracker -- --ignored
```

Benchmarks: [bench/README.md](bench/README.md) (`sandcrab-bench run --backends docker,firecracker,libkrun`).

### 3.4 libkrun on Linux

libkrun needs no root, no taps and no nftables. With networking `disabled` (the default) the
guest has no NIC. `unrestricted` uses a pinned userspace netstack (passt) that the helper
spawns behind its own egress filter, again with no root and no host firewall changes
([4.3](#43-networking)). Results and caveats: [design/libkrun-backend.md §12.2](design/libkrun-backend.md).

#### Install

Build prerequisites: Rust (`cargo`), `make`, `gcc`, `git`, `curl`, `patchelf`, the static
glibc from `libc6-dev` (for passt), and e2fsprogs >= 1.47.1 for the rootfs images.

```sh
cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
sandcrab libkrun install --helper target/release/sandcrab-krun
sandcrab doctor --backend libkrun
```

On Linux, `install` always builds **libkrun from source**: `scripts/fetch-libkrun.sh` checks
out the pinned tag (commit-checked) and builds it on the host with `make BLK=1 NET=1`
(virtio-blk for the rootfs, virtio-net for the netstack), then checks that the needed symbols
are exported before installing. libkrunfw comes from its pinned prebuilt release asset
(`--fw-from-source` builds the guest kernel instead; that needs `bc`, `bison`, `flex`,
`libelf-dev`, `libssl-dev`, `python3-pyelftools`, `cpio` and `rsync`). The netstack, **passt**,
is built statically (`make static`) from its pinned tag, together with its `passt.avx2`
variant. Do **not** use the distro `passt` package: it is unpinned, and the backend only runs
the pinned install. Linux needs no code signing.

passt sandboxes itself with user namespaces. On Ubuntu >= 23.10 with
`kernel.apparmor_restrict_unprivileged_userns=1`, the pinned static binary still works without
any sysctl or AppArmor change (the distro AppArmor profile only attaches to `/usr/bin/passt`;
[design §12.3](design/libkrun-backend.md)).

#### Host-side cap: cgroup v2 delegation (recommended)

Without a cgroup, the guest RAM bounds the guest, but the helper's own VMM overhead
(virtio-fs and vsock threads, device buffers) is not capped. The backend warns once per
process, and `doctor` reports `lk:cgroup`. To cap it, give each helper a cgroup v2 leaf
(`memory.max` = guest RAM + 128 MiB, `memory.swap.max` = 0, `pids.max` = 64; the netstack
runs in the same leaf) under a parent delegated to `<user>`:

```ini
# ~/.config/systemd/user/sandcrab-vms.service
[Unit]
Description=Delegated cgroup v2 parent for sandcrab VMM leaves

[Service]
ExecStart=/bin/sleep infinity
Delegate=memory pids
# Keep the unit cgroup free of processes so it can hold per-VM leaves.
DelegateSubgroup=supervisor
# Runs in <unit>/supervisor: enable memory + pids for the leaves in <unit>.
ExecStartPost=/bin/sh -c "echo +memory +pids > /sys/fs/cgroup$(dirname $(cut -d: -f3 /proc/self/cgroup))/cgroup.subtree_control"
Restart=always

[Install]
WantedBy=default.target
```

```sh
sudo loginctl enable-linger <user>            # keep the user manager (and the unit) running
systemctl --user daemon-reload
systemctl --user enable --now sandcrab-vms.service
export SANDCRAB_LIBKRUN_CGROUP=/sys/fs/cgroup$(systemctl --user show -P ControlGroup sandcrab-vms.service)
```

`DelegateSubgroup=` needs systemd >= 254. Linger is required: without it, the user manager
stops at the last logout and takes the unit with it.

**The owning process must run inside the user manager's tree.** Moving a pid between cgroups
needs write access to `cgroup.procs` of the common ancestor of both cgroups. For a process in
an SSH session (`user-<uid>.slice/session-N.scope`), that ancestor is root-owned. Run the
sandcrab process (CLI, crabagent, bench, tests) under a user scope:

```sh
systemd-run --user --scope -q -- sandcrab --backend libkrun run -t base -- 'uname -a'
systemd-run --user --scope -q -- \
  cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
```

A configured parent that cannot be used (not delegated, controllers not enabled, or the caller
outside the tree) fails backend construction with `InvalidConfig` and the fix. It never
silently runs uncapped. The bench reads the same variable, and `SANDCRAB_BENCH_KRUN_CGROUP`
overrides it there.

#### Run

```sh
systemd-run --user --scope -q -- \
  cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
systemd-run --user --scope -q -- env SANDCRAB_E2E_BACKEND=libkrun \
  cargo test -p sandcrab-sdk --features libkrun -- --ignored --test-threads=2
```

(Without a configured cgroup parent, `systemd-run` is not needed.)

## 4. libkrun reference

Applies to macOS and Linux.

### 4.1 What `sandcrab libkrun install` does

Everything goes under the state root (`--state-dir` / `$SANDCRAB_HOME`), in directories created
`0700` whatever the umask. Nothing outside it is used at runtime: the backend never searches
`/opt/homebrew`, `LD_LIBRARY_PATH` or the distro.

1. **Libraries** into `<state root>/artifacts/libkrun/1.19.8/`, with a `manifest.json` holding
   each file's SHA-256 and stat stamp. macOS: copies the pinned Homebrew bottles and verifies
   their SHA-256 (or builds the pinned tags with `--from-source`). Linux: builds libkrun from
   source and takes the pinned libkrunfw release ([3.4](#34-libkrun-on-linux)). An install that
   already verifies is kept unless `--force` is given.
2. **Netstack** into `<state root>/artifacts/libkrun-net/<kind>-<version>/` with its own
   manifest: on macOS the pinned **gvproxy** v0.9.0 release asset, downloaded and SHA-256
   checked, installed byte for byte; on Linux **passt** built from its pinned tag, plus its
   pinned `passt.avx2`. `--no-netstack` skips it (then `--network unrestricted` is refused);
   `scripts/fetch-libkrun.sh --netstack-only` (re)installs only it.
3. **Helper** into `<state root>/bin/sandcrab-krun`: copied from `--helper` (default:
   `sandcrab-krun` next to the `sandcrab` binary), signed on macOS with the hypervisor
   entitlement (ad hoc, or `$SANDCRAB_SIGN_IDENTITY`), verified, renamed into place, and
   recorded in `<state root>/bin/sandcrab-krun.pin.json` (SHA-256 + stat stamp).
   `--no-helper` installs the libraries only.

`--dry-run` prints the steps without changing anything; `--json` prints what was installed.
Release signing: [5](#5-release-signing-macos).

**How the pins are used.** Every pinned input (rootfs image, libkrun, libkrunfw, netstack,
helper) is opened once, checked through that fd, and used through the same fd. The full
SHA-256 is paid only when the file's stat stamp (dev, inode, size, mtime, ctime) changed: a
match costs one `fstat`, a change re-hashes through the same fd and then re-stamps or refuses.
Linux executes the helper and the netstack through their verified fd; macOS loads the
libraries and disks through `/dev/fd/<n>` but still executes the helper and netstack by path
after the check (macOS has no fd-based spawn). Keep the state root on a local POSIX
filesystem: a stamp is only as good as the filesystem's ctime. Details:
[design §12.4](design/libkrun-backend.md).

A helper with no install record (`.pin.json`) is trusted on first use per process, with a
warning. A long-running process caches the helper's pin, so after reinstalling the helper,
restart such processes.

### 4.2 `sandcrab doctor --backend libkrun`

`doctor` always runs the libkrun checks in builds that include the backend. With
`--backend libkrun` (or `SANDCRAB_BACKEND=libkrun`) a missing prerequisite is a failure
(exit 1); otherwise it is a warning.

| Line | What it checks | Fix when it fails |
| --- | --- | --- |
| `lk:config` | The backend config, notably that the run dir leaves room for Unix socket paths | Shorten `SANDCRAB_HOME` |
| `lk:hypervisor` | macOS: `kern.hv_support = 1`. Linux: `/dev/kvm` opens read-write | Linux: `sudo usermod -aG kvm <user>` and log in again, or a host with KVM |
| `lk:helper` | `<state root>/bin/sandcrab-krun` exists and is executable, carries `com.apple.security.hypervisor` (macOS), and matches its install record | `sandcrab libkrun install --helper <path>` |
| `lk:artifacts` | libkrun and libkrunfw are installed and match the compiled-in pins (version, SHA-256, origin) | `sandcrab libkrun install` (`--force` to replace a mismatching install, which is never used) |
| `lk:crabd` | The static `crabd` that gets injected into rootfs images exists | `scripts/build-crabd.sh`, or `--crabd` / `SANDCRAB_CRABD` |
| `lk:mke2fs` | `mke2fs` is on `PATH` | macOS: `brew install e2fsprogs` and `$(brew --prefix e2fsprogs)/sbin` on `PATH`; Linux: install e2fsprogs |
| `lk:workspace_case` | Whether the managed workspace root and registered host directories are on a case-insensitive volume | Use a case-sensitive volume for workspaces that need it (warning only) |
| `lk:cgroup` | Linux: the `SANDCRAB_LIBKRUN_CGROUP` parent is usable (warns when unset). macOS: info, no cgroups | [3.4](#host-side-cap-cgroup-v2-delegation-recommended) |
| `lk:netstack` | The pinned netstack is installed and verifies. Info when absent (`unrestricted` is then refused) | `sandcrab libkrun install` |
| `lk:network` | Shown only with the deprecated `--libkrun-tsi`: warns that TSI exposes host loopback | Drop the flag; install the netstack |
| `lk:orphans` | Reaps run dirs and helpers left by dead sandcrab processes (live sandboxes are never touched) | |
| `libkrun` | Summary: ready, or the list of blockers | |

The summary line still ends with "e2e pending"; that note is stale (the e2e suites pass on
both platforms).

### 4.3 Networking

| `--network` | libkrun |
| --- | --- |
| `disabled` (every template's default) | No NIC, loopback only. TSI (libkrun's socket proxy) is never enabled implicitly, so the guest cannot reach host loopback or the LAN |
| `unrestricted` | A virtio-net NIC (guest `192.168.127.2`, gateway and DNS `192.168.127.1`, DHCP) on the pinned netstack, **behind the helper's egress filter**. Without the netstack: `PolicyUnenforceable`, never a fallback |
| `egress_restricted` | Refused at spawn (Phase 3 proxy), as on every backend |

The egress filter is default-deny: IPv4 and ARP only (no IPv6); to the gateway only DNS and
DHCP; never host loopback (including gvproxy's `192.168.127.254` alias), the host's own
addresses and on-link subnets (read at spawn), private, link-local, CGNAT, metadata and
reserved ranges, or SMTP (TCP 25/465/587); fragmented or truncated TCP/UDP is dropped. The
netstack runs as `<user>`, listens on nothing (its only channel is a socketpair with the
helper), and exits with the helper. What the filter cannot block: the public internet, DNS
through the host's resolver (so internal or split-horizon names can be resolved, though not
reached), addresses the host gains after spawn, and traffic volume. Details:
[design §9.1](design/libkrun-backend.md).

Published ports (`--port`) work without a NIC: a `127.0.0.1:<ephemeral>` listener per port,
tunnelled over vsock to the guest's loopback, as on Firecracker.

**Deprecated: `--libkrun-tsi`** (`LibkrunConfig::allow_tsi_unrestricted`). It runs
`unrestricted` through TSI, which lets the guest reach services on the host's loopback and
LAN. An installed netstack wins: the flag is then ignored, with an error-level log on every
spawn. It only applies where no netstack is installed.

### 4.4 Workspaces and multi-user hosts

Workspaces are live virtio-fs shares: guest writes land on the host immediately, as the host
user, and read-only mounts are enforced by the host-side server (`EROFS`; a guest remount
cannot lift it). The sandbox user defaults to the host uid:gid. The helper pins each share
root by fd, so swapping the host directory after spawn does not redirect the share.

**setuid bits reach the host.** A guest `chmod u+s` sets the setuid bit on the host file. It
is setuid to `<user>`, so the guest gains nothing, but on a multi-user host another local user
who can reach that file and runs it gets `<user>`'s uid. Docker bind mounts behave the same.
On a shared host, keep workspace directories (and registered host directories) unreadable by
other users (mode `0700` on the workspace root or a parent).

**Not multi-tenant safe.** The device model (virtio-fs server, vsock, block) runs in the
helper as the host user, without Seatbelt on macOS or Landlock/seccomp on Linux. An escape
through device emulation yields that user's privileges, including every other sandbox's
workspaces. The backend reports `multi_tenant_safe = false` for that reason.

## 5. Release signing (macOS)

On macOS only the helper needs a signature. Linux needs none.

| | Ad hoc (default, development) | Developer ID (release) |
| --- | --- | --- |
| Identity | `-` | `SANDCRAB_SIGN_IDENTITY="Developer ID Application: <Name> (<TEAMID>)"` |
| Entitlements | `crates/sandcrab-backend-libkrun/entitlements.plist`: `com.apple.security.hypervisor` | `entitlements-hardened.plist`: also `com.apple.security.cs.disable-library-validation` |
| `codesign` flags | `--force --sign - --entitlements …` | adds `--options runtime --timestamp` (hardened runtime, secure timestamp) |
| Status | Used by every test run | **Documented, not yet validated with a real certificate** |

**Why `disable-library-validation`:** the hardened runtime refuses to map libraries signed by
another team or ad hoc ("mapping process and mapped file have different Team IDs"), which
includes the Homebrew and from-source libkrun builds. The helper verifies both libraries
itself (pinned SHA-256, loaded through the verified fd), so library validation adds nothing it
relies on. The hardened runtime also strips `DYLD_*` variables; the helper does not depend on
them (libkrun's lookup of libkrunfw is bound to the already-loaded copy).

`SANDCRAB_SIGN_RUNTIME=1` signs ad hoc **with** the hardened runtime and the hardened
entitlements. That is how the Developer ID configuration was tested without a certificate:
the libkrun VM suite's boot, tampered-library and userspace-network tests passed with a helper
signed `adhoc,runtime`.

Developer ID steps (not yet run with a real certificate):

1. Build: `cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun`.
2. Sign, install and record in one step:
   `SANDCRAB_SIGN_IDENTITY="Developer ID Application: <Name> (<TEAMID>)" sandcrab libkrun install --helper target/release/sandcrab-krun`.
3. Verify the installed helper:
   `codesign --verify --strict --verbose=2 ~/.sandcrab/bin/sandcrab-krun` and
   `codesign -d --entitlements - ~/.sandcrab/bin/sandcrab-krun` (both entitlements,
   `flags=0x10000(runtime)`).
4. `sandcrab doctor --backend libkrun`, then `sandcrab --backend libkrun run -t base -- true`.
5. Notarize (release pipeline only, not dev or CI): zip the signed binary
   (`ditto -c -k --keepParent <helper> sandcrab-krun.zip`), then
   `xcrun notarytool submit sandcrab-krun.zip --keychain-profile <profile> --wait`.
   `xcrun stapler` staples tickets to an `.app`, `.pkg` or `.dmg`, not to a bare executable, so
   staple the package the helper ships in.

`scripts/sign-krun-helper.sh [--force] <helper>` does the signing alone, with the same
environment (`SANDCRAB_SIGN_IDENTITY`, `SANDCRAB_SIGN_RUNTIME`, and
`SANDCRAB_KRUN_ENTITLEMENTS` to override the plist). After signing in place, re-run
`sandcrab libkrun install --helper <helper>`: the signature changed the file, so its install
record must be rewritten.

**Caveat for distributing a pre-signed helper:** `sandcrab libkrun install` always re-signs
the helper it installs (ad hoc unless `SANDCRAB_SIGN_IDENTITY` is set). A helper that was
signed and notarized elsewhere loses that signature on install, and its notarization no
longer matches. Until install can keep an existing signature, the Developer ID flow is: sign
during install (step 2), then notarize the installed binary.
