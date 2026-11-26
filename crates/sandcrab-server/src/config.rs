//! Server configuration (design §5.6, §5.9): a TOML file, overridden by `sandcrab server`
//! flags. Unknown keys are refused, so a typo is a start error, not a silent default.

use std::path::{Path, PathBuf};

use sandcrab_sdk::protocol::api::MountMode;
use serde::{Deserialize, Deserializer};

use crate::ServerError;

/// Default `max_sandboxes`.
pub const DEFAULT_MAX_SANDBOXES: u32 = 64;
/// Default TTL of a sandbox created without `ttl_secs` (today's core default, 30 min).
pub const DEFAULT_TTL_SECS: u64 = sandcrab_sdk::core::DEFAULT_TTL_SECS;
/// Longest TTL a client may ask for.
pub const DEFAULT_MAX_TTL_SECS: u64 = 24 * 60 * 60;
/// Default socket mode.
pub const DEFAULT_SOCKET_MODE: u32 = 0o600;

/// The backend a server runs. Always named; there is no default and no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendName {
    Docker,
    Libkrun,
    Firecracker,
}

impl BackendName {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
            Self::Firecracker => "firecracker",
        }
    }
}

impl std::str::FromStr for BackendName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "docker" => Ok(Self::Docker),
            "libkrun" => Ok(Self::Libkrun),
            "firecracker" => Ok(Self::Firecracker),
            other => Err(format!(
                "unknown backend `{other}` (docker, libkrun or firecracker)"
            )),
        }
    }
}

/// An allowed root for mounts (`[[mount_roots]]`) and the highest mode beneath it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountRootConfig {
    pub path: PathBuf,
    /// `rw`: ro and rw mounts allowed; `ro`: only ro. Default `rw`.
    #[serde(default = "rw")]
    pub mode: MountMode,
}

fn rw() -> MountMode {
    MountMode::ReadWrite
}

impl std::str::FromStr for MountRootConfig {
    type Err = String;

    /// `PATH`, `PATH:ro` or `PATH:rw`.
    fn from_str(s: &str) -> Result<Self, String> {
        let (path, mode) = match s.rsplit_once(':') {
            Some((p, "ro")) => (p, MountMode::ReadOnly),
            Some((p, "rw")) => (p, MountMode::ReadWrite),
            _ => (s, MountMode::ReadWrite),
        };
        if !Path::new(path).is_absolute() {
            return Err(format!("mount root `{path}` must be an absolute path"));
        }
        Ok(Self {
            path: path.into(),
            mode,
        })
    }
}

/// `[limits]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// Sandboxes alive at once, for all clients together.
    pub max_sandboxes: u32,
    /// TTL of a sandbox created without `ttl_secs`.
    pub default_ttl_secs: u64,
    /// Longest TTL a client may ask for (`null`, no TTL, stays allowed).
    pub max_ttl_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_sandboxes: DEFAULT_MAX_SANDBOXES,
            default_ttl_secs: DEFAULT_TTL_SECS,
            max_ttl_secs: DEFAULT_MAX_TTL_SECS,
        }
    }
}

/// The whole configuration. Every key is optional in the file; [`Self::validate`] requires
/// `socket` and `backend` (a flag may give them).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Path of the Unix socket. Its directory holds the socket and nothing else.
    pub socket: Option<PathBuf>,
    /// Octal string, default `"0600"`.
    #[serde(deserialize_with = "octal_mode")]
    pub socket_mode: u32,
    /// Group of the socket (a name from `/etc/group`, or a numeric gid).
    pub socket_group: Option<String>,
    /// Peer uids admitted. Absent: only the server's own uid.
    pub allow_uids: Option<Vec<u32>>,
    pub backend: Option<BackendName>,
    /// Default: `$SANDCRAB_HOME`, else `~/.sandcrab`.
    pub state_dir: Option<PathBuf>,
    /// Template directories, first wins. Empty: the CLI's defaults.
    pub templates_dirs: Vec<PathBuf>,
    /// Used when `sandbox.create` names no template (an empty string).
    pub default_template: Option<String>,
    /// May a client ask for uid or gid 0 in a sandbox (also as the default user of a root
    /// peer).
    pub allow_root_user: bool,
    /// Inclusive `[first, last]` uid ranges a client may name as its sandbox's uid besides
    /// its own. Default: none, so a sandbox runs as its client's uid. A file a sandbox makes
    /// setuid in a read-write mount is a setuid file of that uid on the host.
    pub allow_sandbox_uids: Vec<[u32; 2]>,
    /// The same for the sandbox's gid besides the client's own gid.
    pub allow_sandbox_gids: Vec<[u32; 2]>,
    /// Templates whose self-test runs once at start, in the background. Default: none. A
    /// failure does not stop the server; `server.capabilities` then says why isolation is
    /// not proven.
    pub selftest_on_start: Vec<String>,
    pub limits: LimitsConfig,
    /// No roots: every mount is refused.
    pub mount_roots: Vec<MountRootConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            socket: None,
            socket_mode: DEFAULT_SOCKET_MODE,
            socket_group: None,
            allow_uids: None,
            backend: None,
            state_dir: None,
            templates_dirs: Vec::new(),
            default_template: None,
            allow_root_user: false,
            allow_sandbox_uids: Vec::new(),
            allow_sandbox_gids: Vec::new(),
            selftest_on_start: Vec::new(),
            limits: LimitsConfig::default(),
            mount_roots: Vec::new(),
        }
    }
}

fn octal_mode<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let s = String::deserialize(d)?;
    parse_mode(&s).map_err(serde::de::Error::custom)
}

/// `"0660"`, `"660"`: permission bits only.
pub fn parse_mode(s: &str) -> Result<u32, String> {
    let bad = || format!("socket_mode `{s}` must be octal permission bits such as \"0600\"");
    let mode = u32::from_str_radix(s, 8).map_err(|_| bad())?;
    if mode > 0o777 { Err(bad()) } else { Ok(mode) }
}

impl ServerConfig {
    pub fn from_toml(text: &str) -> Result<Self, ServerError> {
        toml::from_str(text).map_err(|e| ServerError::Config(e.to_string()))
    }

    pub fn load(path: &Path) -> Result<Self, ServerError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))?;
        Self::from_toml(&text).map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))
    }

    /// The socket path, required and absolute.
    pub fn socket_path(&self) -> Result<&Path, ServerError> {
        let p = self.socket.as_deref().ok_or_else(|| {
            ServerError::Config("no socket: set `socket` in the config or pass --socket".into())
        })?;
        if !p.is_absolute() || p.file_name().is_none() {
            return Err(ServerError::Config(format!(
                "socket `{}` must be an absolute file path",
                p.display()
            )));
        }
        Ok(p)
    }

    /// The uids admitted: `allow_uids`, else `[own_uid]`.
    #[must_use]
    pub fn allowed_uids(&self, own_uid: u32) -> Vec<u32> {
        self.allow_uids.clone().unwrap_or_else(|| vec![own_uid])
    }

    /// Everything a start needs is there and consistent.
    pub fn validate(&self) -> Result<(), ServerError> {
        self.socket_path()?;
        if self.backend.is_none() {
            return Err(ServerError::Config(
                "no backend: set `backend` (docker, libkrun or firecracker) in the config or \
                 pass --backend; the server never picks one"
                    .into(),
            ));
        }
        if self.allow_uids.as_ref().is_some_and(Vec::is_empty) {
            return Err(ServerError::Config(
                "`allow_uids` is empty: no client could connect (leave it out to admit only \
                 the server's own uid)"
                    .into(),
            ));
        }
        let l = &self.limits;
        if l.max_sandboxes == 0 || l.default_ttl_secs == 0 || l.max_ttl_secs == 0 {
            return Err(ServerError::Config(
                "`limits` values must be positive".into(),
            ));
        }
        if l.default_ttl_secs > l.max_ttl_secs {
            return Err(ServerError::Config(format!(
                "`limits.default_ttl_secs` ({}) is above `limits.max_ttl_secs` ({})",
                l.default_ttl_secs, l.max_ttl_secs
            )));
        }
        for (key, ranges) in [
            ("allow_sandbox_uids", &self.allow_sandbox_uids),
            ("allow_sandbox_gids", &self.allow_sandbox_gids),
        ] {
            if let Some([a, b]) = ranges.iter().find(|[a, b]| a > b) {
                return Err(ServerError::Config(format!(
                    "`{key}` range [{a}, {b}] is reversed (write [first, last])"
                )));
            }
        }
        for r in &self.mount_roots {
            if !r.path.is_absolute() {
                return Err(ServerError::Config(format!(
                    "mount root `{}` must be absolute",
                    r.path.display()
                )));
            }
        }
        if self.default_template.as_deref() == Some("") {
            return Err(ServerError::Config("`default_template` is empty".into()));
        }
        if self.selftest_on_start.iter().any(String::is_empty) {
            return Err(ServerError::Config(
                "`selftest_on_start` names an empty template".into(),
            ));
        }
        Ok(())
    }
}

/// The gid of `group`: a number, or a name looked up in `/etc/group`.
pub fn resolve_group(group: &str) -> Result<u32, ServerError> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    let text = std::fs::read_to_string("/etc/group").unwrap_or_default();
    lookup_group(&text, group).ok_or_else(|| {
        ServerError::Config(format!(
            "socket_group `{group}` is not in /etc/group (give the numeric gid)"
        ))
    })
}

fn lookup_group(etc_group: &str, name: &str) -> Option<u32> {
    etc_group.lines().find_map(|line| {
        let mut f = line.split(':');
        (f.next()? == name).then_some(())?;
        f.nth(1)?.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_file_parses() {
        let c = ServerConfig::from_toml(
            r#"
socket = "/run/sandcrab-server/server.sock"
socket_mode = "0660"
socket_group = "sandcrab"
allow_uids = [1500]
backend = "docker"
state_dir = "/var/lib/sandcrab"
templates_dirs = ["/etc/sandcrab/templates"]
default_template = "base"
allow_root_user = false
allow_sandbox_uids = [[60000, 62999]]
allow_sandbox_gids = [[60000, 62999], [70000, 70000]]
selftest_on_start = ["base"]

[limits]
max_sandboxes = 128
default_ttl_secs = 600

[[mount_roots]]
path = "/srv/app-data"
mode = "rw"

[[mount_roots]]
path = "/srv/tools"
mode = "ro"
"#,
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.socket_mode, 0o660);
        assert_eq!(c.backend, Some(BackendName::Docker));
        assert_eq!(c.allowed_uids(0), [1500]);
        assert_eq!(c.limits.max_sandboxes, 128);
        assert_eq!(c.limits.default_ttl_secs, 600);
        assert_eq!(c.limits.max_ttl_secs, DEFAULT_MAX_TTL_SECS);
        assert_eq!(c.mount_roots[1].mode, MountMode::ReadOnly);
        assert_eq!(c.default_template.as_deref(), Some("base"));
        assert_eq!(c.selftest_on_start, ["base"]);
        assert_eq!(c.allow_sandbox_uids, [[60000, 62999]]);
        assert_eq!(c.allow_sandbox_gids[1], [70000, 70000]);
    }

    #[test]
    fn defaults() {
        let c = ServerConfig::from_toml("socket = \"/s/x.sock\"\nbackend = \"libkrun\"").unwrap();
        c.validate().unwrap();
        assert_eq!(c.socket_mode, 0o600);
        assert_eq!(c.allowed_uids(501), [501]);
        assert_eq!(c.limits, LimitsConfig::default());
        assert_eq!(c.limits.max_sandboxes, 64);
        assert!(c.mount_roots.is_empty() && !c.allow_root_user);
        assert!(c.selftest_on_start.is_empty());
    }

    #[test]
    fn refusals() {
        for bad in [
            "sockett = \"/x\"",
            "socket_mode = \"0999\"",
            "socket_mode = \"7777\"",
            "socket_mode = 600",
            "backend = \"qemu\"",
            "[limits]\nmax_processes = 3",
            "[[mount_roots]]\npath = \"/x\"\nmode = \"rx\"",
        ] {
            assert!(ServerConfig::from_toml(bad).is_err(), "{bad}");
        }
        let ok = "socket = \"/s/x.sock\"\nbackend = \"docker\"\n";
        for bad in [
            "backend = \"docker\"",
            "socket = \"rel.sock\"\nbackend = \"docker\"",
            "socket = \"/s/x.sock\"",
        ] {
            let c = ServerConfig::from_toml(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
        for extra in [
            "allow_uids = []",
            "[limits]\nmax_sandboxes = 0",
            "[limits]\ndefault_ttl_secs = 100000",
            "[[mount_roots]]\npath = \"rel\"",
            "default_template = \"\"",
            "selftest_on_start = [\"\"]",
            "allow_sandbox_uids = [[2, 1]]",
        ] {
            let c = ServerConfig::from_toml(&format!("{ok}{extra}")).unwrap();
            assert!(c.validate().is_err(), "{extra}");
        }
    }

    #[test]
    fn mount_root_flags_and_groups() {
        let r: MountRootConfig = "/srv/data:ro".parse().unwrap();
        assert_eq!(
            (r.path.as_path(), r.mode),
            (Path::new("/srv/data"), MountMode::ReadOnly)
        );
        let r: MountRootConfig = "/srv/data".parse().unwrap();
        assert_eq!(r.mode, MountMode::ReadWrite);
        assert!("data:rw".parse::<MountRootConfig>().is_err());

        let etc = "root:x:0:\nsandcrab:x:1234:alice,bob\ndocker:x:999:\n";
        assert_eq!(lookup_group(etc, "sandcrab"), Some(1234));
        assert_eq!(lookup_group(etc, "nope"), None);
        assert_eq!(resolve_group("4321").unwrap(), 4321);
        assert!(parse_mode("660").is_ok() && parse_mode("rw").is_err());
    }
}
