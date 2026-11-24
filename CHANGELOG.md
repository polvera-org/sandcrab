# Changelog

All notable changes to Sandcrab. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
- `sandcrab template pin <names>|--all`: adopt images already present as host-local pins, with a warning.
  `sandcrab_core::pins` (`PinStore`, `TemplatePin`, `effective_pin`), `SandcrabBuilder::template_pins_dir`,
  `SandboxManager::pin_store`.
- Phase 1 (V1): `crabd` guest tools over length-prefixed JSON-RPC 2.0, persistent workspaces, the Docker backend,
  the Firecracker backend (built; real-KVM validation pending), SDK, CLI, `crabagent`, and the benchmark harness.
