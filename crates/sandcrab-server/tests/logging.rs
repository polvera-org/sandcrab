//! Design §12.4: env values and stdin never reach the log, at any level. A process runs with a
//! marker in its env and in its stdin under a trace-level subscriber; the log is searched.
//! Its own test binary, because the subscriber is global.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::json;

#[derive(Clone)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn env_and_stdin_markers_never_reach_a_trace_log() {
    let buf = Buf(Arc::default());
    let sink = buf.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish(),
    )
    .unwrap();

    const ENV_MARKER: &str = "env-marker-7f3a2c";
    const STDIN_MARKER: &str = "stdin-marker-91bc4e";
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = c.ok("sandbox.create", json!({"template": "base"})).await["sandbox_id"].clone();

    let env = json!({"env": {"TOKEN": ENV_MARKER}, "stdin": "pipe"});
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["echo"], env.clone())).await;
    p.record(STDIN, STDIN_MARKER.as_bytes()).await;
    p.record(STDIN_EOF, b"").await;
    let (out, _, exit) = p.drain().await;
    assert_eq!(out, STDIN_MARKER.as_bytes());
    assert_eq!(exit.unwrap()["code"], 0);
    // Refusals carry the env too.
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["nope"], env.clone())).await;
    assert_eq!(code(&r.unwrap()["error"]), -32010);
    let mut bad = spawn_params(&sb, &["echo"], env);
    bad["bogus"] = json!(ENV_MARKER);
    let (_, r) = Client::spawn(&h.socket, &bad).await;
    let e = r.unwrap()["error"].clone();
    assert_eq!(code(&e), -32602);
    assert!(!e.to_string().contains(ENV_MARKER), "error data: {e}");

    let log = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
    assert!(
        log.contains("process.spawn"),
        "the log was captured:\n{log}"
    );
    assert!(!log.contains(ENV_MARKER), "env value in the log:\n{log}");
    assert!(!log.contains(STDIN_MARKER), "stdin in the log:\n{log}");
}
