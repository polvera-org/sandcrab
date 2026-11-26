//! Builds the real `base` template rootfs. Requires docker, the locally built `sandcrab/base:0.1.0`
//! image, e2fsprogs (`debugfs`, `e2fsck`) and a crabd binary in `SANDCRAB_TEST_CRABD`.
//!
//! ```sh
//! SANDCRAB_TEST_CRABD=${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/crabd \
//!   cargo test -p sandcrab-rootfs -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

use sandcrab_rootfs::{Arch, ROOTFS_LABEL, RootfsBuilder, RootfsRequest};

fn debugfs(img: &std::path::Path, req: &str) -> String {
    let out = Command::new("debugfs")
        .args(["-R", req])
        .arg(img)
        .output()
        .expect("debugfs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "requires docker + sandcrab/base:0.1.0 + SANDCRAB_TEST_CRABD"]
fn builds_base_rootfs() {
    let crabd = PathBuf::from(
        std::env::var_os("SANDCRAB_TEST_CRABD").expect("set SANDCRAB_TEST_CRABD to a crabd binary"),
    );
    let cache = tempfile::tempdir().unwrap();
    let builder = RootfsBuilder::new(cache.path());
    let req = RootfsRequest {
        image: "sandcrab/base:0.1.0".into(),
        expected_image_id: None,
        template: Some("base".into()),
        crabd,
        arch: Arch::host().unwrap(),
    };
    let built = builder.build(&req).unwrap();
    assert!(!built.cache_hit);
    assert_eq!(built.manifest.arch, req.arch);
    let img = &built.image_path;

    let fsck = Command::new("e2fsck").arg("-fn").arg(img).output().unwrap();
    assert!(
        fsck.status.success(),
        "e2fsck: {}",
        String::from_utf8_lossy(&fsck.stdout)
    );

    let sb = Command::new("dumpe2fs")
        .arg("-h")
        .arg(img)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&sb.stdout).contains(&format!("volume name:   {ROOTFS_LABEL}"))
    );

    let stat = debugfs(img, &format!("stat {}", built.manifest.crabd_path));
    assert!(stat.contains("Mode:  0755"), "{stat}");
    assert!(stat.contains("User:     0   Group:     0"), "{stat}");
    assert!(debugfs(img, "stat /sbin").contains("Type: symlink"));
    let passwd = debugfs(img, "cat /etc/passwd");
    assert!(passwd.contains("sandcrab:x:1000:1000"), "{passwd}");
    for d in [
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/tmp",
        "/workspace",
        "/mnt/scratch",
        "/mnt/newroot",
    ] {
        assert!(
            debugfs(img, &format!("stat {d}")).contains("Type: directory"),
            "{d}"
        );
    }
    let usr = debugfs(img, "stat /usr/bin/bash");
    assert!(usr.contains("User:     0"), "{usr}");
    assert!(!debugfs(img, "ls -l /").contains(".dockerenv"));

    let hit = builder.build(&req).unwrap();
    assert!(hit.cache_hit);
    assert_eq!(hit.manifest, built.manifest);
}

/// Third-party images, pinned by multi-arch index digest and pulled when missing: one with no
/// shell whose default user is not root (`static-debian12:nonroot`, uid 65532), and one with
/// Node, git and Python (`node:24-bookworm`).
const EXTERNAL_IMAGES: &[&str] = &[
    "gcr.io/distroless/static-debian12@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab",
    "node@sha256:49f7adaf9f1574bc45c20ea317139b39a59d7e758d0e93745e112fd85cb42623",
];

/// The pipeline takes an image built elsewhere (an external template): no Dockerfile, maybe no
/// shell, a non-root `USER`. The export never runs the image, so neither matters.
#[test]
#[ignore = "requires docker + network to pull public images + SANDCRAB_TEST_CRABD"]
fn builds_rootfs_from_external_images() {
    let crabd = PathBuf::from(
        std::env::var_os("SANDCRAB_TEST_CRABD").expect("set SANDCRAB_TEST_CRABD to a crabd binary"),
    );
    let cache = tempfile::tempdir().unwrap();
    let builder = RootfsBuilder::new(cache.path());
    for image in EXTERNAL_IMAGES {
        let pull = Command::new("docker")
            .args(["pull", "-q", image])
            .status()
            .unwrap();
        assert!(pull.success(), "docker pull {image}");
        let image_id = sandcrab_rootfs::docker_image_id(image).unwrap();
        let req = RootfsRequest {
            image: (*image).into(),
            expected_image_id: Some(image_id),
            template: Some("external".into()),
            crabd: crabd.clone(),
            arch: Arch::host().unwrap(),
        };
        let built = builder.build(&req).unwrap();
        let img = &built.image_path;
        let fsck = Command::new("e2fsck").arg("-fn").arg(img).output().unwrap();
        assert!(
            fsck.status.success(),
            "{image}: e2fsck: {}",
            String::from_utf8_lossy(&fsck.stdout)
        );
        let stat = debugfs(img, &format!("stat {}", built.manifest.crabd_path));
        assert!(stat.contains("Mode:  0755"), "{image}: {stat}");
        assert!(
            debugfs(img, "stat /opt/sandcrab/crabd").contains("Type: symlink"),
            "{image}"
        );
        for d in [
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/tmp",
            "/workspace",
            "/mnt/scratch",
            "/mnt/newroot",
        ] {
            assert!(
                debugfs(img, &format!("stat {d}")).contains("Type: directory"),
                "{image}: {d}"
            );
        }
        // No leftover export container (nor its anonymous volumes).
        let ps = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                "label=dev.sandcrab.purpose=rootfs-export",
                "--filter",
                &format!("ancestor={image}"),
            ])
            .output()
            .unwrap();
        assert!(ps.stdout.is_empty(), "{image}: export container left");
    }
    // The no-shell image really has no shell.
    let sh = debugfs(
        &builder
            .build(&RootfsRequest {
                image: EXTERNAL_IMAGES[0].into(),
                expected_image_id: None,
                template: None,
                crabd,
                arch: Arch::host().unwrap(),
            })
            .unwrap()
            .image_path,
        "stat /bin/sh",
    );
    assert!(!sh.contains("Type: regular"), "{sh}");
}
