//! LLM client abstraction. [`anthropic::AnthropicClient`] talks to the Anthropic Messages
//! API (streaming); [`fake::ScriptedLlm`] replays a script for tests.

pub mod anthropic;
pub mod fake;
pub mod sse;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token usage of one response (or a running total).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Self) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_creation_input_tokens += o.cache_creation_input_tokens;
        self.cache_read_input_tokens += o.cache_read_input_tokens;
    }

    /// Overwrite the fields present (and numeric) in an API `usage` object. Streaming
    /// `message_delta` usage is cumulative, so overwriting is correct.
    pub fn merge_json(&mut self, v: &Value) {
        let set = |field: &mut u64, key: &str| {
            if let Some(n) = v.get(key).and_then(Value::as_u64) {
                *field = n;
            }
        };
        set(&mut self.input_tokens, "input_tokens");
        set(&mut self.output_tokens, "output_tokens");
        set(
            &mut self.cache_creation_input_tokens,
            "cache_creation_input_tokens",
        );
        set(&mut self.cache_read_input_tokens, "cache_read_input_tokens");
    }

    /// All prompt tokens (uncached + cache writes + cache reads).
    #[must_use]
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

/// One Messages API request. `tools` are already in Anthropic shape; `messages` is the
/// full (append-only) conversation.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub system: &'a str,
    pub tools: &'a [Value],
    pub messages: &'a [Value],
}

/// A complete assistant message.
#[derive(Debug, Clone, Default)]
pub struct Response {
    pub id: String,
    pub model: String,
    /// Content blocks exactly as produced (thinking blocks keep their signatures).
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    pub usage: Usage,
    /// `tool_use` id → raw streamed input that was not a JSON object. The block's `input`
    /// is `{}`; the harness must answer it with an error instead of running the tool.
    pub invalid_tool_inputs: HashMap<String, String>,
}

/// Streaming progress, for live terminal output.
#[derive(Debug, Clone, Copy)]
pub enum StreamEvent<'a> {
    TextDelta(&'a str),
    /// Progress-update / summarized thinking text.
    ThinkingDelta(&'a str),
    BlockStart {
        kind: &'a str,
    },
    BlockStop,
    Retry {
        attempt: u32,
        wait: Duration,
        reason: &'a str,
    },
}

pub trait EventSink: Send {
    fn on_event(&mut self, ev: StreamEvent<'_>);
}

/// Discards events.
#[derive(Debug, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn on_event(&mut self, _ev: StreamEvent<'_>) {}
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Anthropic API error (HTTP {status}, {kind}): {message}")]
    Api {
        status: u16,
        kind: String,
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("Anthropic stream error ({kind}): {message}")]
    StreamApi { kind: String, message: String },
    #[error("network error: {0}")]
    Transport(String),
    #[error("malformed response stream: {0}")]
    Stream(String),
    #[error("{0}")]
    Config(String),
}

impl LlmError {
    /// 408/409/429/5xx/529, network errors, broken streams and in-stream overloads.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => matches!(status, 408 | 409 | 429) || *status >= 500,
            Self::StreamApi { kind, .. } => matches!(
                kind.as_str(),
                "overloaded_error" | "api_error" | "rate_limit_error"
            ),
            Self::Transport(_) | Self::Stream(_) => true,
            Self::Config(_) => false,
        }
    }

    /// The API refused the request itself (400 / 413 / 422, or an in-stream
    /// `invalid_request_error`): resending the same history fails the same way.
    #[must_use]
    pub fn is_request_rejection(&self) -> bool {
        match self {
            Self::Api { status, .. } => matches!(status, 400 | 413 | 422),
            Self::StreamApi { kind, .. } => {
                matches!(kind.as_str(), "invalid_request_error" | "request_too_large")
            }
            _ => false,
        }
    }

    /// A request rejection that blames an image (dimensions, format, corrupt data, too many).
    #[must_use]
    pub fn is_image_rejection(&self) -> bool {
        let msg = match self {
            Self::Api { message, .. } | Self::StreamApi { message, .. } => message,
            _ => return false,
        };
        let m = msg.to_ascii_lowercase();
        self.is_request_rejection() && (m.contains("image") || m.contains("media"))
    }

    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Model id requests are sent with.
    fn model(&self) -> &str;

    /// Send one request; stream progress into `sink`; return the full message.
    async fn send(&self, req: &Request<'_>, sink: &mut dyn EventSink)
    -> Result<Response, LlmError>;
}
