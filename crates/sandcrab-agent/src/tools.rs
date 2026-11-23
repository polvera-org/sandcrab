//! Sandcrab tool catalog ↔ Anthropic tool-use: schema mapping, tool results, and the
//! compact one-line rendering of calls for the terminal.

use std::time::Duration;

use sandcrab_sdk::{ToolDefinition, is_tool_error};
use serde_json::{Value, json};

use crate::media::{self, ImageAttachment};

/// Map Sandcrab tool definitions 1:1 to Anthropic client tools (`name`, `description`,
/// `input_schema`). Requests are streamed, so `eager_input_streaming` is on: large file
/// bodies stream as generated; inputs are then validated by the strict JSON parse in the
/// stream accumulator and by `tool_dispatch` (unknown fields / wrong types → error).
#[must_use]
pub fn anthropic_tools(defs: &[ToolDefinition]) -> Vec<Value> {
    defs.iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "input_schema": d.input_schema,
                "eager_input_streaming": true,
            })
        })
        .collect()
}

/// Result of running one `tool_use` block.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// Tool result JSON, or `{"error": {"code", "message"}}`.
    pub output: Value,
    pub is_error: bool,
    /// The sandbox is unusable (destroyed / transport lost).
    pub fatal: bool,
    pub elapsed: Duration,
    /// An image sent to the model as an `image` block next to `output` (see [`media`]).
    pub image: Option<ImageAttachment>,
}

impl ToolOutcome {
    /// From a `tool_dispatch` reply: `Ok` (result or rendered recoverable error) or a fatal
    /// error.
    #[must_use]
    pub fn from_dispatch(
        id: &str,
        name: &str,
        input: Value,
        reply: Result<Value, sandcrab_sdk::ToolError>,
        elapsed: Duration,
    ) -> Self {
        let (output, is_error, fatal) = match reply {
            Ok(v) => {
                let err = is_tool_error(&v);
                (v, err, false)
            }
            Err(e) => (e.to_json(), true, e.fatal),
        };
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
            output,
            is_error,
            fatal,
            elapsed,
            image: None,
        }
    }

    /// An error result produced by the harness (tool not run).
    #[must_use]
    pub fn harness_error(id: &str, name: &str, input: Value, code: &str, message: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
            output: json!({"error": {"code": code, "message": message}}),
            is_error: true,
            fatal: false,
            elapsed: Duration::ZERO,
            image: None,
        }
    }

    /// Strip the base64 from a successful `download` result: images become an attachment,
    /// small text is inlined, anything else is summarised (see [`media`]).
    pub fn shape_download(&mut self) {
        if self.is_error || self.name != "download" {
            return;
        }
        let (out, img) = media::shape_download(&self.output);
        self.output = out;
        self.image = img;
    }

    /// The `tool_result` content block: JSON string content, or `[text, image]` blocks when
    /// an image is attached.
    #[must_use]
    pub fn result_block(&self) -> Value {
        let content = match &self.image {
            None => Value::String(self.output.to_string()),
            Some(img) => json!([
                {"type": "text", "text": self.output.to_string()},
                img.block(),
            ]),
        };
        let mut b = json!({
            "type": "tool_result",
            "tool_use_id": self.id,
            "content": content,
        });
        if self.is_error {
            b["is_error"] = Value::Bool(true);
        }
        b
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let multi = s.trim_end().contains('\n');
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || multi {
        out.push('…');
    }
    out
}

/// Short argument summary for the call line (`npm run build`, `src/app.ts`, ...).
#[must_use]
pub fn summarize_call(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" | "process_start" => first_line(s("command"), 80),
        "file_move" => format!("{} → {}", s("from"), s("to")),
        "glob" => s("pattern").to_owned(),
        "grep" => {
            let p = s("path");
            if p.is_empty() {
                format!("/{}/", first_line(s("pattern"), 60))
            } else {
                format!("/{}/ {p}", first_line(s("pattern"), 60))
            }
        }
        "process_poll" | "process_kill" | "process_stdin" => s("process_id").to_owned(),
        "process_list" => String::new(),
        "list_dir" => {
            let p = s("path");
            if p.is_empty() {
                ".".into()
            } else {
                p.to_owned()
            }
        }
        _ => s("path").to_owned(),
    }
}

/// Short result summary for the call line (`exit 0`, `3 matches`, `not_found`, ...).
#[must_use]
pub fn summarize_result(name: &str, output: &Value, is_error: bool) -> String {
    if is_error {
        let code = output["error"]["code"].as_str().unwrap_or("error");
        return format!("error: {code}");
    }
    let n = |k: &str| output.get(k).and_then(Value::as_u64);
    let len = |k: &str| output.get(k).and_then(Value::as_array).map(Vec::len);
    match name {
        "bash" => match (output["exit_code"].as_i64(), output["signal"].as_i64()) {
            _ if output["timed_out"] == true => "timed out".into(),
            (Some(c), _) => format!("exit {c}"),
            (None, Some(s)) => format!("signal {s}"),
            _ => "done".into(),
        },
        "file_write" => n("bytes_written").map_or("ok".into(), |b| format!("{b} bytes")),
        "file_edit" => n("replacements").map_or("ok".into(), |r| format!("{r} replaced")),
        "download" | "file_read" if output.get("mime").is_some() => {
            let mime = output["mime"].as_str().unwrap_or("");
            let size = n("size").map_or(String::new(), |b| format!(", {b} bytes"));
            if output["note"]
                .as_str()
                .is_some_and(|t| t.contains("attached"))
            {
                format!("{mime} attached{size}")
            } else {
                format!("{mime}{size}")
            }
        }
        "file_read" => output["content"]
            .as_str()
            .map_or("ok".into(), |c| format!("{} lines", c.lines().count())),
        "glob" => len("paths")
            .or_else(|| len("matches"))
            .map_or("ok".into(), |k| format!("{k} paths")),
        "grep" => len("matches").map_or("ok".into(), |k| format!("{k} matches")),
        "list_dir" => len("entries").map_or("ok".into(), |k| format!("{k} entries")),
        "process_start" => output["process_id"]
            .as_str()
            .map_or("started".into(), |p| format!("started {p}")),
        "process_poll" => {
            if output["running"] == true {
                "running".into()
            } else if let Some(c) = output["exit_code"].as_i64() {
                format!("exited {c}")
            } else {
                "exited".into()
            }
        }
        _ => "ok".into(),
    }
}

/// Output worth showing under the call line (bash / process output, edit diffs, errors),
/// truncated to `max_lines` (the model still gets the full, crabd-bounded result).
#[must_use]
pub fn preview_output(
    name: &str,
    output: &Value,
    is_error: bool,
    max_lines: usize,
) -> Option<String> {
    let text = if is_error {
        output["error"]["message"].as_str().unwrap_or("").to_owned()
    } else {
        match name {
            "bash" | "process_poll" => {
                let out = output["stdout"].as_str().unwrap_or("");
                let err = output["stderr"].as_str().unwrap_or("");
                match (out.trim().is_empty(), err.trim().is_empty()) {
                    (true, true) => return None,
                    (false, true) => out.to_owned(),
                    (true, false) => err.to_owned(),
                    (false, false) => format!("{}\n{}", out.trim_end(), err),
                }
            }
            "file_edit" => output["diff"].as_str().unwrap_or("").to_owned(),
            _ => return None,
        }
    };
    let text = text.trim_end();
    if text.is_empty() {
        return None;
    }
    Some(truncate_lines(text, max_lines, 200))
}

/// Keep the first `max_lines` lines (each cut at `max_cols` chars), noting what was hidden.
#[must_use]
pub fn truncate_lines(text: &str, max_lines: usize, max_cols: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines
        .iter()
        .take(max_lines)
        .map(|l| {
            if l.chars().count() > max_cols {
                let mut s: String = l.chars().take(max_cols).collect();
                s.push('…');
                s
            } else {
                (*l).to_owned()
            }
        })
        .collect();
    if lines.len() > max_lines {
        out.push(format!("… ({} more lines)", lines.len() - max_lines));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use sandcrab_sdk::{TOOL_NAMES, ToolError, tool_definitions};

    use super::*;

    #[test]
    fn all_tools_map_one_to_one() {
        let defs = tool_definitions();
        let tools = anthropic_tools(&defs);
        assert_eq!(tools.len(), 16);
        assert_eq!(tools.len(), TOOL_NAMES.len());
        for (t, name) in tools.iter().zip(TOOL_NAMES) {
            assert_eq!(t["name"], *name);
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
            assert_eq!(t["input_schema"]["type"], "object", "{name}");
            assert_eq!(t["eager_input_streaming"], true);
            // Only Anthropic tool fields.
            let keys: Vec<&String> = t.as_object().unwrap().keys().collect();
            assert_eq!(keys.len(), 4, "{keys:?}");
            // Tool names must match the API's name pattern.
            assert!(
                name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            );
        }
    }

    #[test]
    fn dispatch_errors_become_is_error() {
        let ok = ToolOutcome::from_dispatch(
            "t1",
            "bash",
            json!({"command": "ls"}),
            Ok(json!({"exit_code": 0, "stdout": "a\n", "stderr": ""})),
            Duration::from_millis(5),
        );
        assert!(!ok.is_error && !ok.fatal);
        let b = ok.result_block();
        assert_eq!(b["tool_use_id"], "t1");
        assert!(b.get("is_error").is_none());
        let content: Value = serde_json::from_str(b["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["stdout"], "a\n");

        let err = ToolOutcome::from_dispatch(
            "t2",
            "file_read",
            json!({"path": "x"}),
            Ok(json!({"error": {"code": "not_found", "message": "x: not found"}})),
            Duration::ZERO,
        );
        assert!(err.is_error && !err.fatal);
        assert_eq!(err.result_block()["is_error"], true);

        // A successful result that merely contains an `error` field is not an error.
        let not_err = ToolOutcome::from_dispatch(
            "t3",
            "file_read",
            json!({}),
            Ok(json!({"error": "text", "content": "..."})),
            Duration::ZERO,
        );
        assert!(!not_err.is_error);

        let fatal_err: ToolError = sandcrab_sdk::SandboxError::NotFound {
            kind: "sandbox",
            id: "x".into(),
        }
        .into();
        let fatal =
            ToolOutcome::from_dispatch("t4", "bash", json!({}), Err(fatal_err), Duration::ZERO);
        assert!(fatal.is_error && fatal.fatal);
        assert_eq!(fatal.output["error"]["code"], "sandbox_unavailable");

        let h = ToolOutcome::harness_error("t5", "bash", json!({}), "INVALID_JSON", "bad");
        assert_eq!(h.result_block()["is_error"], true);
    }

    #[test]
    fn image_download_is_sent_as_an_image_block() {
        use base64::Engine as _;
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let mut o = ToolOutcome::from_dispatch(
            "t9",
            "download",
            json!({"path": "/tmp/h1.png"}),
            Ok(json!({"path": "/tmp/h1.png", "content_base64": b64, "size": 16, "mode": 420})),
            Duration::ZERO,
        );
        o.shape_download();
        assert_eq!(
            summarize_result(&o.name, &o.output, false),
            "image/png attached, 16 bytes"
        );
        let block = o.result_block();

        // The exact request body the Anthropic client sends.
        let client = crate::llm::anthropic::AnthropicClient::new(
            crate::llm::anthropic::AnthropicConfig::new("sk-test", "claude-sonnet-5-5"),
        )
        .unwrap();
        let msgs = [json!({"role": "user", "content": [block]})];
        let body = client.build_body(&crate::llm::Request {
            system: "s",
            tools: &[],
            messages: &msgs,
        });
        let tr = &body["messages"][0]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "t9");
        let parts = tr["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        let note: Value = serde_json::from_str(parts[0]["text"].as_str().unwrap()).unwrap();
        assert!(note.get("content_base64").is_none());
        assert_eq!(note["mime"], "image/png");
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], b64);
        // No base64 anywhere in text form.
        assert!(!parts[0]["text"].as_str().unwrap().contains(&b64));

        // A non-image binary never reaches the model as base64.
        let blob = base64::engine::general_purpose::STANDARD.encode([0u8, 0xFF, 1, 0xFE]);
        let mut o = ToolOutcome::from_dispatch(
            "t10",
            "download",
            json!({"path": "a.bin"}),
            Ok(json!({"path": "/w/a.bin", "content_base64": blob, "size": 4, "mode": 420})),
            Duration::ZERO,
        );
        o.shape_download();
        assert!(o.image.is_none());
        let b = o.result_block();
        let c: Value = serde_json::from_str(b["content"].as_str().unwrap()).unwrap();
        assert!(c.get("content_base64").is_none());
        assert_eq!(c["mime"], "application/octet-stream");
        assert!(c["sha256"].is_string());
    }

    #[test]
    fn summaries() {
        assert_eq!(
            summarize_call("bash", &json!({"command": "npm run build"})),
            "npm run build"
        );
        assert_eq!(summarize_call("bash", &json!({"command": "a\nb"})), "a…");
        assert_eq!(
            summarize_call("file_edit", &json!({"path": "src/a.ts"})),
            "src/a.ts"
        );
        assert_eq!(
            summarize_call("file_move", &json!({"from": "a", "to": "b"})),
            "a → b"
        );
        assert_eq!(summarize_call("list_dir", &json!({})), ".");
        assert_eq!(
            summarize_result("bash", &json!({"exit_code": 2}), false),
            "exit 2"
        );
        assert_eq!(
            summarize_result(
                "bash",
                &json!({"exit_code": null, "timed_out": true}),
                false
            ),
            "timed out"
        );
        assert_eq!(
            summarize_result("x", &json!({"error": {"code": "not_found"}}), true),
            "error: not_found"
        );
        assert_eq!(
            summarize_result("file_write", &json!({"bytes_written": 12}), false),
            "12 bytes"
        );
    }

    #[test]
    fn previews_truncate() {
        let long: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let p = preview_output("bash", &json!({"stdout": long, "stderr": ""}), false, 5).unwrap();
        assert_eq!(p.lines().count(), 6);
        assert!(p.ends_with("(45 more lines)"), "{p}");
        assert!(preview_output("bash", &json!({"stdout": "", "stderr": ""}), false, 5).is_none());
        assert!(preview_output("glob", &json!({"paths": []}), false, 5).is_none());
        let e = preview_output(
            "glob",
            &json!({"error": {"code": "c", "message": "boom"}}),
            true,
            5,
        );
        assert_eq!(e.as_deref(), Some("boom"));
        assert_eq!(truncate_lines(&"x".repeat(10), 1, 4), "xxxx…");
    }
}
