//! Tests for `upload` / `download`.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sandcrab_guest::{Limits, ToolContext, tools::transfer};
use sandcrab_protocol::{DownloadParams, ProtocolError, UploadParams};
use tempfile::TempDir;

fn setup_with(limits: Limits) -> (TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::with_limits(dir.path(), limits);
    (dir, ctx)
}

fn setup() -> (TempDir, ToolContext) {
    setup_with(Limits::default())
}

fn up(path: &str, bytes: &[u8]) -> UploadParams {
    UploadParams {
        path: path.into(),
        content_base64: STANDARD.encode(bytes),
        mode: None,
        overwrite: false,
    }
}

fn down(path: &str, max_bytes: Option<u64>) -> DownloadParams {
    DownloadParams {
        path: path.into(),
        max_bytes,
    }
}

fn assert_no_temp_files(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains("crabd-"), "leftover temp file {name}");
    }
}

#[tokio::test]
async fn upload_download_roundtrip_binary() {
    let (dir, ctx) = setup();
    let bytes: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let r = transfer::upload(&ctx, up("bin/data.bin", &bytes))
        .await
        .unwrap();
    let abs = dir.path().join("bin/data.bin");
    assert_eq!(r.path, abs.to_str().unwrap());
    assert_eq!(r.bytes_written, 70_000);
    assert!(r.created);
    assert_eq!(fs::read(&abs).unwrap(), bytes);
    assert_eq!(
        fs::metadata(&abs).unwrap().permissions().mode() & 0o777,
        0o644
    );

    let d = transfer::download(&ctx, down("bin/data.bin", None))
        .await
        .unwrap();
    assert_eq!(d.path, abs.to_str().unwrap());
    assert_eq!(d.size, 70_000);
    assert_eq!(d.mode, 0o644);
    assert_eq!(STANDARD.decode(d.content_base64).unwrap(), bytes);
    assert_no_temp_files(&dir.path().join("bin"));
}

#[tokio::test]
async fn upload_mode_and_overwrite() {
    let (dir, ctx) = setup();
    let mut p = up("run.sh", b"#!/bin/sh\n");
    p.mode = Some(0o755);
    transfer::upload(&ctx, p.clone()).await.unwrap();
    let f = dir.path().join("run.sh");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o755
    );

    // Existing without overwrite → error, file untouched.
    p.content_base64 = STANDARD.encode(b"changed");
    let e = transfer::upload(&ctx, p.clone()).await.unwrap_err();
    match e {
        ProtocolError::ExecutionFailed(m) => assert!(m.contains("overwrite"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read(&f).unwrap(), b"#!/bin/sh\n");

    p.overwrite = true;
    p.mode = Some(0o600);
    let r = transfer::upload(&ctx, p).await.unwrap();
    assert!(!r.created);
    assert_eq!(fs::read(&f).unwrap(), b"changed");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn upload_errors() {
    let (dir, ctx) = setup_with(Limits {
        max_file_bytes: 100,
        ..Limits::default()
    });
    let mut p = up("x", b"");
    p.content_base64 = "not base64!!".into();
    let e = transfer::upload(&ctx, p).await.unwrap_err();
    assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");

    let mut p = up("x", b"a");
    p.mode = Some(0o10000);
    let e = transfer::upload(&ctx, p).await.unwrap_err();
    assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");

    // Exactly at the limit is fine; one over is not (both the estimate and exact check).
    transfer::upload(&ctx, up("ok", &[7u8; 100])).await.unwrap();
    let e = transfer::upload(&ctx, up("big", &[7u8; 101]))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        ProtocolError::TooLarge {
            size: 101,
            limit: 100
        }
    );
    let e = transfer::upload(&ctx, up("huge", &[7u8; 10_000]))
        .await
        .unwrap_err();
    assert!(
        matches!(e, ProtocolError::TooLarge { limit: 100, .. }),
        "{e:?}"
    );

    fs::create_dir(dir.path().join("d")).unwrap();
    let mut p = up("d", b"a");
    p.overwrite = true;
    let e = transfer::upload(&ctx, p).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
    assert!(!dir.path().join("big").exists());
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn download_limits_and_errors() {
    let (dir, ctx) = setup_with(Limits {
        max_file_bytes: 100,
        ..Limits::default()
    });
    fs::write(dir.path().join("f"), [1u8; 50]).unwrap();
    fs::set_permissions(dir.path().join("f"), fs::Permissions::from_mode(0o640)).unwrap();
    let d = transfer::download(&ctx, down("f", Some(50))).await.unwrap();
    assert_eq!((d.size, d.mode), (50, 0o640));

    let e = transfer::download(&ctx, down("f", Some(49)))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        ProtocolError::TooLarge {
            size: 50,
            limit: 49
        }
    );

    // Caller max above the daemon cap is clamped to the cap.
    fs::write(dir.path().join("g"), [1u8; 101]).unwrap();
    let e = transfer::download(&ctx, down("g", Some(1_000_000)))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        ProtocolError::TooLarge {
            size: 101,
            limit: 100
        }
    );

    let e = transfer::download(&ctx, down("missing", None))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
    fs::create_dir(dir.path().join("d")).unwrap();
    let e = transfer::download(&ctx, down("d", None)).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
}

#[tokio::test]
async fn download_empty_file() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("e"), b"").unwrap();
    let d = transfer::download(&ctx, down("e", None)).await.unwrap();
    assert_eq!((d.size, d.content_base64.as_str()), (0, ""));
}
