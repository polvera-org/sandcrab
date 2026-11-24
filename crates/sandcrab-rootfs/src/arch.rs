//! Guest architecture of a rootfs (libkrun design doc §5).
//!
//! A rootfs is built for one Linux platform: the image is exported with
//! `docker create --platform linux/<arch>`, the injected `crabd` must be an ELF for the same
//! machine, and the arch is part of the cache key (a multi-arch image has a single index digest).

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoCtx, Result};

/// Linux guest architecture, named like Docker / OCI (`GOARCH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    /// `linux/amd64` (`x86_64`).
    Amd64,
    /// `linux/arm64` (`aarch64`).
    Arm64,
}

impl Arch {
    /// The architecture this binary was compiled for, if it is a supported guest arch.
    pub const HOST: Option<Arch> = if cfg!(target_arch = "x86_64") {
        Some(Arch::Amd64)
    } else if cfg!(target_arch = "aarch64") {
        Some(Arch::Arm64)
    } else {
        None
    };

    /// [`Arch::HOST`], or [`Error::Unsupported`] on other hosts.
    pub fn host() -> Result<Self> {
        Self::HOST.ok_or_else(|| {
            Error::Unsupported(format!(
                "host arch {} is not a supported guest arch (amd64, arm64)",
                std::env::consts::ARCH
            ))
        })
    }

    /// Docker / OCI name: `amd64` | `arm64`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Docker `--platform` value: `linux/amd64` | `linux/arm64`.
    pub const fn platform(self) -> &'static str {
        match self {
            Arch::Amd64 => "linux/amd64",
            Arch::Arm64 => "linux/arm64",
        }
    }

    /// Rust / kernel name: `x86_64` | `aarch64`.
    pub const fn rust_arch(self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64",
            Arch::Arm64 => "aarch64",
        }
    }

    /// Rust target triple of the static `crabd` for this arch.
    pub const fn musl_target(self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64-unknown-linux-musl",
            Arch::Arm64 => "aarch64-unknown-linux-musl",
        }
    }

    /// ELF `e_machine` value (`EM_X86_64` = 62, `EM_AARCH64` = 183).
    pub const fn elf_machine(self) -> u16 {
        match self {
            Arch::Amd64 => 62,
            Arch::Arm64 => 183,
        }
    }

    /// Architecture of a 64-bit little-endian ELF from its header bytes.
    pub fn from_elf_header(header: &[u8]) -> std::result::Result<Self, String> {
        if header.len() < 20 || &header[..4] != b"\x7fELF" {
            return Err("not an ELF file".into());
        }
        if header[4] != 2 || header[5] != 1 {
            return Err("not a 64-bit little-endian ELF".into());
        }
        let machine = u16::from_le_bytes([header[18], header[19]]);
        [Arch::Amd64, Arch::Arm64]
            .into_iter()
            .find(|a| a.elf_machine() == machine)
            .ok_or_else(|| format!("unsupported ELF machine {machine}"))
    }

    /// Architecture of the ELF binary at `path`.
    pub fn of_elf(path: &Path) -> Result<Self> {
        let mut header = [0u8; 20];
        File::open(path)
            .and_then(|mut f| f.read_exact(&mut header))
            .at(path)?;
        Self::from_elf_header(&header)
            .map_err(|e| Error::Unsupported(format!("{}: {e}", path.display())))
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Arch {
    type Err = String;

    /// Accepts `amd64` / `x86_64` / `linux/amd64` and `arm64` / `aarch64` / `linux/arm64`.
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        let bare = s.strip_prefix("linux/").unwrap_or(&s);
        match bare {
            "amd64" | "x86_64" | "x86-64" => Ok(Arch::Amd64),
            "arm64" | "aarch64" | "arm64/v8" => Ok(Arch::Arm64),
            _ => Err(format!(
                "unsupported arch/platform {s:?} (expected linux/amd64 or linux/arm64)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_platforms() {
        for s in ["amd64", "x86_64", "linux/amd64", "LINUX/AMD64"] {
            assert_eq!(s.parse::<Arch>(), Ok(Arch::Amd64), "{s}");
        }
        for s in ["arm64", "aarch64", "linux/arm64", "linux/arm64/v8"] {
            assert_eq!(s.parse::<Arch>(), Ok(Arch::Arm64), "{s}");
        }
        assert!("linux/riscv64".parse::<Arch>().is_err());
        assert!("windows/amd64".parse::<Arch>().is_err());
        for a in [Arch::Amd64, Arch::Arm64] {
            assert_eq!(a.platform().parse::<Arch>(), Ok(a));
            assert_eq!(a.to_string().parse::<Arch>(), Ok(a));
            assert_eq!(a.rust_arch().parse::<Arch>(), Ok(a));
            assert!(a.musl_target().starts_with(a.rust_arch()));
        }
    }

    #[test]
    fn serde_uses_docker_names() {
        assert_eq!(serde_json::to_string(&Arch::Arm64).unwrap(), "\"arm64\"");
        assert_eq!(
            serde_json::from_str::<Arch>("\"amd64\"").unwrap(),
            Arch::Amd64
        );
    }

    fn elf(class: u8, data: u8, machine: u16) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = class;
        h[5] = data;
        h[18..20].copy_from_slice(&machine.to_le_bytes());
        h
    }

    #[test]
    fn elf_machine_detection() {
        assert_eq!(Arch::from_elf_header(&elf(2, 1, 62)), Ok(Arch::Amd64));
        assert_eq!(Arch::from_elf_header(&elf(2, 1, 183)), Ok(Arch::Arm64));
        assert!(Arch::from_elf_header(&elf(1, 1, 62)).is_err()); // 32-bit
        assert!(Arch::from_elf_header(&elf(2, 2, 62)).is_err()); // big-endian
        assert!(Arch::from_elf_header(&elf(2, 1, 243)).is_err()); // riscv
        assert!(Arch::from_elf_header(b"#!/bin/sh\n").is_err());

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crabd");
        std::fs::write(&p, elf(2, 1, 183)).unwrap();
        assert_eq!(Arch::of_elf(&p).unwrap(), Arch::Arm64);
        std::fs::write(&p, b"short").unwrap();
        assert!(Arch::of_elf(&p).is_err());
    }

    #[test]
    fn host_is_known_on_supported_hosts() {
        if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            assert_eq!(Arch::host().unwrap().rust_arch(), std::env::consts::ARCH);
        }
    }
}
