//! Template manifests (`templates/<name>/sandcrab.toml`) and the template registry.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::workspace::validate_slug;
use crate::{NetworkConfig, NetworkPolicy, Result, SandboxError, UserSpec};

/// File name of a template manifest inside its template directory.
pub const MANIFEST_FILE: &str = "sandcrab.toml";

/// Manifest parsed from `templates/<name>/sandcrab.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateManifest {
    pub template: TemplateMeta,
    #[serde(default)]
    pub resources: ResourceLimits,
    #[serde(default)]
    pub workspace: WorkspaceDefaults,
    #[serde(default)]
    pub network: TemplateNetwork,
    #[serde(default)]
    pub tools: ToolConfig,
}

impl TemplateManifest {
    /// Parse only; call [`TemplateManifest::validate`] before use.
    pub fn from_toml_str(input: &str) -> Result<Self> {
        toml::from_str(input).map_err(|e| SandboxError::Template(e.to_string()))
    }

    /// Parse and validate `<dir>/sandcrab.toml`; the manifest name must equal the dir name.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).map_err(|e| SandboxError::io_at(&path, e))?;
        let manifest = Self::from_toml_str(&text)
            .map_err(|e| SandboxError::Template(format!("{}: {e}", path.display())))?;
        manifest
            .validate()
            .map_err(|e| SandboxError::Template(format!("{}: {e}", path.display())))?;
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if manifest.template.name != dir_name {
            return Err(SandboxError::Template(format!(
                "{}: template name `{}` does not match its directory `{dir_name}`",
                path.display(),
                manifest.template.name
            )));
        }
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        let t = &self.template;
        validate_slug("template", &t.name).map_err(|e| SandboxError::Template(e.to_string()))?;
        validate_version(&t.version)?;
        match t.image.as_deref() {
            Some(img) if !img.trim().is_empty() && !img.chars().any(char::is_whitespace) => {}
            Some(img) => {
                return Err(SandboxError::Template(format!(
                    "template `{}`: invalid image reference `{img}`",
                    t.name
                )));
            }
            None => {
                return Err(SandboxError::Template(format!(
                    "template `{}`: `[template] image` is required",
                    t.name
                )));
            }
        }
        if let Some(sha) = &t.sha256 {
            validate_sha256(sha)?;
        }
        self.resources.validate()?;
        if !self.workspace.workdir.is_absolute() {
            return Err(SandboxError::Template(format!(
                "template `{}`: workspace.workdir `{}` must be absolute",
                t.name,
                self.workspace.workdir.display()
            )));
        }
        self.network
            .to_config()
            .validate()
            .map_err(|e| SandboxError::Template(format!("template `{}`: {e}", t.name)))
    }

    /// Image reference (validated to be present).
    #[must_use]
    pub fn image(&self) -> &str {
        self.template.image.as_deref().unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// OCI image reference. Required.
    #[serde(default)]
    pub image: Option<String>,
    /// Expected digest of the built image / rootfs artifact, `sha256:<64 hex>`.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Default guest user, `uid:gid`. `SandboxConfig.user` overrides it.
    #[serde(default)]
    pub user: Option<UserSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    #[serde(default = "default_disk_mb")]
    pub disk_mb: u64,
}

impl ResourceLimits {
    /// Lower bounds are what `crabd` + a shell need to boot.
    pub const MIN_MEMORY_MB: u64 = 128;
    pub const MIN_DISK_MB: u64 = 256;

    pub fn validate(&self) -> Result<()> {
        if self.vcpu == 0 || self.vcpu > 256 {
            return Err(SandboxError::InvalidConfig(format!(
                "vcpu must be in 1..=256, got {}",
                self.vcpu
            )));
        }
        if self.memory_mb < Self::MIN_MEMORY_MB {
            return Err(SandboxError::InvalidConfig(format!(
                "memory_mb must be at least {}, got {}",
                Self::MIN_MEMORY_MB,
                self.memory_mb
            )));
        }
        if self.disk_mb < Self::MIN_DISK_MB {
            return Err(SandboxError::InvalidConfig(format!(
                "disk_mb must be at least {}, got {}",
                Self::MIN_DISK_MB,
                self.disk_mb
            )));
        }
        Ok(())
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            vcpu: default_vcpu(),
            memory_mb: default_memory_mb(),
            disk_mb: default_disk_mb(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDefaults {
    #[serde(default = "default_workdir")]
    pub workdir: PathBuf,
}

impl Default for WorkspaceDefaults {
    fn default() -> Self {
        Self {
            workdir: default_workdir(),
        }
    }
}

/// `[network]` section of a manifest: the template's default when `SandboxConfig.network`
/// is unset. Accepts `disabled | egress_restricted | unrestricted`; defaults to `disabled`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TemplateNetwork {
    #[serde(default)]
    pub default_policy: NetworkPolicy,
    #[serde(default)]
    pub allow_domains: Vec<String>,
}

impl TemplateNetwork {
    #[must_use]
    pub fn to_config(&self) -> NetworkConfig {
        NetworkConfig {
            policy: self.default_policy,
            allow_domains: self.allow_domains.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolConfig {
    #[serde(default)]
    pub tiers: Vec<String>,
}

const fn default_vcpu() -> u32 {
    2
}

const fn default_memory_mb() -> u64 {
    1024
}

const fn default_disk_mb() -> u64 {
    4096
}

fn default_workdir() -> PathBuf {
    PathBuf::from("/workspace")
}

/// Semver-ish: `MAJOR.MINOR.PATCH` (numeric, no leading zeros) with optional
/// `-prerelease` and `+build` of `[0-9A-Za-z.-]`.
pub fn validate_version(v: &str) -> Result<()> {
    let bad = || SandboxError::Template(format!("version `{v}` is not MAJOR.MINOR.PATCH semver"));
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    let numeric_ok = |n: &&str| {
        !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && (n.len() == 1 || !n.starts_with('0'))
    };
    if nums.len() != 3 || !nums.iter().all(numeric_ok) {
        return Err(bad());
    }
    let ident_ok = |s: &str| {
        s.split('.')
            .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    };
    if pre.is_some_and(|p| !ident_ok(p)) || build.is_some_and(|b| !ident_ok(b)) {
        return Err(bad());
    }
    Ok(())
}

/// `sha256:` followed by 64 lowercase hex digits.
pub fn validate_sha256(digest: &str) -> Result<()> {
    let ok = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    if ok {
        Ok(())
    } else {
        Err(SandboxError::Template(format!(
            "digest `{digest}` must be `sha256:` + 64 lowercase hex digits"
        )))
    }
}

/// A validated template and the directory it was loaded from (build context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub manifest: TemplateManifest,
    pub dir: PathBuf,
}

impl Template {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.manifest.template.name
    }

    #[must_use]
    pub fn image(&self) -> &str {
        self.manifest.image()
    }
}

/// Validated templates loaded from one or more template directories.
#[derive(Debug, Clone, Default)]
pub struct TemplateRegistry {
    templates: BTreeMap<String, Template>,
}

impl TemplateRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `<dir>/<name>/sandcrab.toml` from `dirs`. Subdirectories without a manifest
    /// are skipped; an invalid manifest is an error. When several dirs define the same name,
    /// the **first** dir wins (put user overrides before the bundled templates).
    pub fn load<P: AsRef<Path>>(dirs: &[P]) -> Result<Self> {
        let mut reg = Self::new();
        for dir in dirs {
            let dir = dir.as_ref();
            let mut subdirs = Vec::new();
            for entry in fs::read_dir(dir).map_err(|e| SandboxError::io_at(dir, e))? {
                let entry = entry.map_err(|e| SandboxError::io_at(dir, e))?;
                let path = entry.path();
                if path.is_dir() && path.join(MANIFEST_FILE).is_file() {
                    subdirs.push(path);
                }
            }
            subdirs.sort();
            for sub in subdirs {
                let manifest = TemplateManifest::load_dir(&sub)?;
                reg.templates
                    .entry(manifest.template.name.clone())
                    .or_insert(Template { manifest, dir: sub });
            }
        }
        Ok(reg)
    }

    /// Add (or replace) a template after validating it.
    pub fn insert(&mut self, template: Template) -> Result<()> {
        template.manifest.validate()?;
        self.templates.insert(template.name().to_owned(), template);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&Template> {
        self.templates
            .get(name)
            .ok_or_else(|| SandboxError::not_found("template", name))
    }

    /// All templates, sorted by name.
    pub fn list(&self) -> impl Iterator<Item = &Template> {
        self.templates.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

/// Outcome of checking a template's pinned digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    /// The artifact digest equals the manifest's `sha256`.
    Verified { digest: String },
    /// The manifest has no `sha256`; `digest` is what is actually present.
    Unpinned { digest: String },
}

/// Backend hook resolving the digest of the artifact a template image reference points to
/// (Docker: image id; Firecracker: rootfs manifest sha). Core has no Docker/VMM deps; each
/// backend implements this and calls [`verify_template_image`] at spawn.
#[async_trait]
pub trait ImageVerifier: Send + Sync {
    /// Digest (`sha256:<hex>`) of the locally available artifact for `image`.
    async fn image_digest(&self, image: &str) -> Result<String>;
}

/// Compare the expected pin (if any) with the actual digest.
pub fn check_digest(image: &str, expected: Option<&str>, actual: &str) -> Result<PinStatus> {
    match expected {
        Some(expected) if expected == actual => Ok(PinStatus::Verified {
            digest: actual.to_owned(),
        }),
        Some(expected) => Err(SandboxError::DigestMismatch {
            image: image.to_owned(),
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        }),
        None => Ok(PinStatus::Unpinned {
            digest: actual.to_owned(),
        }),
    }
}

/// Resolve the template image digest through `verifier` and check it against the pin.
/// With `require_pin`, an unpinned template is rejected.
pub async fn verify_template_image(
    verifier: &dyn ImageVerifier,
    manifest: &TemplateManifest,
    require_pin: bool,
) -> Result<PinStatus> {
    let image = manifest.image();
    if require_pin && manifest.template.sha256.is_none() {
        return Err(SandboxError::Template(format!(
            "template `{}` has no `sha256` pin and pinning is required",
            manifest.template.name
        )));
    }
    let actual = verifier.image_digest(image).await?;
    check_digest(image, manifest.template.sha256.as_deref(), &actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[template]
name = "base"
version = "0.1.0"
description = "Minimal"
image = "sandcrab/base:0.1.0"

[resources]
vcpu = 1
memory_mb = 512
disk_mb = 2048

[workspace]
workdir = "/workspace"

[network]
default_policy = "egress_restricted"

[tools]
tiers = ["core", "process"]
"#;

    fn write_template(root: &Path, dir: &str, body: &str) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(MANIFEST_FILE), body).unwrap();
    }

    #[test]
    fn parses_current_manifest_shape() {
        let m = TemplateManifest::from_toml_str(BASE).unwrap();
        m.validate().unwrap();
        assert_eq!(m.network.default_policy, NetworkPolicy::EgressRestricted);
        assert_eq!(m.resources.vcpu, 1);
        assert_eq!(m.image(), "sandcrab/base:0.1.0");
    }

    #[test]
    fn network_section_defaults_to_disabled() {
        let m =
            TemplateManifest::from_toml_str("[template]\nname='x'\nversion='1.0.0'\nimage='x:1'\n")
                .unwrap();
        m.validate().unwrap();
        assert_eq!(m.network.default_policy, NetworkPolicy::Disabled);
        for p in ["disabled", "egress_restricted", "unrestricted"] {
            let m = TemplateManifest::from_toml_str(&format!(
                "[template]\nname='x'\nversion='1.0.0'\nimage='x:1'\n[network]\ndefault_policy='{p}'\n"
            ))
            .unwrap();
            assert_eq!(m.network.default_policy.as_str(), p);
        }
        assert!(
            TemplateManifest::from_toml_str(
                "[template]\nname='x'\nversion='1.0.0'\nimage='x:1'\n[network]\ndefault_policy='open'\n"
            )
            .is_err()
        );
    }

    #[test]
    fn validation_failures() {
        let cases = [
            ("version = \"0.1.0\"", "version = \"0.1\""),
            ("version = \"0.1.0\"", "version = \"01.1.0\""),
            ("image = \"sandcrab/base:0.1.0\"", ""),
            ("image = \"sandcrab/base:0.1.0\"", "image = \"\""),
            ("image = \"sandcrab/base:0.1.0\"", "image = \"a b\""),
            ("name = \"base\"", "name = \"Base\""),
            ("workdir = \"/workspace\"", "workdir = \"workspace\""),
            ("vcpu = 1", "vcpu = 0"),
            ("memory_mb = 512", "memory_mb = 16"),
            (
                "description = \"Minimal\"",
                "description = \"Minimal\"\nsha256 = \"sha256:abc\"",
            ),
            (
                "default_policy = \"egress_restricted\"",
                "default_policy = \"unrestricted\"\nallow_domains = [\"a.com\"]",
            ),
        ];
        for (from, to) in cases {
            let body = BASE.replace(from, to);
            let m = TemplateManifest::from_toml_str(&body);
            assert!(
                m.and_then(|m| m.validate()).is_err(),
                "expected failure for {to:?}"
            );
        }
    }

    #[test]
    fn versions_and_digests() {
        for ok in [
            "0.1.0",
            "1.2.3",
            "10.0.0-rc.1",
            "1.0.0+build.5",
            "1.0.0-alpha-1+x",
        ] {
            validate_version(ok).unwrap();
        }
        for bad in [
            "1.0", "1.0.0.0", "a.b.c", "1.0.0-", "1.0.0+", "1..0", "v1.0.0",
        ] {
            assert!(validate_version(bad).is_err(), "{bad}");
        }
        validate_sha256(&format!("sha256:{}", "a".repeat(64))).unwrap();
        for bad in [
            "a".repeat(64),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha512:{}", "a".repeat(64)),
        ] {
            assert!(validate_sha256(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registry_loads_and_validates_names() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_template(&a, "base", BASE);
        write_template(
            &b,
            "base",
            &BASE.replace("description = \"Minimal\"", "description = \"Shadowed\""),
        );
        write_template(
            &b,
            "python-dev",
            &BASE.replace("\"base\"", "\"python-dev\""),
        );
        fs::create_dir_all(b.join("not-a-template")).unwrap();

        let reg = TemplateRegistry::load(&[&a, &b]).unwrap();
        assert_eq!(
            reg.list().map(Template::name).collect::<Vec<_>>(),
            ["base", "python-dev"]
        );
        assert_eq!(
            reg.get("base").unwrap().manifest.template.description,
            "Minimal"
        );
        assert_eq!(reg.get("base").unwrap().dir, a.join("base"));
        assert!(matches!(
            reg.get("nope"),
            Err(SandboxError::NotFound { .. })
        ));

        // Name must match its directory.
        write_template(&b, "renamed", BASE);
        assert!(matches!(
            TemplateRegistry::load(&[&b]),
            Err(SandboxError::Template(_))
        ));
    }

    #[test]
    fn loads_repo_templates() {
        let repo_templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
        let reg = TemplateRegistry::load(&[repo_templates]).unwrap();
        assert!(reg.get("base").is_ok());
        assert!(reg.len() >= 4);
    }

    struct FixedDigest(&'static str);

    #[async_trait]
    impl ImageVerifier for FixedDigest {
        async fn image_digest(&self, _image: &str) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    #[tokio::test]
    async fn image_pin_check() {
        let good = format!("sha256:{}", "a".repeat(64));
        let bad = format!("sha256:{}", "b".repeat(64));
        let good_static: &'static str = Box::leak(good.clone().into_boxed_str());
        let mut m = TemplateManifest::from_toml_str(BASE).unwrap();

        assert_eq!(
            verify_template_image(&FixedDigest(good_static), &m, false)
                .await
                .unwrap(),
            PinStatus::Unpinned {
                digest: good.clone()
            }
        );
        assert!(
            verify_template_image(&FixedDigest(good_static), &m, true)
                .await
                .is_err()
        );
        m.template.sha256 = Some(good.clone());
        assert_eq!(
            verify_template_image(&FixedDigest(good_static), &m, true)
                .await
                .unwrap(),
            PinStatus::Verified { digest: good }
        );
        m.template.sha256 = Some(bad);
        assert!(matches!(
            verify_template_image(&FixedDigest(good_static), &m, false).await,
            Err(SandboxError::DigestMismatch { .. })
        ));
    }
}
