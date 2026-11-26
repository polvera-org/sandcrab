//! `sandcrab doctor --server-config <FILE>`: the checks for a server's config (design §5.8,
//! §5.9). Run it as the user the server runs as (root for the Linux service).
//!
//! The file must parse and validate; the socket directory, if it exists, must pass the
//! server's own start check; the allow list is shown (and a root server that admits only
//! root is a warning); every mount root must exist, be a directory and lie outside the state
//! directory, and the self-test needs one read-write root. The backend checks and the
//! template pins run as usual, with the config's backend, state and template directories,
//! and the templates the server names are summed up from them.

use std::path::{Path, PathBuf};

use sandcrab_sdk::protocol::api::MountMode;
use sandcrab_server::ServerConfig;

use crate::doctor::Status;

/// One check: name, status, detail.
pub type Line = (String, Status, String);

fn line(name: &str, status: Status, detail: impl Into<String>) -> Line {
    (name.to_owned(), status, detail.into())
}

/// Load and validate the file.
pub fn load(path: &Path) -> Result<ServerConfig, String> {
    let c = ServerConfig::load(path).map_err(|e| e.to_string())?;
    c.validate().map_err(|e| e.to_string())?;
    Ok(c)
}

/// The resolved path when it exists, else the path as given.
fn resolved(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The checks that need only the config, the uid the server runs as and its state directory.
#[must_use]
pub fn checks(c: &ServerConfig, uid: u32, state_dir: &Path) -> Vec<Line> {
    let mut out = Vec::new();
    socket_dir(c, uid, &mut out);
    allow_list(c, uid, &mut out);
    mount_roots(c, state_dir, &mut out);
    out
}

fn socket_dir(c: &ServerConfig, uid: u32, out: &mut Vec<Line>) {
    const NAME: &str = "server:socket_dir";
    let Some(dir) = c.socket.as_deref().and_then(Path::parent) else {
        return out.push(line(NAME, Status::Fail, "the socket has no directory"));
    };
    let Ok(meta) = std::fs::metadata(dir) else {
        return out.push(line(
            NAME,
            Status::Info,
            format!(
                "{} does not exist yet (the server creates it 0700, or 0710 with socket_group)",
                dir.display()
            ),
        ));
    };
    if let Err(e) = sandcrab_sdk::core::private_dir::check_foreign_dir(
        dir,
        uid,
        "the server would refuse to start",
    ) {
        return out.push(line(NAME, Status::Fail, e.to_string()));
    }
    let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
    let socket_name = c.socket.as_deref().and_then(Path::file_name);
    let others: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name())
                .filter(|n| Some(n.as_os_str()) != socket_name)
                .map(|n| n.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if others.is_empty() {
        out.push(line(
            NAME,
            Status::Ok,
            format!("{} (mode {mode:o})", dir.display()),
        ));
    } else {
        out.push(line(
            NAME,
            Status::Warn,
            format!(
                "{} holds more than the socket ({}): a client mounts this directory, so keep \
                 it for the socket alone",
                dir.display(),
                others.join(", ")
            ),
        ));
    }
}

fn allow_list(c: &ServerConfig, uid: u32, out: &mut Vec<Line>) {
    const NAME: &str = "server:allow_uids";
    let allowed = c.allowed_uids(uid);
    if c.allow_uids.is_none() && uid == 0 {
        out.push(line(
            NAME,
            Status::Warn,
            "no allow_uids: a root server admits only root; list the client's uid",
        ));
    } else {
        let list: Vec<String> = allowed.iter().map(u32::to_string).collect();
        out.push(line(
            NAME,
            Status::Ok,
            format!(
                "uids admitted: {}{}",
                list.join(", "),
                if c.allow_uids.is_none() {
                    " (the server's own, the default)"
                } else {
                    ""
                }
            ),
        ));
    }
    if c.socket_mode & 0o007 != 0 {
        out.push(line(
            "server:socket_mode",
            Status::Warn,
            format!(
                "socket_mode {:o} lets every local user connect (the uid check still applies); \
                 prefer 0600, or 0660 with socket_group",
                c.socket_mode
            ),
        ));
    }
}

fn mount_roots(c: &ServerConfig, state_dir: &Path, out: &mut Vec<Line>) {
    const NAME: &str = "server:mount_roots";
    if c.mount_roots.is_empty() {
        out.push(line(
            NAME,
            Status::Info,
            "no mount roots: every mount is refused, and server.selftest cannot run (it needs a \
             read-write root for its scratch directory)",
        ));
        return;
    }
    let state = resolved(state_dir);
    let mut rw = false;
    for r in &c.mount_roots {
        let name = format!("server:mount_root:{}", r.path.display());
        rw |= r.mode == MountMode::ReadWrite;
        let path = resolved(&r.path);
        match std::fs::metadata(&path) {
            Err(e) => {
                out.push(line(&name, Status::Fail, format!("cannot be read: {e}")));
                continue;
            }
            Ok(m) if !m.is_dir() => {
                out.push(line(&name, Status::Fail, "is not a directory"));
                continue;
            }
            Ok(_) => {}
        }
        if path.starts_with(&state) {
            out.push(line(
                &name,
                Status::Fail,
                format!(
                    "lies inside the state directory {}: the server's own files must not be \
                     mountable",
                    state.display()
                ),
            ));
            continue;
        }
        let mode = match r.mode {
            MountMode::ReadOnly => "ro",
            MountMode::ReadWrite => "rw",
        };
        out.push(line(&name, Status::Ok, format!("exists ({mode})")));
    }
    if !rw {
        out.push(line(
            NAME,
            Status::Info,
            "no read-write root: server.selftest cannot run (it needs one for its scratch \
             directory)",
        ));
    }
}

/// The templates the server names: `default_template` and `selftest_on_start`.
#[must_use]
pub fn named_templates(c: &ServerConfig) -> Vec<String> {
    let mut t: Vec<String> = c.default_template.iter().cloned().collect();
    for s in &c.selftest_on_start {
        if !t.contains(s) {
            t.push(s.clone());
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use sandcrab_server::{BackendName, MountRootConfig};

    use super::*;

    fn config(tmp: &Path) -> ServerConfig {
        ServerConfig {
            socket: Some(tmp.join("run/server.sock")),
            backend: Some(BackendName::Docker),
            ..ServerConfig::default()
        }
    }

    fn me() -> u32 {
        sandcrab_sdk::core::private_dir::current_uid()
    }

    fn find<'a>(lines: &'a [Line], name: &str) -> &'a Line {
        lines
            .iter()
            .find(|l| l.0 == name)
            .unwrap_or_else(|| panic!("no {name} in {lines:?}"))
    }

    #[test]
    fn config_must_parse_and_validate() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("server.toml");
        std::fs::write(
            &p,
            "socket = \"/run/sandcrab/server.sock\"\nbackend = \"docker\"\n",
        )
        .unwrap();
        assert!(load(&p).is_ok());
        std::fs::write(&p, "sockett = \"/x\"\n").unwrap();
        assert!(load(&p).unwrap_err().contains("sockett"));
        std::fs::write(&p, "socket = \"/run/sandcrab/server.sock\"\n").unwrap();
        assert!(load(&p).unwrap_err().contains("no backend"));
        assert!(load(&tmp.path().join("missing.toml")).is_err());
    }

    #[test]
    fn socket_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let c = config(tmp.path());
        let state = tmp.path().join("state");
        // Missing: created at start.
        assert_eq!(
            find(&checks(&c, me(), &state), "server:socket_dir").1,
            Status::Info
        );
        // Private: ok.
        let run = tmp.path().join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find(&checks(&c, me(), &state), "server:socket_dir").1,
            Status::Ok
        );
        // Something else in it: a warning.
        std::fs::write(run.join("other"), "x").unwrap();
        let l = checks(&c, me(), &state);
        let l = find(&l, "server:socket_dir");
        assert_eq!(l.1, Status::Warn, "{l:?}");
        assert!(l.2.contains("other"));
        // Writable by others: the start would fail.
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            find(&checks(&c, me(), &state), "server:socket_dir").1,
            Status::Fail
        );
        // Owned by another uid (not root): fails too.
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        if me() != 0 {
            assert_eq!(
                find(&checks(&c, me() + 1, &state), "server:socket_dir").1,
                Status::Fail
            );
        }
    }

    #[test]
    fn allow_list_and_socket_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let mut c = config(tmp.path());
        let l = checks(&c, 1000, &state);
        let a = find(&l, "server:allow_uids");
        assert_eq!(a.1, Status::Ok);
        assert!(a.2.contains("1000"), "{a:?}");
        // A root server with no list admits only root.
        assert_eq!(
            find(&checks(&c, 0, &state), "server:allow_uids").1,
            Status::Warn
        );
        c.allow_uids = Some(vec![1500, 1501]);
        let l = checks(&c, 0, &state);
        let a = find(&l, "server:allow_uids");
        assert_eq!(a.1, Status::Ok);
        assert!(a.2.contains("1500, 1501"), "{a:?}");
        assert!(l.iter().all(|l| l.0 != "server:socket_mode"));
        c.socket_mode = 0o666;
        assert_eq!(
            find(&checks(&c, 0, &state), "server:socket_mode").1,
            Status::Warn
        );
    }

    #[test]
    fn mount_roots_exist_and_stay_out_of_the_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(state.join("inner")).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::write(tmp.path().join("file"), "x").unwrap();
        let mut c = config(tmp.path());

        // None: every mount refused, no self-test.
        let l = checks(&c, me(), &state);
        assert_eq!(find(&l, "server:mount_roots").1, Status::Info);

        let root = |p: PathBuf, mode| MountRootConfig { path: p, mode };
        c.mount_roots = vec![
            root(data.clone(), MountMode::ReadOnly),
            root(tmp.path().join("missing"), MountMode::ReadWrite),
            root(tmp.path().join("file"), MountMode::ReadWrite),
            root(state.join("inner"), MountMode::ReadWrite),
        ];
        let l = checks(&c, me(), &state);
        let st = |p: &Path| find(&l, &format!("server:mount_root:{}", p.display())).1;
        assert_eq!(st(&data), Status::Ok);
        assert_eq!(st(&tmp.path().join("missing")), Status::Fail);
        assert_eq!(st(&tmp.path().join("file")), Status::Fail);
        assert_eq!(st(&state.join("inner")), Status::Fail);
        // Only the read-only root is usable... but a rw one is configured: no self-test note.
        assert!(l.iter().all(|l| l.0 != "server:mount_roots"), "{l:?}");

        c.mount_roots = vec![root(data, MountMode::ReadOnly)];
        let l = checks(&c, me(), &state);
        let n = find(&l, "server:mount_roots");
        assert_eq!(n.1, Status::Info);
        assert!(n.2.contains("selftest"), "{n:?}");
    }

    #[test]
    fn named_templates_are_the_default_and_the_selftests() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = config(tmp.path());
        assert!(named_templates(&c).is_empty());
        c.default_template = Some("base".into());
        c.selftest_on_start = vec!["base".into(), "py".into()];
        assert_eq!(named_templates(&c), ["base", "py"]);
    }
}
