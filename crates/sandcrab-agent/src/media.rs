//! Binary tool results → what the model should see.
//!
//! `download` returns file bytes as base64, and `file_read` of a binary file returns no
//! content. Inlining base64 as text is useless to the model and burns context (a 128 KB PNG
//! is ~170k characters), so:
//!
//! - an image (PNG, JPEG, GIF, WebP, detected by magic bytes) whose base64 fits the API's
//!   per-image limit is sent as an `image` content block inside the `tool_result`, next to a
//!   short text note;
//! - other UTF-8 text up to [`MAX_INLINE_TEXT`] bytes comes back as plain `content_text`;
//! - any other file is summarised (size, sha256, MIME type) and its base64 is dropped.
//!
//! The model never receives base64 as text. If it truly needs raw bytes it can use `bash`
//! (`base64`, `xxd`, …), whose output crabd bounds.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Anthropic's per-image limit applies to the base64 payload: 5 MiB.
pub const MAX_IMAGE_BASE64: usize = 5 * 1024 * 1024;
/// Largest raw image whose base64 still fits [`MAX_IMAGE_BASE64`].
pub const MAX_IMAGE_BYTES: u64 = (MAX_IMAGE_BASE64 as u64 / 4) * 3;
/// Text files up to this size are returned as text from `download`.
pub const MAX_INLINE_TEXT: usize = 64 * 1024;

/// An image to attach to a `tool_result` as an `image` content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: &'static str,
    /// Standard, padded base64 (as the API expects; no newlines).
    pub data: String,
    /// Decoded size in bytes.
    pub bytes: usize,
}

impl ImageAttachment {
    /// The Messages API `image` block.
    #[must_use]
    pub fn block(&self) -> Value {
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": self.media_type, "data": self.data},
        })
    }
}

/// Image MIME type from magic bytes (formats the Messages API accepts).
#[must_use]
pub fn image_media_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Best-effort MIME type for the summary of a non-image file.
#[must_use]
pub fn guess_mime(b: &[u8]) -> &'static str {
    if let Some(m) = image_media_type(b) {
        m
    } else if b.starts_with(b"%PDF-") {
        "application/pdf"
    } else if b.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if b.starts_with(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if b.starts_with(b"\x7fELF") {
        "application/x-elf"
    } else if b.starts_with(b"<svg") || b.starts_with(b"<?xml") && contains(b, b"<svg") {
        "image/svg+xml"
    } else if std::str::from_utf8(b).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `true` for paths whose extension names an image format the API accepts.
#[must_use]
pub fn has_image_extension(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

/// Rewrite a successful `download` result: strip `content_base64`, then attach the image,
/// inline small text, or summarise. Results without `content_base64` are left alone.
#[must_use]
pub fn shape_download(output: &Value) -> (Value, Option<ImageAttachment>) {
    let Some(obj) = output.as_object() else {
        return (output.clone(), None);
    };
    let Some(b64) = obj.get("content_base64").and_then(Value::as_str) else {
        return (output.clone(), None);
    };
    let mut out: Map<String, Value> = obj.clone();
    out.remove("content_base64");
    let Ok(bytes) = STANDARD.decode(b64) else {
        out.insert(
            "note".into(),
            "content omitted: the base64 payload did not decode".into(),
        );
        return (Value::Object(out), None);
    };
    if let Some(media_type) = image_media_type(&bytes)
        && b64.len() <= MAX_IMAGE_BASE64
    {
        out.insert("mime".into(), media_type.into());
        out.insert(
            "note".into(),
            format!("{media_type} image attached below; look at it directly").into(),
        );
        let img = ImageAttachment {
            media_type,
            data: b64.to_owned(),
            bytes: bytes.len(),
        };
        return (Value::Object(out), Some(img));
    }
    let mime = guess_mime(&bytes);
    if mime == "text/plain" && bytes.len() <= MAX_INLINE_TEXT {
        // Checked UTF-8 by guess_mime.
        out.insert("mime".into(), mime.into());
        out.insert(
            "content_text".into(),
            String::from_utf8_lossy(&bytes).into_owned().into(),
        );
        return (Value::Object(out), None);
    }
    summarise(&mut out, &bytes, mime);
    (Value::Object(out), None)
}

fn summarise(out: &mut Map<String, Value>, bytes: &[u8], mime: &str) {
    let sha = Sha256::digest(bytes);
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    out.insert("mime".into(), mime.into());
    out.insert("sha256".into(), hex.into());
    let why = if image_media_type(bytes).is_some() {
        format!(
            "image too large to attach (base64 over {} MiB); downscale it first, e.g. to a \
             smaller PNG/JPEG",
            MAX_IMAGE_BASE64 / (1024 * 1024)
        )
    } else if mime == "text/plain" {
        "text too large to inline; use file_read with offset/limit".to_owned()
    } else {
        "binary content omitted (not shown to the model); inspect it with bash if needed".to_owned()
    };
    out.insert("note".into(), why.into());
}

/// For a binary `file_read` of an image path, the path to fetch with `download`.
#[must_use]
pub fn file_read_image_path(input: &Value, output: &Value) -> Option<String> {
    if output["is_binary"] != true {
        return None;
    }
    let path = input["path"].as_str()?;
    has_image_extension(path).then(|| path.to_owned())
}

/// Merge a `download` reply into a binary `file_read` result: the image (if it is one and
/// fits) or a note explaining why not.
#[must_use]
pub fn attach_to_file_read(output: &Value, download: &Value) -> (Value, Option<ImageAttachment>) {
    let mut out = output.clone();
    let (shaped, img) = shape_download(download);
    if let Some(img) = img {
        out["mime"] = img.media_type.into();
        out["note"] = format!(
            "{} image attached below; look at it directly",
            img.media_type
        )
        .into();
        return (out, Some(img));
    }
    let note = match shaped.get("error") {
        Some(e) => format!(
            "binary file; could not attach it as an image: {}",
            e["message"].as_str().unwrap_or("download failed")
        ),
        None => shaped["note"]
            .as_str()
            .unwrap_or("binary file; not a supported image format (png, jpeg, gif, webp)")
            .to_owned(),
    };
    out["note"] = note.into();
    (out, None)
}

/// A copy of `v` with every `image` block's base64 `data` replaced by a size marker, for
/// transcripts and logs.
#[must_use]
pub fn redact_images(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(redact_images).collect()),
        Value::Object(o) => {
            let mut o = o.clone();
            if o.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(src) = o.get_mut("source").and_then(Value::as_object_mut)
                    && let Some(n) = src.get("data").and_then(Value::as_str).map(str::len)
                {
                    src.insert("data".into(), format!("<{n} base64 chars elided>").into());
                }
                return Value::Object(o);
            }
            Value::Object(o.into_iter().map(|(k, v)| (k, redact_images(&v))).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
    ];

    fn download(bytes: &[u8]) -> Value {
        json!({"path": "/tmp/x", "content_base64": STANDARD.encode(bytes), "size": bytes.len(), "mode": 420})
    }

    #[test]
    fn magic_bytes() {
        assert_eq!(image_media_type(PNG_1X1), Some("image/png"));
        assert_eq!(
            image_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(image_media_type(b"GIF89a.."), Some("image/gif"));
        assert_eq!(
            image_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_media_type(b"hello"), None);
        assert_eq!(guess_mime(b"%PDF-1.7"), "application/pdf");
        assert_eq!(guess_mime(&[0, 159, 146, 150]), "application/octet-stream");
        assert!(has_image_extension("a/B.PNG") && has_image_extension("x.webp"));
        assert!(!has_image_extension("x.svg") && !has_image_extension("png"));
    }

    #[test]
    fn image_download_becomes_attachment() {
        let (out, img) = shape_download(&download(PNG_1X1));
        let img = img.unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(img.bytes, PNG_1X1.len());
        assert_eq!(STANDARD.decode(&img.data).unwrap(), PNG_1X1);
        assert!(out.get("content_base64").is_none());
        assert_eq!(out["path"], "/tmp/x");
        assert!(out["note"].as_str().unwrap().contains("attached"));
        let b = img.block();
        assert_eq!(b["type"], "image");
        assert_eq!(b["source"]["type"], "base64");
        assert_eq!(b["source"]["media_type"], "image/png");
    }

    #[test]
    fn oversized_image_and_binary_are_summarised() {
        let mut big = PNG_1X1.to_vec();
        big.resize(MAX_IMAGE_BYTES as usize + 3, 0);
        let (out, img) = shape_download(&download(&big));
        assert!(img.is_none());
        assert!(out.get("content_base64").is_none());
        assert_eq!(out["mime"], "image/png");
        assert_eq!(out["sha256"].as_str().unwrap().len(), 64);
        assert!(out["note"].as_str().unwrap().contains("too large"));

        let (out, img) = shape_download(&download(&[0u8, 1, 2, 0xFF, 0xFE]));
        assert!(img.is_none());
        assert_eq!(out["mime"], "application/octet-stream");
        assert!(out.to_string().len() < 400, "{out}");
    }

    #[test]
    fn small_text_is_inlined_large_text_is_not() {
        let (out, img) = shape_download(&download(b"hello\n"));
        assert!(img.is_none());
        assert_eq!(out["content_text"], "hello\n");
        let big = "x".repeat(MAX_INLINE_TEXT + 1);
        let (out, _) = shape_download(&download(big.as_bytes()));
        assert!(out.get("content_text").is_none());
        assert!(out.get("sha256").is_some());
    }

    #[test]
    fn redaction() {
        let (_, img) = shape_download(&download(PNG_1X1));
        let msg = json!([{"type": "tool_result", "content": [{"type": "text", "text": "t"}, img.unwrap().block()]}]);
        let r = redact_images(&msg);
        let data = r[0]["content"][1]["source"]["data"].as_str().unwrap();
        assert!(data.starts_with('<') && data.ends_with("elided>"), "{data}");
        assert_eq!(r[0]["content"][0], msg[0]["content"][0]);
    }

    #[test]
    fn file_read_of_an_image() {
        let input = json!({"path": "shot.png"});
        let fr = json!({"content": "", "total_lines": 0, "start_line": 1, "end_line": 0, "truncated": false, "is_binary": true});
        assert_eq!(
            file_read_image_path(&input, &fr).as_deref(),
            Some("shot.png")
        );
        assert!(file_read_image_path(&json!({"path": "a.bin"}), &fr).is_none());
        assert!(file_read_image_path(&input, &json!({"is_binary": false})).is_none());

        let (out, img) = attach_to_file_read(&fr, &download(PNG_1X1));
        assert_eq!(img.unwrap().media_type, "image/png");
        assert_eq!(out["is_binary"], true);
        assert_eq!(out["mime"], "image/png");

        let err = json!({"error": {"code": "too_large", "message": "file is 9 MB"}});
        let (out, img) = attach_to_file_read(&fr, &err);
        assert!(img.is_none());
        assert!(out["note"].as_str().unwrap().contains("9 MB"));
    }
}
