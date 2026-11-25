//! Hardening sysctls written by `crabd init` before any sandbox code runs
//! ([`HARDENING_SYSCTLS`]). Best effort per key: a sysctl the kernel lacks is skipped, one it
//! refuses is reported, and neither stops the boot. Each write is read back, so a kernel that
//! accepts a value but stores another (clamping) counts as refused for that value.
//!
//! Generic over the `/proc/sys` root so it is unit-tested against a temp dir.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub use sandcrab_protocol::init::{HARDENING_SYSCTLS, HardeningSysctl};

/// The kernel's sysctl tree.
pub const PROC_SYS: &str = "/proc/sys";

/// What happened to one sysctl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Set; the value read back.
    Set(&'static str),
    /// The kernel has no such sysctl (feature compiled out or a distro-only knob).
    Missing,
    /// Every accepted value was refused; the last error.
    Failed(String),
}

/// `kernel.kptr_restrict` → `<root>/kernel/kptr_restrict`.
#[must_use]
pub fn key_path(root: &Path, key: &str) -> PathBuf {
    key.split('.')
        .fold(root.to_path_buf(), |p, part| p.join(part))
}

/// Write `s` under `root`, trying its values in order.
#[must_use]
pub fn apply(root: &Path, s: &HardeningSysctl) -> Outcome {
    let path = key_path(root, s.key);
    if !path.exists() {
        return Outcome::Missing;
    }
    let mut last = String::from("no value to write");
    for &value in s.values {
        match write_and_check(&path, value) {
            Ok(()) => return Outcome::Set(value),
            Err(e) => last = format!("{value}: {e}"),
        }
    }
    Outcome::Failed(last)
}

fn write_and_check(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)?;
    let got = fs::read_to_string(path)?;
    if got.trim() == value {
        Ok(())
    } else {
        Err(io::Error::other(format!("reads back as {}", got.trim())))
    }
}

/// [`apply`] every entry of `list`.
#[must_use]
pub fn apply_all(
    root: &Path,
    list: &'static [HardeningSysctl],
) -> Vec<(&'static HardeningSysctl, Outcome)> {
    list.iter().map(|s| (s, apply(root, s))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(root: &Path, key: &str, v: &str) -> PathBuf {
        let p = key_path(root, key);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, v).unwrap();
        p
    }

    #[test]
    fn path_of_a_dotted_key() {
        assert_eq!(
            key_path(Path::new("/proc/sys"), "kernel.kptr_restrict"),
            Path::new("/proc/sys/kernel/kptr_restrict")
        );
    }

    #[test]
    fn sets_present_and_skips_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = seed(d.path(), "kernel.io_uring_disabled", "0\n");
        let s = HardeningSysctl {
            key: "kernel.io_uring_disabled",
            values: &["2", "1"],
        };
        assert_eq!(apply(d.path(), &s), Outcome::Set("2"));
        assert_eq!(fs::read_to_string(p).unwrap(), "2");
        let missing = HardeningSysctl {
            key: "kernel.unprivileged_userns_clone",
            values: &["0"],
        };
        assert_eq!(apply(d.path(), &missing), Outcome::Missing);
    }

    #[test]
    fn falls_back_to_the_next_value_and_reports_failure() {
        let d = tempfile::tempdir().unwrap();
        // A directory where the file should be: every write fails.
        fs::create_dir_all(key_path(d.path(), "kernel.perf_event_paranoid")).unwrap();
        let s = HardeningSysctl {
            key: "kernel.perf_event_paranoid",
            values: &["3", "2"],
        };
        match apply(d.path(), &s) {
            Outcome::Failed(e) => assert!(e.starts_with("2: "), "{e}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn whole_list_against_an_empty_tree_is_all_missing() {
        let d = tempfile::tempdir().unwrap();
        let out = apply_all(d.path(), HARDENING_SYSCTLS);
        assert_eq!(out.len(), HARDENING_SYSCTLS.len());
        assert!(out.iter().all(|(_, o)| *o == Outcome::Missing));
    }
}
