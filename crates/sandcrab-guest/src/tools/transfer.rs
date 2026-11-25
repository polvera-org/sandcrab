//! Binary transfer: `upload` / `download` (single file, base64; `tar.zst` archives later).
//!
//! Contract (see `UploadParams` / `UploadResult` / `DownloadParams` / `DownloadResult` in
//! `sandcrab-protocol`). Paths go through `ctx.resolve_path`; results report absolute paths.
//! - `upload`: decode `content_base64` (standard alphabet, padded; bad base64 →
//!   `InvalidParams`). Decoded size over `ctx.limits.max_file_bytes` → `TooLarge` (check the
//!   encoded length first to avoid decoding huge input). Create parent dirs. Existing file
//!   without `overwrite` → `ExecutionFailed`. Write atomically (temp + `rename`) and apply
//!   `mode` (default 0o644). `created` = did not exist before.
//! - `download`: limit = `min(p.max_bytes.unwrap_or(max), ctx.limits.max_file_bytes)`;
//!   check size via metadata *before* reading; over limit → `TooLarge`. Missing →
//!   `NotFound`; directory → `ExecutionFailed`. Return base64 content, size and mode bits.
//!
//! Implementation notes: `mode` above `0o7777` → `InvalidParams`. Like `file_write`, an
//! `upload` onto a symlink writes the link's final target (the link is kept). An existing
//! directory at the destination → `ExecutionFailed` even with `overwrite`. `download` follows
//! symlinks and re-checks the size after reading (the file may grow in between).

use std::{fs, io, io::Read, os::unix::fs::PermissionsExt};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sandcrab_protocol::{
    DownloadParams, DownloadResult, ProtocolError, UploadParams, UploadResult,
};

use super::fs::{
    DEFAULT_NEW_FILE_MODE, blocking, create_parents, io_err, is_dir_err, path_str,
    resolve_write_target, write_atomic,
};
use crate::ToolContext;

/// Write a file from base64 bytes.
pub async fn upload(ctx: &ToolContext, p: UploadParams) -> Result<UploadResult, ProtocolError> {
    let limit = ctx.limits.max_file_bytes;
    let mode = p.mode.unwrap_or(DEFAULT_NEW_FILE_MODE);
    if mode > 0o7777 {
        return Err(ProtocolError::InvalidParams(format!(
            "`mode` {mode} is not valid permission bits (max 4095 = 0o7777); e.g. 420 for 0644"
        )));
    }
    // Cheap bound before decoding: every 4 base64 chars decode to at most 3 bytes.
    let encoded = p.content_base64.trim();
    let estimate = (encoded.len() as u64).div_ceil(4) * 3;
    if estimate > limit.saturating_add(3) {
        return Err(ProtocolError::TooLarge {
            size: estimate,
            limit,
        });
    }
    let bytes = STANDARD.decode(encoded).map_err(|e| {
        ProtocolError::InvalidParams(format!(
            "`content_base64` is not valid base64 (standard alphabet, padded): {e}"
        ))
    })?;
    if bytes.len() as u64 > limit {
        return Err(ProtocolError::TooLarge {
            size: bytes.len() as u64,
            limit,
        });
    }
    let path = ctx.resolve_path(&p.path);
    let overwrite = p.overwrite;
    blocking(move || {
        let target = resolve_write_target(&path)?;
        let existed = match fs::metadata(&target) {
            Ok(m) if m.is_dir() => return Err(is_dir_err(&target)),
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_err("cannot write", &target, &e)),
        };
        if existed && !overwrite {
            return Err(ProtocolError::ExecutionFailed(format!(
                "{} already exists; set `overwrite: true` to replace it",
                path.display()
            )));
        }
        create_parents(&target)?;
        write_atomic(&target, &bytes, Some(mode))
            .map_err(|e| io_err("cannot write", &target, &e))?;
        Ok(UploadResult {
            path: path_str(&path),
            bytes_written: bytes.len() as u64,
            created: !existed,
        })
    })
    .await
}

/// Read a file as base64 bytes.
pub async fn download(
    ctx: &ToolContext,
    p: DownloadParams,
) -> Result<DownloadResult, ProtocolError> {
    let max = ctx.limits.max_file_bytes;
    let limit = p.max_bytes.unwrap_or(max).min(max);
    let path = ctx.resolve_path(&p.path);
    blocking(move || {
        let meta = fs::metadata(&path).map_err(|e| io_err("cannot download", &path, &e))?;
        if meta.is_dir() {
            return Err(is_dir_err(&path));
        }
        if meta.len() > limit {
            return Err(ProtocolError::TooLarge {
                size: meta.len(),
                limit,
            });
        }
        let file = fs::File::open(&path).map_err(|e| io_err("cannot open", &path, &e))?;
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        // Read one byte past the limit to detect growth since `metadata`.
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| io_err("cannot read", &path, &e))?;
        if bytes.len() as u64 > limit {
            return Err(ProtocolError::TooLarge {
                size: bytes.len() as u64,
                limit,
            });
        }
        Ok(DownloadResult {
            path: path_str(&path),
            content_base64: STANDARD.encode(&bytes),
            size: bytes.len() as u64,
            mode: meta.permissions().mode() & 0o7777,
        })
    })
    .await
}
