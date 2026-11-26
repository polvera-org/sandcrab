# Review: sdk-milestone (server, TypeScript SDK, forwards)

Scope: `git log 7dd2ad0..feat/sdk-milestone` (113 commits, 170 files). Read from the code, not from step
reports. No cargo or node run on the review machine; the gates on the build host are taken as reported
(1037 Rust tests, TS unit 64/64, TS e2e on Docker and libkrun/KVM).

## Verdict

**Do not ship as is. One Blocker, fixable in a small, local change; after that, ship with follow-ups.**

The Blocker only matters on a Linux host where the server runs as root, which is the documented Linux
deployment. On the Mac (server = client's uid) it gives nothing.

## Findings

### Blocker

**B1. The root server creates the self-test scratch through path names in a client-writable root and
follows symlinks. A client, or code in a sandbox with the root mounted, can make root chown or overwrite any
host file.**
`crates/sandcrab-sdk/src/selftest.rs:296-315` (`Scratch::create`), called from
`crates/sandcrab-server/src/selftest.rs:152-155`.

- What is wrong: `create_dir(dir)`, then `create_dir(dir/ro)`, `create_dir(dir/rw)`, `write(dir/ro/canary)`,
  `write(dir/canary-outside)`, `metadata(dir/rw)`, `chown(dir/rw, uid, gid)`. Every call resolves the path
  again from the root, and `write`, `metadata` and `chown` follow symlinks. The scratch root is the first
  `rw` mount root, which the client can write by design (it creates its folders there). Nothing makes it
  sticky, so the client can rename root's `sandcrab-selftest-<id>` directory even though it cannot write
  inside it. The name shows up in `readdir` as soon as it is created.
- Failure scenario (Linux, root server, `allow_uids = [1500]`, root `/srv/sandcrab` rw): uid 1500 prepares
  `/srv/sandcrab/evil/rw -> /etc/shadow`, calls `server.selftest`, watches `/srv/sandcrab` with inotify,
  and after `rw` is created renames `sandcrab-selftest-<id>` away and puts a symlink to `evil` in its place.
  `metadata` then sees `/etc/shadow` (uid 0, not 1500), and `chown` gives `/etc/shadow` to 1500. With a
  swap one step earlier, `write(.../canary-outside)` truncates any file root can write. The client can
  trigger the self-test as often as it likes, so it can retry the race. The same attack works from
  untrusted code inside a sandbox whose rw mount is the root itself. `MountPolicy::resolve` allows mounting
  a root (`hostmount.rs:388-390` test "The root itself"). The symlink target is resolved by the host's root
  process, not inside the sandbox.
- This breaks "the server never opens a host path outside the allowed roots or its own directories
  (symlinks, races)". It also answers the brief's question: a non-root allowed client can make root chown.
- Smallest fix: do it through descriptors. Open the root through the policy's held fd (or `open_chain`),
  `mkdirat` the scratch directory, then `openat(O_DIRECTORY|O_NOFOLLOW)` it and work only relative to that
  fd: `mkdirat` ro and rw, `openat(O_CREAT|O_EXCL|O_NOFOLLOW)` for the canaries, `fchown` on the opened
  `rw` fd. Hand the mounts the `(dev, ino)` from those fds (`resolve` re-checks anyway). Add a test that
  swaps the directory for a symlink between steps (an injectable hook).

### Major

**M1. A program can kill its supervisor and its tree then outlives it. The design describes a backstop
that does not exist.**
`crates/sandcrab-guest/src/supervise.rs:471-490` (`hand_over` waits on the supervisor and only records
`clean`); `crates/sandcrab-guest/src/supervise.rs:794-799` (only `crabd supervise` is a subreaper);
`crates/sandcrab-backend-docker/src/spec.rs:424` (`init: true`, so PID 1 is docker-init, which reaps but
does not kill).

- What is wrong: the program runs as the supervisor's uid, so it can signal it. The design (§3.5, lines
  304, 354, 370-372) says "`crabd serve` is a subreaper too … kills the processes it adopts". The code
  never calls `set_child_subreaper` in `crabd serve` and has no sweep after an unclean supervisor exit.
- Failure scenario: agent code runs `setsid sh -c 'sleep 1d' & kill -9 $PPID`. The client gets `lost`
  with `treeClean: false`. The `sleep` lives on in the sandbox. Contract point 7 wants one sandbox per run,
  reused for that run's short execs, so the leftover keeps running beside later execs until the sandbox is
  destroyed. The outcome is reported honestly (`treeClean: false`, and the TS doc says "destroy the
  sandbox"), but the cancel invariant "nothing the command started is left" holds only if the adapter acts
  on it.
- Smallest fix: make `crabd serve` a subreaper. When a supervisor exits with a non-zero status, list the
  descendants of `crabd serve` that are not another live supervisor or its subtree, and kill them with the
  same force loop. Add a guest test for "the program kills its supervisor". Alternatively, until that
  exists: correct the design and state in the SDK README that `treeClean: false` means the sandbox must be
  destroyed.

**M2. On a root server, a non-root client may run its sandbox as any non-zero uid and gid. With a
read-write mount, that becomes a setuid or setgid file the host will honour.**
`crates/sandcrab-server/src/control.rs:452-464`.

- What is wrong: `user` accepts any `uid:gid` except 0. Docker bind mounts keep host ownership, and Docker
  cannot set `nosuid` on a bind mount. Design §6.4 (lines 890-891) accepts "act as any ordinary user inside
  the allowed roots". It does not consider what such a file can do on the host.
- Failure scenario: a client of uid 1500 asks for `user: "1500:999"`, where 999 is the host's `docker`
  group, and a rw mount `/srv/sandcrab/x`. In the sandbox it copies an interpreter into the mount and runs
  `chmod g+s` on it. That is allowed: it owns the file and is in the group. On the host, a process of that
  user runs the file and gets egid `docker`, which is root. The same works with `user: "1000:1000"` and
  `u+s` to become the admin user. Within the roots, "read and write as any user" is documented. Becoming
  that user on the host, or root through a group, is not.
- Smallest fix: on a root server, accept only the peer's own uid, and a gid that is the peer's own gid
  (or, for a host client, one of its groups). Any other `user` needs a new `allow_other_users = true`.
  Also tell operators to keep mount roots on a `nosuid` filesystem (doctor can check `statvfs` `ST_NOSUID`).
  This also covers review item R9 for Docker.

**M3. Nothing bounds the server's file descriptors. Code in a sandbox can push a server into `EMFILE` and
break every client.**
`crates/sandcrab-server/src/lib.rs:50`, `crates/sandcrab-server/src/listener.rs:184-202`,
`crates/sandcrab-server/src/lifecycle.rs:135-147`, `packaging/systemd/sandcrab-server.service`.

- What is wrong: `MAX_CONNECTIONS = 1024` counts only tasks in `conns`. An attached forward leaves that
  task (`attach` hands the stream on and returns), so forwarded connections are not counted. Each one holds
  two server fds: the attach socket and the `forward.accept` connection to `crabd`. Per sandbox the guest
  allows 256 (`DEFAULT_MAX_FORWARD_CONNS`); across the server nothing bounds them. The unit sets no
  `LimitNOFILE`, and no code raises the soft limit, so systemd's default soft limit of 1024 applies.
- Failure scenario: untrusted code in two sandboxes opens 256 connections each to its forward port and
  holds them. The adapter's target accepts them, as any proxy would. That is about 1024 fds. From then on
  `accept` fails (`EMFILE`, retried every 50 ms), and new `crabd` connections for `process.spawn` fail
  (`UNAVAILABLE` for every client). The limit `max_connections: 1024` advertised in `hello` is
  unreachable under the default soft limit anyway.
- Smallest fix: raise `RLIMIT_NOFILE` soft to hard at server start, and put `LimitNOFILE=65536` in the
  unit. Count attached forwards against a server-wide cap (for example a semaphore that the relay holds,
  refused as `cap`). Advertise a `max_connections` the fd limit can actually hold.

**M4. `resources.disk_mb` has no ceiling.**
`crates/sandcrab-server/src/control.rs:465-486` caps vCPU, memory, pids and nofile, but passes `disk_mb`
through. On libkrun it sizes the scratch drive (`crates/sandcrab-backend-libkrun/src/backend.rs:529-538`,
`crates/sandcrab-rootfs/src/cache.rs:545` `size_mib * MIB`, with no overflow check).

- Failure scenario: a client asks for `disk_mb: 500000` by mistake. A sparse 500 GB drive is created, and
  a build in the sandbox fills the host disk. On a Mac that breaks Docker, libkrun and every other
  sandbox. A huge value overflows the multiplication: release builds silently wrap to a small or wrong
  size; debug builds panic in the blocking task.
- Smallest fix: `MAX_DISK_MB` (a config limit, default for example 32768) checked like the others and
  advertised in `ServerLimits`; `checked_mul` in `create_scratch`.

**M5. Privacy: the committed tree names the adopter.** This predates the milestone, but the tree is public.
`docs/site/bench-report.html:272` and `docs/bench/results/20261123T091401Z-dev-vps.json` (also
`…103824Z-dev-vps.json`), in the `declared` noise note: the names of the adopter's database and workspace
containers on the dev host. Fix: rewrite the note as "idle containers of another project" in the JSON,
regenerate the report, and check the git history if the repo is to be scrubbed.

### Minor

- **m1. The TS SDK checks the version but not `features`.** `sdk/typescript/src/client.ts:197` sets
  `_incompatible` from `api_version` only. Nothing refuses forwards, mounts or the self-test when the
  server does not list the feature. The plan's "SDK rules" ask for both. With API 1.0 every server has all
  three features, so a feature dropped later would surface as `METHOD_NOT_FOUND` / `INVALID_PARAMS`
  instead of a clear error. Fix: check the feature in `createSandbox` (mounts/forwards) and `selftest`.
- **m2. Two codes named `LIMIT_EXCEEDED`.** The server's process limit is `-32104` and the guest's is
  `-32011` (passed through `process.spawn`). TS maps both to the name `LIMIT_EXCEEDED`, but `rpcCode`
  differs, so callers that test the number miss one. `PATH_NOT_ALLOWED` (-32003) and `UNIMPLEMENTED`
  (-32007, which `map_err` emits for `SandboxError::Unimplemented`) are missing from
  `sdk/typescript/src/errors.ts:4-17` and show as bare numbers.
- **m3. `server.capabilities` claims `tree_kill: "subreaper"` with no evidence.** It does so when no
  sandbox is live (`crates/sandcrab-server/src/control.rs:377-396`). Design §3.5 says the fallback is
  never silent. Fix: report `"unknown"` until a live sandbox's `hello` or a self-test shows it.
- **m4. `destroy_by_label` can answer `empty: true` while a matching sandbox is live.** A sandbox between
  `spawn` returning and `set_owner` (`control.rs:519-526`) is live but has no owner, so `owned_matches`
  skips it on both looks (`control.rs:660`, `:676`). A create racing a sweep therefore reports empty. Fix:
  record the owner before `detach` (or count in-flight creates of that uid with these labels as
  remaining).
- **m5. Protection gaps in the mount policy.** The "contains a protected directory" check walks the
  mount's own ancestor chain only (`crates/sandcrab-core/src/hostmount.rs:192-198`). A protected
  directory bind-mounted somewhere below a requested mount is not seen. A mount root of `/` is accepted,
  and the Docker socket's directory is never protected by default. Fix: refuse `/` (and `/run`, `/var/run`)
  as roots in `validate`, and add `dirname(docker socket)` to `protected` for the Docker backend.
- **m6. The cross-uid guarantee has unit evidence only.** The TS e2e test is skipped
  (`sdk/typescript/test/e2e/lifetime.test.ts:175`). The evidence is
  `crates/sandcrab-server/src/control.rs:733`
  (`each_uid_sees_and_touches_only_its_own_sandboxes`, FakeBackend), plus the process cancel uid check,
  which is not tested across uids. Fix: one `#[ignore]` Linux test with a second uid (`sudo -u nobody`) on
  the build host.
- **m7. Sleeps used as synchronisation in tests.** `crates/sandcrab-server/tests/streams.rs:322` (sleep,
  then expects a free process slot), `:361`; `crates/sandcrab-server/tests/server.rs:550`;
  `sdk/typescript/test/unit/process.test.ts:190,225,311`. These can flake under load. Use `until(...)` as
  `streams.rs:23` already does. The "nothing happened within N ms" assertions (`streams.rs:194-198`,
  `:487`) can pass vacuously on a slow host. Acceptable, but note them.
- **m8. The systemd unit is untested.** No test or e2e runs the server under
  `packaging/systemd/sandcrab-server.service` (`ProtectSystem=strict`, `RestrictAddressFamilies=AF_UNIX`,
  `ProtectHome=read-only`). One manual run on `kvm-dedicated` before the pilot would settle it.
- **m9. Self-test sandboxes bypass `max_sandboxes`.** `crates/sandcrab-server/src/selftest.rs` does not
  call `reserve_slot`. That means two sandboxes per template run, one run at a time per template, so the
  bypass is small.

### Nit

- `crates/sandcrab-core/src/manager.rs:547`: a string continuation without `\` leaves about 18 spaces in
  the middle of the `WorkspaceRootChanged` message.
- Process ids share one namespace across uids (`crates/sandcrab-server/src/process.rs:121`). Random UUIDs
  make a collision theoretical.
- The stderr tail is 4096 bytes. The contract says "4096-character". The trimming is UTF-8 safe
  (`supervise.rs:1162-1174`), but a multi-byte tail holds fewer characters. State "bytes" in the TS docs.
- `docs/reviews/bench-tldr-review.md:54` lists `private-mesh` and `ubuntu@` as grep terms. They are
  harmless, but they hint at the private host naming.

## Contract table, row by row

| Row | Implemented | Tested | Evidence |
| --- | --- | --- | --- |
| argv, no shell, exact env | Yes: `env_clear().envs()`, argv direct (`supervise.rs:840-848`) | Yes | `sdk/typescript/test/e2e/process-contract.test.ts:34`, `crates/sandcrab-guest/tests/process_open.rs` |
| stdout ordered, complete, exit after last byte, backpressure | Yes: one record in flight per direction (`server/src/process.rs:18-21`), drain before `EXIT` (`supervise.rs:704-718`) | Yes | `process-contract.test.ts:49`, `crates/sandcrab-server/tests/streams.rs:39,181`, `process-extras.test.ts:170` |
| stdin once and closed, or kept open | Yes (`STDIN`/`STDIN_EOF` records) | Yes | `process-contract.test.ts:77,87,122`, `streams.rs:109,159` |
| stderr 4096 tail or merged | Yes, in the guest (bytes, see Nit) | Yes | `process-contract.test.ts:211`, `exec-contract.test.ts:145` |
| cancel: ask, grace, force; grandchild that ignores TERM | Yes, except a program that kills its supervisor (M1) | Partly | `process-contract.test.ts:102,145`, `streams.rs:327`; no test for supervisor kill |
| bounded exec: timeout, output cap kill/tail, abort, in the guest | Yes | Yes | `exec-contract.test.ts:34,43,53,71,79,87`, `process-contract.test.ts:231` |
| rlimits per exec; pids and nofile per sandbox | Yes | Yes (nofile, data, fsize per exec; pids at backend level) | `process-extras.test.ts:79`, `process-contract.test.ts:222`, `crates/sandcrab-guest/tests/process_open.rs:785,848`, `docker_e2e.rs`, `libkrun_vm.rs` (pids_max) |
| folders at the same path, ro/rw, under roots, not nested | Yes (`hostmount.rs`), with B1/M2/m5 | Yes | `mounts.test.ts:66,112,131,145,163,179`, `crates/sandcrab-server/tests/server.rs:407`, `hostmount.rs` unit tests |
| network none / endpoints / open | Yes: `disabled` + forwards that end at the client; `unrestricted` | `disabled` + forwards yes; `unrestricted` through the server not e2e (backend only) | `forwards.test.ts:157-268`, `streams.rs:531,583,682`; `docker_e2e.rs:566` |
| sweep by label, true only when empty | Yes (m4 race) | Yes | `lifetime.test.ts:50,155`, `control.rs:733` |
| reap by label after a client or server restart | Yes: labels in server memory, a server restart ends runs, predecessor reaped (Docker) | Yes | `lifetime.test.ts:120,177`, `server-contract.test.ts:68`, `crates/sandcrab-cli/tests/e2e_server.rs:120` |
| capabilities: self-test with named lines | Yes (m3 for `tree_kill`) | Yes | `selftest.test.ts:31`, `crates/sandcrab-server/tests/ops.rs:72,135,213`, `crates/sandcrab-sdk/tests/e2e_selftest.rs` |
| ENOENT vs backend unavailable | Yes (`spawnError` / `unavailable` / `lost`) | Yes | `process-contract.test.ts:177`, `server-contract.test.ts:52,68,103`, `streams.rs:212` |
| `host` identity | Nothing (by decision) | n/a | n/a |

Guarantees from the brief:

| Guarantee | Status |
| --- | --- |
| No host path outside the roots or the server's own directories | Mount path: holds (fd walk, `O_NOFOLLOW`, identity re-check; Docker/Linux also after start through `fs.identity`). **Self-test scratch: broken (B1)** |
| Forwards fail closed; the server never dials on a client's say-so | Holds: the hub only relays to an attach from the same uid; no TCP or Unix dial in the server |
| Exact env, no shell | Holds |
| Nothing outlives its process or sandbox | Sandbox: holds (stop destroys all; Docker predecessor reaped). Process: M1 |
| Another uid cannot reach a sandbox | Holds in code (`owned`, `claim` by uid, cancel by uid); unit-tested only (m6) |
| Secrets never logged | Holds as far as read: argv[0] and env count only; `crates/sandcrab-server/tests/logging.rs:28` checks trace output |
| No silent backend downgrade | Holds: backend required, built backend compared with config (`lifecycle.rs:67-74`); Firecracker refuses forwards |
| No `unsafe` outside the FFI island | Holds: no `unsafe` added; workspace `forbid`, libkrun `deny` + `ffi.rs` |
| Resource bounds | Frames 16 MiB, first-frame timeout, 64 in flight, 64 processes per sandbox, tail caps: hold. fds (M3) and disk (M4) unbounded |
| Root server: what a non-root client can make root do | Mount any directory under the roots: yes (as designed). Run as uid 0: no unless `allow_root_user`. Run as any other uid/gid: yes (M2). chown: yes, through the B1 race |
| Wire compatibility | Server: same major, client minor ≥ server minor - 1 (`api.rs:121-135`). TS: refuses an older minor or another major. Features not checked by TS (m1). Schema snapshot and TS copy are identical |

Cut list: really cut. The API has no upload/download, no tool call, no template list/verify, no PTY or
signals on `process.spawn` (`crates/sandcrab-protocol/src/api.rs:55-75`, `sdk/typescript/src`).

## Stale statements (for the docs step)

- `docs/design/server-and-sdk.md:12`, `:1314` ("Clients are not separated from each other"), `:1322`
  ("Create, list and destroy **every** sandbox … including other clients'") and
  `docs/decisions/0002-server-and-sdk.md:106` ("Every client that passes the uid check controls every
  sandbox"). The code separates by uid: a sandbox belongs to the uid that created it.
- `docs/design/server-and-sdk.md:304`, `:354`, `:370-372`, `:1421`: `crabd serve` as a subreaper that kills
  what it adopts. It is not (M1).
- `docs/design/server-and-sdk.md:326-341`, `:1420`, `:1543`: the spec in a `memfd`, the connection as stdin
  and stdout, the spec on fd 2. The code uses a control pipe on stdin (spec frame, then the `A`/`F` cancel
  bytes), the connection on stdout, and stderr inherited (`supervise.rs:1-21`).
- `docs/design/server-and-sdk.md:343-344`: "`process.cancel` is two signals from `crabd serve` to the
  supervisor". It is control-pipe bytes.
- `docs/design/server-and-sdk.md:890-891`: "the roots are the bound" for any non-zero `user`. Not true on
  the host (M2).
- `docs/server.md`: no mention that the self-test scratch root must not be writable by untrusted parties
  (B1), no `LimitNOFILE` (M3), no `disk_mb` limit (M4).

## What I did not review

- libkrun helper, netstack (passt/gvproxy) and the FFI island beyond the milestone's `workspace.rs` and
  `backend.rs` mount and user changes; the Firecracker backend; `sandcrab-bench`.
- `crates/sandcrab-guest/src/probe.rs` line by line (the self-test probes) and `init/config.rs`.
- `crates/sandcrab-cli/src/doctor_server.rs`, `sdk/typescript/src/testing/fake-server.ts`,
  `scripts/generate-types.mjs` and the generated types beyond the schema-equality check.
- The Rust SDK's process API in depth. There is no Rust client of the server, so error-shape consistency
  was checked server ↔ TS only.
- Nothing was run. Tests worth running on the build host: a guest test where the program
  `kill -9 $PPID`s after `setsid sleep` (M1); a race harness for B1 on Linux as root (swap the scratch
  directory between `create_dir(rw)` and `chown`); a server under the systemd unit with 600 held
  forwarded connections (M3).
