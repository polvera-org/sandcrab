//! Resolved mounts → virtio-fs shares / block disks, plus identity re-check data for the
//! helper (design §8).
//!
//! - **Host-dir / managed-dir workspaces** become one live virtio-fs share each
//!   (`krun_add_virtiofs3`). Read-only is enforced twice: by libkrun's host-side server
//!   (`read_only = true`, which a guest remount cannot lift) and by the guest mount
//!   (`MS_RDONLY`, from [`GuestMount::read_only`]).
//! - **Block-volume workspaces** are attached with `krun_add_disk` (parity with Firecracker).
//!
//! **Tag scheme:** mount `n` (its index in `ResolvedSpec::mounts`) gets the tag / block id
//! `ws<n>`. That is 3–4 bytes of `[a-z0-9]`, so it satisfies the guest rule (1–36 bytes of
//! `[A-Za-z0-9_.:-]`), the helper's stricter device-id rule (`[A-Za-z0-9_-]`) and the 20-byte
//! virtio-blk serial limit, and it is unique by construction. It never collides with the
//! `rootfs` / `scratch` drive ids.
//!
//! **Leases and (dev, ino) pinning:** the manager holds the rw (exclusive) or ro (shared)
//! lease for the sandbox's whole life, so host mutation stays refused while it runs; the
//! backend never touches workspace contents. Each dir-backed root is re-verified
//! (symlink-free, same `(dev, ino)` as at lease time) right before the share is planned, and
//! that identity is handed to the helper, which checks it again immediately before
//! `krun_add_virtiofs3`.

use std::path::{Path, PathBuf};

use sandcrab_core::{MountMode, ResolvedMount, Result, SandboxError, WorkspaceBacking};
use sandcrab_protocol::init::{GuestMount, MAX_VIRTIOFS_TAG_LEN};

use crate::helper_proto::{
    DiskSpec, HostIdentity, MAX_EXTRA_DISKS, MAX_VIRTIOFS_SHARES, VirtioFsShare,
};

/// Tag / block id of mount `index`.
#[must_use]
pub fn mount_tag(index: usize) -> String {
    format!("ws{index}")
}

/// Whether `tag` satisfies the guest virtio-fs tag rule (1–36 bytes of `[A-Za-z0-9_.:-]`).
#[must_use]
pub fn is_valid_guest_tag(tag: &str) -> bool {
    (1..=MAX_VIRTIOFS_TAG_LEN).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// The helper's half (devices) and the guest's half (`init.configure` mounts) of a spec's
/// workspace mounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountPlan {
    pub shares: Vec<VirtioFsShare>,
    pub disks: Vec<DiskSpec>,
    pub guest_mounts: Vec<GuestMount>,
}

/// Plan every mount, re-verifying dir-backed roots against their lease identity.
pub fn plan_mounts(mounts: &[ResolvedMount]) -> Result<MountPlan> {
    plan_with(mounts, |m| {
        sandcrab_core::verify_workspace_root(&m.workspace_name, &m.host_path, m.root_identity).map(
            |id| HostIdentity {
                dev: id.dev,
                ino: id.ino,
            },
        )
    })
}

/// [`plan_mounts`] with an injectable root check (pure, for tests).
pub(crate) fn plan_with(
    mounts: &[ResolvedMount],
    mut verify_root: impl FnMut(&ResolvedMount) -> Result<HostIdentity>,
) -> Result<MountPlan> {
    let mut plan = MountPlan::default();
    for (n, m) in mounts.iter().enumerate() {
        let tag = mount_tag(n);
        debug_assert!(is_valid_guest_tag(&tag));
        let read_only = m.mode == MountMode::ReadOnly;
        check_host_path(m)?;
        match &m.backing {
            WorkspaceBacking::HostDir(_) | WorkspaceBacking::ManagedDir(_) => {
                if plan.shares.len() == MAX_VIRTIOFS_SHARES {
                    return Err(SandboxError::InvalidConfig(format!(
                        "the libkrun backend supports at most {MAX_VIRTIOFS_SHARES} directory \
                         workspace mounts per sandbox"
                    )));
                }
                let identity = verify_root(m)?;
                plan.shares.push(VirtioFsShare {
                    tag: tag.clone(),
                    host_path: m.host_path.clone(),
                    read_only,
                    identity: Some(identity),
                });
                plan.guest_mounts
                    .push(GuestMount::virtio_fs(tag, m.guest_path.clone(), read_only));
            }
            WorkspaceBacking::BlockVolume(_) => {
                if plan.disks.len() == MAX_EXTRA_DISKS {
                    return Err(SandboxError::InvalidConfig(format!(
                        "the libkrun backend supports at most {MAX_EXTRA_DISKS} block-volume \
                         workspace mounts per sandbox"
                    )));
                }
                plan.disks.push(DiskSpec {
                    block_id: tag.clone(),
                    path: m.host_path.clone(),
                    read_only,
                    sha256: None,
                    stamp: None,
                });
                // The volume's own ext4 label is unknown: resolve by serial only.
                plan.guest_mounts.push(GuestMount::block(
                    tag,
                    None,
                    m.guest_path.clone(),
                    read_only,
                ));
            }
        }
    }
    Ok(plan)
}

/// The helper needs an absolute, normalized host path (the lease's canonical path); fail here
/// with the workspace name rather than later with a field index.
fn check_host_path(m: &ResolvedMount) -> Result<()> {
    let p: &Path = &m.host_path;
    let normalized = p.is_absolute()
        && p.components().all(|c| {
            !matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        });
    if normalized {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "workspace `{}` host path `{}` is not an absolute canonical path",
            m.workspace_name,
            p.display()
        )))
    }
}

/// Host paths of every dir-backed mount (for logs / `doctor`).
#[must_use]
pub fn shared_roots(plan: &MountPlan) -> Vec<PathBuf> {
    plan.shares.iter().map(|s| s.host_path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use sandcrab_core::{RootIdentity, WorkspaceId};
    use sandcrab_protocol::init::MountSource;

    use super::*;

    fn mount(backing: WorkspaceBacking, guest: &str, mode: MountMode) -> ResolvedMount {
        ResolvedMount {
            workspace_id: WorkspaceId::new(),
            workspace_name: format!("ws-{guest}"),
            host_path: backing.path().to_path_buf(),
            backing,
            guest_path: guest.into(),
            mode,
            generation: 1,
            root_identity: Some(RootIdentity { dev: 7, ino: 9 }),
        }
    }

    fn fixed(_: &ResolvedMount) -> Result<HostIdentity> {
        Ok(HostIdentity { dev: 7, ino: 9 })
    }

    #[test]
    fn tags_satisfy_every_rule() {
        // Mount counts are capped far below this (MAX_VIRTIOFS_SHARES + MAX_EXTRA_DISKS).
        for n in [0, 1, 15, 23, 999_999] {
            let t = mount_tag(n);
            assert!(is_valid_guest_tag(&t), "{t}");
            assert!(t.len() <= crate::helper_proto::MAX_BLOCK_ID, "{t}");
            assert!(
                t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
        }
        assert!(!is_valid_guest_tag(""));
        assert!(!is_valid_guest_tag(&"t".repeat(37)));
        assert!(!is_valid_guest_tag("a/b"));
        assert!(is_valid_guest_tag("a.b:c-d_e"));
    }

    #[test]
    fn dir_mounts_become_virtio_fs_shares() {
        let mounts = [
            mount(
                WorkspaceBacking::HostDir("/Users/u/proj".into()),
                "/workspace",
                MountMode::ReadWrite,
            ),
            mount(
                WorkspaceBacking::ManagedDir("/s/workspaces/1/data".into()),
                "/ref",
                MountMode::ReadOnly,
            ),
            mount(
                WorkspaceBacking::BlockVolume("/v/vol.ext4".into()),
                "/vol",
                MountMode::ReadWrite,
            ),
        ];
        let p = plan_with(&mounts, fixed).unwrap();
        assert_eq!(
            p.shares,
            [
                VirtioFsShare {
                    tag: "ws0".into(),
                    host_path: "/Users/u/proj".into(),
                    read_only: false,
                    identity: Some(HostIdentity { dev: 7, ino: 9 }),
                },
                VirtioFsShare {
                    tag: "ws1".into(),
                    host_path: "/s/workspaces/1/data".into(),
                    read_only: true,
                    identity: Some(HostIdentity { dev: 7, ino: 9 }),
                },
            ]
        );
        assert_eq!(
            p.disks,
            [DiskSpec {
                block_id: "ws2".into(),
                path: "/v/vol.ext4".into(),
                read_only: false,
                sha256: None,
                stamp: None,
            }]
        );
        assert_eq!(
            p.guest_mounts,
            [
                GuestMount::virtio_fs("ws0", "/workspace", false),
                GuestMount::virtio_fs("ws1", "/ref", true),
                GuestMount::block("ws2", None, "/vol", false),
            ]
        );
        // Read-only reaches the guest mount too (MS_RDONLY, defense in depth).
        assert!(p.guest_mounts[1].read_only);
        assert!(matches!(
            &p.guest_mounts[1].source,
            MountSource::VirtioFs { tag } if tag == "ws1"
        ));
        assert_eq!(shared_roots(&p).len(), 2);
    }

    #[test]
    fn identity_failure_aborts_the_plan() {
        let mounts = [mount(
            WorkspaceBacking::HostDir("/p".into()),
            "/w",
            MountMode::ReadWrite,
        )];
        let err = plan_with(&mounts, |m| {
            Err(SandboxError::WorkspaceRootChanged(m.workspace_name.clone()))
        })
        .unwrap_err();
        assert!(
            matches!(err, SandboxError::WorkspaceRootChanged(_)),
            "{err}"
        );
    }

    #[test]
    fn block_volumes_are_not_identity_checked() {
        let mounts = [mount(
            WorkspaceBacking::BlockVolume("/v/a.ext4".into()),
            "/w",
            MountMode::ReadOnly,
        )];
        let p = plan_with(&mounts, |_| panic!("no dir check for block volumes")).unwrap();
        assert!(p.disks[0].read_only);
    }

    #[test]
    fn real_root_check_pins_dev_ino() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let mut m = mount(
            WorkspaceBacking::HostDir(root.clone()),
            "/w",
            MountMode::ReadWrite,
        );
        m.root_identity = None;
        let p = plan_mounts(std::slice::from_ref(&m)).unwrap();
        let id = p.shares[0].identity.unwrap();
        // The planned identity is the real one: pinning it on the lease passes…
        m.root_identity = Some(RootIdentity {
            dev: id.dev,
            ino: id.ino,
        });
        plan_mounts(std::slice::from_ref(&m)).unwrap();
        // …and a different one (root replaced since the lease) is refused.
        m.root_identity = Some(RootIdentity {
            dev: id.dev,
            ino: id.ino + 1,
        });
        assert!(plan_mounts(std::slice::from_ref(&m)).is_err());
    }

    #[test]
    fn limits_and_paths() {
        let many: Vec<_> = (0..=MAX_VIRTIOFS_SHARES)
            .map(|i| {
                mount(
                    WorkspaceBacking::HostDir(format!("/p/{i}").into()),
                    &format!("/w{i}"),
                    MountMode::ReadOnly,
                )
            })
            .collect();
        let err = plan_with(&many, fixed).unwrap_err().to_string();
        assert!(err.contains("at most"), "{err}");
        assert!(plan_with(&many[..MAX_VIRTIOFS_SHARES], fixed).is_ok());

        let disks: Vec<_> = (0..=MAX_EXTRA_DISKS)
            .map(|i| {
                mount(
                    WorkspaceBacking::BlockVolume(format!("/v/{i}.ext4").into()),
                    &format!("/d{i}"),
                    MountMode::ReadOnly,
                )
            })
            .collect();
        assert!(plan_with(&disks, fixed).is_err());

        let mut rel = mount(
            WorkspaceBacking::HostDir("/p".into()),
            "/w",
            MountMode::ReadWrite,
        );
        rel.host_path = "rel/p".into();
        assert!(plan_with(&[rel.clone()], fixed).is_err());
        rel.host_path = "/a/../p".into();
        assert!(plan_with(&[rel], fixed).is_err());
    }
}
