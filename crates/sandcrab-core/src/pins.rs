//! Host-local template image pins.
//!
//! A pin records the image ID this host built (or explicitly adopted) for one
//! `template × version × arch`. Spawns verify the local image against it. This is **local
//! integrity** ("the image is unchanged since this host built or adopted it"), not provenance:
//! template images are not reproducible across hosts yet (unpinned apt packages, layer
//! timestamps, classic vs containerd image stores), so committed manifests carry no image IDs.
//! Reproducible template content digests are a planned follow-up.
//!
//! Layout (one small JSON file per pin, written atomically; `templates/build.sh` writes the
//! same files from bash):
//!
//! ```text
//! <pins dir>/<template>/<version>/<arch>.json
//! {"schema": 1, "template": "base", "version": "0.1.0", "arch": "arm64",
//!  "image": "sandcrab/base:0.1.0-arm64", "image_id": "sha256:…",
//!  "source": "build", "recorded_at": "2026-11-24T12:00:00Z"}
//! ```
//!
//! The pins dir is host-wide (images live in the host's Docker store, not in a state dir):
//! `$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::template::{ImageArch, TemplateManifest, validate_sha256, validate_version};
use crate::workspace::validate_slug;
use crate::{Result, SandboxError, default_state_root};

/// Env var overriding the pins directory.
pub const PINS_DIR_ENV: &str = "SANDCRAB_TEMPLATE_PINS";
/// Pins directory name under the state root.
pub const PINS_DIR_NAME: &str = "template-pins";
/// Current pin file schema.
pub const PIN_SCHEMA: u32 = 1;

/// How a pin was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinSource {
    /// `templates/build.sh` / `sandcrab template build` after building the image.
    Build,
    /// `sandcrab template pin`: an image already present was adopted as-is.
    Adopt,
}

/// One host-local pin file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplatePin {
    pub schema: u32,
    pub template: String,
    pub version: String,
    pub arch: ImageArch,
    /// Image reference that was inspected when the pin was recorded (informational).
    pub image: String,
    /// Local image ID (`docker image inspect --format {{.Id}}`), `sha256:<64 hex>`.
    pub image_id: String,
    pub source: PinSource,
    pub recorded_at: DateTime<Utc>,
}

impl TemplatePin {
    #[must_use]
    pub fn new(
        manifest: &TemplateManifest,
        arch: ImageArch,
        image: impl Into<String>,
        image_id: impl Into<String>,
        source: PinSource,
    ) -> Self {
        Self {
            schema: PIN_SCHEMA,
            template: manifest.template.name.clone(),
            version: manifest.template.version.clone(),
            arch,
            image: image.into(),
            image_id: image_id.into(),
            source,
            recorded_at: Utc::now(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema != PIN_SCHEMA {
            return Err(SandboxError::Template(format!(
                "unsupported pin schema {} (this build reads {PIN_SCHEMA})",
                self.schema
            )));
        }
        validate_key(&self.template, &self.version)?;
        validate_sha256(&self.image_id)
    }
}

fn validate_key(template: &str, version: &str) -> Result<()> {
    // Both become path components: slug + semver keep them traversal-free.
    validate_slug("template", template).map_err(|e| SandboxError::Template(e.to_string()))?;
    validate_version(version)
}

/// The host-local pin store (a directory of pin files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinStore {
    root: PathBuf,
}

impl PinStore {
    /// Store rooted at `dir` (created on the first [`PinStore::record`]).
    #[must_use]
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { root: dir.into() }
    }

    /// `$SANDCRAB_TEMPLATE_PINS`, else `<default state root>/template-pins`.
    pub fn open_default() -> Result<Self> {
        Ok(Self::open(default_pins_dir()?))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<template>/<version>/<arch>.json`.
    pub fn pin_path(&self, template: &str, version: &str, arch: ImageArch) -> Result<PathBuf> {
        validate_key(template, version)?;
        Ok(self
            .root
            .join(template)
            .join(version)
            .join(format!("{arch}.json")))
    }

    /// The pin for `manifest` (its name + version) on `arch`, if recorded. A pin file that
    /// exists but is unreadable, malformed or for another key is an error, never "unpinned".
    pub fn get(&self, manifest: &TemplateManifest, arch: ImageArch) -> Result<Option<TemplatePin>> {
        let t = &manifest.template;
        let path = self.pin_path(&t.name, &t.version, arch)?;
        let text = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SandboxError::io_at(&path, e)),
        };
        let bad = |why: String| {
            SandboxError::Template(format!(
                "corrupt template pin {}: {why}; re-pin with `sandcrab template build {}` \
                 or `sandcrab template pin {}`",
                path.display(),
                t.name,
                t.name
            ))
        };
        let pin: TemplatePin = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
        pin.validate().map_err(|e| bad(e.to_string()))?;
        if pin.template != t.name || pin.version != t.version || pin.arch != arch {
            return Err(bad(format!(
                "it is for {}@{} {}",
                pin.template, pin.version, pin.arch
            )));
        }
        Ok(Some(pin))
    }

    /// Write `pin` atomically (temp file + rename); returns its path.
    pub fn record(&self, pin: &TemplatePin) -> Result<PathBuf> {
        pin.validate()?;
        let path = self.pin_path(&pin.template, &pin.version, pin.arch)?;
        let dir = path.parent().expect("pin path has a parent");
        create_private_dirs(dir)?;
        let tmp = dir.join(format!(".{}.json.{}.tmp", pin.arch, std::process::id()));
        let mut body = serde_json::to_vec_pretty(pin)
            .map_err(|e| SandboxError::Template(format!("serialize pin: {e}")))?;
        body.push(b'\n');
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        };
        write().map_err(|e| {
            let _ = fs::remove_file(&tmp);
            SandboxError::io_at(&path, e)
        })?;
        Ok(path)
    }
}

fn create_private_dirs(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| SandboxError::io_at(dir, e))
}

/// `$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins`.
pub fn default_pins_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(PINS_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    Ok(default_state_root()?.join(PINS_DIR_NAME))
}

/// Where an [`EffectivePin`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOrigin {
    /// A host-local pin file.
    HostLocal(PathBuf),
    /// Deprecated in-manifest `sha256` / `sha256_by_arch` (only used when no host-local pin
    /// exists for the arch).
    LegacyManifest,
}

/// The pin a spawn verifies against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePin {
    pub digest: String,
    pub origin: PinOrigin,
}

/// Resolve the pin for `manifest` on `arch`: the host-local pin in `store`, else a legacy
/// in-manifest pin, else `None` (unpinned). With `arch = None` (a host arch without an OCI
/// name) only the legacy single-arch `sha256` can apply.
pub fn effective_pin(
    store: Option<&PinStore>,
    manifest: &TemplateManifest,
    arch: Option<ImageArch>,
) -> Result<Option<EffectivePin>> {
    if let (Some(store), Some(arch)) = (store, arch)
        && let Some(pin) = store.get(manifest, arch)?
    {
        let origin = PinOrigin::HostLocal(store.pin_path(&pin.template, &pin.version, arch)?);
        return Ok(Some(EffectivePin {
            digest: pin.image_id,
            origin,
        }));
    }
    Ok(manifest.template.pinned_sha256(arch).map(|d| EffectivePin {
        digest: d.to_owned(),
        origin: PinOrigin::LegacyManifest,
    }))
}

/// Refusal for a template with no pin for `arch` on this host. Backends that require a pin
/// return this so every backend names the same fix.
#[must_use]
pub fn unpinned_error(
    manifest: &TemplateManifest,
    arch: Option<ImageArch>,
    actual: Option<&str>,
) -> SandboxError {
    SandboxError::Template(unpinned_message(manifest, arch, actual))
}

/// The text of [`unpinned_error`].
#[must_use]
pub fn unpinned_message(
    manifest: &TemplateManifest,
    arch: Option<ImageArch>,
    actual: Option<&str>,
) -> String {
    let t = &manifest.template;
    let arch = arch.map_or_else(|| "this host's arch".to_owned(), |a| a.to_string());
    let local = actual.map_or_else(String::new, |d| format!(" (local image {d})"));
    format!(
        "UnpinnedImage: template `{name}` {version} has no host-local pin for {arch}; image \
         `{image}`{local} is not trusted until this host pins it. Build and pin it with \
         `templates/build.sh {name}` or `sandcrab template build {name}`, or adopt the image \
         already present with `sandcrab template pin {name}`. Pins are a local integrity \
         check (unchanged since this host built it), not provenance",
        name = t.name,
        version = t.version,
        image = manifest.image(),
    )
}

/// Hint appended to a pin mismatch.
#[must_use]
pub fn mismatch_hint(template: &str) -> String {
    format!(
        "the local image changed since this host pinned it; rebuild and re-pin with \
         `templates/build.sh {template}` (or `sandcrab template build {template}`), or adopt \
         the current image with `sandcrab template pin {template}`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str =
        "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n";

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn manifest(extra: &str) -> TemplateManifest {
        let m = TemplateManifest::from_toml_str(&format!("{BASE}{extra}")).unwrap();
        m.validate().unwrap();
        m
    }

    #[test]
    fn record_get_round_trip_and_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PinStore::open(tmp.path().join("pins"));
        let m = manifest("");
        assert_eq!(store.get(&m, ImageArch::Arm64).unwrap(), None);

        let pin = TemplatePin::new(
            &m,
            ImageArch::Arm64,
            "sandcrab/base:0.1.0-arm64",
            digest('a'),
            PinSource::Adopt,
        );
        let path = store.record(&pin).unwrap();
        assert_eq!(path, tmp.path().join("pins/base/0.1.0/arm64.json"));
        assert_eq!(store.get(&m, ImageArch::Arm64).unwrap(), Some(pin.clone()));
        assert_eq!(store.get(&m, ImageArch::Amd64).unwrap(), None);
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["source"], "adopt");
        assert_eq!(v["arch"], "arm64");

        // Another version of the template is a different key.
        let m2 = TemplateManifest::from_toml_str(&BASE.replace("0.1.0\"\nimage", "0.2.0\"\nimage"))
            .unwrap();
        assert_eq!(store.get(&m2, ImageArch::Arm64).unwrap(), None);
    }

    #[test]
    fn bash_written_pin_parses() {
        // The exact shape templates/build.sh writes.
        let tmp = tempfile::tempdir().unwrap();
        let store = PinStore::open(tmp.path());
        let dir = tmp.path().join("base/0.1.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("amd64.json"),
            format!(
                "{{\n  \"schema\": 1,\n  \"template\": \"base\",\n  \"version\": \"0.1.0\",\n  \
                 \"arch\": \"amd64\",\n  \"image\": \"sandcrab/base:0.1.0-amd64\",\n  \
                 \"image_id\": \"{}\",\n  \"source\": \"build\",\n  \
                 \"recorded_at\": \"2026-11-24T12:00:00Z\"\n}}\n",
                digest('b')
            ),
        )
        .unwrap();
        let pin = store.get(&manifest(""), ImageArch::Amd64).unwrap().unwrap();
        assert_eq!(pin.image_id, digest('b'));
        assert_eq!(pin.source, PinSource::Build);
    }

    #[test]
    fn corrupt_or_foreign_pin_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PinStore::open(tmp.path());
        let m = manifest("");
        let path = store.pin_path("base", "0.1.0", ImageArch::Arm64).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for body in [
            "not json".to_owned(),
            serde_json::to_string(&TemplatePin {
                image_id: "sha256:abc".into(),
                ..TemplatePin::new(&m, ImageArch::Arm64, "x", digest('a'), PinSource::Build)
            })
            .unwrap(),
            serde_json::to_string(&TemplatePin::new(
                &m,
                ImageArch::Amd64,
                "x",
                digest('a'),
                PinSource::Build,
            ))
            .unwrap(),
            serde_json::to_string(&TemplatePin {
                schema: 2,
                ..TemplatePin::new(&m, ImageArch::Arm64, "x", digest('a'), PinSource::Build)
            })
            .unwrap(),
        ] {
            fs::write(&path, &body).unwrap();
            let err = store.get(&m, ImageArch::Arm64).unwrap_err().to_string();
            assert!(err.contains("corrupt template pin"), "{body}: {err}");
        }
    }

    #[test]
    fn keys_are_path_safe() {
        let store = PinStore::open("/x");
        assert!(store.pin_path("../etc", "0.1.0", ImageArch::Amd64).is_err());
        assert!(store.pin_path("base", "../../x", ImageArch::Amd64).is_err());
    }

    #[test]
    fn host_local_pin_wins_over_legacy_manifest_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PinStore::open(tmp.path());
        let legacy = manifest(&format!("sha256 = \"{}\"\n", digest('a')));
        // Legacy only.
        let p = effective_pin(Some(&store), &legacy, Some(ImageArch::Amd64))
            .unwrap()
            .unwrap();
        assert_eq!(
            (p.digest.as_str(), p.origin),
            (digest('a').as_str(), PinOrigin::LegacyManifest)
        );
        // Host-local wins.
        store
            .record(&TemplatePin::new(
                &legacy,
                ImageArch::Amd64,
                "i",
                digest('c'),
                PinSource::Build,
            ))
            .unwrap();
        let p = effective_pin(Some(&store), &legacy, Some(ImageArch::Amd64))
            .unwrap()
            .unwrap();
        assert_eq!(p.digest, digest('c'));
        assert!(matches!(p.origin, PinOrigin::HostLocal(_)));
        // Nothing at all.
        let bare = manifest("");
        assert_eq!(
            effective_pin(Some(&store), &bare, Some(ImageArch::Arm64)).unwrap(),
            None
        );
        assert_eq!(
            effective_pin(None, &bare, Some(ImageArch::Amd64)).unwrap(),
            None
        );
        // Unknown host arch: only the legacy single-arch pin can apply.
        assert_eq!(effective_pin(Some(&store), &bare, None).unwrap(), None);
    }

    #[test]
    fn unpinned_error_names_every_fix() {
        let e = unpinned_error(&manifest(""), Some(ImageArch::Arm64), Some("sha256:x")).to_string();
        for needle in [
            "UnpinnedImage",
            "no host-local pin for arm64",
            "templates/build.sh base",
            "sandcrab template build base",
            "sandcrab template pin base",
            "not provenance",
        ] {
            assert!(e.contains(needle), "{needle}: {e}");
        }
    }
}
