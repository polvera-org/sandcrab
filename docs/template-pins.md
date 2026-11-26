# Template image pins

Sandcrab refuses to start a sandbox from a template image it has not pinned. This page says
what a pin is, what it does and does not prove, and how to create one.

## What is pinned where

| Input | Pinned how | Where |
| --- | --- | --- |
| Template base images (`FROM debian:…@sha256:…`, `node`, `uv`, `bun`) | OCI digest, reproducible | Committed `templates/*/Dockerfile` |
| Built template images (`sandcrab/<name>:<version>`) | Local Docker image ID, **per host** | `~/.sandcrab/template-pins/` (not committed) |
| microVM rootfs, Firecracker kernel and binary, libkrun libraries, `crabd` | SHA-256 | Rootfs cache manifests, compiled-in artifact pins |
| The built rootfs image, libkrun/libkrunfw, the netstack, the libkrun helper | SHA-256 + stat stamp, checked per spawn through the fd that is then used | Rootfs cache `manifest.json` (`rootfs_stamp`), artifact `manifest.json` (`stamp`), `<helper>.pin.json` |

A cached rootfs image is `0444`, and its manifest records its SHA-256 next to a stat
stamp (device, inode, size, mtime, ctime). Each spawn opens the image once, compares
`fstat` of that fd with the stamp, re-hashes through the same fd only on a mismatch, and
hands the VMM that fd, never the path. ctime is the part a same-user attacker cannot
forge: see [the libkrun design, §12.4](design/libkrun-backend.md#124-pins-and-verification-step-lk-verified-fd-2026-11-25).

The committed `sandcrab.toml` manifests carry **no** image IDs. A local image ID does not
reproduce across hosts: apt packages are not version-pinned, layer timestamps differ
between builds, and Docker's classic and containerd image stores compute IDs differently.
A committed ID therefore only ever matched the host that built it, and every other host
refused its own fresh build.

## What a pin means: local integrity, not provenance

A host-local pin says "this image is unchanged since this host built (or adopted) it". It
catches a re-tagged, rebuilt or swapped local image. It does **not** say who built the image
or that it matches anyone else's build. Anyone who can write the pin store or the Docker
daemon can change both, so a pin is not a defense against a compromised host.

## Pin store

One JSON file per template, version and arch, written atomically:

```text
$SANDCRAB_TEMPLATE_PINS/<template>/<version>/<arch>.json
  default: ${SANDCRAB_HOME:-~/.sandcrab}/template-pins/…
```

```json
{
  "schema": 1,
  "template": "base",
  "version": "0.1.0",
  "arch": "arm64",
  "image": "sandcrab/base:0.1.0-arm64",
  "image_id": "sha256:…",
  "source": "build",
  "recorded_at": "2026-11-24T12:00:00Z"
}
```

`source` is `build` (written by `templates/build.sh`) or `adopt` (written by
`sandcrab template pin`, the only way to pin an [external image](#external-images)). The store is host-wide because it describes the host's image
store: `--state-dir` does not move it. Use `SANDCRAB_TEMPLATE_PINS`, or
`SandcrabBuilder::template_pins_dir` in the SDK, to point somewhere else. A pin file that
exists but is unreadable, malformed or for another key fails the spawn. It is never
treated as "unpinned".

## Creating pins

```sh
templates/build.sh                     # build all four templates and pin them on this host
templates/build.sh base ts-dev         # a subset
sandcrab template build base           # same, through the CLI
sandcrab template pin base             # adopt the image already present, without rebuilding
sandcrab template list                 # PINNED column: host-local / legacy-manifest / no
sandcrab doctor                        # checks each template image against this host's pin
```

`sandcrab template pin` is for hosts that already have images, for example built before
host-local pins existed. It records the image's current ID and prints a warning: it trusts
whatever is tagged now. Prefer a rebuild when you are unsure what the image contains.

`templates/build.sh --no-pin` builds without recording pins. `--pin` is still accepted and
does nothing, because pinning is now the default.

## At spawn

The manager resolves the pin for the host arch: first the host-local pin, then a deprecated
in-manifest pin (see below). The backend then checks it:

- **Docker:** the image's local ID must equal the pin. With no pin, the spawn fails with
  `UnpinnedImage` unless `--allow-unpinned` / `DockerConfig::allow_unpinned_images` is set.
- **libkrun:** the same check runs in the rootfs pipeline. With no pin, the spawn fails.
  A configured prebuilt rootfs image skips the image check.
- **Firecracker:** the same check runs in the rootfs pipeline. With no pin, the spawn fails,
  as on libkrun.

The errors look like this:

```text
UnpinnedImage: template `base` 0.1.0 has no host-local pin for arm64; image
`sandcrab/base:0.1.0` (local image sha256:…) is not trusted until this host pins it.
Build and pin it with `templates/build.sh base` or `sandcrab template build base`, or
adopt the image already present with `sandcrab template pin base`. Pins are a local
integrity check (unchanged since this host built it), not provenance

digest mismatch for `sandcrab/base:0.1.0`: expected sha256:…, got sha256:… (the local
image changed since this host pinned it; rebuild and re-pin with `templates/build.sh
base` (or `sandcrab template build base`), or adopt the current image with `sandcrab
template pin base`)
```

## External images

A template can name an image somebody else built (bring your own image). Its directory holds
only `sandcrab.toml`, with `external = true`:

```toml
[template]
name = "myapp"
version = "1.4.0"
image = "registry.example/myapp:1.4.0"   # a digest reference (`name@sha256:…`) also works
external = true                          # no Dockerfile; never built by Sandcrab

[tools]
tiers = ["core"]                         # leave out "process" when the image has no bash
```

An external template with a `Dockerfile` next to it, or a non-external one without, fails to
load. `templates/build.sh` and `sandcrab template build` skip or refuse external templates.

Pin it the same way as a built template: pull the image, then adopt it.

```sh
docker pull registry.example/myapp:1.4.0
sandcrab template pin myapp      # records its local image ID, `source: "adopt"`
```

The pin means what it means for a built template: the local image is unchanged since this
host adopted it. It says nothing about who built the image or what is in it, so pin only an
image you trust, ideally pulled by digest. Sandcrab never pulls at spawn and never trusts an
unpinned image: the spawn fails with `UnpinnedImage` and the two commands above. After a
`docker pull` moves the tag, spawns fail with a digest mismatch until you pin again.

What the image must provide:

| Requirement | Why |
| --- | --- |
| Linux for the host's architecture (`linux/amd64` or `linux/arm64`) | Guests run natively; the rootfs pipeline exports that platform and fails if the image lacks it |
| Nothing else for the file tools | `crabd` is a static (musl) binary that Sandcrab injects; it needs no libc, shell or user database from the image |
| `bash` on `PATH`, only for the `bash` and `process_*` tools | They run `bash -c` |
| The template's `workdir` (default `/workspace`) exists, or is a mount target | It is the default cwd. The microVM rootfs creates `/workspace`; Docker creates a missing one owned by root |
| A writable `HOME` if its programs need one | The sandbox runs as the host uid:gid (or the configured user), not the image's `USER` |
| No reliance on `ENTRYPOINT`, `CMD`, `USER` or an init system | None of them is used: `crabd` is the entrypoint (Docker) or PID 1 (microVMs) |

The microVM rootfs pipeline (libkrun, Firecracker) exports the image's files without running
it, so an image with no shell and a non-root `USER` works there too.

## Legacy in-manifest pins

`[template] sha256` and `sha256_by_arch` still parse, so third-party templates that set them
keep working. They are only used when no host-local pin exists for the arch: a match passes
and a mismatch is refused. A host-local pin always wins. `sandcrab doctor` warns when a
template relies on a legacy pin. The repository's own manifests no longer set them, and a
test keeps it that way.

## Follow-up: reproducible template digests

The planned follow-up is a digest of the template's built *content* (for example, the
rootfs file tree, normalized for timestamps and ownership) that reproduces across hosts.
Committed manifests could then pin real provenance again. That needs pinned apt package
versions and a normalized export, and is not built yet.
