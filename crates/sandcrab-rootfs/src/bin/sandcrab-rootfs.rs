//! `sandcrab-rootfs`: operator tool for the microVM rootfs pipeline (Firecracker, libkrun).
//! Wrapped by `scripts/build-rootfs.sh`. Pinned Firecracker kernel/binaries are fetched by
//! `sandcrab-fc-fetch` (`scripts/fetch-artifacts.sh`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcrab_rootfs::{
    self as rootfs, Arch, MkfsStrategy, RootfsBuilder, RootfsRequest, default_cache_root,
};

#[derive(Parser)]
#[command(
    name = "sandcrab-rootfs",
    about = "Build ext4 rootfs images for Sandcrab microVM backends"
)]
struct Cli {
    /// Cache root (default: $SANDCRAB_HOME/cache or ~/.sandcrab/cache).
    #[arg(long, global = true)]
    out: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build (or reuse) the rootfs for a template.
    Build {
        /// Template name (directory under --templates-dir).
        template: String,
        /// Static musl crabd binary to inject as /sbin/crabd.
        #[arg(long)]
        crabd: PathBuf,
        #[arg(long, default_value = "templates")]
        templates_dir: PathBuf,
        /// mkfs strategy: native | container | fakeroot (default: auto-detect).
        #[arg(long)]
        mkfs: Option<MkfsStrategy>,
        /// Re-hash the cached image on a cache hit.
        #[arg(long)]
        verify: bool,
        /// Guest platform: linux/amd64 | linux/arm64 (also amd64, arm64, x86_64, aarch64).
        /// Default: the host arch. crabd must be built for it.
        #[arg(long, visible_alias = "platform", value_name = "ARCH")]
        arch: Option<Arch>,
    },
    /// Create a sparse scratch drive.
    Scratch {
        path: PathBuf,
        #[arg(long, default_value_t = 2048)]
        size_mib: u64,
    },
}

#[derive(serde::Deserialize)]
struct TemplateToml {
    template: TemplateSection,
}
#[derive(serde::Deserialize)]
struct TemplateSection {
    #[serde(default)]
    version: String,
    image: String,
    /// Deprecated in-manifest pins (fallback only; pins are host-local now).
    sha256: Option<String>,
    #[serde(default)]
    sha256_by_arch: BTreeMap<String, String>,
}

impl TemplateSection {
    /// The deprecated in-manifest pin for `arch`: `sha256_by_arch.<arch>`, else `sha256`.
    fn pin(&self, arch: Arch) -> Option<String> {
        self.sha256_by_arch
            .get(arch.as_str())
            .or(self.sha256.as_ref())
            .cloned()
    }
}

/// Host-local pin file (`sandcrab_core::pins`; this tool does not depend on core, so it reads
/// the same schema-1 layout itself): `<pins dir>/<name>/<version>/<arch>.json`.
#[derive(serde::Deserialize)]
struct HostPin {
    schema: u32,
    template: String,
    version: String,
    arch: String,
    image_id: String,
}

/// `$SANDCRAB_TEMPLATE_PINS`, else `<sandcrab home>/template-pins`.
fn pins_dir() -> Result<PathBuf, String> {
    match std::env::var_os("SANDCRAB_TEMPLATE_PINS").filter(|v| !v.is_empty()) {
        Some(p) => Ok(PathBuf::from(p)),
        None => Ok(rootfs::sandcrab_home()
            .map_err(|e| e.to_string())?
            .join("template-pins")),
    }
}

/// The image-id pin for `name`/`version`/`arch`: the host-local pin file, else the
/// deprecated in-manifest pin. A present but invalid pin file is an error.
fn resolve_pin(
    pins: &Path,
    name: &str,
    t: &TemplateSection,
    arch: Arch,
) -> Result<Option<String>, String> {
    let safe = |s: &str| {
        !s.is_empty()
            && !s.contains("..")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    };
    if !safe(name) || !safe(&t.version) {
        return Ok(t.pin(arch));
    }
    let path = pins
        .join(name)
        .join(&t.version)
        .join(format!("{}.json", arch.as_str()));
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(t.pin(arch)),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let bad = |why: &str| format!("corrupt template pin {}: {why}", path.display());
    let pin: HostPin = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
    let hex_ok = pin.image_id.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    if pin.schema != 1
        || pin.template != name
        || pin.version != t.version
        || pin.arch != arch.as_str()
        || !hex_ok
    {
        return Err(bad("unexpected schema, key or image_id"));
    }
    Ok(Some(pin.image_id))
}

fn load_template(dir: &Path, name: &str) -> Result<TemplateSection, String> {
    let p = dir.join(name).join("sandcrab.toml");
    let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    toml::from_str::<TemplateToml>(&s)
        .map(|t| t.template)
        .map_err(|e| format!("{}: {e}", p.display()))
}

fn run(cli: Cli) -> Result<(), String> {
    let cache = match cli.out {
        Some(p) => p,
        None => default_cache_root().map_err(|e| e.to_string())?,
    };
    match cli.cmd {
        Cmd::Build {
            template,
            crabd,
            templates_dir,
            mkfs,
            verify,
            arch,
        } => {
            let arch = match arch {
                Some(a) => a,
                None => Arch::host().map_err(|e| e.to_string())?,
            };
            let t = load_template(&templates_dir, &template)?;
            let pin = resolve_pin(&pins_dir()?, &template, &t, arch)?;
            if pin.is_none() {
                eprintln!(
                    "warning: template `{template}` is not pinned on this host \
                     (`templates/build.sh {template}`); building without an image-id check"
                );
            }
            let mut b = RootfsBuilder::new(&cache).verify_on_hit(verify);
            if let Some(s) = mkfs {
                b = b.strategy(s);
            }
            let built = b
                .build(&RootfsRequest {
                    expected_image_id: pin,
                    image: t.image,
                    template: Some(template),
                    crabd,
                    arch,
                })
                .map_err(|e| e.to_string())?;
            eprintln!(
                "{} {} ({}) in {:.2?} ({} MiB, mkfs={:?})",
                if built.cache_hit {
                    "cache hit"
                } else {
                    "built"
                },
                built.manifest.cache_key,
                built.manifest.arch.platform(),
                built.elapsed,
                built.manifest.size_bytes >> 20,
                built.manifest.mkfs,
            );
            println!("{}", built.image_path.display());
        }
        Cmd::Scratch { path, size_mib } => {
            let d = rootfs::create_scratch(&path, size_mib).map_err(|e| e.to_string())?;
            eprintln!("scratch {} MiB in {:.2?}", size_mib, d);
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_prefers_per_arch_then_falls_back() {
        let t: TemplateToml = toml::from_str(
            r#"
            [template]
            name = "base"
            image = "sandcrab/base:0.1.0"
            sha256 = "sha256:single"
            [template.sha256_by_arch]
            arm64 = "sha256:arm"
            "#,
        )
        .unwrap();
        assert_eq!(t.template.pin(Arch::Arm64).as_deref(), Some("sha256:arm"));
        assert_eq!(
            t.template.pin(Arch::Amd64).as_deref(),
            Some("sha256:single")
        );
        let bare: TemplateToml = toml::from_str("[template]\nimage = \"i\"\n").unwrap();
        assert_eq!(bare.template.pin(Arch::Amd64), None);
    }

    #[test]
    fn host_local_pin_wins_and_corrupt_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let t: TemplateToml = toml::from_str(
            "[template]\nversion = \"0.1.0\"\nimage = \"i\"\nsha256 = \"sha256:legacy\"\n",
        )
        .unwrap();
        let t = t.template;
        assert_eq!(
            resolve_pin(tmp.path(), "base", &t, Arch::Arm64).unwrap(),
            Some("sha256:legacy".into())
        );
        let id = format!("sha256:{}", "a".repeat(64));
        let dir = tmp.path().join("base/0.1.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("arm64.json"),
            format!(
                r#"{{"schema":1,"template":"base","version":"0.1.0","arch":"arm64","image":"i","image_id":"{id}","source":"build","recorded_at":"2026-11-24T00:00:00Z"}}"#
            ),
        )
        .unwrap();
        assert_eq!(
            resolve_pin(tmp.path(), "base", &t, Arch::Arm64).unwrap(),
            Some(id)
        );
        std::fs::write(dir.join("arm64.json"), "{}").unwrap();
        assert!(resolve_pin(tmp.path(), "base", &t, Arch::Arm64).is_err());
    }
}
