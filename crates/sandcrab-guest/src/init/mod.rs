//! `crabd init`: PID 1 of a Sandcrab microVM (see `docs/design/firecracker-backend.md` §3).
//!
//! Pure, portable parts (unit-tested everywhere):
//! - [`config`] — `InitConfig` re-validation and the workspace mount plan (ext4 block
//!   drives and libkrun virtio-fs shares);
//! - [`dirs`] — symlink-safe creation of mount points and `default_cwd`;
//! - [`drives`] — `drive_id` → `/dev/vdX` via sysfs serial, ext4-label fallback;
//! - [`command`] — the privilege-dropped `crabd serve` child command;
//! - [`export`] — `pack_tree` / `stream_tree`, the guest side of workspace sync-back;
//! - [`control`] — the `init.*` control-connection handler, generic over [`control::InitOps`];
//! - [`netlink`] — the one `RTM_NEWLINK` message that brings `lo` up;
//! - [`reap`] — the reaper's `waitpid(-1)` loop (any process group, so orphans too).
//! - [`sysctl`] — the hardening sysctls, written and read back under `/proc/sys`.
//!
//! Linux-only: `sys` (mounts, pivot_root, reboot, ...) and `pid1` (boot sequence, reaper,
//! the real `InitOps`). Those need a booted microVM and are covered by the Firecracker e2e
//! suite.

pub mod command;
pub mod config;
pub mod control;
pub mod dirs;
pub mod drives;
pub mod export;
pub mod netlink;
#[cfg(target_os = "linux")]
mod pid1;
pub mod reap;
#[cfg(target_os = "linux")]
pub mod sys;
pub mod sysctl;

use std::ffi::OsString;

/// Whether this process should run as `crabd init`: explicitly (`crabd init`), or as PID 1
/// unless the first argument is `serve`. The kernel starts `init=/sbin/crabd` with no
/// arguments, or with stray cmdline words it did not recognise; `crabd serve ...` as PID 1
/// (a container entrypoint) stays in serve mode.
#[must_use]
pub fn is_init_invocation(pid: u32, args: &[OsString]) -> bool {
    match args.get(1) {
        Some(a) if a == "init" => true,
        Some(a) if a == "serve" => false,
        _ => pid == 1,
    }
}

/// Run PID 1. Refuses unless this process really is PID 1 (it mounts, pivots and reboots).
pub async fn run() -> std::process::ExitCode {
    if std::process::id() != 1 {
        eprintln!("crabd init must run as PID 1 inside a Sandcrab microVM");
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(target_os = "linux")]
    {
        pid1::run().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("crabd init is only supported on Linux");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn role_selection() {
        assert!(is_init_invocation(1, &args(&["/sbin/crabd"])));
        assert!(!is_init_invocation(7, &args(&["crabd"])));
        assert!(is_init_invocation(7, &args(&["crabd", "init"])));
        assert!(is_init_invocation(1, &args(&["/sbin/crabd", "nomodeset"])));
        assert!(!is_init_invocation(7, &args(&["crabd", "nomodeset"])));
        assert!(!is_init_invocation(
            1,
            &args(&["crabd", "serve", "--unix", "/s"])
        ));
    }
}
