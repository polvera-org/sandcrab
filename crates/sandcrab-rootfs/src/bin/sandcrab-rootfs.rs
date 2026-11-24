//! `sandcrab-rootfs`: operator tool for the microVM rootfs pipeline (Firecracker, libkrun).
//! Wrapped by `scripts/build-rootfs.sh`. Pinned Firecracker kernel/binaries are fetched by
//! `sandcrab-fc-fetch` (`scripts/fetch-artifacts.sh`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sandcrab_rootfs::{
    self as rootfs, MkfsStrategy, RootfsBuilder, RootfsRequest, default_cache_root,
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
    image: String,
    sha256: Option<String>,
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
        } => {
            let t = load_template(&templates_dir, &template)?;
            let mut b = RootfsBuilder::new(&cache).verify_on_hit(verify);
            if let Some(s) = mkfs {
                b = b.strategy(s);
            }
            let built = b
                .build(&RootfsRequest {
                    image: t.image,
                    expected_image_id: t.sha256,
                    template: Some(template),
                    crabd,
                })
                .map_err(|e| e.to_string())?;
            eprintln!(
                "{} {} in {:.2?} ({} MiB, mkfs={:?})",
                if built.cache_hit {
                    "cache hit"
                } else {
                    "built"
                },
                built.manifest.cache_key,
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
