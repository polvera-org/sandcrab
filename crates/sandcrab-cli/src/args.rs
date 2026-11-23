//! Argument value parsers and small pure helpers (unit-tested).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use sandcrab_sdk::{NetworkPolicy, SandboxConfig, WorkspaceMount};

/// Runtime backends this CLI knows about. Only compiled ones parse successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendArg {
    /// DockerDev: shared kernel, local dev / CI only.
    #[cfg(feature = "docker")]
    Docker,
    /// Firecracker microVMs: hardware isolated, needs /dev/kvm.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    Firecracker,
}

/// Is the firecracker backend compiled into this binary?
pub const FIRECRACKER_COMPILED: bool = cfg!(all(feature = "firecracker", target_os = "linux"));

/// Backends compiled into this binary (for messages).
pub fn compiled_backends() -> &'static str {
    match (cfg!(feature = "docker"), FIRECRACKER_COMPILED) {
        (true, true) => "docker, firecracker",
        (true, false) => "docker",
        (false, true) => "firecracker",
        (false, false) => "none",
    }
}

pub fn parse_backend(s: &str) -> Result<BackendArg, String> {
    match s {
        #[cfg(feature = "docker")]
        "docker" | "docker-dev" => Ok(BackendArg::Docker),
        #[cfg(not(feature = "docker"))]
        "docker" | "docker-dev" => Err(format!(
            "backend `{s}` is not compiled into this build (available: {})",
            compiled_backends()
        )),
        #[cfg(all(feature = "firecracker", target_os = "linux"))]
        "firecracker" | "fc" => Ok(BackendArg::Firecracker),
        #[cfg(not(all(feature = "firecracker", target_os = "linux")))]
        "firecracker" | "fc" => Err(format!(
            "backend `{s}` is not compiled into this build (Linux only, cargo feature \
             `firecracker`; available: {})",
            compiled_backends()
        )),
        "cloud-hypervisor" | "libkrun" => Err(format!(
            "backend `{s}` is not compiled into / available in this build (available: {})",
            compiled_backends()
        )),
        other => Err(format!(
            "unknown backend `{other}` (available: {})",
            compiled_backends()
        )),
    }
}

/// `<workspace>:<guest_path>[:rw|ro]`.
pub fn parse_mount(s: &str) -> Result<WorkspaceMount, String> {
    WorkspaceMount::parse_spec(s).map_err(|e| e.to_string())
}

/// `KEY=VALUE` (value may be empty or contain `=`).
pub fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("env `{s}` must be KEY=VALUE"))?;
    if k.is_empty() {
        return Err(format!("env `{s}` has an empty name"));
    }
    Ok((k.to_owned(), v.to_owned()))
}

pub fn parse_network(s: &str) -> Result<NetworkPolicy, String> {
    s.parse()
        .map_err(|e: sandcrab_sdk::SandboxError| e.to_string())
}

/// `1`, `true`, `yes`, `on` (case-insensitive).
#[cfg(any(feature = "docker", test))]
pub fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// One argument is a shell script as-is (`-- 'ls | wc -l'`); several are an argv, each
/// shell-quoted (`-- bash -c 'exit 3'`).
pub fn shell_join(args: &[String]) -> String {
    match args {
        [single] => single.clone(),
        _ => args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub fn shell_quote(a: &str) -> Cow<'_, str> {
    let safe = !a.is_empty()
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
    if safe {
        Cow::Borrowed(a)
    } else {
        Cow::Owned(format!("'{}'", a.replace('\'', r"'\''")))
    }
}

/// Template search path when `--templates-dir` is not given: `./templates`, then
/// `<state root>/templates` — whichever exist.
pub fn default_templates_dirs(state_root: &Path) -> Vec<PathBuf> {
    [PathBuf::from("templates"), state_root.join("templates")]
        .into_iter()
        .filter(|d| d.is_dir())
        .collect()
}

/// Sandbox flags shared by `run` and `tool`.
#[derive(Debug, Clone, clap::Args)]
pub struct SpawnArgs {
    /// Template name (base, python-dev, ts-dev, render, ...).
    #[arg(long, short = 't', default_value = "base")]
    pub template: String,

    /// Mount a workspace: `<workspace>:<guest_path>[:rw|ro]` (default rw). Repeatable.
    #[arg(long = "mount", short = 'm', value_parser = parse_mount)]
    pub mounts: Vec<WorkspaceMount>,

    /// Default working directory inside the sandbox (default: first rw mount, else the
    /// template workdir).
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Environment variable `KEY=VALUE`. Repeatable.
    #[arg(long = "env", short = 'e', value_parser = parse_env)]
    pub env: Vec<(String, String)>,

    /// Network policy: disabled | unrestricted (default: the template's).
    #[arg(long, value_parser = parse_network)]
    pub network: Option<NetworkPolicy>,

    /// Publish a guest port on host loopback. Repeatable.
    #[arg(long = "port", short = 'p')]
    pub ports: Vec<u16>,

    /// Sandbox TTL in seconds (0 = none). Default 1800, or none with `--keep`.
    #[arg(long)]
    pub ttl: Option<u64>,
}

impl SpawnArgs {
    pub fn to_config(&self, keep: bool) -> SandboxConfig {
        let mut c = SandboxConfig::from_template(&self.template);
        c.mounts.clone_from(&self.mounts);
        c.default_cwd.clone_from(&self.cwd);
        c.env.extend(self.env.iter().cloned());
        if let Some(n) = self.network {
            c = c.network(n);
        }
        for p in &self.ports {
            c = c.port(*p);
        }
        c.ttl_secs = match self.ttl {
            Some(0) => None,
            Some(s) => Some(s),
            None if keep => None,
            None => c.ttl_secs,
        };
        c
    }
}

#[cfg(test)]
mod tests {
    use sandcrab_sdk::{MountMode, WorkspaceRef};

    use super::*;

    #[test]
    fn mount_specs() {
        let m = parse_mount("demo:/workspace").unwrap();
        assert_eq!(m.workspace, WorkspaceRef::parse("demo"));
        assert_eq!(m.guest_path, PathBuf::from("/workspace"));
        assert_eq!(m.mode, MountMode::ReadWrite);
        assert_eq!(
            parse_mount("demo:/src:ro").unwrap().mode,
            MountMode::ReadOnly
        );
        assert_eq!(
            parse_mount("demo:/src:rw").unwrap().mode,
            MountMode::ReadWrite
        );
        for bad in ["demo", "demo:/x:rx", ":/x", "demo:", "a:/b:ro:extra"] {
            assert!(parse_mount(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_specs() {
        assert_eq!(parse_env("A=1").unwrap(), ("A".into(), "1".into()));
        assert_eq!(parse_env("A=").unwrap(), ("A".into(), String::new()));
        assert_eq!(parse_env("A=b=c").unwrap(), ("A".into(), "b=c".into()));
        assert!(parse_env("A").is_err());
        assert!(parse_env("=x").is_err());
    }

    #[test]
    fn backends() {
        #[cfg(feature = "docker")]
        assert_eq!(parse_backend("docker").unwrap(), BackendArg::Docker);
        if FIRECRACKER_COMPILED {
            #[cfg(all(feature = "firecracker", target_os = "linux"))]
            {
                assert_eq!(
                    parse_backend("firecracker").unwrap(),
                    BackendArg::Firecracker
                );
                assert_eq!(parse_backend("fc").unwrap(), BackendArg::Firecracker);
            }
            assert!(compiled_backends().contains("firecracker"));
        } else {
            let e = parse_backend("firecracker").unwrap_err();
            assert!(e.contains("not compiled"), "{e}");
        }
        let e = parse_backend("libkrun").unwrap_err();
        assert!(e.contains("not compiled"), "{e}");
        assert!(parse_backend("qemu").unwrap_err().contains("unknown"));
    }

    #[test]
    fn network_and_truthy() {
        assert_eq!(
            parse_network("unrestricted").unwrap(),
            NetworkPolicy::Unrestricted
        );
        assert!(parse_network("open").is_err());
        assert!(truthy("1") && truthy("TRUE") && truthy("yes"));
        assert!(!truthy("0") && !truthy("") && !truthy("no"));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_join(&["ls -la | wc -l".into()]), "ls -la | wc -l");
        assert_eq!(
            shell_join(&["bash".into(), "-c".into(), "exit 3".into()]),
            "bash -c 'exit 3'"
        );
        assert_eq!(
            shell_join(&["echo".into(), "it's".into(), String::new()]),
            r"echo 'it'\''s' ''"
        );
        assert_eq!(shell_join(&["cat".into(), "a/b.txt".into()]), "cat a/b.txt");
    }

    #[test]
    fn spawn_args_to_config() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            s: SpawnArgs,
        }
        let t = T::try_parse_from([
            "t",
            "-t",
            "ts-dev",
            "-m",
            "site:/workspace",
            "-m",
            "lib:/lib2:ro",
            "--cwd",
            "/workspace",
            "-e",
            "A=1",
            "--network",
            "unrestricted",
            "-p",
            "5173",
        ])
        .unwrap();
        let c = t.s.to_config(false);
        assert_eq!(c.template, "ts-dev");
        assert_eq!(c.mounts.len(), 2);
        assert_eq!(c.mounts[1].mode, MountMode::ReadOnly);
        assert_eq!(c.env["A"], "1");
        assert_eq!(c.ports[0].guest, 5173);
        assert_eq!(c.network.unwrap().policy, NetworkPolicy::Unrestricted);
        assert_eq!(c.ttl_secs, Some(sandcrab_sdk::core::DEFAULT_TTL_SECS));
        assert_eq!(t.s.to_config(true).ttl_secs, None);

        assert!(T::try_parse_from(["t", "-m", "bad"]).is_err());
        assert!(T::try_parse_from(["t", "-e", "NOEQ"]).is_err());
    }
}
