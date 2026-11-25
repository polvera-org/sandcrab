//! Streaming tar rewriter: `docker export` tar in → bootable-rootfs tar out.
//!
//! Every input entry is re-emitted with its original header (uid/gid/mode/mtime) so ownership survives
//! without root. On top we:
//! - drop Docker runtime placeholders (`.dockerenv`, everything under `dev/`),
//! - replace `etc/{resolv.conf,hosts,hostname}` (empty bind-mount placeholders in an export) with stubs,
//! - inject `crabd` as `/sbin/crabd` (written to the real directory if `sbin` is a merged-usr symlink),
//!   plus a `/opt/sandcrab/crabd` symlink so the binary sits at the same path as under Docker,
//! - create any missing mountpoints ([`MOUNTPOINTS`]).

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::Path;

use tar::{Builder, EntryType, Header};

use crate::error::Result;

/// Directories that must exist in the rootfs (guest init mounts on them). Mode per entry.
pub const MOUNTPOINTS: &[(&str, u32)] = &[
    ("proc", 0o555),
    ("sys", 0o555),
    ("dev", 0o755),
    ("run", 0o755),
    ("tmp", 0o1777),
    ("mnt", 0o755),
    ("mnt/scratch", 0o755),
    ("mnt/newroot", 0o755),
    ("workspace", 0o1777),
    ("opt", 0o755),
    ("opt/sandcrab", 0o755),
];

/// Where crabd lives in the guest (kernel `init=` points here).
pub const CRABD_GUEST_PATH: &str = "/sbin/crabd";
/// Compat symlink matching the Docker bind-mount location.
pub const CRABD_COMPAT_LINK: &str = "opt/sandcrab/crabd";

const STUBS: &[(&str, &[u8])] = &[
    (
        "etc/resolv.conf",
        b"# written by sandcrab init (init.configure dns)\n",
    ),
    (
        "etc/hosts",
        b"127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n",
    ),
    ("etc/hostname", b"sandcrab\n"),
];

/// Fixed mtime for injected entries (reproducibility).
const INJECTED_MTIME: u64 = 0;

/// Result of an augmentation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AugmentStats {
    /// Number of entries in the output tar.
    pub entries: u64,
    /// Sum of regular-file payload bytes in the output.
    pub file_bytes: u64,
    /// Path (in the tar, no leading `/`) where the crabd binary was written.
    pub crabd_tar_path: String,
}

fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.trim_start_matches("./").trim_start_matches('/');
    s.trim_end_matches('/').to_string()
}

fn dropped(path: &str) -> bool {
    path == ".dockerenv"
        || path.starts_with("dev/")
        || path == CRABD_COMPAT_LINK
        || path == "sbin/crabd"
        || path == "usr/sbin/crabd"
        || STUBS.iter().any(|(p, _)| *p == path)
}

/// Resolve a top-level directory through (possibly chained) relative/absolute symlinks recorded from
/// the archive, e.g. `sbin -> usr/sbin`. Returns the path to use inside the tar.
fn resolve_dir(symlinks: &HashMap<String, String>, dir: &str) -> String {
    let mut cur = dir.to_string();
    for _ in 0..8 {
        match symlinks.get(&cur) {
            Some(target) => {
                let t = target.trim_start_matches('/');
                cur = if target.starts_with('/') {
                    t.to_string()
                } else {
                    match cur.rsplit_once('/') {
                        Some((parent, _)) => format!("{parent}/{t}"),
                        None => t.to_string(),
                    }
                };
                cur = cur.trim_end_matches('/').to_string();
            }
            None => return cur,
        }
    }
    cur
}

fn dir_header(mode: u32) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(EntryType::Directory);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(INJECTED_MTIME);
    h.set_size(0);
    h
}

fn file_header(mode: u32, size: u64) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(EntryType::Regular);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(INJECTED_MTIME);
    h.set_size(size);
    h
}

/// Rewrite `input` (a `docker export` stream) into `output`, injecting `crabd` (bytes read from
/// `crabd`, `crabd_len` long).
pub fn augment<R: Read, W: Write, C: Read>(
    input: R,
    output: W,
    mut crabd: C,
    crabd_len: u64,
) -> Result<AugmentStats> {
    let mut archive = tar::Archive::new(input);
    let mut out = Builder::new(output);
    out.mode(tar::HeaderMode::Complete);
    out.follow_symlinks(false);

    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut symlinks: HashMap<String, String> = HashMap::new();
    let mut entries = 0u64;
    let mut file_bytes = 0u64;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = normalize(&entry.path()?);
        if path.is_empty() || dropped(&path) {
            continue;
        }
        let mut header = entry.header().clone();
        // The path is re-set below. tar's ustar `set_path` only rewrites `name` when the new
        // path fits in it, so a `prefix` inherited from the source header (`docker export`
        // splits long paths into prefix/name) would survive and readers (mke2fs -d) would
        // see `<old prefix>/<new path>`.
        if let Some(ustar) = header.as_ustar_mut() {
            ustar.prefix = [0; 155];
        }
        let kind = header.entry_type();

        // Forward xattrs (e.g. security.capability) carried as PAX records.
        if let Some(pax) = entry.pax_extensions()? {
            let xattrs: Vec<(String, Vec<u8>)> = pax
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let k = e.key().ok()?;
                    k.starts_with("SCHILY.xattr.")
                        .then(|| (k.to_string(), e.value_bytes().to_vec()))
                })
                .collect();
            if !xattrs.is_empty() {
                out.append_pax_extensions(xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
            }
        }

        match kind {
            EntryType::Symlink | EntryType::Link => {
                let target = entry
                    .link_name()?
                    .map(|l| l.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if kind == EntryType::Symlink {
                    symlinks.insert(path.clone(), target.clone());
                }
                let target_in_tar = if kind == EntryType::Link {
                    normalize(Path::new(&target))
                } else {
                    target
                };
                out.append_link(&mut header, &path, &target_in_tar)?;
            }
            _ => {
                if kind == EntryType::Directory {
                    dirs.insert(path.clone());
                } else if kind == EntryType::Regular || kind == EntryType::Continuous {
                    file_bytes += header.size()?;
                }
                out.append_data(&mut header, &path, &mut entry)?;
            }
        }
        entries += 1;
    }

    // Missing mountpoints (parents first: MOUNTPOINTS is ordered).
    for (dir, mode) in MOUNTPOINTS {
        if dirs.contains(*dir) || symlinks.contains_key(*dir) {
            continue;
        }
        let mut h = dir_header(*mode);
        out.append_data(&mut h, format!("{dir}/"), std::io::empty())?;
        dirs.insert(dir.to_string());
        entries += 1;
    }

    // Stubs (etc/ always exists in a Debian-derived export; create it otherwise).
    if !dirs.contains("etc") && !symlinks.contains_key("etc") {
        out.append_data(&mut dir_header(0o755), "etc/", std::io::empty())?;
        dirs.insert("etc".into());
        entries += 1;
    }
    for (path, body) in STUBS {
        let mut h = file_header(0o644, body.len() as u64);
        out.append_data(&mut h, path, *body)?;
        file_bytes += body.len() as u64;
        entries += 1;
    }

    // crabd → real directory behind `sbin`.
    let sbin = resolve_dir(&symlinks, "sbin");
    if !dirs.contains(&sbin) {
        // Create every missing component.
        let mut acc = String::new();
        for comp in sbin.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(comp);
            if !dirs.contains(&acc) {
                out.append_data(&mut dir_header(0o755), format!("{acc}/"), std::io::empty())?;
                dirs.insert(acc.clone());
                entries += 1;
            }
        }
    }
    let crabd_tar_path = format!("{sbin}/crabd");
    let mut h = file_header(0o755, crabd_len);
    out.append_data(&mut h, &crabd_tar_path, (&mut crabd).take(crabd_len))?;
    file_bytes += crabd_len;
    entries += 1;

    let mut h = Header::new_gnu();
    h.set_entry_type(EntryType::Symlink);
    h.set_mode(0o777);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(INJECTED_MTIME);
    h.set_size(0);
    out.append_link(&mut h, CRABD_COMPAT_LINK, CRABD_GUEST_PATH)?;
    entries += 1;

    out.into_inner()?.flush()?;
    Ok(AugmentStats {
        entries,
        file_bytes,
        crabd_tar_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src_tar() -> Vec<u8> {
        let mut b = Builder::new(Vec::new());
        let add_dir = |b: &mut Builder<Vec<u8>>, p: &str, uid: u64| {
            let mut h = Header::new_ustar();
            h.set_entry_type(EntryType::Directory);
            h.set_mode(0o755);
            h.set_uid(uid);
            h.set_gid(uid);
            h.set_size(0);
            b.append_data(&mut h, p, std::io::empty()).unwrap();
        };
        add_dir(&mut b, ".dockerenv", 0);
        add_dir(&mut b, "usr/", 0);
        add_dir(&mut b, "usr/sbin/", 0);
        add_dir(&mut b, "etc/", 0);
        add_dir(&mut b, "dev/", 0);
        add_dir(&mut b, "dev/pts/", 0);
        add_dir(&mut b, "home/", 0);
        add_dir(&mut b, "home/sandcrab/", 1000);
        let mut h = Header::new_ustar();
        h.set_entry_type(EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_uid(0);
        h.set_gid(0);
        b.append_link(&mut h, "sbin", "usr/sbin").unwrap();
        let mut h = Header::new_ustar();
        h.set_entry_type(EntryType::Regular);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        b.append_data(&mut h, "etc/hosts", std::io::empty())
            .unwrap();
        let long = format!("home/sandcrab/{}", "x".repeat(150));
        let mut h = Header::new_gnu();
        h.set_entry_type(EntryType::Regular);
        h.set_size(3);
        h.set_mode(0o600);
        h.set_uid(1000);
        h.set_gid(1000);
        b.append_data(&mut h, &long, &b"abc"[..]).unwrap();
        b.into_inner().unwrap()
    }

    /// `docker export` stores `<prefix>/tests/` as ustar prefix + name; once normalised (no
    /// trailing `/`) the full path fits in `name`, and the stale prefix must not be kept.
    #[test]
    fn split_ustar_paths_are_not_doubled() {
        let dir = format!("opt/{}/certifi", "p".repeat(82));
        let mut b = Builder::new(Vec::new());
        for p in [format!("{dir}/"), format!("{dir}/tests/")] {
            let mut h = Header::new_ustar();
            h.set_entry_type(EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, &p, std::io::empty()).unwrap();
        }
        let src = b.into_inner().unwrap();
        // Precondition: the source really uses the ustar prefix field for `tests/`.
        let mut entries = tar::Archive::new(&src[..]);
        let tests = entries.entries().unwrap().nth(1).unwrap().unwrap();
        assert!(tests.header().as_ustar().unwrap().prefix[0] != 0);
        let want = format!("{dir}/tests");
        assert!(want.len() <= 100, "{}", want.len());

        let mut out = Vec::new();
        augment(&src[..], &mut out, &b""[..], 0).unwrap();
        let paths: Vec<String> = tar::Archive::new(&out[..])
            .entries()
            .unwrap()
            .map(|e| normalize(&e.unwrap().path().unwrap()))
            .collect();
        assert!(paths.contains(&want), "{paths:?}");
        assert!(
            paths.iter().all(|p| !p.contains("certifi/opt")),
            "{paths:?}"
        );
    }

    #[test]
    fn injects_crabd_and_preserves_ownership() {
        let crabd = b"\x7fELFfake";
        let mut out = Vec::new();
        let stats = augment(&src_tar()[..], &mut out, &crabd[..], crabd.len() as u64).unwrap();
        assert_eq!(stats.crabd_tar_path, "usr/sbin/crabd");

        let mut seen: HashMap<String, (u64, u32, EntryType)> = HashMap::new();
        for e in tar::Archive::new(&out[..]).entries().unwrap() {
            let e = e.unwrap();
            let p = normalize(&e.path().unwrap());
            let h = e.header();
            assert!(
                seen.insert(
                    p.clone(),
                    (h.uid().unwrap(), h.mode().unwrap(), h.entry_type())
                )
                .is_none(),
                "duplicate {p}"
            );
        }
        assert!(!seen.contains_key(".dockerenv"));
        assert!(!seen.contains_key("dev/pts"));
        assert!(seen.contains_key("dev"));
        for (d, _) in MOUNTPOINTS {
            assert!(seen.contains_key(*d), "missing {d}");
        }
        assert_eq!(seen["usr/sbin/crabd"], (0, 0o755, EntryType::Regular));
        assert_eq!(seen["home/sandcrab"].0, 1000);
        let long = format!("home/sandcrab/{}", "x".repeat(150));
        assert_eq!(seen[&long].0, 1000);
        assert_eq!(seen[CRABD_COMPAT_LINK].2, EntryType::Symlink);
        assert_eq!(seen["tmp"].1, 0o1777);
        assert_eq!(seen.keys().filter(|k| k.as_str() == "etc/hosts").count(), 1);
    }

    #[test]
    fn resolves_chained_symlinks() {
        let mut m = HashMap::new();
        m.insert("sbin".to_string(), "/usr/sbin".to_string());
        assert_eq!(resolve_dir(&m, "sbin"), "usr/sbin");
        assert_eq!(resolve_dir(&HashMap::new(), "sbin"), "sbin");
    }
}
