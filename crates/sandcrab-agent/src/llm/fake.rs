//! Scripted fake LLM for tests: replays canned responses, records every request.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{EventSink, LlmClient, LlmError, Request, Response, StreamEvent, Usage};

/// Replays `responses` (or errors) in order; fails once the script is exhausted.
#[derive(Debug, Default)]
pub struct ScriptedLlm {
    script: Mutex<VecDeque<Result<Response, LlmError>>>,
    /// `messages` of every request received, in order.
    pub requests: Mutex<Vec<Vec<Value>>>,
}

impl ScriptedLlm {
    #[must_use]
    pub fn new(responses: Vec<Response>) -> Self {
        Self::with_results(responses.into_iter().map(Ok).collect())
    }

    /// A script that may also fail requests (e.g. an HTTP 400).
    #[must_use]
    pub fn with_results(results: Vec<Result<Response, LlmError>>) -> Self {
        Self {
            script: Mutex::new(results.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn requests(&self) -> Vec<Vec<Value>> {
        self.requests.lock().expect("lock").clone()
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.script.lock().expect("lock").len()
    }
}

/// A `text` block.
#[must_use]
pub fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

/// A `tool_use` block.
#[must_use]
pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

/// An HTTP 400 `invalid_request_error` with `message`.
#[must_use]
pub fn bad_request(message: &str) -> LlmError {
    LlmError::Api {
        status: 400,
        kind: "invalid_request_error".into(),
        message: message.into(),
        retry_after: None,
    }
}

/// A canned response with fixed token usage (100 in / 20 out).
#[must_use]
pub fn reply(content: Vec<Value>, stop_reason: &str) -> Response {
    Response {
        id: format!("msg_fake_{}", uuid::Uuid::new_v4().simple()),
        model: "fake-model".into(),
        content,
        stop_reason: Some(stop_reason.into()),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            ..Usage::default()
        },
        ..Response::default()
    }
}

#[async_trait]
impl LlmClient for ScriptedLlm {
    fn model(&self) -> &str {
        "fake-model"
    }

    async fn send(
        &self,
        req: &Request<'_>,
        sink: &mut dyn EventSink,
    ) -> Result<Response, LlmError> {
        self.requests
            .lock()
            .expect("lock")
            .push(req.messages.to_vec());
        let resp = self
            .script
            .lock()
            .expect("lock")
            .pop_front()
            .ok_or_else(|| LlmError::Stream("fake LLM script exhausted".into()))??;
        for b in &resp.content {
            let kind = b["type"].as_str().unwrap_or("");
            sink.on_event(StreamEvent::BlockStart { kind });
            if kind == "text" {
                sink.on_event(StreamEvent::TextDelta(b["text"].as_str().unwrap_or("")));
            }
            sink.on_event(StreamEvent::BlockStop);
        }
        Ok(resp)
    }
}
