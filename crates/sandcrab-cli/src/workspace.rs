//! `sandcrab workspace ...` and `sandcrab template ...`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Subcommand;
use sandcrab_sdk::core::pins::default_pins_dir;
use sandcrab_sdk::core::{EffectivePin, ImageArch, PinOrigin, PinStore, effective_pin};
use sandcrab_sdk::{FileKind, Template, Workspace, WorkspaceBacking};
use serde_json::json;

use crate::Global;
use crate::util::{self, print_json};

#[derive(Debug, Subcommand)]
pub enum WorkspaceCmd {
    /// Create a Sandcrab-managed workspace.
    Create { name: String },
    /// Register an existing host directory as a workspace (never deleted by Sandcrab).
    Register { name: String, path: PathBuf },
    /// List workspaces.
    #[command(alias = "list")]
    Ls,
    /// Write a file into a workspace from a local file or `-` (stdin).
    Put {
        workspace: String,
        rel_path: PathBuf,
        source: String,
    },
    /// Print a workspace file to stdout.
    Cat {
        workspace: String,
        rel_path: PathBuf,
    },
    /// List a workspace directory (non-recursive).
    Files {
        workspace: String,
        /// Directory relative to the workspace root (default: the root).
        dir: Option<PathBuf>,
    },
    /// Unregister a workspace.
    Rm {
        workspace: String,
        /// Also delete a managed workspace's data (registered host dirs are never deleted).
        #[arg(long)]
        delete_data: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TemplateCmd {
    /// List templates found in the template directories.
    #[command(alias = "ls")]
    List,
    /// Build template images with `templates/build.sh` and pin their image IDs on this host.
    Build {
        /// Template names.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        names: Vec<String>,
        /// Build every template (base first).
        #[arg(long)]
        all: bool,
        /// Do not record host-local pins for the built images.
        #[arg(long)]
        no_pin: bool,
    },
    /// Adopt the template images already present on this host: record their current image
    /// IDs as host-local pins without rebuilding. Local integrity only (spawns then check the
    /// image is unchanged); it says nothing about how the image was built.
    Pin {
        /// Template names.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        names: Vec<String>,
        /// Pin every template.
        #[arg(long)]
        all: bool,
    },
}

fn kind(ws: &Workspace) -> &'static str {
    match ws.backing {
        WorkspaceBacking::HostDir(_) => "host_dir",
        WorkspaceBacking::ManagedDir(_) => "managed",
        WorkspaceBacking::BlockVolume(_) => "block_volume",
    }
}

fn state(ws: &Workspace) -> String {
    serde_json::to_value(ws.state)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "?".into())
}

fn print_ws(g: &Global, ws: &Workspace, verb: &str) -> anyhow::Result<()> {
    if g.json {
        print_json(ws)
    } else {
        println!(
            "{verb} workspace `{}` ({}, {}) at {}",
            ws.name,
            ws.id,
            kind(ws),
            ws.backing.path().display()
        );
        Ok(())
    }
}

pub fn workspace(g: &Global, cmd: WorkspaceCmd) -> anyhow::Result<u8> {
    let client = util::workspaces(g)?;
    match cmd {
        WorkspaceCmd::Create { name } => print_ws(g, &client.create(&name)?, "created")?,
        WorkspaceCmd::Register { name, path } => {
            print_ws(g, &client.register_host_dir(&name, &path)?, "registered")?;
        }
        WorkspaceCmd::Ls => {
            let all = client.list()?;
            if g.json {
                print_json(&all)?;
            } else if all.is_empty() {
                println!("no workspaces (create one with `sandcrab workspace create <name>`)");
            } else {
                println!(
                    "{:<24} {:<12} {:>4} {:<14} PATH",
                    "NAME", "KIND", "GEN", "STATE"
                );
                for ws in &all {
                    println!(
                        "{:<24} {:<12} {:>4} {:<14} {}",
                        ws.name,
                        kind(ws),
                        ws.generation,
                        state(ws),
                        ws.backing.path().display()
                    );
                }
            }
        }
        WorkspaceCmd::Put {
            workspace,
            rel_path,
            source,
        } => {
            let bytes = if source == "-" {
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("reading stdin")?;
                buf
            } else {
                std::fs::read(&source).with_context(|| format!("reading `{source}`"))?
            };
            client.put_file(workspace.as_str(), &rel_path, &bytes)?;
            if g.json {
                print_json(&json!({
                    "workspace": workspace,
                    "path": rel_path,
                    "bytes": bytes.len(),
                }))?;
            } else {
                println!(
                    "wrote {} bytes to {workspace}:{}",
                    bytes.len(),
                    rel_path.display()
                );
            }
        }
        WorkspaceCmd::Cat {
            workspace,
            rel_path,
        } => {
            // Streamed: no size cap, constant memory.
            let (mut file, _) = client.open_file(workspace.as_str(), &rel_path)?;
            let mut out = std::io::stdout().lock();
            std::io::copy(&mut file, &mut out)?;
            out.flush()?;
        }
        WorkspaceCmd::Files { workspace, dir } => {
            let dir = dir.unwrap_or_default();
            let entries = client.list_files(workspace.as_str(), &dir)?;
            if g.json {
                print_json(&entries)?;
            } else {
                for e in &entries {
                    let k = match e.kind {
                        FileKind::File => "file",
                        FileKind::Dir => "dir",
                        FileKind::Symlink => "link",
                        FileKind::Other => "other",
                    };
                    println!("{k:<5} {:>10} {}", e.size, e.path.display());
                }
            }
        }
        WorkspaceCmd::Rm {
            workspace,
            delete_data,
        } => {
            let ws = client.remove(workspace.as_str(), delete_data)?;
            let verb = if delete_data && matches!(ws.backing, WorkspaceBacking::ManagedDir(_)) {
                "removed (data deleted)"
            } else {
                "removed"
            };
            print_ws(g, &ws, verb)?;
        }
    }
    Ok(0)
}

fn template_json(t: &Template, pin: Option<&EffectivePin>) -> serde_json::Value {
    let m = &t.manifest.template;
    json!({
        "name": m.name,
        "version": m.version,
        "description": m.description,
        "image": t.image(),
        "pin": pin.map(|p| json!({
            "image_id": p.digest,
            "origin": pin_origin(p),
            "path": match &p.origin {
                PinOrigin::HostLocal(path) => Some(path),
                PinOrigin::LegacyManifest => None,
            },
        })),
        "dir": t.dir,
    })
}

fn pin_origin(p: &EffectivePin) -> &'static str {
    match p.origin {
        PinOrigin::HostLocal(_) => "host-local",
        PinOrigin::LegacyManifest => "legacy-manifest",
    }
}

/// The host-local pin (else legacy manifest pin) of `t` for this host's arch.
fn host_pin(store: Option<&PinStore>, t: &Template) -> anyhow::Result<Option<EffectivePin>> {
    Ok(effective_pin(store, &t.manifest, ImageArch::host())?)
}

/// `build.sh` next to the template directories (`<templates dir>/build.sh`).
fn find_build_script(dirs: &[&Path]) -> anyhow::Result<PathBuf> {
    dirs.iter()
        .map(|d| d.join("build.sh"))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no build.sh found in {:?}; template builds use the repo's templates/build.sh",
                dirs
            )
        })
}

pub async fn template(g: &Global, cmd: TemplateCmd) -> anyhow::Result<u8> {
    let dirs = util::templates_dirs(g)?;
    if dirs.is_empty() {
        bail!("no template directories: pass --templates-dir or run from the repo root");
    }
    let templates = util::templates(g)?;
    match cmd {
        TemplateCmd::List => {
            let all = templates.list();
            let store = PinStore::open_default().ok();
            let pins = all
                .iter()
                .map(|t| host_pin(store.as_ref(), t))
                .collect::<anyhow::Result<Vec<_>>>()?;
            if g.json {
                print_json(
                    &all.iter()
                        .zip(&pins)
                        .map(|(t, p)| template_json(t, p.as_ref()))
                        .collect::<Vec<_>>(),
                )?;
            } else {
                println!(
                    "{:<12} {:<8} {:<28} PINNED (this host)",
                    "NAME", "VERSION", "IMAGE"
                );
                for (t, pin) in all.iter().zip(&pins) {
                    let pinned = pin.as_ref().map_or("no".to_owned(), |p| {
                        let d = &p.digest;
                        format!("{} {}…", pin_origin(p), &d[..d.len().min(19)])
                    });
                    println!(
                        "{:<12} {:<8} {:<28} {pinned}",
                        t.name(),
                        t.manifest.template.version,
                        t.image()
                    );
                }
            }
            Ok(0)
        }
        TemplateCmd::Build { names, all, no_pin } => {
            let (script, targets) = if all {
                let refs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
                (find_build_script(&refs)?, Vec::new())
            } else {
                let mut script = None;
                for n in &names {
                    if templates.get(n)?.manifest.template.external {
                        bail!(
                            "template `{n}` is external (its image is built elsewhere): pull \
                             the image, then adopt it with `sandcrab template pin {n}`"
                        );
                    }
                    let dir = templates.dir(n)?;
                    let parent = dir.parent().unwrap_or(Path::new("."));
                    let s = find_build_script(&[parent])?;
                    if script.as_ref().is_some_and(|prev| prev != &s) {
                        bail!("templates come from different directories; build them separately");
                    }
                    script = Some(s);
                }
                (script.expect("at least one name"), names)
            };
            let mut cmd = Command::new("bash");
            cmd.arg(&script);
            if no_pin {
                cmd.arg("--no-pin");
            }
            // build.sh resolves the pins dir the same way; pass it so both always agree.
            cmd.env(sandcrab_sdk::core::PINS_DIR_ENV, default_pins_dir()?);
            cmd.args(&targets);
            let status = cmd
                .status()
                .with_context(|| format!("running {}", script.display()))?;
            if !status.success() {
                bail!("{} failed ({status})", script.display());
            }
            Ok(0)
        }
        TemplateCmd::Pin { names, all } => {
            let selected: Vec<Template> = if all {
                templates.list()
            } else {
                names
                    .iter()
                    .map(|n| templates.get(n))
                    .collect::<Result<_, _>>()?
            };
            adopt(g, &selected).await
        }
    }
}

/// `template pin`: record the current local image IDs as host-local pins (with a warning).
#[cfg(feature = "docker")]
async fn adopt(g: &Global, selected: &[Template]) -> anyhow::Result<u8> {
    use sandcrab_sdk::core::ImageVerifier;
    use sandcrab_sdk::core::pins::{Adopted, obtain_hint};
    use sandcrab_sdk::docker::DockerImageVerifier;

    let Some(arch) = ImageArch::host() else {
        bail!(
            "unsupported host arch `{}`: pins are keyed by amd64 / arm64",
            std::env::consts::ARCH
        );
    };
    let store = PinStore::open_default()?;
    let verifier = DockerImageVerifier::connect(None).context("cannot connect to Docker")?;
    let mut out = Vec::new();
    for t in selected {
        let image = t.image();
        let id = verifier.image_digest(image).await.with_context(|| {
            format!(
                "`{image}` is not present on this host; {}",
                obtain_hint(&t.manifest)
            )
        })?;
        match store.adopt(&t.manifest, arch, &id)? {
            Adopted::Unchanged if !g.json => println!("{}: already pinned to {id}", t.name()),
            Adopted::Unchanged => {}
            Adopted::Recorded { path, replaced } => {
                eprintln!(
                    "warning: adopted the existing local image `{image}` ({id}) as this host's \
                     pin for template `{}` {} {arch} without rebuilding it{}. From now on spawns \
                     only check that it stays unchanged (local integrity); this does not \
                     establish how the image was built (not provenance).",
                    t.name(),
                    t.manifest.template.version,
                    replaced.map_or_else(String::new, |r| format!(", replacing pin {r}")),
                );
                if !g.json {
                    println!("{}: pinned {id} -> {}", t.name(), path.display());
                }
            }
        }
        out.push(json!({"name": t.name(), "arch": arch, "image_id": id}));
    }
    if g.json {
        print_json(&out)?;
    }
    Ok(0)
}

#[cfg(not(feature = "docker"))]
async fn adopt(_g: &Global, _selected: &[Template]) -> anyhow::Result<u8> {
    bail!("`sandcrab template pin` needs the docker feature (it inspects local Docker images)")
}
