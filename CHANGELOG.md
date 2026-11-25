# Changelog

All notable changes to Sandcrab. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- libkrun: `network.policy = unrestricted` without TSI (design §9 Phase B). The helper attaches a virtio-net NIC
  (`krun_add_net_unixstream`, DHCP by `init.krun`) to a pinned userspace netstack (passt on Linux, gvproxy on macOS)
  through its own egress filter: host loopback (including gvproxy's `192.168.127.254` alias), the host's own addresses
  and on-link subnets, private / metadata / reserved ranges and SMTP are dropped; the gateway serves only DNS and DHCP.
  The netstack is SHA-256 pinned, verified before exec, runs as the user with no listening socket and exits with the
  helper. `sandcrab libkrun install` installs it (`--no-netstack` skips it), `doctor` reports `lk:netstack`, and
  `scripts/fetch-libkrun.sh` gains `--netstack-only` / `--no-netstack`. Without it, `unrestricted` stays
  `PolicyUnenforceable` (never a TSI fallback).

- crabagent: `--backend libkrun` (macOS Apple Silicon, Linux) and `--backend firecracker` (Linux), through the same
  `sandcrab-sdk` cargo features and `cfg` gating as the CLI (`docker`, `firecracker`, `libkrun`, all on by default;
  a backend not compiled in is a clear error, never a fallback). libkrun uses the CLI's config: host-dir workspaces
  over virtio-fs, `--network unrestricted` through the pinned userspace netstack (refused without it; the
  deprecated TSI opt-in is not offered). The session's backend line now reports the backend's declared capabilities
  (hardware isolation, multi-tenant safety, live host-dir mounts or copy-in) next to the `disk_mb` warning. New
  ignored test `libkrun_e2e` (fake LLM, live host-dir edit in a microVM).

### Deprecated
- libkrun TSI opt-in (`LibkrunConfig::allow_tsi_unrestricted`, CLI `--libkrun-tsi`): still explicit and still exposes
  host loopback; use the userspace netstack instead.

### Changed
- `KrunLibs` gains `netstack: Option<NetstackBin>`, `LibkrunConfig` gains `netstack_dir`, `HelperConfig` gains
  `netstack` (network `userspace`). The Linux libkrun build now also enables `NET=1`; the install script checks for
  `krun_add_net_unixstream`.

### Changed (breaking)
- Template image pins are **host-local** ([docs/template-pins.md](docs/template-pins.md)). Image IDs do not reproduce
  across hosts, so committed `sandcrab.toml` manifests no longer carry `sha256` / `sha256_by_arch`; a fresh host's
  build used to be refused until someone re-pinned it by hand. `templates/build.sh` now records each built image ID
  under `$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins/<name>/<version>/<arch>.json`
  (pinning is the default; `--no-pin` skips it, `--pin` is a no-op), and spawns verify against that pin. This is local
  integrity ("unchanged since this host built it"), not provenance. Reproducible template content digests are a planned
  follow-up. Hosts with existing images run `sandcrab template pin --all` once (or rebuild).
- `sandcrab_core::verify_template_image` / `verify_template_image_for` are replaced by
  `verify_image_pin(verifier, image, pin)`; the pin comes from `ResolvedSpec::image_sha256`, which the manager now
  resolves with `pins::effective_pin` (host-local pin, else legacy manifest pin). `ManagerConfig` gains
  `template_pins`.
- The libkrun backend refuses a template rootfs with no pin (it used to warn and continue).
- `sandcrab template list --json`: `sha256` → `pin { image_id, origin, path }`.
- The `DockerDev` backend is renamed to `Docker` and is now an officially supported backend (hardened, shared kernel;
  reports `hardware_isolated = false`, `multi_tenant_safe = false`). `BackendKind::DockerDev` → `BackendKind::Docker`,
  `DockerDevBackend` / `DockerDevConfig` → `DockerBackend` / `DockerConfig` (the old names remain as deprecated type
  aliases; `docker-dev` is still accepted in config, CLI and bench arguments).
- `SandboxError::InsecureBackendRejected` → `SandboxError::HardwareIsolationRequired`, raised when
  `require_hardware_isolation` is set and the selected backend cannot provide it.
- Removed `DockerConfig::allow_insecure_dev_backend` and `BackendConfig::Custom { allow_insecure }`. Docker no longer
  needs an opt-in; the backend is still selected explicitly and never chosen as a fallback.
- Container label `sandcrab.backend=docker-dev` → `sandcrab.backend=docker` (the reaper still cleans up both).

### Deprecated
- In-manifest `[template] sha256` / `sha256_by_arch`: still parsed (`TemplateManifest` stays backward compatible) and
  honored as a fallback when no host-local pin exists (a match passes, a mismatch is refused); `sandcrab doctor` warns.
- `--allow-insecure` and `SANDCRAB_ALLOW_INSECURE`: accepted, hidden from `--help`, print a warning, and do nothing.

### Added
- Docker backend on macOS (Docker Desktop): a `tcp` transport (`DockerTransport`, default on macOS, `unix` elsewhere,
  override with `SANDCRAB_DOCKER_TRANSPORT=unix|tcp`). `crabd` serves a Unix socket in a per-sandbox tmpfs volume and a
  hardened relay container (`crabd relay`) publishes it on `127.0.0.1` only, with an ephemeral port. The sandbox
  container's network is unchanged, so `disabled` is still network mode `none`. `sandcrab doctor` shows the transport;
  `reap_orphans` also removes relay containers and socket volumes. See
  [docs/hosting.md](docs/hosting.md#docker-on-macos-docker-desktop).
- Connection authentication (protocol **0.2**): `crabd serve --auth-token-file` requires an `auth` frame with a
  per-sandbox 256-bit token as the first frame of every connection (tools, reconnects, `tunnel.open`), compared in
  constant time; anything else gets `-32008 UNAUTHORIZED` and the connection is closed. The Docker backend always
  uses it (both transports); the token is passed as a 0400 file that `crabd` deletes, held in memory by
  `sandcrab_core::AuthConnector`, and redacted in `Debug`. See [docs/protocol.md](docs/protocol.md#authentication-auth).
- `sandcrab template pin <names>|--all`: adopt images already present as host-local pins, with a warning.
  `sandcrab_core::pins` (`PinStore`, `TemplatePin`, `effective_pin`), `SandcrabBuilder::template_pins_dir`,
  `SandboxManager::pin_store`.
- Phase 1 (V1): `crabd` guest tools over length-prefixed JSON-RPC 2.0, persistent workspaces, the Docker backend,
  the Firecracker backend (built; real-KVM validation pending), SDK, CLI, `crabagent`, and the benchmark harness.
