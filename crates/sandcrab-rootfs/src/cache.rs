//! Rootfs build + content-addressed cache (Firecracker design doc §2, libkrun design doc §5).
//!
//! ```text
//! sandcrab/<tpl>:0.1.0 ─docker create+export─▶ tar ─augment─▶ rootfs.tar ─mke2fs -d─▶ rootfs.ext4 (ro, shared)
//! ```
//!
//! Output is cached at `<cache>/rootfs/<key>/{rootfs.ext4, manifest.json}` with
//! `key = sha256(image_id ‖ crabd_sha256 ‖ PIPELINE_VERSION)`. Builds happen in a temp dir next to the
//! cache entry and are published with an atomic `rename`, so a concurrent reader never sees a partial
//! image. Per-sandbox writable state goes on a sparse scratch drive ([`create_scratch`]) that guest
//! init overlays on top of the read-only base.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::augment;
use crate::error::{Error, IoCtx, Result};
use crate::hash::{sha256_bytes, sha256_file};
use crate::mkfs::{self, MkfsStrategy};

/// Bump whenever the produced image changes for the same inputs (augmentation, mkfs flags, layout).
pub const PIPELINE_VERSION: u32 = 1;
/// ext4 label of the shared read-only base (design doc §2.3).
pub const ROOTFS_LABEL: &str = "sc-root";
/// ext4 label of the per-sandbox scratch drive (guest init's fallback drive lookup).
/// Equal to the scratch `drive_id`: guest init falls back to this label when no virtio-blk
/// serial matches (`sandcrab_protocol::init::SCRATCH_DRIVE_ID`).
pub const SCRATCH_LABEL: &str = "scratch";
pub const ROOTFS_FILE: &str = "rootfs.ext4";
pub const MANIFEST_FILE: &str = "manifest.json";
const MIB: u64 = 1024 * 1024;

/// Namespace for the deterministic filesystem UUID (uuid5 over the cache key).
const UUID_NS: uuid::Uuid = uuid::Uuid::from_u128(0x5a1d_c4ab_0f00_4c0d_9e2b_6b5f_1d3a_7c01);

/// `sha256(image_id ‖ crabd_sha256 ‖ pipeline_version)`, NUL-separated, hex.
///
/// Digests are normalised (optional `sha256:` prefix stripped, lowercased) so `docker inspect` ids and
/// bare hex compare equal.
pub fn cache_key(image_id: &str, crabd_sha256: &str, pipeline_version: u32) -> String {
    let norm = |s: &str| s.strip_prefix("sha256:").unwrap_or(s).to_ascii_lowercase();
    sha256_bytes(
        format!(
            "sandcrab-rootfs\0{}\0{}\0{pipeline_version}",
            norm(image_id),
            norm(crabd_sha256)
        )
        .as_bytes(),
    )
}

/// Deterministic fs UUID for a cache key.
pub fn fs_uuid(key: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&UUID_NS, key.as_bytes())
}

/// Image size for a read-only base: `ceil(content × 1.15) + 64 MiB`, rounded up to 64 MiB.
///
/// `content_bytes` is the augmented tar size (includes 512 B headers, which roughly offsets ext4's
/// 4 KiB block rounding for small files). No free-space headroom: the image is mounted read-only.
pub fn image_size(content_bytes: u64) -> u64 {
    let raw = content_bytes.saturating_mul(115).div_ceil(100) + 64 * MIB;
    raw.div_ceil(64 * MIB) * (64 * MIB)
}

/// Inode count: entries + 25 % + 4096 slack (mke2fs's default ratio under-provisions node_modules trees).
pub fn inode_count(entries: u64) -> u64 {
    entries + entries / 4 + 4096
}

/// Metadata written next to every cached rootfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsManifest {
    pub cache_key: String,
    pub template: Option<String>,
    pub image: String,
    /// `docker image inspect .Id` (the template's pinned `sha256`).
    pub image_id: String,
    pub crabd_sha256: String,
    pub pipeline_version: u32,
    /// Logical size of `rootfs.ext4`.
    pub size_bytes: u64,
    /// Augmented tar size that drove sizing.
    pub content_bytes: u64,
    pub entries: u64,
    pub inodes: u64,
    pub label: String,
    pub fs_uuid: String,
    /// Where crabd sits in the image (`/sbin/crabd` resolves to it).
    pub crabd_path: String,
    pub mkfs: MkfsStrategy,
    pub rootfs_sha256: String,
    pub created_at: DateTime<Utc>,
}

impl RootfsManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).at(path)?;
        serde_json::from_slice(&data)
            .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("manifest serializes")
    }
}

/// What to build.
#[derive(Debug, Clone)]
pub struct RootfsRequest {
    /// Docker image reference (e.g. `sandcrab/base:0.1.0`).
    pub image: String,
    /// Expected image id (template `sha256` pin). Mismatch is a hard error.
    pub expected_image_id: Option<String>,
    /// Template name, recorded in the manifest.
    pub template: Option<String>,
    /// Static musl crabd binary to inject.
    pub crabd: PathBuf,
}

/// A ready rootfs.
#[derive(Debug, Clone)]
pub struct BuiltRootfs {
    pub dir: PathBuf,
    pub image_path: PathBuf,
    pub manifest: RootfsManifest,
    pub cache_hit: bool,
    pub elapsed: Duration,
}

/// Builds and caches rootfs images under `<cache_root>/rootfs/`.
#[derive(Debug, Clone)]
pub struct RootfsBuilder {
    root: PathBuf,
    strategy: Option<MkfsStrategy>,
    verify_on_hit: bool,
}

impl RootfsBuilder {
    /// `cache_root` is the sandcrab cache dir (e.g. `~/.sandcrab/cache`); images land in `<it>/rootfs`.
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            root: cache_root.into().join("rootfs"),
            strategy: None,
            verify_on_hit: false,
        }
    }

    /// Force a mkfs strategy instead of auto-detecting.
    pub fn strategy(mut self, s: MkfsStrategy) -> Self {
        self.strategy = Some(s);
        self
    }

    /// Re-hash `rootfs.ext4` against the manifest on cache hits (costs a full read).
    pub fn verify_on_hit(mut self, v: bool) -> Self {
        self.verify_on_hit = v;
        self
    }

    pub fn rootfs_dir(&self) -> &Path {
        &self.root
    }

    /// Cached entry for `key`, if complete (manifest parses, key matches, image length matches).
    pub fn lookup(&self, key: &str) -> Result<Option<(PathBuf, RootfsManifest)>> {
        let dir = self.root.join(key);
        let mpath = dir.join(MANIFEST_FILE);
        let img = dir.join(ROOTFS_FILE);
        if !mpath.is_file() || !img.is_file() {
            return Ok(None);
        }
        let m = RootfsManifest::load(&mpath)?;
        let len = fs::metadata(&img).at(&img)?.len();
        if m.cache_key != key || m.size_bytes != len {
            return Err(Error::Manifest(format!(
                "{} is inconsistent (key/size); delete the entry to rebuild",
                dir.display()
            )));
        }
        if self.verify_on_hit {
            crate::hash::verify_sha256(&img, &m.rootfs_sha256)?;
        }
        Ok(Some((dir, m)))
    }

    /// Build (or reuse) the rootfs for `req`.
    pub fn build(&self, req: &RootfsRequest) -> Result<BuiltRootfs> {
        let t0 = Instant::now();
        let image_id = docker_image_id(&req.image)?;
        if let Some(exp) = &req.expected_image_id {
            let n = |s: &str| s.strip_prefix("sha256:").unwrap_or(s).to_ascii_lowercase();
            if n(exp) != n(&image_id) {
                return Err(Error::ImagePinMismatch {
                    image: req.image.clone(),
                    expected: exp.clone(),
                    actual: image_id,
                });
            }
        }
        let crabd_sha = sha256_file(&req.crabd)?;
        let key = cache_key(&image_id, &crabd_sha, PIPELINE_VERSION);

        if let Some((dir, manifest)) = self.lookup(&key)? {
            return Ok(BuiltRootfs {
                image_path: dir.join(ROOTFS_FILE),
                dir,
                manifest,
                cache_hit: true,
                elapsed: t0.elapsed(),
            });
        }

        let tmp = mkfs::tempdir_in(&self.root, &format!("tmp-{}", &key[..12]))?;
        let res = self.build_into(&tmp, req, &image_id, &crabd_sha, &key);
        let manifest = match res {
            Ok(m) => m,
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };
        let dest = self.root.join(&key);
        match fs::rename(&tmp, &dest) {
            Ok(()) => {}
            Err(_) if dest.join(MANIFEST_FILE).is_file() => {
                // Lost a race with a concurrent identical build: theirs is equivalent.
                let _ = fs::remove_dir_all(&tmp);
                let (dir, manifest) = self.lookup(&key)?.ok_or_else(|| {
                    Error::Manifest(format!("{} vanished after race", dest.display()))
                })?;
                return Ok(BuiltRootfs {
                    image_path: dir.join(ROOTFS_FILE),
                    dir,
                    manifest,
                    cache_hit: true,
                    elapsed: t0.elapsed(),
                });
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e).at(&dest);
            }
        }
        sync_dir(&self.root);
        Ok(BuiltRootfs {
            image_path: dest.join(ROOTFS_FILE),
            dir: dest,
            manifest,
            cache_hit: false,
            elapsed: t0.elapsed(),
        })
    }

    fn build_into(
        &self,
        tmp: &Path,
        req: &RootfsRequest,
        image_id: &str,
        crabd_sha: &str,
        key: &str,
    ) -> Result<RootfsManifest> {
        // 1+2. export + augment, streamed straight into rootfs.tar.
        let tar_path = tmp.join("rootfs.tar");
        let stats = {
            let crabd = File::open(&req.crabd).at(&req.crabd)?;
            let crabd_len = crabd.metadata().at(&req.crabd)?.len();
            let out = BufWriter::new(File::create(&tar_path).at(&tar_path)?);
            let container = ExportContainer::create(&req.image)?;
            let mut child = Command::new("docker")
                .args(["export", &container.id])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(Error::IoBare)?;
            let stdout = child.stdout.take().expect("piped");
            let stats = augment::augment(
                BufReader::with_capacity(1 << 20, stdout),
                out,
                crabd,
                crabd_len,
            );
            let output = child.wait_with_output().map_err(Error::IoBare)?;
            if !output.status.success() {
                return Err(Error::Command {
                    cmd: format!("docker export {}", container.id),
                    status: output.status.to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                });
            }
            stats?
        };
        let content_bytes = fs::metadata(&tar_path).at(&tar_path)?.len();

        // 3. mke2fs -d (rootless), growing once or twice if the estimate was short.
        let strategy = match self.strategy {
            Some(s) => s,
            None => mkfs::detect(tmp)?,
        };
        let uuid = fs_uuid(key).to_string();
        let img = tmp.join(ROOTFS_FILE);
        let mut params = mkfs::MkfsParams {
            label: ROOTFS_LABEL.into(),
            uuid: uuid.clone(),
            size_bytes: image_size(content_bytes),
            inodes: inode_count(stats.entries),
            journal: false,
            root_owner: None,
        };
        let mut attempt = 0;
        loop {
            match mkfs::mkfs_from_tar(strategy, tmp, &tar_path, &img, &params) {
                Ok(()) => break,
                Err(Error::Command { stderr, .. })
                    if attempt < 2
                        && (stderr.contains("No space left")
                            || stderr.contains("Could not allocate")) =>
                {
                    attempt += 1;
                    let _ = fs::remove_file(&img);
                    params.size_bytes =
                        (params.size_bytes + params.size_bytes / 2).div_ceil(64 * MIB) * (64 * MIB);
                    params.inodes *= 2;
                }
                Err(e) => return Err(e),
            }
        }
        fs::remove_file(&tar_path).at(&tar_path)?;

        // 4. manifest.
        let rootfs_sha256 = sha256_file(&img)?;
        let size_bytes = fs::metadata(&img).at(&img)?.len();
        let manifest = RootfsManifest {
            cache_key: key.to_string(),
            template: req.template.clone(),
            image: req.image.clone(),
            image_id: image_id.to_string(),
            crabd_sha256: crabd_sha.to_string(),
            pipeline_version: PIPELINE_VERSION,
            size_bytes,
            content_bytes,
            entries: stats.entries,
            inodes: params.inodes,
            label: ROOTFS_LABEL.into(),
            fs_uuid: uuid,
            crabd_path: format!("/{}", stats.crabd_tar_path),
            mkfs: strategy,
            rootfs_sha256,
            created_at: Utc::now(),
        };
        let mpath = tmp.join(MANIFEST_FILE);
        let mut f = File::create(&mpath).at(&mpath)?;
        f.write_all(manifest.to_json().as_bytes()).at(&mpath)?;
        f.sync_all().at(&mpath)?;
        File::open(&img).and_then(|f| f.sync_all()).at(&img)?;
        Ok(manifest)
    }
}

fn sync_dir(p: &Path) {
    if let Ok(f) = File::open(p) {
        let _ = f.sync_all();
    }
}

/// `docker image inspect --format {{.Id}}`.
pub fn docker_image_id(image: &str) -> Result<String> {
    Ok(
        mkfs::run(Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", image]))?
            .trim()
            .to_string(),
    )
}

/// A stopped container used only as an export source; removed on drop.
struct ExportContainer {
    id: String,
}

impl ExportContainer {
    fn create(image: &str) -> Result<Self> {
        // Images carry no CMD (see templates/base); the command is never run.
        let id = mkfs::run(Command::new("docker").args([
            "create",
            "--network",
            "none",
            "--label",
            "dev.sandcrab.purpose=rootfs-export",
            image,
            "/sandcrab-export-placeholder",
        ]))?
        .trim()
        .to_string();
        Ok(Self { id })
    }
}

impl Drop for ExportContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Create a sparse, journal-less ext4 scratch drive of `size_mib` MiB at `path` (label
/// [`SCRATCH_LABEL`]). Physical size is a few MiB thanks to lazy inode-table init.
pub fn create_scratch(path: &Path, size_mib: u64) -> Result<Duration> {
    let t0 = Instant::now();
    if size_mib < 8 {
        return Err(Error::Unsupported(format!(
            "scratch drive of {size_mib} MiB is too small (min 8)"
        )));
    }
    {
        let f = File::create(path).at(path)?;
        f.set_len(size_mib * MIB).at(path)?;
    }
    let res = mkfs::run(
        Command::new("mke2fs")
            .env("E2FSPROGS_FAKE_TIME", mkfs::FAKE_TIME)
            .args([
                "-q",
                "-F",
                "-t",
                "ext4",
                "-b",
                "4096",
                "-m",
                "0",
                "-O",
                "^has_journal",
                "-E",
                "lazy_itable_init=1,lazy_journal_init=1,nodiscard",
                "-L",
                SCRATCH_LABEL,
            ])
            .arg(path),
    );
    if let Err(e) = res {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(t0.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_normalised() {
        let a = cache_key("sha256:ABCDEF", "0123", 1);
        assert_eq!(a, cache_key("abcdef", "sha256:0123", 1));
        assert_eq!(a.len(), 64);
        assert_ne!(a, cache_key("abcdef", "0123", 2));
        assert_ne!(a, cache_key("abcdef", "0124", 1));
        // separator prevents ambiguous concatenations
        assert_ne!(cache_key("ab", "c", 1), cache_key("a", "bc", 1));
        // pinned vector: changing the derivation must be a conscious PIPELINE_VERSION bump
        assert_eq!(
            cache_key("abcdef", "0123", 1),
            sha256_bytes(b"sandcrab-rootfs\0abcdef\x000123\x001")
        );
    }

    #[test]
    fn fs_uuid_deterministic() {
        assert_eq!(fs_uuid("k"), fs_uuid("k"));
        assert_ne!(fs_uuid("k"), fs_uuid("j"));
    }

    #[test]
    fn size_policy() {
        assert_eq!(image_size(0), 64 * MIB);
        assert_eq!(image_size(1), 128 * MIB);
        // 100 MiB → 115 MiB + 64 = 179 → 192
        assert_eq!(image_size(100 * MIB), 192 * MIB);
        // 1 GiB → 1177.6 + 64 = 1241.6 → 1280
        assert_eq!(image_size(1024 * MIB), 1280 * MIB);
        for c in [3u64, 77 * MIB + 5, 999 * MIB] {
            let s = image_size(c);
            assert_eq!(s % (64 * MIB), 0);
            assert!(s >= c * 115 / 100 + 64 * MIB);
        }
        assert_eq!(inode_count(1000), 1000 + 250 + 4096);
    }

    #[test]
    fn manifest_round_trip() {
        let m = RootfsManifest {
            cache_key: cache_key("a", "b", PIPELINE_VERSION),
            template: Some("base".into()),
            image: "sandcrab/base:0.1.0".into(),
            image_id: "sha256:a".into(),
            crabd_sha256: "b".into(),
            pipeline_version: PIPELINE_VERSION,
            size_bytes: 128 * MIB,
            content_bytes: 10,
            entries: 3,
            inodes: 4099,
            label: ROOTFS_LABEL.into(),
            fs_uuid: fs_uuid("x").to_string(),
            crabd_path: "/usr/sbin/crabd".into(),
            mkfs: MkfsStrategy::NativeTar,
            rootfs_sha256: "c".into(),
            created_at: Utc::now(),
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(MANIFEST_FILE);
        fs::write(&p, m.to_json()).unwrap();
        assert_eq!(RootfsManifest::load(&p).unwrap(), m);
        assert!(m.to_json().contains("\"mkfs\": \"native_tar\""));
    }

    #[test]
    fn lookup_misses_and_detects_inconsistency() {
        let dir = tempfile::tempdir().unwrap();
        let b = RootfsBuilder::new(dir.path());
        assert!(b.lookup("nope").unwrap().is_none());
        let key = cache_key("a", "b", 1);
        let entry = b.rootfs_dir().join(&key);
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join(ROOTFS_FILE), b"1234").unwrap();
        let mut m: serde_json::Value = serde_json::json!({
            "cache_key": key, "template": null, "image": "i", "image_id": "a",
            "crabd_sha256": "b", "pipeline_version": 1, "size_bytes": 4, "content_bytes": 4,
            "entries": 1, "inodes": 1, "label": "sc-root", "fs_uuid": "u",
            "crabd_path": "/sbin/crabd", "mkfs": "fakeroot",
            "rootfs_sha256": sha256_bytes(b"1234"), "created_at": "2026-11-23T00:00:00Z"
        });
        fs::write(entry.join(MANIFEST_FILE), m.to_string()).unwrap();
        assert!(
            b.clone()
                .verify_on_hit(true)
                .lookup(&key)
                .unwrap()
                .is_some()
        );
        m["size_bytes"] = 5.into();
        fs::write(entry.join(MANIFEST_FILE), m.to_string()).unwrap();
        assert!(b.lookup(&key).is_err());
    }

    #[test]
    fn scratch_is_sparse_ext4() {
        if Command::new("mke2fs").arg("-V").output().is_err() {
            eprintln!("mke2fs not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("scratch.ext4");
        create_scratch(&p, 1024).unwrap();
        let meta = fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), 1024 * MIB);
        use std::os::unix::fs::MetadataExt;
        assert!(
            meta.blocks() * 512 < 64 * MIB,
            "not sparse: {}",
            meta.blocks()
        );
        assert!(create_scratch(&p, 1).is_err());
    }
}
