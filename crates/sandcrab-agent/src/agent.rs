//! The agent loop: prompt → model → tool calls (in parallel, in the sandbox) → tool
//! results → model … until `end_turn` or the turn limit.
//!
//! History is append-only, so cached prefixes and thinking blocks stay valid across
//! requests, with three bounded exceptions, each of which keeps the history sendable:
//!
//! - **image elision**: past [`media::MAX_IMAGES_IN_HISTORY`] image blocks, the oldest are
//!   replaced by text placeholders (see [`Agent::bound_images`]);
//! - **image rejection recovery**: if the API rejects a request over an image, the images
//!   are replaced by a note and the request is retried once;
//! - **rollback**: a turn the API rejects outright is removed, so one bad request does not
//!   fail every later turn.
//!
//! Every edit of an earlier message also strips the `thinking` blocks that follow it (see
//! [`strip_thinking_from`]): models with preserved thinking reject a thinking block whose
//! preceding history changed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sandcrab_sdk::Sandbox;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::cost::UsageTotals;
use crate::llm::{LlmClient, LlmError, Request};
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
        let turn_start = self.messages.len();
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
            let mut retried = false;
            let res = loop {
                let req = Request {
                    system: &self.system,
                    tools: &self.tools,
                    messages: &self.messages,
                };
                let r = tokio::select! {
                    r = llm.send(&req, &mut self.ui) => Some(r),
                    () = intr.beyond(base) => None,
                };
                match r {
                    // One automatic retry per request, without the images.
                    Some(Err(e)) if !retried && e.is_image_rejection() => {
                        retried = true;
                        if self.drop_rejected_images(turn_start, &e) {
                            continue;
                        }
                        break Some(Err(e));
                    }
                    other => break other,
                }
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
                    if e.is_request_rejection() {
                        self.rollback(turn_start, e.is_image_rejection());
                    }
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
        if results.iter().any(|r| r.image.is_some()) {
            self.bound_images();
        }
    }

    /// Keep at most [`media::MAX_IMAGES_IN_HISTORY`] image blocks in history: past that, the
    /// oldest are replaced by `[image elided: …]` placeholders, down to
    /// [`media::KEEP_IMAGES_AFTER_ELISION`]. This keeps every request under the API's
    /// 20-image threshold (above which each image must be ≤ 2000 px per side) and stops
    /// screenshots from being re-sent (and re-billed) on every request.
    ///
    /// Trade-off: eliding edits an earlier message, which invalidates the prompt cache from
    /// that message on and (with preserved thinking) every later thinking block, which
    /// [`strip_thinking_from`] removes. Eliding one image per new image would pay that on
    /// every request; eliding in a batch (8 → 4) pays it once per 5 new images, and the
    /// requests in between hit the cache as usual.
    fn bound_images(&mut self) {
        if count_images(&self.messages) <= media::MAX_IMAGES_IN_HISTORY {
            return;
        }
        if let Some(first) = replace_images(
            &mut self.messages,
            0,
            media::KEEP_IMAGES_AFTER_ELISION,
            "elided to save context; download it again to look at it",
        ) {
            strip_thinking_from(&mut self.messages, first);
            self.log(
                "images_elided",
                json!({ "first_message": first, "kept": media::KEEP_IMAGES_AFTER_ELISION }),
            );
        }
    }

    /// After the API rejected a request over an image: replace the images added this turn
    /// (or, if there are none, every image) with a note. `true` if anything changed.
    fn drop_rejected_images(&mut self, turn_start: usize, e: &LlmError) -> bool {
        let why = format!(
            "removed: the API rejected it ({})",
            short(&e.to_string(), 160)
        );
        let first = replace_images(&mut self.messages, turn_start, 0, &why)
            .or_else(|| replace_images(&mut self.messages, 0, 0, &why));
        let Some(first) = first else {
            return false;
        };
        strip_thinking_from(&mut self.messages, first);
        self.ui
            .info("⚠ the API rejected an image; retrying without it");
        self.log(
            "image_rejected",
            json!({ "error": e.to_string(), "first_message": first }),
        );
        true
    }

    /// Undo a turn the API rejected, so the next turn starts from a history that worked.
    /// After an image rejection, the images left in history go too.
    fn rollback(&mut self, turn_start: usize, image: bool) {
        self.messages.truncate(turn_start);
        self.logged = self.logged.min(turn_start);
        if image
            && let Some(first) = replace_images(
                &mut self.messages,
                0,
                0,
                "removed after the API rejected a request",
            )
        {
            strip_thinking_from(&mut self.messages, first);
        }
        self.ui.info(
            "this turn was rolled back; the conversation continues from before it \
             (sandbox changes made during the turn remain)",
        );
        self.log("rollback", json!({ "n_messages": self.messages.len() }));
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

fn short(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// Number of `image` blocks in user messages (top level or inside a `tool_result`).
#[must_use]
pub fn count_images(messages: &[Value]) -> usize {
    fn count(content: &[Value]) -> usize {
        content
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("image") => 1,
                Some("tool_result") => b["content"].as_array().map_or(0, |c| count(c)),
                _ => 0,
            })
            .sum()
    }
    messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_array())
        .map(|c| count(c))
        .sum()
}

/// Replace image blocks in `messages[from..]`, oldest first, with a text placeholder
/// `[image <why>: <path> <W>x<H>]`, until at most `keep` images remain in that range.
/// Returns the index of the first message changed.
pub fn replace_images(
    messages: &mut [Value],
    from: usize,
    keep: usize,
    why: &str,
) -> Option<usize> {
    let tail = messages.get_mut(from..)?;
    let mut budget = count_images(tail).saturating_sub(keep);
    let mut first = None;
    for (i, m) in tail.iter_mut().enumerate() {
        if budget == 0 {
            break;
        }
        if m["role"] != "user" {
            continue;
        }
        if let Some(c) = m["content"].as_array_mut()
            && replace_in(c, &mut budget, why) > 0
        {
            first.get_or_insert(from + i);
        }
    }
    first
}

fn replace_in(content: &mut [Value], budget: &mut usize, why: &str) -> usize {
    let mut n = 0;
    for b in content.iter_mut() {
        if b["type"] == "tool_result"
            && let Some(inner) = b["content"].as_array_mut()
        {
            n += replace_in(inner, budget, why);
        }
    }
    let label = image_label(content);
    for b in content.iter_mut() {
        if *budget == 0 {
            break;
        }
        if b["type"] == "image" {
            *b = json!({"type": "text", "text": format!("[image {why}: {label}]")});
            *budget -= 1;
            n += 1;
        }
    }
    n
}

/// `<path> <W>x<H>` from the JSON note next to an image in a `tool_result`.
fn image_label(content: &[Value]) -> String {
    let note = content
        .iter()
        .filter_map(|b| b["text"].as_str())
        .filter_map(|t| serde_json::from_str::<Value>(t).ok())
        .find(|v| v.get("path").is_some());
    let Some(v) = note else {
        return "image".into();
    };
    let path = v["path"].as_str().unwrap_or("image");
    match (v["width"].as_u64(), v["height"].as_u64()) {
        (Some(w), Some(h)) => format!("{path} {w}x{h}"),
        _ => path.to_owned(),
    }
}

/// Drop `thinking` / `redacted_thinking` blocks from the assistant messages at or after
/// `from`: their preceding history was just edited, so models with preserved thinking
/// would reject them. An assistant message left empty gets a placeholder text block.
pub fn strip_thinking_from(messages: &mut [Value], from: usize) {
    for m in messages.iter_mut().skip(from) {
        if m["role"] != "assistant" {
            continue;
        }
        if let Some(c) = m["content"].as_array_mut() {
            c.retain(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")));
            if c.is_empty() {
                c.push(json!({"type": "text", "text": "…"}));
            }
        }
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

    fn image_result(id: &str, path: &str) -> Value {
        let note = json!({"path": path, "width": 1280, "height": 800, "mime": "image/png"});
        json!({"role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": id,
            "content": [
                {"type": "text", "text": note.to_string()},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ],
        }]})
    }

    fn assistant_with_thinking(id: &str) -> Value {
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {"type": "tool_use", "id": id, "name": "download", "input": {}},
        ]})
    }

    /// user prompt, then `n` rounds of (assistant with thinking, image tool_result).
    fn history(n: usize) -> Vec<Value> {
        let mut m = vec![json!({"role": "user", "content": "go"})];
        for i in 0..n {
            m.push(assistant_with_thinking(&format!("t{i}")));
            m.push(image_result(&format!("t{i}"), &format!("shot{i}.png")));
        }
        m
    }

    #[test]
    fn elision_keeps_the_most_recent_images() {
        let mut m = history(9);
        assert_eq!(count_images(&m), 9);
        let first = replace_images(&mut m, 0, 4, "elided").unwrap();
        assert_eq!(first, 2, "the oldest image is in message 2");
        assert_eq!(count_images(&m), 4);
        let t = m[2]["content"][0]["content"][1]["text"].as_str().unwrap();
        assert_eq!(t, "[image elided: shot0.png 1280x800]");
        // the newest images are untouched
        assert_eq!(m[18]["content"][0]["content"][1]["type"], "image");
        assert_eq!(m[12]["content"][0]["content"][1]["type"], "image");
        assert_eq!(m[10]["content"][0]["content"][1]["type"], "text");
        // nothing to do when under the cap
        assert!(replace_images(&mut m, 0, 4, "elided").is_none());

        strip_thinking_from(&mut m, first);
        assert_eq!(
            m[1]["content"][0]["type"], "thinking",
            "before the edit: kept"
        );
        for a in m.iter().skip(first).filter(|x| x["role"] == "assistant") {
            assert_eq!(a["content"].as_array().unwrap().len(), 1);
            assert_eq!(a["content"][0]["type"], "tool_use");
        }
        let mut only = vec![
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "s"}]}),
        ];
        strip_thinking_from(&mut only, 0);
        assert_eq!(only[0]["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn bounded_images_edit_history_in_batches() {
        let dir = tempfile::tempdir().unwrap();
        let sc = fake_sandcrab(dir.path());
        let sb = Arc::new(
            sc.spawn(sandcrab_sdk::SandboxConfig::from_template("base"))
                .await
                .unwrap(),
        );
        let llm = Arc::new(crate::llm::fake::ScriptedLlm::new(Vec::new()));
        let mut agent = Agent::new(llm, sb.clone(), "sys".into(), Vec::new()).with_ui(Ui::quiet());
        let mut edits = 0;
        for i in 0..20 {
            agent
                .messages
                .push(assistant_with_thinking(&format!("t{i}")));
            let before = agent.messages.clone();
            agent.messages.push(image_result(&format!("t{i}"), "s.png"));
            agent.bound_images();
            assert!(count_images(&agent.messages) <= media::MAX_IMAGES_IN_HISTORY);
            if agent.messages[..before.len()] != before[..] {
                edits += 1;
            }
        }
        // 20 images: elide at the 9th, 14th and 19th → 3 cache-breaking edits, not 12.
        assert_eq!(edits, 3);
        assert_eq!(count_images(&agent.messages), 5);
        drop(agent);
        Arc::try_unwrap(sb).unwrap().destroy().await.unwrap();
    }

    fn fake_sandcrab(dir: &std::path::Path) -> sandcrab_sdk::Sandcrab {
        let t = dir.join("templates/base");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(
            t.join("sandcrab.toml"),
            "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n\
             [resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n",
        )
        .unwrap();
        sandcrab_sdk::Sandcrab::builder()
            .state_dir(dir.join("state"))
            .templates_dir(dir.join("templates"))
            .ready_timeout(std::time::Duration::from_secs(5))
            .backend(sandcrab_core::testing::FakeBackend::new())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn image_rejection_strips_the_images_and_retries_once() {
        use crate::llm::fake::{ScriptedLlm, bad_request, reply, text};
        let dir = tempfile::tempdir().unwrap();
        let sc = fake_sandcrab(dir.path());
        let llm = Arc::new(ScriptedLlm::with_results(vec![
            Err(bad_request(
                "messages.2.content.0.content.1.image.source.base64: image dimensions exceed max allowed size for many-image requests: 2000 pixels",
            )),
            Ok(reply(vec![text("ok")], "end_turn")),
        ]));
        let sb = Arc::new(
            sc.spawn(sandcrab_sdk::SandboxConfig::from_template("base"))
                .await
                .unwrap(),
        );
        let mut agent =
            Agent::new(llm.clone(), sb.clone(), "sys".into(), Vec::new()).with_ui(Ui::quiet());
        agent.messages = history(1);
        agent.messages.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": "late"},
            {"type": "text", "text": "looked"},
        ]}));
        let out = agent.run("again", &Interrupt::default()).await;
        assert_eq!(out, TurnOutcome::Done);
        let reqs = llm.requests();
        assert_eq!(reqs.len(), 2, "exactly one retry");
        assert_eq!(count_images(&reqs[0]), 1);
        assert_eq!(count_images(&reqs[1]), 0);
        let note = reqs[1][2]["content"][0]["content"][1]["text"]
            .as_str()
            .unwrap();
        assert!(
            note.starts_with("[image removed: the API rejected it"),
            "{note}"
        );
        assert!(note.contains("shot0.png 1280x800"), "{note}");
        // thinking after the edit is gone, thinking before it stays
        assert_eq!(reqs[1][1]["content"][0]["type"], "thinking");
        assert_eq!(reqs[1][3]["content"][0]["type"], "text");
        assert_eq!(agent.messages().len(), 6);

        drop(agent);

        // A second image rejection on the retry is not retried again: the turn fails and
        // is rolled back, and no image is left to fail the next turn.
        let llm2 = Arc::new(ScriptedLlm::with_results(vec![
            Err(bad_request("Could not process image")),
            Err(bad_request("Could not process image")),
        ]));
        let mut agent =
            Agent::new(llm2.clone(), sb.clone(), "sys".into(), Vec::new()).with_ui(Ui::quiet());
        agent.messages = history(2);
        let n = agent.messages.len();
        let out = agent.run("again", &Interrupt::default()).await;
        assert!(matches!(out, TurnOutcome::Failed(_)), "{out:?}");
        assert_eq!(llm2.requests().len(), 2);
        assert_eq!(agent.messages().len(), n, "turn rolled back");
        assert_eq!(count_images(agent.messages()), 0);

        drop(agent);
        Arc::try_unwrap(sb).unwrap().destroy().await.unwrap();
    }
}
