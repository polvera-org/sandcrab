//! `sandcrab workspace ...` and `sandcrab template ...`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Subcommand;
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
    /// Build template images with `templates/build.sh` and pin their digests.
    Build {
        /// Template names.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        names: Vec<String>,
        /// Build every template (base first).
        #[arg(long)]
        all: bool,
        /// Do not write the built image IDs into the manifests' `sha256` pins.
        #[arg(long)]
        no_pin: bool,
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
            let bytes = client.read_file(workspace.as_str(), &rel_path)?;
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)?;
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

fn template_json(t: &Template) -> serde_json::Value {
    let m = &t.manifest.template;
    json!({
        "name": m.name,
        "version": m.version,
        "description": m.description,
        "image": t.image(),
        "sha256": m.sha256,
        "dir": t.dir,
    })
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

pub fn template(g: &Global, cmd: TemplateCmd) -> anyhow::Result<u8> {
    let dirs = util::templates_dirs(g)?;
    if dirs.is_empty() {
        bail!("no template directories: pass --templates-dir or run from the repo root");
    }
    let templates = util::templates(g)?;
    match cmd {
        TemplateCmd::List => {
            let all = templates.list();
            if g.json {
                print_json(&all.iter().map(template_json).collect::<Vec<_>>())?;
            } else {
                println!("{:<12} {:<8} {:<28} PINNED", "NAME", "VERSION", "IMAGE");
                for t in &all {
                    let pinned = t
                        .manifest
                        .template
                        .sha256
                        .as_deref()
                        .map_or("no".to_owned(), |d| format!("{}…", &d[..d.len().min(19)]));
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
            if !no_pin {
                cmd.arg("--pin");
            }
            cmd.args(&targets);
            let status = cmd
                .status()
                .with_context(|| format!("running {}", script.display()))?;
            if !status.success() {
                bail!("{} failed ({status})", script.display());
            }
            Ok(0)
        }
    }
}
