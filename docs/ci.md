# CI and releases

Three GitHub Actions workflows live in [`.github/workflows/`](../.github/workflows/). All of
them run on GitHub-hosted runners only.

| Workflow | File | Runs on | What it does |
| --- | --- | --- | --- |
| CI | `ci.yml` | every PR, push to `main`, manual | fmt, clippy, unit tests, static `crabd` build |
| release | `release.yml` | `v*` tag push, manual | per-target tarballs and a **draft** GitHub release |
| E2E | `e2e.yml` | nightly 03:17 UTC, manual | Docker and Firecracker end-to-end suites |

## CI (`ci.yml`)

Triggers: every pull request, every push to `main`, and `workflow_dispatch`. A newer push to a
PR cancels the older run; pushes to `main` always finish.

Jobs:

1. **`changes`** decides whether the Rust jobs run. A PR that touches only `docs/` or `*.md`
   files skips them. Pushes and manual runs always run them. If the diff cannot be computed,
   everything runs.
2. **`fmt`**: `cargo fmt --all -- --check` on `ubuntu-24.04`.
3. **`check`**: `cargo clippy --workspace --all-targets --locked -- -D warnings` and
   `cargo test --workspace --locked` on `ubuntu-24.04` (x86_64), `ubuntu-24.04-arm` (arm64) and
   `macos-26` (Apple Silicon). Linux also installs `e2fsprogs` and `fakeroot`, so the rootfs and
   fake-VMM tests run instead of skipping. Only non-ignored tests run here.
4. **`crabd-musl`**: `scripts/build-crabd.sh` for x86_64 and aarch64; the static `crabd` and its
   `.sha256` are uploaded as workflow artifacts (kept 3 days).
5. **`ci-ok`** always reports. It fails if any job above failed or was cancelled; skipped jobs
   (docs-only PRs) count as success.

**Branch protection: require only `ci-ok`.** It reports on every PR, including docs-only ones,
so a required check never sits in "expected" forever. Do not add `paths-ignore` to the workflow
for the same reason.

**`docs/bench/results/` counts as code.** `sandcrab-bench` compiles files from there with
`include_str!`, so a PR that changes them runs the full matrix. If you add another
`include_str!` / `include_bytes!` path outside `crates/`, add it to the `changes` job too.

The Rust cache (`Swatinem/rust-cache`) is saved only from `main`; PRs restore it but never
write it.

## Releases (`release.yml`)

**Tag push (`v*`).** Each target is built natively on its own runner:

| Target | Runner | Contents |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-26` | `sandcrab`, `crabagent`, `sandcrab-krun` (ad-hoc signed), `crabd` (aarch64 musl) |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `sandcrab`, `crabagent`, `sandcrab-krun`, `crabd` (x86_64 musl) |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `sandcrab`, `crabagent`, `sandcrab-krun`, `crabd` (aarch64 musl) |

Each archive is `sandcrab-<version>-<target>.tar.gz` with a `.sha256` next to it. It also holds
`crabd.sha256`, `LICENSE`, `README.md`, a generated `INSTALL.md`, `templates/`,
`scripts/fetch-libkrun.sh` and, on Linux, `scripts/fc-net-setup.sh`.

The `publish` job then checks every `.sha256`, writes a combined `SHA256SUMS` and creates a
**draft** release with generated notes. The tag must match the workspace version (`v0.1.0` ↔
`0.1.0`), or the build fails.

To cut a release:

1. Bump the workspace version in `Cargo.toml`, update `CHANGELOG.md`, merge to `main`.
2. Tag and push: `git tag v0.1.0 && git push origin v0.1.0`.
3. Wait for the workflow, then open the draft on GitHub, check the assets and notes, and
   publish it by hand.

Details:

- A tag with a `-` in it (`v0.2.0-rc.1`) makes a **prerelease**.
- Re-running the workflow updates a draft's assets. It refuses to touch a release that is
  already published.
- **`workflow_dispatch`** builds the archives and uploads them as workflow artifacts (kept 14
  days). It never creates a release.
- **No Rust cache in release builds.** A tag run can restore caches saved by `main`, which would
  let a poisoned cache reach shipped binaries. Release binaries are always built clean.

**Build an archive locally** (native only: the host triple must match the target):

```sh
scripts/package-release.sh aarch64-apple-darwin --dry-run   # print the plan, build nothing
scripts/package-release.sh aarch64-apple-darwin             # -> target/dist/
scripts/package-release.sh x86_64-unknown-linux-gnu --out dist --expect-version 0.1.0
```

It needs the rustup targets `<target>` and `<arch>-unknown-linux-musl`, and `musl-tools` on
Linux. `CARGO_TARGET_DIR` and `SANDCRAB_SIGN_IDENTITY` are honoured.

## End-to-end tests (`e2e.yml`)

Triggers: nightly at 03:17 UTC, and `workflow_dispatch` with a `backend` input (`all`, `docker`,
`firecracker`). The schedule runs every suite. The workflow has no secrets; the agent test uses
a scripted fake LLM.

- **Docker e2e** (`ubuntu-24.04`): builds a static `crabd` and the `base`, `python-dev` and
  `ts-dev` templates, then runs the `#[ignore]`d suites of `sandcrab-backend-docker`,
  `sandcrab-sdk`, `sandcrab-cli`, `sandcrab-rootfs` and `sandcrab-agent`.
- **Firecracker e2e** (`ubuntu-24.04`, x86_64 only): enables `/dev/kvm`, fetches the pinned
  kernel and `firecracker`, sets up the tap pool with `scripts/fc-net-setup.sh`, then runs
  `firecracker_kvm`, the SDK suite with `SANDCRAB_E2E_BACKEND=firecracker`, and the `crabd`
  vsock loopback test (skipped with a notice if `vsock_loopback` cannot be loaded).

KVM on standard hosted Linux runners works today but is not documented by GitHub. If it goes
away, the first step fails fast with a "No KVM" error instead of failing deep inside a test.

Each suite runs even if an earlier one failed, as long as its prerequisites built.

**libkrun e2e is a local run.** Hosted macOS runners have no nested virtualization, so there are
no Hypervisor.framework guests in CI. Run it on an Apple Silicon Mac set up as in
[hosting.md §2](hosting.md#2-macos-apple-silicon):

```sh
cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
sandcrab libkrun install --helper target/release/sandcrab-krun
sandcrab template pin base ts-dev
scripts/build-crabd.sh
export PATH="$(brew --prefix e2fsprogs)/sbin:$PATH"
cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
SANDCRAB_E2E_BACKEND=libkrun cargo test -p sandcrab-sdk --features libkrun -- --ignored --test-threads=2
cargo test -p sandcrab-agent --test libkrun_e2e -- --ignored
```

On a Linux KVM host, see [hosting.md §3.4](hosting.md#34-libkrun-on-linux).

GitHub disables scheduled workflows after 60 days without repository activity. If the nightly
run stops, re-enable it from the Actions tab.

## Runners and cost

Every job runs on GitHub-hosted standard runners, including `ubuntu-24.04-arm` and `macos-26`.
These are free for public repositories.

There are no self-hosted runners, on purpose: a pull request from a fork would run its code on
our machines.

If CI speed ever matters, these are drop-in paid options (change `runs-on`):

- **Blacksmith**: 3,000 free minutes a month, KVM on x86_64.
- **WarpBuild**: KVM through a runner label.
- **Ubicloud**: the cheapest Linux runners.

BuildJet shut down in March 2026.

## Platform notes

- **Linux.** Release binaries are built on Ubuntu 24.04 and need glibc 2.39 or newer (Ubuntu
  24.04+, Debian 13+). `crabd` is static and runs in any Linux guest.
- **macOS.** Binaries are ad-hoc signed, not notarized. If you downloaded the archive with a
  browser, clear the quarantine flag in the extracted directory:
  `xattr -dr com.apple.quarantine .`. Developer ID signing and notarization are planned for the
  official launch (review item R10; see [hosting.md §5](hosting.md#5-release-signing-macos)).
- **Windows.** Not supported natively. Use WSL2 with Ubuntu and the Linux x86_64 archive. Use the
  Docker backend, with Docker Desktop's WSL integration or Docker Engine installed inside WSL.
