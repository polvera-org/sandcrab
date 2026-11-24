//! [`DockerConfig`] and host-side checks on the injected `crabd` binary.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::{Result, SandboxError, default_state_root};

/// Environment variable overriding the default host path of the static `crabd` binary.
pub const CRABD_ENV: &str = "SANDCRAB_CRABD";

/// Per-container limits that are not part of a template's `[resources]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerLimits {
    /// Max processes + threads in the container (fork-bomb guard, `--pids-limit`).
    pub pids_limit: i64,
    /// Size of the tmpfs mounted at `/tmp`, in MiB.
    pub tmp_size_mb: u64,
    /// SIGTERM → SIGKILL grace when destroying a sandbox.
    pub stop_grace: Duration,
}

impl Default for DockerLimits {
    fn default() -> Self {
        Self {
            // Shared with the microVM guests (`crabd init`), so every backend has one baseline.
            pids_limit: i64::from(sandcrab_protocol::init::SANDBOX_PIDS_MAX),
            tmp_size_mb: sandcrab_protocol::init::SANDBOX_TMP_SIZE_MB,
            stop_grace: Duration::from_secs(2),
        }
    }
}

/// Configuration for [`crate::DockerBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerConfig {
    /// Docker daemon address (`unix:///…`, `tcp://…`). `None` = `$DOCKER_HOST`, else the
    /// local default socket. The daemon must run on this host: workspaces, `crabd` and the
    /// socket directory are bind-mounted from host paths.
    pub docker_host: Option<String>,
    /// Host path of the **static** (musl) `crabd` binary, bind-mounted read-only into every
    /// container. Default: `$SANDCRAB_CRABD`, else
    /// `target/<arch>-unknown-linux-musl/release/crabd` (built by `scripts/build-crabd.sh`).
    pub crabd_path: PathBuf,
    /// Host directory holding one `0700` socket directory per sandbox. Default:
    /// `<state root>/run` (`$SANDCRAB_HOME/run`, else `~/.sandcrab/run`).
    pub state_dir: PathBuf,
    pub limits: DockerLimits,
    /// Run template images that have no host-local pin (`sandcrab_core::pins`). **Defaults
    /// to `false`**: spawning an unpinned template fails with an `UnpinnedImage` error (pin
    /// with `templates/build.sh <name>` or adopt with `sandcrab template pin <name>`).
    pub allow_unpinned_images: bool,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            docker_host: None,
            crabd_path: default_crabd_path(),
            state_dir: default_state_dir(),
            limits: DockerLimits::default(),
            allow_unpinned_images: false,
        }
    }
}

impl DockerConfig {
    /// Former explicit opt-in constructor. The Docker backend no longer needs an opt-in, so
    /// this is [`Self::default`]; kept for one release.
    #[deprecated(note = "the Docker backend needs no opt-in; use `DockerConfig::default()`")]
    #[must_use]
    pub fn allow_insecure() -> Self {
        Self::default()
    }
}

/// `$SANDCRAB_CRABD`, else `target/<host arch>-unknown-linux-musl/release/crabd`.
#[must_use]
pub fn default_crabd_path() -> PathBuf {
    std::env::var_os(CRABD_ENV)
        .filter(|v| !v.is_empty())
        .map_or_else(
            || {
                PathBuf::from(format!(
                    "target/{}-unknown-linux-musl/release/crabd",
                    std::env::consts::ARCH
                ))
            },
            PathBuf::from,
        )
}

/// `<state root>/run`.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    default_state_root().map_or_else(|_| PathBuf::from(".sandcrab/run"), |root| root.join("run"))
}

/// Check that `path` is an executable, statically linked ELF for the host architecture and
/// return its canonical path.
pub fn check_static_crabd(path: &Path) -> Result<PathBuf> {
    let fail = |why: String| {
        SandboxError::InvalidConfig(format!(
            "crabd binary `{}` {why}; build it with `scripts/build-crabd.sh` \
             (or point ${CRABD_ENV} / DockerConfig::crabd_path at a static musl crabd)",
            path.display()
        ))
    };
    let canon = fs::canonicalize(path).map_err(|e| fail(format!("is not accessible ({e})")))?;
    let meta = fs::metadata(&canon).map_err(|e| fail(format!("is not accessible ({e})")))?;
    if !meta.is_file() {
        return Err(fail("is not a regular file".into()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(fail("is not executable".into()));
    }
    // Program headers sit right after the ELF header; 64 KiB is plenty.
    let mut head = Vec::with_capacity(64 * 1024);
    fs::File::open(&canon)
        .and_then(|f| f.take(64 * 1024).read_to_end(&mut head))
        .map_err(|e| fail(format!("cannot be read ({e})")))?;
    inspect_elf(&head, std::env::consts::ARCH).map_err(fail)?;
    Ok(canon)
}

const PT_INTERP: u32 = 3;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;

/// ELF `e_machine` for a Rust `target_arch`.
fn elf_machine(arch: &str) -> Option<u16> {
    match arch {
        "x86_64" => Some(62),
        "aarch64" => Some(183),
        _ => None,
    }
}

/// Validate a 64-bit little-endian ELF executable for `arch` with no `PT_INTERP`
/// (i.e. static or static-pie). `bytes` must cover the ELF and program headers.
pub(crate) fn inspect_elf(bytes: &[u8], arch: &str) -> std::result::Result<(), String> {
    let not_elf = || "is not an ELF executable".to_owned();
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
        return Err(not_elf());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err("is not a 64-bit little-endian ELF".into());
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let e_type = u16_at(16);
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(not_elf());
    }
    let machine = u16_at(18);
    if let Some(want) = elf_machine(arch)
        && machine != want
    {
        return Err(format!(
            "targets ELF machine {machine}, but the host ({arch}) needs {want}"
        ));
    }
    let mut phoff = [0u8; 8];
    phoff.copy_from_slice(&bytes[32..40]);
    let phoff = usize::try_from(u64::from_le_bytes(phoff)).map_err(|_| not_elf())?;
    let phentsize = usize::from(u16_at(54));
    let phnum = usize::from(u16_at(56));
    if phentsize < 4 {
        return Err(not_elf());
    }
    for i in 0..phnum {
        let off = i
            .checked_mul(phentsize)
            .and_then(|x| x.checked_add(phoff))
            .filter(|o| o.checked_add(4).is_some_and(|end| end <= bytes.len()))
            .ok_or_else(|| "has truncated program headers".to_owned())?;
        let p_type =
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        if p_type == PT_INTERP {
            return Err(
                "is dynamically linked (has an ELF interpreter); crabd must be a static \
                 musl binary"
                    .into(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ELF64 LE header + `phnum` program headers of the given types.
    fn elf(machine: u16, e_type: u16, ptypes: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 56 * ptypes.len()];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&e_type.to_le_bytes());
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&u16::try_from(ptypes.len()).unwrap().to_le_bytes());
        for (i, t) in ptypes.iter().enumerate() {
            let o = 64 + 56 * i;
            b[o..o + 4].copy_from_slice(&t.to_le_bytes());
        }
        b
    }

    #[test]
    fn config_defaults_are_safe() {
        let c = DockerConfig::default();
        assert!(!c.allow_unpinned_images);
        assert!(c.docker_host.is_none());
        assert!(c.crabd_path.ends_with("crabd"));
        assert!(c.state_dir.ends_with("run"));
        assert_eq!(c.limits.stop_grace, Duration::from_secs(2));
        #[allow(deprecated)]
        let legacy = DockerConfig::allow_insecure();
        assert_eq!(
            legacy, c,
            "deprecated constructor is a no-op alias of default()"
        );
    }

    #[test]
    fn static_elf_accepted() {
        // PT_LOAD(1), PT_GNU_STACK — static; ET_DYN = static-pie.
        inspect_elf(&elf(62, ET_EXEC, &[1, 0x6474_e551]), "x86_64").unwrap();
        inspect_elf(&elf(62, ET_DYN, &[1]), "x86_64").unwrap();
        inspect_elf(&elf(183, ET_DYN, &[1]), "aarch64").unwrap();
    }

    #[test]
    fn bad_elf_rejected() {
        let dynamic = inspect_elf(&elf(62, ET_DYN, &[6, PT_INTERP, 1]), "x86_64").unwrap_err();
        assert!(dynamic.contains("dynamically linked"), "{dynamic}");
        let arch = inspect_elf(&elf(183, ET_EXEC, &[1]), "x86_64").unwrap_err();
        assert!(arch.contains("machine"), "{arch}");
        assert!(inspect_elf(b"#!/bin/sh\necho hi\n", "x86_64").is_err());
        assert!(inspect_elf(&elf(62, 1, &[1]), "x86_64").is_err()); // ET_REL
        let mut truncated = elf(62, ET_EXEC, &[1, 1]);
        truncated.truncate(64 + 56 + 2);
        assert!(inspect_elf(&truncated, "x86_64").is_err());
    }

    #[test]
    fn check_static_crabd_errors_name_the_build_script() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = check_static_crabd(&tmp.path().join("nope")).unwrap_err();
        assert!(
            missing.to_string().contains("scripts/build-crabd.sh"),
            "{missing}"
        );

        let script = tmp.path().join("crabd");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let not_elf = check_static_crabd(&script).unwrap_err().to_string();
        assert!(not_elf.contains("not an ELF"), "{not_elf}");

        let fake = tmp.path().join("crabd-static");
        let arch = std::env::consts::ARCH;
        let Some(machine) = elf_machine(arch) else {
            return;
        };
        fs::write(&fake, elf(machine, ET_DYN, &[1])).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            check_static_crabd(&fake)
                .unwrap_err()
                .to_string()
                .contains("not executable")
        );
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            check_static_crabd(&fake).unwrap(),
            fs::canonicalize(&fake).unwrap()
        );
    }
}
