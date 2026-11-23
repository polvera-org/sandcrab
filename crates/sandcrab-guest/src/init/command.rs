//! The `crabd serve` child command that `crabd init` spawns as the sandbox user.

use std::{
    ffi::OsStr,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

use super::config::ValidatedConfig;

/// `PATH` when the config does not set one.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Build `crabd serve --vsock <tools_port> --default-cwd <cwd>` running as the sandbox user.
///
/// Privilege drop is done by `std` in the child between `fork` and `exec`, in the safe order:
/// `setgroups(0, NULL)` (std does this itself when the parent is root and no supplementary
/// groups were requested), then `setgid`, then `setuid`. The environment is cleared and
/// rebuilt from the config, so nothing from PID 1's environment (kernel cmdline leftovers)
/// leaks into tool processes.
#[must_use]
pub fn serve_command(exe: &Path, v: &ValidatedConfig) -> Command {
    let cfg = &v.config;
    let mut cmd = Command::new(exe);
    cmd.arg0("crabd")
        .arg("serve")
        .arg("--vsock")
        .arg(cfg.tools_port.to_string())
        .arg("--default-cwd")
        .arg(&v.default_cwd)
        .current_dir(&v.default_cwd)
        .env_clear()
        .env("PATH", DEFAULT_PATH)
        .env(
            "HOME",
            cfg.user.home.as_deref().unwrap_or(v.default_cwd.as_path()),
        )
        .env("HOSTNAME", &cfg.hostname)
        .env("SANDCRAB_SANDBOX_ID", &cfg.sandbox_id)
        .stdin(Stdio::null())
        .uid(cfg.user.uid)
        .gid(cfg.user.gid);
    if let Some(name) = &cfg.user.name {
        cmd.env("USER", name).env("LOGNAME", name);
    }
    // Config env last so it can override the defaults above (e.g. PATH).
    cmd.envs(cfg.env.iter().map(|(k, v)| (OsStr::new(k), OsStr::new(v))));
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::config::validate_config;
    use sandcrab_protocol::init::{InitConfig, InitUser};
    use std::{collections::BTreeMap, ffi::OsString};

    fn validated(env: &[(&str, &str)], home: Option<&str>) -> ValidatedConfig {
        validate_config(InitConfig {
            sandbox_id: "sb-1".into(),
            hostname: "box".into(),
            user: InitUser {
                uid: 1000,
                gid: 1001,
                home: home.map(Into::into),
                name: Some("agent".into()),
            },
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            default_cwd: "/workspace".into(),
            mounts: vec![],
            network: false,
            dns: vec![],
            tools_port: 2048,
            host_time_unix_ms: None,
        })
        .unwrap()
    }

    fn envs(cmd: &Command) -> BTreeMap<OsString, Option<OsString>> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect()
    }

    #[test]
    fn args_cwd_and_env() {
        let cmd = serve_command(
            Path::new("/proc/self/exe"),
            &validated(&[("TOKEN", "s3cret")], None),
        );
        assert_eq!(cmd.get_program(), "/proc/self/exe");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["serve", "--vsock", "2048", "--default-cwd", "/workspace"]
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/workspace")));
        let e = envs(&cmd);
        assert_eq!(
            e[OsStr::new("TOKEN")].as_deref(),
            Some(OsStr::new("s3cret"))
        );
        assert_eq!(
            e[OsStr::new("HOME")].as_deref(),
            Some(OsStr::new("/workspace"))
        );
        assert_eq!(e[OsStr::new("USER")].as_deref(), Some(OsStr::new("agent")));
        assert_eq!(
            e[OsStr::new("PATH")].as_deref(),
            Some(OsStr::new(DEFAULT_PATH))
        );
    }

    #[test]
    fn config_env_overrides_defaults() {
        let cmd = serve_command(
            Path::new("/sbin/crabd"),
            &validated(&[("PATH", "/opt/bin")], Some("/home/agent")),
        );
        let e = envs(&cmd);
        assert_eq!(
            e[OsStr::new("PATH")].as_deref(),
            Some(OsStr::new("/opt/bin"))
        );
        assert_eq!(
            e[OsStr::new("HOME")].as_deref(),
            Some(OsStr::new("/home/agent"))
        );
    }
}
