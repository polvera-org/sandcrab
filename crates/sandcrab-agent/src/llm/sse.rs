//! Server-sent events: a byte-level frame parser and the Messages API stream accumulator.

use serde_json::Value;

use super::{EventSink, LlmError, Response, StreamEvent};

/// One SSE frame (`event:` + joined `data:` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser. Feed arbitrary byte chunks (UTF-8 may be split across chunks).
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut out);
        }
        out
    }

    /// Flush a final frame that was not terminated by a blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.line(
                String::from_utf8_lossy(&rest).trim_end_matches('\r'),
                &mut out,
            );
        }
        self.line("", &mut out);
        out.pop()
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseFrame>) {
        if line.is_empty() {
            if self.data.is_some() || self.event.is_some() {
                out.push(SseFrame {
                    event: self.event.take(),
                    data: self.data.take().unwrap_or_default(),
                });
            }
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
    }
}

/// Builds a [`Response`] from Messages API stream events.
///
/// Blocks are kept as raw JSON so unknown block types (server tools, `fallback`, ...) are
/// echoed back unchanged; known deltas are applied in place.
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    resp: Response,
    json_bufs: Vec<Option<String>>,
    done: bool,
}

fn append_str(block: &mut Value, key: &str, s: &str) {
    if let Some(obj) = block.as_object_mut() {
        let e = obj
            .entry(key)
            .or_insert_with(|| Value::String(String::new()));
        if let Value::String(t) = e {
            t.push_str(s);
        } else {
            *e = Value::String(s.to_owned());
        }
    }
}

impl MessageAccumulator {
    /// Apply one decoded event (`data:` JSON).
    pub fn apply(&mut self, ev: &Value, sink: &mut dyn EventSink) -> Result<(), LlmError> {
        let index = ev.get("index").and_then(Value::as_u64).map(|i| i as usize);
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let m = &ev["message"];
                self.resp.id = m["id"].as_str().unwrap_or_default().to_owned();
                self.resp.model = m["model"].as_str().unwrap_or_default().to_owned();
                self.resp.usage.merge_json(&m["usage"]);
                // A fallback restart re-sends message_start; content already received
                // is kept (the API never invalidates it).
            }
            "content_block_start" => {
                let i = index.ok_or_else(|| LlmError::Stream("block without index".into()))?;
                let block = ev["content_block"].clone();
                let kind = block["type"].as_str().unwrap_or("").to_owned();
                if self.resp.content.len() <= i {
                    self.resp.content.resize(i + 1, Value::Null);
                    self.json_bufs.resize(i + 1, None);
                }
                if matches!(kind.as_str(), "tool_use" | "server_tool_use") {
                    self.json_bufs[i] = Some(String::new());
                }
                self.resp.content[i] = block;
                sink.on_event(StreamEvent::BlockStart { kind: &kind });
            }
            "content_block_delta" => {
                let i = index.ok_or_else(|| LlmError::Stream("delta without index".into()))?;
                let block = self
                    .resp
                    .content
                    .get_mut(i)
                    .ok_or_else(|| LlmError::Stream(format!("delta for unknown block {i}")))?;
                let d = &ev["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        append_str(block, "text", t);
                        sink.on_event(StreamEvent::TextDelta(t));
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        append_str(block, "thinking", t);
                        sink.on_event(StreamEvent::ThinkingDelta(t));
                    }
                    "signature_delta" => {
                        if let Some(obj) = block.as_object_mut() {
                            obj.insert("signature".into(), d["signature"].clone());
                        }
                    }
                    "input_json_delta" => {
                        if let Some(Some(buf)) = self.json_bufs.get_mut(i) {
                            buf.push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    "citations_delta" => {
                        if let Some(obj) = block.as_object_mut() {
                            let arr = obj
                                .entry("citations")
                                .or_insert_with(|| Value::Array(Vec::new()));
                            if let Value::Array(a) = arr {
                                a.push(d["citation"].clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = index.ok_or_else(|| LlmError::Stream("stop without index".into()))?;
                if let Some(buf) = self.json_bufs.get_mut(i).and_then(Option::take) {
                    let block = &mut self.resp.content[i];
                    let parsed = if buf.trim().is_empty() {
                        Ok(Value::Object(serde_json::Map::new()))
                    } else {
                        serde_json::from_str::<Value>(&buf)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                if v.is_object() {
                                    Ok(v)
                                } else {
                                    Err("not a JSON object".into())
                                }
                            })
                    };
                    let input = match parsed {
                        Ok(v) => v,
                        Err(_) => {
                            let id = block["id"].as_str().unwrap_or_default().to_owned();
                            self.resp.invalid_tool_inputs.insert(id, buf);
                            Value::Object(serde_json::Map::new())
                        }
                    };
                    if let Some(obj) = block.as_object_mut() {
                        obj.insert("input".into(), input);
                    }
                }
                sink.on_event(StreamEvent::BlockStop);
            }
            "message_delta" => {
                let d = &ev["delta"];
                if let Some(r) = d["stop_reason"].as_str() {
                    self.resp.stop_reason = Some(r.to_owned());
                }
                if !d["stop_details"].is_null() {
                    self.resp.stop_details = Some(d["stop_details"].clone());
                }
                self.resp.usage.merge_json(&ev["usage"]);
            }
            "message_stop" => self.done = true,
            "error" => {
                let e = &ev["error"];
                return Err(LlmError::StreamApi {
                    kind: e["type"].as_str().unwrap_or("unknown").to_owned(),
                    message: e["message"].as_str().unwrap_or("").to_owned(),
                });
            }
            _ => {} // ping and future event types
        }
        Ok(())
    }

    /// Decode and apply one SSE frame.
    pub fn apply_frame(&mut self, f: &SseFrame, sink: &mut dyn EventSink) -> Result<(), LlmError> {
        if f.data.trim().is_empty() {
            return Ok(());
        }
        let v: Value = serde_json::from_str(&f.data)
            .map_err(|e| LlmError::Stream(format!("bad event JSON: {e}")))?;
        self.apply(&v, sink)
    }

    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn finish(mut self) -> Result<Response, LlmError> {
        if !self.done {
            return Err(LlmError::Stream("stream ended before message_stop".into()));
        }
        self.resp.content.retain(|b| !b.is_null());
        Ok(self.resp)
    }
}

/// Parse a whole recorded SSE body (tests, fixtures).
pub fn parse_stream(
    body: &[u8],
    chunk: usize,
    sink: &mut dyn EventSink,
) -> Result<Response, LlmError> {
    let mut p = SseParser::default();
    let mut acc = MessageAccumulator::default();
    for c in body.chunks(chunk.max(1)) {
        for f in p.feed(c) {
            acc.apply_frame(&f, sink)?;
        }
    }
    if let Some(f) = p.finish() {
        acc.apply_frame(&f, sink)?;
    }
    acc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::NoopSink;

    const TOOL_STREAM: &str = include_str!("../../tests/fixtures/stream_tool_use.sse");
    const ERROR_STREAM: &str = include_str!("../../tests/fixtures/stream_overloaded.sse");

    #[derive(Default)]
    struct Rec(Vec<String>);
    impl EventSink for Rec {
        fn on_event(&mut self, ev: StreamEvent<'_>) {
            match ev {
                StreamEvent::TextDelta(t) => self.0.push(format!("text:{t}")),
                StreamEvent::ThinkingDelta(t) => self.0.push(format!("think:{t}")),
                StreamEvent::BlockStart { kind } => self.0.push(format!("start:{kind}")),
                StreamEvent::BlockStop => self.0.push("stop".into()),
                StreamEvent::Retry { .. } => self.0.push("retry".into()),
            }
        }
    }

    #[test]
    fn frames_basic_and_multiline() {
        let mut p = SseParser::default();
        let f = p.feed(b"event: a\ndata: 1\ndata: 2\n\n: keepalive\n\ndata:x\r\n\r\n");
        assert_eq!(
            f,
            vec![
                SseFrame {
                    event: Some("a".into()),
                    data: "1\n2".into()
                },
                SseFrame {
                    event: None,
                    data: "x".into()
                }
            ]
        );
        assert!(p.feed(b"event: b\ndata: {}").is_empty());
        assert_eq!(p.finish().unwrap().data, "{}");
        assert!(p.finish().is_none());
    }

    #[test]
    fn recorded_tool_use_stream_any_chunking() {
        for chunk in [1, 3, 7, 64, 100_000] {
            let mut rec = Rec::default();
            let r = parse_stream(TOOL_STREAM.as_bytes(), chunk, &mut rec).unwrap();
            assert_eq!(r.id, "msg_01Fixture");
            assert_eq!(r.model, "claude-sonnet-5-5");
            assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
            assert_eq!(r.usage.input_tokens, 12);
            assert_eq!(r.usage.cache_read_input_tokens, 4096);
            assert_eq!(r.usage.cache_creation_input_tokens, 0);
            assert_eq!(r.usage.output_tokens, 87);
            assert_eq!(r.content.len(), 4);
            // thinking block keeps text + signature for echo
            assert_eq!(r.content[0]["type"], "thinking");
            assert_eq!(r.content[0]["thinking"], "Writing the page first.");
            assert_eq!(r.content[0]["signature"], "c2lnbmF0dXJl");
            assert_eq!(r.content[1]["text"], "I'll create the page — then list it.");
            assert_eq!(r.content[2]["name"], "file_write");
            assert_eq!(r.content[2]["input"]["path"], "index.html");
            assert_eq!(
                r.content[2]["input"]["content"],
                "<h1>Hello \"crab\" 🦀</h1>\n"
            );
            assert_eq!(r.content[3]["input"], serde_json::json!({"command": "ls"}));
            assert!(r.invalid_tool_inputs.is_empty());
            assert!(rec.0.contains(&"think:Writing the page first.".to_owned()));
            assert_eq!(
                rec.0.iter().filter(|e| e.starts_with("text:")).count(),
                2,
                "{:?}",
                rec.0
            );
        }
    }

    #[test]
    fn invalid_tool_json_is_reported_not_run() {
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\": \\\"ls\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let r = parse_stream(body.as_bytes(), 5, &mut NoopSink).unwrap();
        assert_eq!(r.content[0]["input"], serde_json::json!({}));
        assert_eq!(r.invalid_tool_inputs["toolu_1"], "{\"command\": \"ls");
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn in_stream_error_is_retryable_and_truncation_detected() {
        let err = parse_stream(ERROR_STREAM.as_bytes(), 9, &mut NoopSink).unwrap_err();
        assert!(matches!(&err, LlmError::StreamApi { kind, .. } if kind == "overloaded_error"));
        assert!(err.is_retryable());

        let cut = TOOL_STREAM.split("event: message_stop").next().unwrap();
        let err = parse_stream(cut.as_bytes(), 50, &mut NoopSink).unwrap_err();
        assert!(matches!(err, LlmError::Stream(_)), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_statuses() {
        let api = |status| LlmError::Api {
            status,
            kind: String::new(),
            message: String::new(),
            retry_after: None,
        };
        for s in [408, 409, 429, 500, 503, 529] {
            assert!(api(s).is_retryable(), "{s}");
        }
        for s in [400, 401, 403, 404, 413] {
            assert!(!api(s).is_retryable(), "{s}");
        }
    }
}
