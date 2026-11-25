//! Block-device discovery: map Firecracker `drive_id`s to `/dev/vdX`.
//!
//! Primary key: `/sys/block/vd*/serial` (virtio-blk `GET_ID`, which Firecracker fills with
//! the `drive_id`). Fallback: the ext4 superblock volume label. Both readers take their root
//! directories as parameters so tests run against a fake `/sys` and fake device files.

use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// A virtio block device seen in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    /// Kernel name, e.g. `vdb`.
    pub name: String,
    /// Device node, e.g. `/dev/vdb`.
    pub dev_path: PathBuf,
    /// virtio-blk serial (= Firecracker `drive_id`), if exposed.
    pub serial: Option<String>,
}

/// List `vd*` devices under `sys_block` (normally `/sys/block`), sorted by name, with device
/// nodes under `dev_dir` (normally `/dev`).
pub fn scan_block_devices(sys_block: &Path, dev_dir: &Path) -> io::Result<Vec<BlockDevice>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(sys_block)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("vd") {
            continue;
        }
        let serial = match fs::read(entry.path().join("serial")) {
            Ok(bytes) => parse_serial(&bytes),
            Err(_) => None,
        };
        out.push(BlockDevice {
            dev_path: dev_dir.join(&name),
            name,
            serial,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The serial attribute is padded with NULs and/or ends with a newline.
fn parse_serial(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

const EXT4_SB_OFFSET: u64 = 1024;
const EXT4_MAGIC_OFFSET: usize = 0x38;
const EXT4_MAGIC: u16 = 0xEF53;
const EXT4_LABEL_OFFSET: usize = 0x78;
const EXT4_LABEL_LEN: usize = 16;

/// Volume label from an ext4 superblock (the 1024 bytes at device offset 1024). `None` when
/// the magic does not match or the label is empty.
#[must_use]
pub fn parse_ext4_label(sb: &[u8]) -> Option<String> {
    let magic = sb.get(EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2)?;
    if u16::from_le_bytes([magic[0], magic[1]]) != EXT4_MAGIC {
        return None;
    }
    let raw = sb.get(EXT4_LABEL_OFFSET..EXT4_LABEL_OFFSET + EXT4_LABEL_LEN)?;
    parse_serial(raw)
}

/// Read the ext4 label of a device (or image file).
pub fn read_ext4_label(dev: &Path) -> io::Result<Option<String>> {
    let mut f = fs::File::open(dev)?;
    f.seek(SeekFrom::Start(EXT4_SB_OFFSET))?;
    let mut sb = vec![0u8; 1024];
    let mut filled = 0;
    while filled < sb.len() {
        match f.read(&mut sb[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(parse_ext4_label(&sb[..filled]))
}

/// Find the device for `drive_id`: serial match first, then `label` via `label_of`.
pub fn resolve_drive(
    devices: &[BlockDevice],
    drive_id: &str,
    label: &str,
    label_of: impl Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    if let Some(d) = devices
        .iter()
        .find(|d| d.serial.as_deref() == Some(drive_id))
    {
        return Some(d.dev_path.clone());
    }
    devices
        .iter()
        .find(|d| label_of(&d.dev_path).as_deref() == Some(label))
        .map(|d| d.dev_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sys(root: &Path, devs: &[(&str, Option<&[u8]>)]) {
        for (name, serial) in devs {
            let d = root.join("sys/block").join(name);
            fs::create_dir_all(&d).unwrap();
            if let Some(s) = serial {
                fs::write(d.join("serial"), s).unwrap();
            }
        }
        fs::create_dir_all(root.join("dev")).unwrap();
    }

    fn ext4_image(path: &Path, label: &str) {
        let mut img = vec![0u8; 4096];
        let sb = &mut img[1024..2048];
        sb[EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        sb[EXT4_LABEL_OFFSET..EXT4_LABEL_OFFSET + label.len()].copy_from_slice(label.as_bytes());
        fs::write(path, img).unwrap();
    }

    #[test]
    fn scan_reads_serials_and_ignores_non_virtio() {
        let t = tempfile::tempdir().unwrap();
        fake_sys(
            t.path(),
            &[
                ("vdb", Some(b"ws0\0\0\0\0\0")),
                ("vda", Some(b"rootfs\n")),
                ("vdc", None),
                ("loop0", Some(b"x")),
            ],
        );
        let devs = scan_block_devices(&t.path().join("sys/block"), &t.path().join("dev")).unwrap();
        let names: Vec<_> = devs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["vda", "vdb", "vdc"]);
        assert_eq!(devs[0].serial.as_deref(), Some("rootfs"));
        assert_eq!(devs[1].serial.as_deref(), Some("ws0"));
        assert_eq!(devs[1].dev_path, t.path().join("dev/vdb"));
        assert_eq!(devs[2].serial, None);
    }

    #[test]
    fn resolve_prefers_serial_then_label() {
        let t = tempfile::tempdir().unwrap();
        fake_sys(
            t.path(),
            &[
                ("vda", Some(b"rootfs")),
                ("vdb", None),
                ("vdc", Some(b"ws0")),
            ],
        );
        ext4_image(&t.path().join("dev/vda"), "sc-root");
        ext4_image(&t.path().join("dev/vdb"), "ws1");
        let devs = scan_block_devices(&t.path().join("sys/block"), &t.path().join("dev")).unwrap();
        let label_of = |p: &Path| read_ext4_label(p).ok().flatten();

        assert_eq!(
            resolve_drive(&devs, "ws0", "ws0", label_of),
            Some(t.path().join("dev/vdc"))
        );
        assert_eq!(
            resolve_drive(&devs, "ws1", "ws1", label_of),
            Some(t.path().join("dev/vdb"))
        );
        assert_eq!(resolve_drive(&devs, "ws9", "ws9", label_of), None);
    }

    #[test]
    fn label_parsing_checks_magic() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("img");
        ext4_image(&p, "sixteen-chars-xx");
        assert_eq!(
            read_ext4_label(&p).unwrap().as_deref(),
            Some("sixteen-chars-xx")
        );
        fs::write(&p, vec![0u8; 4096]).unwrap();
        assert_eq!(read_ext4_label(&p).unwrap(), None);
        fs::write(&p, b"short").unwrap();
        assert_eq!(read_ext4_label(&p).unwrap(), None);
    }
}
