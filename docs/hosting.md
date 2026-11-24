# Host setup: Docker host and KVM host

Two kinds of Linux host run Sandcrab V1:

1. A **Docker host** for the Docker backend: any Linux machine or VPS with Docker.
   Officially supported and hardened, but containers share the host kernel
   (`hardware_isolated = false`, `multi_tenant_safe = false`): use it for your own agents
   and trusted code. For hostile multi-tenant code, use Firecracker.
2. A **KVM host** for the Firecracker backend: bare metal, or a VM with nested
   virtualization, with a usable `/dev/kvm`.

Both run the same templates, `crabd` and protocol. Placeholders below: `<user>` is the
account that runs Sandcrab, `<host>` the machine.

> **Upgrading an existing host?** Stop all running `sandcrab` and `crabagent` sessions
> first. V1 moved the workspace lease lock files to `<state root>/locks/`, so a process
> running the old build and one running the new build would not see each other's leases.

## 1. Docker host

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
network `none` unless `unrestricted`, ports on `127.0.0.1` only, and `crabd` bind-mounted
read-only. Template `disk_mb` is **not** enforced (overlay2 would need xfs with pquota).
Images must match this host's pin ([template-pins.md](template-pins.md); local integrity, not
provenance). It still shares the host kernel, so it is not hardware isolation.

## 2. KVM host (Firecracker)

> **Status:** the Firecracker backend is built and tested against a fake VMM. Its
> real-KVM end-to-end tests and the benchmark are pending until a KVM host is available.
> The backend reports `multi_tenant_safe = false` until `jailer` confinement lands.

### Requirements

| Requirement | Check |
| --- | --- |
| Linux x86_64 (the pinned kernel and binary are x86_64) | `uname -m` |
| `/dev/kvm` readable and writable by `<user>` | `ls -l /dev/kvm`; `sudo usermod -aG kvm <user>`, then log in again |
| VT-x / AMD-V visible to this kernel | `grep -cE 'vmx\|svm' /proc/cpuinfo` > 0 |
| Docker (to export template images into rootfs images) | `docker info` |
| e2fsprogs >= 1.47.1 (`mke2fs -d` with tar input, no root needed). Debian 13 and Proxmox VE 9 ship 1.47.2 | `mke2fs -V` |
| `curl` (artifact downloads) | |
| Optional: nftables, for `unrestricted` networking | `nft --version` |

`sandcrab doctor` checks all of these and says whether the host is bare metal, a VM with
nested virtualization, or a VM without it.

### Nested virtualization (Proxmox example)

If the host is itself a VM, the hypervisor must pass virtualization extensions through.

**Finding from the V1 attempt.** The planned KVM host was a Proxmox VM whose CPU type was
the default **"QEMU Virtual CPU version 2.5+"** (`qemu64`). That model does not expose
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

For benchmarks, prefer a dedicated VM (or bare metal) and run **both** backends on the
same host. Nested virtualization adds overhead to Firecracker numbers, and a busy VM
skews both.

### Pinned artifacts

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

### Rootfs images

```sh
scripts/build-crabd.sh
templates/build.sh base ts-dev
sandcrab rootfs build base ts-dev     # template image → export → inject crabd → mke2fs
sandcrab rootfs list
```

No KVM is needed for this step. Images are cached by (image ID, `crabd` sha256, pipeline
version). See [cli.md](cli.md#rootfs).

### Networking: `fc-net-setup.sh`

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

#### Egress policy

Guests with `unrestricted` networking get IPv4 internet access and nothing else:

| Blocked | Why | Override |
| --- | --- | --- |
| Spoofed source addresses (tap N only accepts `172.30.N.2`), all non-IPv4 traffic (so no IPv6) | A guest must not impersonate another guest or the host | none |
| Guest → host (every host address, incl. the tap gateway, the public IP and `docker0`); only replies to host-initiated traffic | Host services are not part of the sandbox | none |
| Guest → guest | Sandboxes are isolated from each other | none |
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10` | LAN, Docker networks, cloud metadata (`169.254.169.254`), CGNAT and tailnets | `--allow-private` |
| The uplink's own network: its on-link subnet(s) and default gateway, read from the routing table at each run | On a dedicated server these are public addresses (the provider's subnet and router), so the ranges above miss them. They are still the host's LAN: neighbouring servers and the provider's router | `--allow-private` |
| `0.0.0.0/8`, `192.0.0.0/24`, `192.0.2.0/24`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `224.0.0.0/4` (multicast), `240.0.0.0/4` (class E, incl. `255.255.255.255`) | Reserved / special-purpose ranges (RFC 6890): nothing legitimate lives there, and they can reach odd places on some networks | none |
| Outbound SMTP, tcp `25`, `465`, `587` (rejected with a TCP reset, so mailers fail fast) | Spam from a sandbox goes out from the host's IP. That gets the IP blocklisted and dedicated servers suspended by their provider | `--allow-smtp` |

Use `--allow-smtp` only when the host is allowed to send mail and you trust the workloads.
The drop / reject rules have counters: `sudo nft list chain inet sandcrab forward` shows
how often each one hit.

#### Docker coexistence

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

There is no egress allowlist in V1. `egress_restricted` policies are refused at spawn time
(Phase 3 adds a proxy). Published ports (`--port`) work on Firecracker without a NIC: each is a
`127.0.0.1:<ephemeral>` listener tunnelled over vsock to the guest's loopback (`tunnel.open`).

### Run

```sh
sandcrab doctor
sandcrab --backend firecracker run -t base -- 'uname -a'
cargo test -p sandcrab-backend-firecracker --test firecracker_kvm -- --ignored   # real-KVM tests
```

Benchmarks: [bench/README.md](bench/README.md) (`sandcrab-bench run --backends docker,firecracker`).

## 3. libkrun on Linux (KVM host)

The libkrun backend runs on the same KVM host as Firecracker, as the unprivileged `<user>`. It
uses the same `/dev/kvm` access (the `kvm` group), templates, template pins and `crabd`. It
needs no root, no taps and no nftables: with networking `disabled` (the default), TSI is off
and the guest has no NIC. Results and caveats are in
[design/libkrun-backend.md §12.2](design/libkrun-backend.md).

### Install

Build prerequisites: Rust (`cargo`), `make`, `gcc`, `git`, `curl`, `patchelf`, and
e2fsprogs ≥ 1.47.1 for the rootfs images.

```sh
cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
sandcrab libkrun install --helper target/release/sandcrab-krun
sandcrab doctor --backend libkrun
```

`sandcrab libkrun install` runs `scripts/fetch-libkrun.sh`. It builds libkrun from the pinned
tag (with `BLK=1`), takes libkrunfw from its pinned prebuilt release, and installs both into
`<state root>/artifacts/libkrun/<version>/`. It also copies the helper into
`<state root>/bin/`. Linux needs no code signing.

### Host-side cap: cgroup v2 delegation (recommended)

Without a cgroup, the guest RAM bounds the guest, but the helper's own VMM overhead
(virtio-fs and vsock threads, device buffers) is not capped. The backend warns once per
process, and `doctor` reports `lk:cgroup`. To cap it, give each helper a cgroup v2 leaf
(`memory.max` = guest RAM + 128 MiB, `pids.max` = 64) under a parent delegated to `<user>`:

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

`DelegateSubgroup=` needs systemd ≥ 254. Linger is required: without it, the user manager
stops at the last logout and takes the unit with it.

**The owning process must run inside the user manager's tree.** Moving a pid between cgroups
needs write access to `cgroup.procs` of the common ancestor of both cgroups. For a process in
an SSH session (`user-<uid>.slice/session-N.scope`), that ancestor is root-owned. Run the
sandcrab process (CLI, bench, tests) under a user scope:

```sh
systemd-run --user --scope -q -- sandcrab --backend libkrun run -t base -- 'uname -a'
systemd-run --user --scope -q -- \
  cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
```

A configured parent that cannot be used (not delegated, controllers not enabled, or the caller
outside the tree) fails backend construction with `InvalidConfig` and the fix. It never
silently runs uncapped. The bench reads the same variable, and `SANDCRAB_BENCH_KRUN_CGROUP`
overrides it there.

### Run

```sh
cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
SANDCRAB_E2E_BACKEND=libkrun cargo test -p sandcrab-sdk --features libkrun -- --ignored --test-threads=2
```
