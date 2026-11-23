//! The agent loop: prompt → model → tool calls (in parallel, in the sandbox) → tool
//! results → model … until `end_turn` or the turn limit.
//!
//! History is append-only (never edited), so cached prefixes and thinking blocks stay
//! valid across requests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sandcrab_sdk::Sandbox;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::cost::UsageTotals;
use crate::llm::{LlmClient, Request};
use crate::media;
use crate::tools::ToolOutcome;
use crate::transcript::Transcript;
use crate::ui::Ui;

/// Signal state shared by the session. Cloneable.
///
/// - Ctrl-C (SIGINT) increments a counter: it interrupts the current turn, the session
///   goes on (REPL) or ends with 130 (one-shot).
/// - SIGTERM / SIGHUP / SIGQUIT are **terminating**: the first one is recorded and the
///   session must destroy its sandbox and exit `128 + signal` (see [`Self::terminated`]).
#[derive(Debug, Clone)]
pub struct Interrupt {
    tx: Arc<watch::Sender<u64>>,
    rx: watch::Receiver<u64>,
    term_tx: Arc<watch::Sender<Option<i32>>>,
    term_rx: watch::Receiver<Option<i32>>,
}

impl Default for Interrupt {
    fn default() -> Self {
        let (tx, rx) = watch::channel(0);
        let (term_tx, term_rx) = watch::channel(None);
        Self {
            tx: Arc::new(tx),
            rx,
            term_tx: Arc::new(term_tx),
            term_rx,
        }
    }
}

/// SIGTERM, SIGHUP, SIGQUIT.
#[cfg(unix)]
pub const TERMINATING_SIGNALS: [i32; 3] = [15, 1, 3];

impl Interrupt {
    /// Route SIGINT to the counter and SIGTERM / SIGHUP / SIGQUIT to the termination latch
    /// (none of them kills the process any more).
    #[must_use]
    pub fn install() -> Self {
        let me = Self::default();
        let tx = me.tx.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                tx.send_modify(|n| *n += 1);
            }
        });
        #[cfg(unix)]
        for sig in TERMINATING_SIGNALS {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::from_raw(sig)) {
                Ok(mut stream) => {
                    let me = me.clone();
                    tokio::spawn(async move {
                        while stream.recv().await.is_some() {
                            me.terminate(sig);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(signal = sig, error = %e, "cannot install signal handler");
                }
            }
        }
        me
    }

    /// Simulate a Ctrl-C (tests).
    pub fn trigger(&self) {
        self.tx.send_modify(|n| *n += 1);
    }

    /// Latch a terminating signal (the first one wins) and interrupt the current turn.
    pub fn terminate(&self, sig: i32) {
        self.term_tx
            .send_if_modified(|v| v.is_none() && v.replace(sig).is_none());
        self.trigger();
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        *self.rx.borrow()
    }

    /// The terminating signal received, if any.
    #[must_use]
    pub fn termination(&self) -> Option<i32> {
        *self.term_rx.borrow()
    }

    /// Resolves once the counter exceeds `base`.
    pub async fn beyond(&self, base: u64) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(|n| *n > base).await;
    }

    /// Resolves with the signal number once a terminating signal was received.
    pub async fn terminated(&self) -> i32 {
        let mut rx = self.term_rx.clone();
        let sig = rx.wait_for(Option::is_some).await.ok().and_then(|v| *v);
        match sig {
            Some(s) => s,
            // Sender gone (cannot happen while `self` holds it): never resolve.
            None => std::future::pending().await,
        }
    }
}

/// How a user turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model finished (`end_turn`).
    Done,
    MaxTurns,
    /// The model declined (`stop_reason: refusal`).
    Refused,
    Interrupted,
    /// The sandbox is gone; the session cannot continue.
    SandboxLost(String),
    /// The API failed after retries.
    Failed(String),
}

/// What to send back for an assistant turn: the content as produced, except that after a
/// mid-output refusal fallback the declined model's `thinking` / `redacted_thinking` /
/// `tool_use` blocks before the last `fallback` block are omitted (the block itself stays).
#[must_use]
pub fn echo_content(content: &[Value]) -> Vec<Value> {
    let Some(fb) = content.iter().rposition(|b| b["type"] == "fallback") else {
        return content.to_vec();
    };
    content
        .iter()
        .enumerate()
        .filter(|(i, b)| {
            *i >= fb
                || !matches!(
                    b["type"].as_str(),
                    Some("thinking" | "redacted_thinking" | "tool_use")
                )
        })
        .map(|(_, b)| b.clone())
        .collect()
}

pub struct Agent {
    llm: Arc<dyn LlmClient>,
    sandbox: Arc<Sandbox>,
    system: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    logged: usize,
    pub max_turns: u32,
    pub usage: UsageTotals,
    pub ui: Ui,
    transcript: Option<Transcript>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.llm.model())
            .field("messages", &self.messages.len())
            .finish_non_exhaustive()
    }
}

const MAX_TOKENS_TOOL: &str = "Your response hit the max_tokens limit while writing this tool \
    call, so it was not run. Split the work into smaller steps (for example, write a large file \
    in several parts using file_write with append: true).";
const MAX_TOKENS_CONTINUE: &str = "(Your previous response was cut off by the max_tokens limit. Continue from where you stopped.)";

impl Agent {
    #[must_use]
    pub fn new(
        llm: Arc<dyn LlmClient>,
        sandbox: Arc<Sandbox>,
        system: String,
        tools: Vec<Value>,
    ) -> Self {
        Self {
            llm,
            sandbox,
            system,
            tools,
            messages: Vec::new(),
            logged: 0,
            max_turns: 100,
            usage: UsageTotals::default(),
            ui: Ui::default(),
            transcript: None,
        }
    }

    #[must_use]
    pub fn with_transcript(mut self, t: Transcript) -> Self {
        self.transcript = Some(t);
        self
    }

    #[must_use]
    pub fn with_ui(mut self, ui: Ui) -> Self {
        self.ui = ui;
        self
    }

    #[must_use]
    pub fn system(&self) -> &str {
        &self.system
    }

    #[must_use]
    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    #[must_use]
    pub fn transcript_path(&self) -> Option<&std::path::Path> {
        self.transcript.as_ref().map(Transcript::path)
    }

    pub fn log(&mut self, kind: &str, fields: Value) {
        if let Some(t) = &mut self.transcript {
            t.log(kind, fields);
        }
    }

    /// Forget the conversation (same sandbox, same workspace contents).
    pub fn reset(&mut self) {
        self.messages.clear();
        self.logged = 0;
        self.log("reset", json!({}));
    }

    /// Run one user turn to completion.
    pub async fn run(&mut self, prompt: &str, intr: &Interrupt) -> TurnOutcome {
        let base = intr.count();
        self.messages
            .push(json!({"role": "user", "content": prompt}));
        self.log("user", json!({ "text": prompt }));

        for step in 1..=self.max_turns {
            let new_msgs = self.messages[self.logged..].to_vec();
            self.logged = self.messages.len();
            let model = self.llm.model().to_owned();
            self.log(
                "request",
                json!({
                    "step": step,
                    "model": model,
                    "n_messages": self.messages.len(),
                    "new_messages": media::redact_images(&Value::Array(new_msgs)),
                }),
            );

            let started = Instant::now();
            let llm = self.llm.clone();
            let req = Request {
                system: &self.system,
                tools: &self.tools,
                messages: &self.messages,
            };
            let res = tokio::select! {
                r = llm.send(&req, &mut self.ui) => Some(r),
                () = intr.beyond(base) => None,
            };
            let ms = started.elapsed().as_millis() as u64;
            let resp = match res {
                None => {
                    self.ui.info("⏹ interrupted");
                    self.log("interrupted", json!({ "step": step, "during": "model" }));
                    return TurnOutcome::Interrupted;
                }
                Some(Err(e)) => {
                    let msg = e.to_string();
                    self.ui.line(&format!("error: {msg}"));
                    self.log("error", json!({ "step": step, "error": msg, "ms": ms }));
                    return TurnOutcome::Failed(msg);
                }
                Some(Ok(r)) => r,
            };
            let served = if resp.model.is_empty() {
                model.clone()
            } else {
                resp.model.clone()
            };
            self.usage.add(&served, &resp.usage);
            self.log(
                "response",
                json!({
                    "step": step,
                    "id": resp.id,
                    "model": served,
                    "stop_reason": resp.stop_reason,
                    "stop_details": resp.stop_details,
                    "usage": resp.usage,
                    "content": resp.content,
                    "ms": ms,
                }),
            );

            let content = echo_content(&resp.content);
            let calls: Vec<(String, String, Value)> = content
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    (
                        b["id"].as_str().unwrap_or_default().to_owned(),
                        b["name"].as_str().unwrap_or_default().to_owned(),
                        b["input"].clone(),
                    )
                })
                .collect();
            if !content.is_empty() {
                self.messages
                    .push(json!({"role": "assistant", "content": content}));
            }
            self.logged = self.messages.len();

            match resp.stop_reason.as_deref().unwrap_or("end_turn") {
                "refusal" => {
                    if !calls.is_empty() {
                        let results = calls
                            .iter()
                            .map(|(id, name, input)| {
                                ToolOutcome::harness_error(
                                    id,
                                    name,
                                    input.clone(),
                                    "refused",
                                    "not run: the response was declined",
                                )
                            })
                            .collect::<Vec<_>>();
                        self.push_results(&results);
                    }
                    let cat = resp
                        .stop_details
                        .as_ref()
                        .and_then(|d| d["category"].as_str())
                        .unwrap_or("unspecified");
                    self.ui.line(&format!(
                        "⚠ the model declined this request (category: {cat})"
                    ));
                    return TurnOutcome::Refused;
                }
                "max_tokens" if !calls.is_empty() => {
                    let results = calls
                        .iter()
                        .map(|(id, name, input)| {
                            ToolOutcome::harness_error(
                                id,
                                name,
                                input.clone(),
                                "max_tokens",
                                MAX_TOKENS_TOOL,
                            )
                        })
                        .collect::<Vec<_>>();
                    self.ui.info("[max_tokens: tool call truncated, not run]");
                    self.push_results(&results);
                }
                "max_tokens" => {
                    self.ui.info("[max_tokens: asking the model to continue]");
                    self.messages
                        .push(json!({"role": "user", "content": MAX_TOKENS_CONTINUE}));
                }
                "pause_turn" => {}
                _ if !calls.is_empty() => {
                    let invalid = resp.invalid_tool_inputs.clone();
                    let (results, interrupted) = self.run_tools(&calls, &invalid, intr, base).await;
                    self.push_results(&results);
                    if interrupted {
                        self.ui.info("⏹ interrupted");
                        self.log("interrupted", json!({ "step": step, "during": "tools" }));
                        return TurnOutcome::Interrupted;
                    }
                    if let Some(f) = results.iter().find(|o| o.fatal) {
                        let msg = f.output["error"]["message"]
                            .as_str()
                            .unwrap_or("sandbox lost")
                            .to_owned();
                        self.ui.line(&format!("error: sandbox unusable: {msg}"));
                        return TurnOutcome::SandboxLost(msg);
                    }
                }
                _ => {
                    self.ui.newline();
                    return TurnOutcome::Done;
                }
            }
        }
        self.ui.line(&format!(
            "⚠ stopped after {} model turns (--max-turns)",
            self.max_turns
        ));
        self.log("max_turns", json!({ "max_turns": self.max_turns }));
        TurnOutcome::MaxTurns
    }

    /// All results of one assistant turn go back in a single user message, in call order.
    fn push_results(&mut self, results: &[ToolOutcome]) {
        let blocks: Vec<Value> = results.iter().map(ToolOutcome::result_block).collect();
        self.messages
            .push(json!({"role": "user", "content": blocks}));
    }

    /// Run every call of one turn concurrently; returns results in call order.
    async fn run_tools(
        &mut self,
        calls: &[(String, String, Value)],
        invalid: &HashMap<String, String>,
        intr: &Interrupt,
        base: u64,
    ) -> (Vec<ToolOutcome>, bool) {
        let mut slots: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
        let mut set = JoinSet::new();
        for (i, (id, name, input)) in calls.iter().enumerate() {
            if let Some(raw) = invalid.get(id) {
                let o = ToolOutcome::harness_error(
                    id,
                    name,
                    input.clone(),
                    "INVALID_JSON",
                    &format!(
                        "tool input was not a valid JSON object, so the tool was not run: {raw}"
                    ),
                );
                self.finish_tool(&o);
                slots[i] = Some(o);
                continue;
            }
            let sb = self.sandbox.clone();
            let (id, name, input) = (id.clone(), name.clone(), input.clone());
            set.spawn(async move {
                let t = Instant::now();
                let reply = sb.tool_dispatch(&name, input.clone()).await;
                let mut o = ToolOutcome::from_dispatch(&id, &name, input, reply, t.elapsed());
                attach_media(&sb, &mut o).await;
                o.elapsed = t.elapsed();
                (i, o)
            });
        }

        let mut interrupted = false;
        loop {
            tokio::select! {
                next = set.join_next() => match next {
                    None => break,
                    Some(Ok((i, o))) => {
                        self.finish_tool(&o);
                        slots[i] = Some(o);
                    }
                    Some(Err(e)) if e.is_cancelled() => {}
                    Some(Err(e)) => tracing::warn!("tool task failed: {e}"),
                },
                () = intr.beyond(base), if !interrupted => {
                    interrupted = true;
                    set.abort_all();
                }
            }
        }

        let results = slots
            .into_iter()
            .zip(calls)
            .map(|(slot, (id, name, input))| {
                slot.unwrap_or_else(|| {
                    let (code, msg) = if interrupted {
                        ("interrupted", "the user interrupted this tool call")
                    } else {
                        ("internal_error", "the tool call crashed in the harness")
                    };
                    ToolOutcome::harness_error(id, name, input.clone(), code, msg)
                })
            })
            .collect();
        (results, interrupted)
    }

    fn finish_tool(&mut self, o: &ToolOutcome) {
        self.ui.tool(o);
        self.log(
            "tool",
            json!({
                "id": o.id,
                "name": o.name,
                "input": o.input,
                "output": o.output,
                "is_error": o.is_error,
                "image": o.image.as_ref().map(|i| json!({"media_type": i.media_type, "bytes": i.bytes})),
                "ms": o.elapsed.as_millis() as u64,
            }),
        );
    }
}

/// Images reach the model as `image` blocks, never as base64 text: a `download` is
/// reshaped; a binary `file_read` of an image path fetches the bytes with `download`.
async fn attach_media(sb: &Sandbox, o: &mut ToolOutcome) {
    if o.is_error {
        return;
    }
    match o.name.as_str() {
        "download" => o.shape_download(),
        "file_read" => {
            let Some(path) = media::file_read_image_path(&o.input, &o.output) else {
                return;
            };
            let params = json!({"path": path, "max_bytes": media::MAX_IMAGE_BYTES});
            let dl = match sb.tool_dispatch("download", params).await {
                Ok(v) => v,
                Err(e) => e.to_json(),
            };
            let (out, img) = media::attach_to_file_read(&o.output, &dl);
            o.output = out;
            o.image = img;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_drops_declined_blocks_before_fallback() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "a"}),
            json!({"type": "text", "text": "partial"}),
            json!({"type": "tool_use", "id": "t0", "name": "bash", "input": {}}),
            json!({"type": "fallback", "from": {"model": "x"}, "to": {"model": "y"}}),
            json!({"type": "thinking", "thinking": "", "signature": "b"}),
            json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {}}),
        ];
        let e = echo_content(&content);
        let types: Vec<&str> = e.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["text", "fallback", "thinking", "tool_use"]);
        assert_eq!(e[3]["id"], "t1");

        let plain = vec![content[0].clone(), content[1].clone()];
        assert_eq!(echo_content(&plain), plain);
    }

    #[tokio::test]
    async fn interrupt_counter() {
        let i = Interrupt::default();
        assert_eq!(i.count(), 0);
        let c = i.clone();
        let h = tokio::spawn(async move { c.beyond(0).await });
        i.trigger();
        h.await.unwrap();
        assert_eq!(i.count(), 1);
    }

    #[tokio::test]
    async fn termination_latch_keeps_the_first_signal() {
        let i = Interrupt::default();
        assert_eq!(i.termination(), None);
        let c = i.clone();
        let h = tokio::spawn(async move { c.terminated().await });
        i.terminate(15);
        i.terminate(1);
        assert_eq!(h.await.unwrap(), 15);
        assert_eq!(i.termination(), Some(15));
        // A terminating signal also interrupts the running turn.
        assert_eq!(i.count(), 2);
    }
}
