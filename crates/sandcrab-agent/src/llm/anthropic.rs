//! Anthropic Messages API client: raw HTTP + SSE (no official Rust SDK), prompt caching,
//! adaptive thinking with progress updates, server-side refusal fallback, and
//! retry/backoff on 408/409/429/5xx/overloaded.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::sse::{MessageAccumulator, SseParser};
use super::{EventSink, LlmClient, LlmError, Request, Response, StreamEvent};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2026-10-21";
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;
pub const MAX_ATTEMPTS: u32 = 6;

const BETA_THINKING_UPDATES: &str = "thinking-display-updates-2026-08-18";
const BETA_FALLBACK: &str = "server-side-fallback-2026-07-01";

/// Models that accept `thinking.display = "updates"` (short progress notes between tool
/// calls, reasoning hidden).
fn supports_thinking_updates(model: &str) -> bool {
    model.starts_with("claude-fable-5")
        || model.starts_with("claude-mythos-5")
        || matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5")
}

/// Models that accept the server-side refusal fallback (`fallbacks: "default"`).
fn supports_fallback(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-2"
            | "claude-fable-5-1"
            | "claude-opus-5-5"
            | "claude-opus-5"
            | "claude-sonnet-5-5"
    )
}

/// Adaptive thinking is accepted by every current model except Haiku 4.5 and older.
fn supports_adaptive(model: &str) -> bool {
    !(model.starts_with("claude-haiku-4")
        || model.starts_with("claude-3")
        || model.contains("-4-5")
        || model.contains("-4-1")
        || model.contains("-4-0"))
}

#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    /// `output_config.effort` (`low` .. `max`); `None` = model default.
    pub effort: Option<String>,
    /// Send `fallbacks: "default"` on models that support it.
    pub fallback: bool,
    pub base_url: String,
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("effort", &self.effort)
            .field("fallback", &self.fallback)
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl AnthropicConfig {
    #[must_use]
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: None,
            fallback: true,
            base_url: DEFAULT_BASE_URL.into(),
        }
    }
}

#[derive(Debug)]
pub struct AnthropicClient {
    http: reqwest::Client,
    cfg: AnthropicConfig,
}

impl AnthropicClient {
    pub fn new(cfg: AnthropicConfig) -> Result<Self, LlmError> {
        if cfg.api_key.trim().is_empty() {
            return Err(LlmError::Config("ANTHROPIC_API_KEY is empty".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Streaming: no total timeout, but a stalled stream (no bytes, not even pings)
            // is a failure worth retrying.
            .read_timeout(Duration::from_secs(300))
            .user_agent(concat!("crabagent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| LlmError::Config(format!("HTTP client: {e}")))?;
        Ok(Self { http, cfg })
    }

    #[must_use]
    pub fn config(&self) -> &AnthropicConfig {
        &self.cfg
    }

    /// `anthropic-beta` values for this model.
    #[must_use]
    pub fn betas(&self) -> Vec<&'static str> {
        let mut b = Vec::new();
        if supports_thinking_updates(&self.cfg.model) {
            b.push(BETA_THINKING_UPDATES);
        }
        if self.cfg.fallback && supports_fallback(&self.cfg.model) {
            b.push(BETA_FALLBACK);
        }
        b
    }

    /// The JSON request body. Cache breakpoints: the system prompt (covers tools, which
    /// render before it) and top-level automatic caching (the growing message history).
    #[must_use]
    pub fn build_body(&self, req: &Request<'_>) -> Value {
        let model = self.cfg.model.as_str();
        let mut body = json!({
            "model": model,
            "max_tokens": self.cfg.max_tokens,
            "stream": true,
            "system": [{
                "type": "text",
                "text": req.system,
                "cache_control": {"type": "ephemeral"},
            }],
            "tools": req.tools,
            "messages": req.messages,
            "cache_control": {"type": "ephemeral"},
        });
        let obj = body.as_object_mut().expect("object literal");
        if supports_thinking_updates(model) {
            obj.insert(
                "thinking".into(),
                json!({"type": "adaptive", "display": "updates"}),
            );
        } else if supports_adaptive(model) {
            obj.insert("thinking".into(), json!({"type": "adaptive"}));
        }
        if let Some(effort) = &self.cfg.effort {
            obj.insert("output_config".into(), json!({ "effort": effort }));
        }
        if self.cfg.fallback && supports_fallback(model) {
            obj.insert("fallbacks".into(), json!("default"));
        }
        body
    }

    async fn send_once(&self, body: &[u8], sink: &mut dyn EventSink) -> Result<Response, LlmError> {
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let mut rb = self
            .http
            .post(url)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        let betas = self.betas();
        if !betas.is_empty() {
            rb = rb.header("anthropic-beta", betas.join(","));
        }
        let mut resp = rb
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.without_url().to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(|s| Duration::from_secs_f64(s.clamp(0.0, 300.0)));
            let text = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &text, retry_after));
        }

        let mut parser = SseParser::default();
        let mut acc = MessageAccumulator::default();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| LlmError::Transport(e.without_url().to_string()))?
        {
            for frame in parser.feed(&chunk) {
                acc.apply_frame(&frame, sink)?;
            }
            if acc.is_done() {
                break;
            }
        }
        if let Some(frame) = parser.finish() {
            acc.apply_frame(&frame, sink)?;
        }
        acc.finish()
    }
}

/// Map an HTTP error body (`{"type":"error","error":{"type","message"}}`) to [`LlmError`].
#[must_use]
pub fn api_error(status: u16, body: &str, retry_after: Option<Duration>) -> LlmError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let kind = v["error"]["type"]
        .as_str()
        .unwrap_or("http_error")
        .to_owned();
    let message = v["error"]["message"]
        .as_str()
        .map_or_else(|| body.chars().take(500).collect::<String>(), str::to_owned);
    LlmError::Api {
        status,
        kind,
        message,
        retry_after,
    }
}

/// Exponential backoff: 1, 2, 4, 8, 16 s (+ up to 25% jitter), capped at 60 s.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    let base = 2f64.powi(attempt.saturating_sub(1).min(6) as i32).min(60.0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let jitter = f64::from(nanos % 1000) / 1000.0 * 0.25 * base;
    Duration::from_secs_f64(base + jitter)
}

#[async_trait]
impl LlmClient for AnthropicClient {
    fn model(&self) -> &str {
        &self.cfg.model
    }

    async fn send(
        &self,
        req: &Request<'_>,
        sink: &mut dyn EventSink,
    ) -> Result<Response, LlmError> {
        let body = serde_json::to_vec(&self.build_body(req))
            .map_err(|e| LlmError::Config(format!("request encoding: {e}")))?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(&body, sink).await {
                Ok(r) => return Ok(r),
                Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => {
                    let wait = e.retry_after().unwrap_or_else(|| backoff(attempt));
                    let reason = e.to_string();
                    sink.on_event(StreamEvent::Retry {
                        attempt,
                        wait,
                        reason: &reason,
                    });
                    tokio::time::sleep(wait).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(model: &str) -> AnthropicClient {
        AnthropicClient::new(AnthropicConfig::new("sk-ant-secret-test", model)).unwrap()
    }

    #[test]
    fn body_has_caching_thinking_fallback() {
        let c = client("claude-sonnet-5-5");
        let tools = [json!({"name": "bash", "input_schema": {"type": "object"}})];
        let msgs = [json!({"role": "user", "content": "hi"})];
        let b = c.build_body(&Request {
            system: "sys",
            tools: &tools,
            messages: &msgs,
        });
        assert_eq!(b["model"], "claude-sonnet-5-5");
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["cache_control"]["type"], "ephemeral");
        assert_eq!(b["thinking"]["display"], "updates");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("output_config").is_none());
        assert_eq!(b["tools"][0]["name"], "bash");
        assert_eq!(c.betas(), vec![BETA_THINKING_UPDATES, BETA_FALLBACK]);
        assert!(!b.to_string().contains("sk-ant"));
    }

    #[test]
    fn body_per_model_features() {
        let mut cfg = AnthropicConfig::new("k", "claude-haiku-5-5");
        cfg.effort = Some("low".into());
        let c = AnthropicClient::new(cfg).unwrap();
        let b = c.build_body(&Request {
            system: "s",
            tools: &[],
            messages: &[],
        });
        assert_eq!(b["thinking"], json!({"type": "adaptive"}));
        assert!(b.get("fallbacks").is_none());
        assert_eq!(b["output_config"]["effort"], "low");
        assert!(c.betas().is_empty());

        let b = client("claude-haiku-4-5").build_body(&Request {
            system: "s",
            tools: &[],
            messages: &[],
        });
        assert!(b.get("thinking").is_none());

        let mut cfg = AnthropicConfig::new("k", "claude-opus-5-5");
        cfg.fallback = false;
        let c = AnthropicClient::new(cfg).unwrap();
        assert_eq!(c.betas(), vec![BETA_THINKING_UPDATES]);
    }

    #[test]
    fn key_is_redacted_and_required() {
        let c = client("claude-sonnet-5-5");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("secret"), "{dbg}");
        assert!(dbg.contains("redacted"));
        assert!(AnthropicClient::new(AnthropicConfig::new(" ", "m")).is_err());
    }

    #[test]
    fn api_errors_parse() {
        let e = api_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            None,
        );
        assert!(e.is_retryable());
        assert!(e.to_string().contains("overloaded_error"), "{e}");
        let e = api_error(400, "not json", Some(Duration::from_secs(3)));
        assert!(!e.is_retryable());
        assert_eq!(e.retry_after(), Some(Duration::from_secs(3)));
        assert!(e.to_string().contains("not json"));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(1) >= Duration::from_secs(1) && backoff(1) < Duration::from_secs(2));
        assert!(backoff(3) >= Duration::from_secs(4));
        assert!(backoff(20) <= Duration::from_secs(75));
    }
}
