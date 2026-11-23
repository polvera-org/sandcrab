//! Guest `pack_tree` (crabd) → host `unpack_safe` (core): the two halves of workspace
//! sync-back agree on the `sandcrab-tar-v1` format.

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

use sandcrab_core::archive::{UnpackLimits, unpack_safe};
use sandcrab_guest::init::export::pack_tree;

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let md = fs::symlink_metadata(&p).unwrap();
            let desc = if md.file_type().is_symlink() {
                format!("link -> {}", fs::read_link(&p).unwrap().display())
            } else if md.is_dir() {
                stack.push(p.clone());
                format!("dir {:o}", md.mode() & 0o7777)
            } else {
                format!(
                    "file {:o} mtime={} {:?}",
                    md.mode() & 0o7777,
                    md.mtime(),
                    fs::read(&p).unwrap()
                )
            };
            out.push((rel, desc));
        }
    }
    out.sort();
    out
}

#[test]
fn pack_then_unpack_round_trips() {
    let src = tempfile::tempdir().unwrap();
    let r = src.path();
    fs::create_dir_all(r.join("src/deep/er")).unwrap();
    fs::write(r.join("README.md"), b"# hi\n").unwrap();
    fs::write(r.join("src/deep/er/x.bin"), vec![0xAB; 70_000]).unwrap();
    fs::write(r.join("empty"), b"").unwrap();
    fs::write(r.join("script.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(r.join("script.sh"), fs::Permissions::from_mode(0o750)).unwrap();
    symlink("README.md", r.join("readme-link")).unwrap();
    symlink("/usr/bin/python3", r.join("abs-link")).unwrap();
    let long_dir = "d".repeat(120);
    fs::create_dir(r.join(&long_dir)).unwrap();
    fs::write(r.join(&long_dir).join("f".repeat(120)), b"long").unwrap();
    fs::create_dir(r.join("locked")).unwrap();
    fs::write(r.join("locked/inside"), b"in").unwrap();
    fs::set_permissions(r.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
    fs::create_dir(r.join("lost+found")).unwrap();

    let mut tar = Vec::new();
    pack_tree(r, Some((1000, 1000)), &mut tar).unwrap();

    let dst = tempfile::tempdir().unwrap();
    let staging = dst.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let stats = unpack_safe(&tar[..], &staging, &UnpackLimits::default()).unwrap();
    assert_eq!(stats.files, 6);
    assert_eq!(stats.symlinks, 2);

    let mut want = snapshot(r);
    want.retain(|(p, _)| p != "lost+found");
    assert_eq!(snapshot(&staging), want);

    // Restore write permission so the tempdirs can be cleaned up.
    fs::set_permissions(r.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(staging.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn truncated_export_is_rejected() {
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("a"), vec![1u8; 10_000]).unwrap();
    fs::write(src.path().join("b"), b"b").unwrap();
    let mut tar = Vec::new();
    pack_tree(src.path(), None, &mut tar).unwrap();
    // Drop the end-of-archive marker and the last entry: looks like a guest crash.
    for cut in [tar.len() - 1024, 512 * 3, 100] {
        let dst = tempfile::tempdir().unwrap();
        let err = unpack_safe(&tar[..cut], dst.path(), &UnpackLimits::default()).unwrap_err();
        assert!(err.to_string().contains("rejected"), "cut {cut}: {err}");
    }
}
