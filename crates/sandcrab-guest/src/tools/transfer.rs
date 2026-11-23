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

use sandcrab_protocol::{
    DownloadParams, DownloadResult, ProtocolError, UploadParams, UploadResult,
};

use crate::ToolContext;

/// Write a file from base64 bytes.
pub async fn upload(ctx: &ToolContext, p: UploadParams) -> Result<UploadResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("upload"))
}

/// Read a file as base64 bytes.
pub async fn download(
    ctx: &ToolContext,
    p: DownloadParams,
) -> Result<DownloadResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("download"))
}
