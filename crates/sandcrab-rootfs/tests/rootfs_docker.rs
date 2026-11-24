//! Builds the real `base` template rootfs. Requires docker, the locally built `sandcrab/base:0.1.0`
//! image, e2fsprogs (`debugfs`, `e2fsck`) and a crabd binary in `SANDCRAB_TEST_CRABD`.
//!
//! ```sh
//! SANDCRAB_TEST_CRABD=target/x86_64-unknown-linux-musl/release/crabd \
//!   cargo test -p sandcrab-rootfs -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

use sandcrab_rootfs::{ROOTFS_LABEL, RootfsBuilder, RootfsRequest};

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
    };
    let built = builder.build(&req).unwrap();
    assert!(!built.cache_hit);
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
