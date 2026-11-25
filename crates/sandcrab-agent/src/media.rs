//! Binary tool results → what the model should see.
//!
//! `download` returns file bytes as base64, and `file_read` of a binary file returns no
//! content. Inlining base64 as text is useless to the model and burns context (a 128 KB PNG
//! is ~170k characters), so:
//!
//! - an image (PNG, JPEG, GIF, WebP, detected by magic bytes) that is complete (its header
//!   parses and its end marker is present), at most [`MAX_IMAGE_SIDE`] px per side and whose
//!   base64 fits the API's per-image limit is sent as an `image` content block inside the
//!   `tool_result`, next to a short text note; any other image is summarised (dimensions,
//!   size, why it was not sent, how to fix it), because one rejected image fails the whole
//!   request;
//! - other UTF-8 text up to [`MAX_INLINE_TEXT`] bytes comes back as plain `content_text`;
//! - any other file is summarised (size, sha256, MIME type) and its base64 is dropped.
//!
//! The model never receives base64 as text. If it truly needs raw bytes it can use `bash`
//! (`base64`, `xxd`, …), whose output crabd bounds.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

// Messages API image limits, in one place.

/// Anthropic's per-image limit applies to the base64 payload: 5 MiB.
pub const MAX_IMAGE_BASE64: usize = 5 * 1024 * 1024;
/// The API rejects an image whose width or height exceeds this.
pub const MAX_IMAGE_SIDE: u32 = 8000;
/// With more than [`MANY_IMAGES`] images in one request, the per-side limit drops to this.
pub const MAX_IMAGE_SIDE_MANY: u32 = 2000;
/// See [`MAX_IMAGE_SIDE_MANY`].
pub const MANY_IMAGES: usize = 20;
/// History keeps at most this many image blocks; past it the oldest are elided down to
/// [`KEEP_IMAGES_AFTER_ELISION`] (see `agent`). Well under [`MANY_IMAGES`], so the 2000 px
/// rule never applies.
pub const MAX_IMAGES_IN_HISTORY: usize = 8;
/// Image blocks kept (the most recent) when history is elided.
pub const KEEP_IMAGES_AFTER_ELISION: usize = 4;
const _: () = assert!(KEEP_IMAGES_AFTER_ELISION < MAX_IMAGES_IN_HISTORY);
const _: () = assert!(MAX_IMAGES_IN_HISTORY <= MANY_IMAGES);
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
    pub width: u32,
    pub height: u32,
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
    if let Some(media_type) = image_media_type(&bytes) {
        out.insert("mime".into(), media_type.into());
        let dims = image_dimensions(&bytes);
        if let Ok((w, h)) = dims {
            out.insert("width".into(), w.into());
            out.insert("height".into(), h.into());
        }
        match image_problem(&dims, b64.len()) {
            None => {
                let (width, height) = dims.unwrap_or_default();
                out.insert(
                    "note".into(),
                    format!(
                        "{media_type} image ({width}x{height}) attached below; look at it directly"
                    )
                    .into(),
                );
                let img = ImageAttachment {
                    media_type,
                    data: b64.to_owned(),
                    bytes: bytes.len(),
                    width,
                    height,
                };
                return (Value::Object(out), Some(img));
            }
            Some(why) => {
                insert_sha(&mut out, &bytes);
                out.insert("note".into(), why.into());
                return (Value::Object(out), None);
            }
        }
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

fn insert_sha(out: &mut Map<String, Value>, bytes: &[u8]) {
    let sha = Sha256::digest(bytes);
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    out.insert("sha256".into(), hex.into());
}

fn summarise(out: &mut Map<String, Value>, bytes: &[u8], mime: &str) {
    insert_sha(out, bytes);
    out.insert("mime".into(), mime.into());
    let why = if mime == "text/plain" {
        "text too large to inline; use file_read with offset/limit"
    } else {
        "binary content omitted (not shown to the model); inspect it with bash if needed"
    };
    out.insert("note".into(), why.into());
}

const RESIZE_HINT: &str = "take a viewport-sized screenshot instead of a full-page one \
    (Playwright: fullPage: false, or clip to a region), or downscale it first (e.g. \
    `convert in.png -resize 2000x2000 out.png`, `ffmpeg -i in.png -vf scale=2000:-1 out.png` \
    or Pillow `Image.thumbnail((2000, 2000))`)";

/// Why an image must not be sent (`None` = fine). The wording avoids "attached" (the UI
/// keys on it).
fn image_problem(dims: &Result<(u32, u32), String>, b64_len: usize) -> Option<String> {
    match dims {
        Err(e) => Some(format!(
            "image not shown to the model: {e}. If it is still being written, wait and \
             retry; otherwise regenerate it"
        )),
        Ok((w, h)) if *w > MAX_IMAGE_SIDE || *h > MAX_IMAGE_SIDE => Some(format!(
            "image not shown to the model: {w}x{h} px exceeds the API limit of \
             {MAX_IMAGE_SIDE} px per side; {RESIZE_HINT}"
        )),
        Ok((w, h)) if *w == 0 || *h == 0 => Some(format!(
            "image not shown to the model: invalid dimensions {w}x{h}"
        )),
        Ok((w, h)) if b64_len > MAX_IMAGE_BASE64 => Some(format!(
            "image not shown to the model: {w}x{h} px but too large to send (base64 over \
             {} MiB); {RESIZE_HINT}, or save it as JPEG",
            MAX_IMAGE_BASE64 / (1024 * 1024)
        )),
        Ok(_) => None,
    }
}

/// Width and height of a complete PNG / JPEG / GIF / WebP, from its header. An error
/// (human-readable) if the header does not parse or the file is truncated (no PNG `IEND`,
/// no JPEG `EOI` after the scan, no GIF trailer, RIFF size past the end of a WebP).
pub fn image_dimensions(b: &[u8]) -> Result<(u32, u32), String> {
    match image_media_type(b) {
        Some("image/png") => png_dimensions(b),
        Some("image/jpeg") => jpeg_dimensions(b),
        Some("image/gif") => gif_dimensions(b),
        Some("image/webp") => webp_dimensions(b),
        _ => Err("not a PNG, JPEG, GIF or WebP image".into()),
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
}

fn png_dimensions(b: &[u8]) -> Result<(u32, u32), String> {
    if b.get(12..16) != Some(b"IHDR") {
        return Err("PNG header (IHDR) missing or truncated".into());
    }
    let (Some(w), Some(h)) = (be32(b, 16), be32(b, 20)) else {
        return Err("PNG header (IHDR) truncated".into());
    };
    // Walk the chunks to IEND: length (4) + type (4) + data + CRC (4).
    let mut at = 8usize;
    loop {
        let Some(len) = be32(b, at) else {
            return Err("PNG truncated (no IEND chunk)".into());
        };
        let Some(kind) = b.get(at + 4..at + 8) else {
            return Err("PNG truncated (no IEND chunk)".into());
        };
        let end = at
            .checked_add(12)
            .and_then(|x| x.checked_add(len as usize))
            .filter(|e| *e <= b.len());
        let Some(end) = end else {
            return Err("PNG truncated (a chunk runs past the end of the file)".into());
        };
        if kind == b"IEND" {
            return Ok((w, h));
        }
        at = end;
    }
}

fn jpeg_dimensions(b: &[u8]) -> Result<(u32, u32), String> {
    let mut at = 2usize;
    let mut dims = None;
    loop {
        // Markers: 0xFF (fill bytes allowed), then the marker code.
        if b.get(at) != Some(&0xFF) {
            return Err(if at >= b.len() {
                "JPEG truncated (ends inside its headers)".into()
            } else {
                "JPEG header is corrupt (expected a marker)".into()
            });
        }
        while b.get(at) == Some(&0xFF) {
            at += 1;
        }
        let Some(&m) = b.get(at) else {
            return Err("JPEG truncated (ends inside its headers)".into());
        };
        at += 1;
        if m == 0x01 || (0xD0..=0xD7).contains(&m) {
            continue; // standalone markers, no length
        }
        if m == 0xD9 {
            return Err("JPEG has no image data (EOI before the scan)".into());
        }
        let Some(len) = b
            .get(at..at + 2)
            .map(|s| usize::from(s[0]) << 8 | usize::from(s[1]))
        else {
            return Err("JPEG truncated (ends inside its headers)".into());
        };
        if len < 2 || at + len > b.len() {
            return Err("JPEG truncated (a segment runs past the end of the file)".into());
        }
        // SOF0..SOF15 except DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            if len < 7 {
                return Err("JPEG frame header (SOF) is corrupt".into());
            }
            let h = u32::from(b[at + 3]) << 8 | u32::from(b[at + 4]);
            let w = u32::from(b[at + 5]) << 8 | u32::from(b[at + 6]);
            dims = Some((w, h));
        }
        if m == 0xDA {
            // Start of scan: entropy-coded data follows, in which 0xFF is always stuffed
            // (FF 00), so an FF D9 after this point is the real end of image.
            let Some((w, h)) = dims else {
                return Err("JPEG has no frame header (SOF) before its scan".into());
            };
            let scan = &b[at + len..];
            if !scan.windows(2).any(|p| p == [0xFF, 0xD9]) {
                return Err("JPEG truncated (no end-of-image marker)".into());
            }
            return Ok((w, h));
        }
        at += len;
    }
}

fn gif_dimensions(b: &[u8]) -> Result<(u32, u32), String> {
    let (Some(w), Some(h)) = (le16(b, 6), le16(b, 8)) else {
        return Err("GIF header truncated".into());
    };
    let last = b.iter().rposition(|&x| x != 0).map(|i| b[i]);
    if b.len() < 14 || last != Some(0x3B) {
        return Err("GIF truncated (no trailer)".into());
    }
    Ok((w, h))
}

fn webp_dimensions(b: &[u8]) -> Result<(u32, u32), String> {
    let riff = b
        .get(4..8)
        .map(|s| u32::from_le_bytes(s.try_into().expect("4 bytes")));
    match riff {
        Some(n) if (n as usize).saturating_add(8) <= b.len() => {}
        _ => return Err("WebP truncated (RIFF size past the end of the file)".into()),
    }
    let bad = || "WebP header truncated or corrupt".to_owned();
    match b.get(12..16) {
        Some(b"VP8 ") => {
            if b.get(23..26) != Some(&[0x9D, 0x01, 0x2A]) {
                return Err(bad());
            }
            let w = le16(b, 26).ok_or_else(bad)? & 0x3FFF;
            let h = le16(b, 28).ok_or_else(bad)? & 0x3FFF;
            Ok((w, h))
        }
        Some(b"VP8L") => {
            let s = b.get(20..25).ok_or_else(bad)?;
            if s[0] != 0x2F {
                return Err(bad());
            }
            let bits = u32::from_le_bytes([s[1], s[2], s[3], s[4]]);
            Ok(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        Some(b"VP8X") => {
            let w = le24(b, 24).ok_or_else(bad)? + 1;
            let h = le24(b, 27).ok_or_else(bad)? + 1;
            Ok((w, h))
        }
        _ => Err(bad()),
    }
}

/// Minimal well-formed images (header + end marker, no pixel data) for tests.
#[doc(hidden)]
pub mod fixtures {
    /// PNG: signature, IHDR (w×h, RGBA8), IEND.
    #[must_use]
    pub fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]); // depth, colour, ..., CRC
        v.extend(b"\0\0\0\0IEND\xaeB`\x82");
        v
    }

    /// JPEG: SOI, APP0, SOF0 (w×h), SOS, one byte of scan data, EOI.
    #[must_use]
    pub fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, b'J', b'F'];
        v.extend([0xFF, 0xC0, 0, 11, 8]);
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([1, 1, 0x11, 0]);
        v.extend([0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 0x3F, 0, 0x12, 0xFF, 0x00]);
        v.extend([0xFF, 0xD9]);
        v
    }

    /// GIF89a: header, logical screen w×h, trailer.
    #[must_use]
    pub fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend(w.to_le_bytes());
        v.extend(h.to_le_bytes());
        v.extend([0, 0, 0, 0x3B]);
        v
    }

    /// WebP (VP8X canvas w×h).
    #[must_use]
    pub fn webp(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend(22u32.to_le_bytes());
        v.extend(b"WEBPVP8X");
        v.extend(10u32.to_le_bytes());
        v.extend([0, 0, 0, 0]);
        v.extend(&(w - 1).to_le_bytes()[..3]);
        v.extend(&(h - 1).to_le_bytes()[..3]);
        v
    }
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
        out["width"] = img.width.into();
        out["height"] = img.height.into();
        out["note"] = shaped["note"].clone();
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

    fn png1() -> Vec<u8> {
        fixtures::png(1, 1)
    }

    fn download(bytes: &[u8]) -> Value {
        json!({"path": "/tmp/x", "content_base64": STANDARD.encode(bytes), "size": bytes.len(), "mode": 420})
    }

    #[test]
    fn magic_bytes() {
        assert_eq!(image_media_type(&png1()), Some("image/png"));
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
        let (out, img) = shape_download(&download(&png1()));
        let img = img.unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(img.bytes, png1().len());
        assert_eq!(STANDARD.decode(&img.data).unwrap(), png1());
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
        let mut big = png1();
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
        let (_, img) = shape_download(&download(&png1()));
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

        let (out, img) = attach_to_file_read(&fr, &download(&png1()));
        assert_eq!(img.unwrap().media_type, "image/png");
        assert_eq!(out["is_binary"], true);
        assert_eq!(out["mime"], "image/png");

        let err = json!({"error": {"code": "too_large", "message": "file is 9 MB"}});
        let (out, img) = attach_to_file_read(&fr, &err);
        assert!(img.is_none());
        assert!(out["note"].as_str().unwrap().contains("9 MB"));
    }

    #[test]
    fn dimensions_of_each_format() {
        assert_eq!(image_dimensions(&fixtures::png(1280, 800)), Ok((1280, 800)));
        assert_eq!(image_dimensions(&fixtures::jpeg(640, 480)), Ok((640, 480)));
        assert_eq!(image_dimensions(&fixtures::gif(32, 16)), Ok((32, 16)));
        assert_eq!(
            image_dimensions(&fixtures::webp(3000, 9000)),
            Ok((3000, 9000))
        );
        // VP8L: 0x2F, then 14-bit width-1 and height-1.
        let (w, h) = (300u32, 200u32);
        let bits = (w - 1) | (h - 1) << 14;
        let mut l = b"RIFF\x11\0\0\0WEBPVP8L\x05\0\0\0\x2F".to_vec();
        l.extend(bits.to_le_bytes());
        assert_eq!(image_dimensions(&l), Ok((300, 200)));
        // VP8 (lossy): start code, then 14-bit width / height.
        let mut v = b"RIFF\x16\0\0\0WEBPVP8 \x0a\0\0\0\0\0\0\x9d\x01\x2a".to_vec();
        v.extend(64u16.to_le_bytes());
        v.extend(48u16.to_le_bytes());
        assert_eq!(image_dimensions(&v), Ok((64, 48)));
        assert!(image_dimensions(b"hello").is_err());
    }

    #[test]
    fn truncated_images_are_detected() {
        let png = fixtures::png(10, 10);
        let e = image_dimensions(&png[..png.len() - 12]).unwrap_err();
        assert!(e.contains("IEND"), "{e}");
        assert!(image_dimensions(&png[..20]).is_err());
        let jpg = fixtures::jpeg(10, 10);
        let e = image_dimensions(&jpg[..jpg.len() - 2]).unwrap_err();
        assert!(e.contains("end-of-image"), "{e}");
        assert!(image_dimensions(&jpg[..12]).is_err());
        let gif = fixtures::gif(10, 10);
        assert!(image_dimensions(&gif[..gif.len() - 1]).is_err());
        let webp = fixtures::webp(10, 10);
        assert!(image_dimensions(&webp[..webp.len() - 1]).is_err());
        // Corrupt: a PNG signature with garbage after it.
        assert!(image_dimensions(b"\x89PNG\r\n\x1a\nxxxxxxxxxxxxxxxxxxxxxxxx").is_err());
    }

    #[test]
    fn every_format_within_limits_is_attached() {
        for bytes in [
            fixtures::png(8000, 600),
            fixtures::jpeg(1280, 8000),
            fixtures::gif(10, 10),
            fixtures::webp(1, 1),
        ] {
            let (out, img) = shape_download(&download(&bytes));
            let img = img.unwrap_or_else(|| panic!("{out}"));
            assert_eq!(out["width"], img.width);
            assert!(out["note"].as_str().unwrap().contains("attached"));
        }
    }

    #[test]
    fn oversize_image_is_summarised_with_dimensions_and_a_hint() {
        let (out, img) = shape_download(&download(&fixtures::png(1280, 12000)));
        assert!(img.is_none());
        assert_eq!(out["width"], 1280);
        assert_eq!(out["height"], 12000);
        assert_eq!(out["mime"], "image/png");
        assert!(out.get("sha256").is_some());
        let note = out["note"].as_str().unwrap();
        assert!(
            note.contains("1280x12000") && note.contains("8000"),
            "{note}"
        );
        assert!(note.contains("fullPage: false"), "{note}");
        assert!(!note.contains("attached"), "{note}");
        let (_, img) = shape_download(&download(&fixtures::webp(8001, 10)));
        assert!(img.is_none());
    }

    #[test]
    fn truncated_image_is_summarised() {
        let png = fixtures::png(100, 100);
        let (out, img) = shape_download(&download(&png[..png.len() - 6]));
        assert!(img.is_none());
        assert_eq!(out["mime"], "image/png");
        let note = out["note"].as_str().unwrap();
        assert!(
            note.contains("truncated") && !note.contains("attached"),
            "{note}"
        );
        // file_read of the same image: the note explains why it is missing.
        let fr = json!({"content": "", "is_binary": true});
        let (out, img) = attach_to_file_read(&fr, &download(&png[..png.len() - 6]));
        assert!(img.is_none());
        assert!(out["note"].as_str().unwrap().contains("truncated"));
    }
}
