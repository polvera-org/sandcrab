use std::io::Cursor;

use sandcrab_protocol::*;
use serde_json::{Value, json};

fn frame_of(v: &Value) -> Vec<u8> {
    encode_frame(v).unwrap()
}

#[test]
fn encode_prefixes_big_endian_length() {
    let frame = frame_of(&json!({"a": 1}));
    let body = br#"{"a":1}"#;
    assert_eq!(&frame[..4], &(body.len() as u32).to_be_bytes());
    assert_eq!(&frame[4..], body);
}

#[test]
fn decoder_handles_byte_by_byte_chunks() {
    let msg = json!({"jsonrpc": "2.0", "id": 7, "method": "ping"});
    let frame = frame_of(&msg);
    let mut dec = FrameDecoder::new();
    for (i, b) in frame.iter().enumerate() {
        dec.push(std::slice::from_ref(b));
        let out = dec.next_message::<Value>().unwrap();
        if i + 1 < frame.len() {
            assert!(out.is_none(), "frame yielded early at byte {i}");
        } else {
            assert_eq!(out.unwrap(), msg);
        }
    }
    assert_eq!(dec.buffered_len(), 0);
    dec.finish().unwrap();
}

#[test]
fn decoder_yields_multiple_frames_from_one_chunk() {
    let msgs: Vec<Value> = (0..3).map(|i| json!({ "n": i })).collect();
    let mut chunk: Vec<u8> = msgs.iter().flat_map(frame_of).collect();
    // Plus half of a fourth frame.
    let fourth = frame_of(&json!({"n": 3}));
    chunk.extend_from_slice(&fourth[..5]);

    let mut dec = FrameDecoder::new();
    dec.push(&chunk);
    for m in &msgs {
        assert_eq!(&dec.next_message::<Value>().unwrap().unwrap(), m);
    }
    assert!(dec.next_frame().unwrap().is_none());
    assert!(matches!(
        dec.finish(),
        Err(FrameError::Truncated { expected, got: 5 }) if expected == fourth.len()
    ));
    dec.push(&fourth[5..]);
    assert_eq!(
        dec.next_message::<Value>().unwrap().unwrap(),
        json!({"n": 3})
    );
}

#[test]
fn decoder_rejects_oversize_header_and_poisons() {
    let mut dec = FrameDecoder::new();
    dec.push(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
    assert!(matches!(
        dec.next_frame(),
        Err(FrameError::TooLarge { len, max: MAX_FRAME_LEN }) if len == MAX_FRAME_LEN + 1
    ));
    dec.push(&frame_of(&json!({})));
    assert!(matches!(dec.next_frame(), Err(FrameError::Poisoned)));
}

#[test]
fn decoder_rejects_zero_length_frame() {
    let mut dec = FrameDecoder::new();
    dec.push(&[0, 0, 0, 0]);
    assert!(matches!(dec.next_frame(), Err(FrameError::Empty)));
}

#[test]
fn garbage_bytes_are_rejected_cleanly() {
    // ASCII "GET " as a header announces ~1.1 GiB: rejected as oversize.
    let mut dec = FrameDecoder::new();
    dec.push(b"GET / HTTP/1.1\r\n");
    assert!(matches!(dec.next_frame(), Err(FrameError::TooLarge { .. })));

    // A well-formed header around non-JSON: frame ok, payload fails as Json / Parse.
    let mut bytes = 4u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(b"\xff\xfe{]");
    let mut dec = FrameDecoder::new();
    dec.push(&bytes);
    let err = dec.next_message::<Value>().unwrap_err();
    assert!(matches!(err, FrameError::Json(_)));
    assert_eq!(ProtocolError::from(err).code(), codes::PARSE_ERROR);
}

#[test]
fn encode_rejects_oversize_payload() {
    let big = "x".repeat(MAX_FRAME_LEN);
    assert!(matches!(
        encode_frame(&big),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn max_size_frame_is_accepted() {
    // JSON string of MAX_FRAME_LEN - 2 chars + 2 quotes == MAX_FRAME_LEN bytes.
    let s = "y".repeat(MAX_FRAME_LEN - 2);
    let frame = encode_frame(&s).unwrap();
    assert_eq!(frame.len(), HEADER_LEN + MAX_FRAME_LEN);
    let back: String = read_message(&mut Cursor::new(frame)).unwrap().unwrap();
    assert_eq!(back.len(), s.len());
}

#[test]
fn blocking_read_write_round_trip_and_clean_eof() {
    let mut wire = Vec::new();
    write_frame(&mut wire, &json!({"a": 1})).unwrap();
    write_frame(&mut wire, &json!({"b": 2})).unwrap();
    let mut r = Cursor::new(wire);
    assert_eq!(
        read_message::<Value, _>(&mut r).unwrap(),
        Some(json!({"a": 1}))
    );
    assert_eq!(
        read_message::<Value, _>(&mut r).unwrap(),
        Some(json!({"b": 2}))
    );
    assert_eq!(read_message::<Value, _>(&mut r).unwrap(), None);
}

#[test]
fn blocking_read_reports_truncated_header_and_body() {
    let mut r = Cursor::new(vec![0u8, 0]);
    assert!(matches!(
        read_frame(&mut r),
        Err(FrameError::Truncated {
            expected: 4,
            got: 2
        })
    ));

    let mut frame = frame_of(&json!({"abc": "def"}));
    let full = frame.len() - HEADER_LEN;
    frame.truncate(frame.len() - 3);
    assert!(matches!(
        read_frame(&mut Cursor::new(frame)),
        Err(FrameError::Truncated { expected, got }) if expected == full && got == full - 3
    ));
}

#[test]
fn parse_header_validates() {
    assert_eq!(parse_header(5u32.to_be_bytes()).unwrap(), 5);
    assert!(matches!(parse_header([0; 4]), Err(FrameError::Empty)));
    assert!(matches!(
        parse_header(u32::MAX.to_be_bytes()),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn frame_error_maps_to_protocol_error() {
    let e: ProtocolError = FrameError::TooLarge { len: 10, max: 5 }.into();
    assert_eq!(e, ProtocolError::TooLarge { size: 10, limit: 5 });
    let e: ProtocolError = FrameError::Empty.into();
    assert_eq!(e.code(), codes::INVALID_REQUEST);
}
