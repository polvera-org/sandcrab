# Changelog

All notable changes to Sandcrab. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed (breaking)
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
- `--allow-insecure` and `SANDCRAB_ALLOW_INSECURE`: accepted, hidden from `--help`, print a warning, and do nothing.

### Added
- Phase 1 (V1): `crabd` guest tools over length-prefixed JSON-RPC 2.0, persistent workspaces, the Docker backend,
  the Firecracker backend (built; real-KVM validation pending), SDK, CLI, `crabagent`, and the benchmark harness.
