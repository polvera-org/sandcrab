//! A scratch directory made under a strict umask (a root service runs with `UMask=0077`)
//! still gives the sandbox user what it needs: the canaries readable by anyone, the mounted
//! directories handed over by `fchown`. Its own test binary, because the umask is per process.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustix::fs::Mode;
use sandcrab_core::ScratchDir;

#[test]
fn scratch_under_umask_077_is_usable_by_the_sandbox_user() {
    let old = rustix::process::umask(Mode::from_raw_mode(0o077));
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let s = ScratchDir::create(&root, "scratch").unwrap();
    s.mkdir("ro").unwrap();
    s.mkdir("rw").unwrap();
    s.write_new("ro/canary", b"c").unwrap();
    s.write_new("canary-outside", b"c").unwrap();
    rustix::process::umask(old);

    let mode = |p: &str| fs::metadata(root.join("scratch").join(p)).unwrap();
    // The umask strips the directories to 0700: only their owner may enter them, so the
    // sandbox user must own both mounted ones.
    assert_eq!(mode("ro").permissions().mode() & 0o777, 0o700);
    // The files are readable by a non-owner whatever the umask.
    for f in ["ro/canary", "canary-outside"] {
        assert_eq!(mode(f).permissions().mode() & 0o777, 0o644, "{f}");
    }
    // As root (a root server), hand both to another user; otherwise to the owner, a no-op.
    let (uid, gid) = if rustix::process::geteuid().is_root() {
        (65_534, 65_534)
    } else {
        (mode("ro").uid(), mode("ro").gid())
    };
    for d in ["ro", "rw"] {
        s.chown_dir(d, uid, gid).unwrap();
        assert_eq!((mode(d).uid(), mode(d).gid()), (uid, gid), "{d}");
    }
    drop(s);
    assert!(!root.join("scratch").exists());
}
