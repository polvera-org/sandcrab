//! Scripted fake LLM driving a real DockerDev sandbox (real crabd). Needs Docker, the
//! template image and a musl crabd:
//!
//! ```sh
//! cargo test -p sandcrab-agent --test docker_e2e -- --ignored
//! ```
//!
//! Env: `SANDCRAB_CRABD` (default `<target dir>/<arch>-unknown-linux-musl/release/crabd`,
//! target dir = `$CARGO_TARGET_DIR`, else `<repo>/target`), `CRABAGENT_E2E_TEMPLATE`
//! (default `base`).
#![cfg(feature = "docker")]

use std::path::PathBuf;
use std::sync::Arc;

use sandcrab_agent::agent::{Interrupt, TurnOutcome};
use sandcrab_agent::llm::fake::{ScriptedLlm, reply, text, tool_use};
use sandcrab_agent::session::{SessionOptions, run_session};
use sandcrab_sdk::{DockerDevConfig, Sandcrab};
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd` where the
/// target dir is `$CARGO_TARGET_DIR` (relative to the repo root) or `<repo>/target`.
fn crabd_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| repo_root().join("target"), |d| repo_root().join(d));
    target.join(format!(
        "{}-unknown-linux-musl/release/crabd",
        std::env::consts::ARCH
    ))
}

fn sandcrab(state: &std::path::Path) -> Sandcrab {
    let crabd = crabd_path();
    assert!(crabd.is_file(), "crabd not found at {}", crabd.display());
    Sandcrab::builder()
        .state_dir(state)
        .templates_dir(repo_root().join("templates"))
        .docker(DockerDevConfig {
            crabd_path: crabd,
            ..DockerDevConfig::allow_insecure()
        })
        .build()
        .unwrap()
}

fn results(messages: &[Value]) -> Vec<(Value, bool)> {
    messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                serde_json::from_str(b["content"].as_str().unwrap()).unwrap(),
                b["is_error"] == true,
            )
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs Docker, template images and a musl crabd"]
async fn fake_llm_builds_a_page_in_a_real_sandbox_and_it_persists() {
    let template = std::env::var("CRABAGENT_E2E_TEMPLATE").unwrap_or_else(|_| "base".into());
    let dir = tempfile::tempdir().unwrap();
    let sc = sandcrab(&dir.path().join("state"));
    let page = "<!doctype html>\n<h1>Sandcrab</h1>\n";

    // Session 1: write the page, then list + cat it (two parallel calls), then finish.
    let llm = Arc::new(ScriptedLlm::new(vec![
        reply(
            vec![
                text("Writing index.html."),
                tool_use(
                    "w1",
                    "file_write",
                    json!({"path": "index.html", "content": page}),
                ),
            ],
            "tool_use",
        ),
        reply(
            vec![
                tool_use("b1", "bash", json!({"command": "ls -la && pwd"})),
                tool_use("r1", "file_read", json!({"path": "index.html"})),
            ],
            "tool_use",
        ),
        reply(vec![text("Page written.")], "end_turn"),
    ]));
    let opts = SessionOptions {
        workspace: Some("landing".into()),
        template: template.clone(),
        max_turns: 10,
        transcript: Some(dir.path().join("s1.jsonl")),
        prompt: Some("write a landing page".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report = run_session(&sc, llm.clone(), opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report.outcome, Some(TurnOutcome::Done), "{report:?}");
    assert_eq!(report.exit_code, 0);
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3);

    let r = results(&reqs[1]);
    assert!(!r[0].1, "file_write failed: {}", r[0].0);
    assert_eq!(r[0].0["bytes_written"], page.len());

    let r = results(&reqs[2]);
    assert!(!r[0].1 && !r[1].1, "{r:?}");
    assert_eq!(r[0].0["exit_code"], 0);
    let stdout = r[0].0["stdout"].as_str().unwrap();
    assert!(stdout.contains("index.html"), "{stdout}");
    assert!(stdout.contains("/workspace"), "{stdout}");
    // file_read numbers lines (`     2\t<h1>…`).
    let read = r[1].0["content"].as_str().unwrap();
    assert!(read.contains("\t<h1>Sandcrab</h1>"), "{read}");
    assert!(sc.list_sandboxes().is_empty());

    // Host side: the workspace kept the file.
    let bytes = sc.workspaces().read_file("landing", "index.html").unwrap();
    assert_eq!(bytes, page.as_bytes());

    // Session 2 (new sandbox) sees session 1's work.
    let llm2 = Arc::new(ScriptedLlm::new(vec![
        reply(
            vec![tool_use("g1", "grep", json!({"pattern": "Sandcrab"}))],
            "tool_use",
        ),
        reply(vec![text("Found it.")], "end_turn"),
    ]));
    let opts = SessionOptions {
        workspace: Some("landing".into()),
        template,
        max_turns: 5,
        transcript: Some(dir.path().join("s2.jsonl")),
        prompt: Some("is the page there?".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report2 = run_session(&sc, llm2.clone(), opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report2.exit_code, 0);
    assert_ne!(report2.sandbox_id, report.sandbox_id);
    let r = results(&llm2.requests()[1]);
    assert!(!r[0].1, "{r:?}");
    assert!(r[0].0.to_string().contains("index.html"), "{}", r[0].0);
    sc.shutdown().await.unwrap();
}

/// Images reach the model as `image` blocks (download and binary file_read), non-image
/// binaries as a summary; caches point into the workspace; core dumps are off.
#[tokio::test]
#[ignore = "needs Docker, template images and a musl crabd"]
async fn images_reach_the_model_as_image_blocks() {
    let template = std::env::var("CRABAGENT_E2E_TEMPLATE").unwrap_or_else(|_| "base".into());
    let dir = tempfile::tempdir().unwrap();
    let sc = sandcrab(&dir.path().join("state"));
    let png: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
    sc.workspaces().create("pics").unwrap();
    sc.workspaces().put_file("pics", "shot.png", png).unwrap();
    sc.workspaces()
        .put_file("pics", "blob.bin", &[0u8, 0xFF, 0x00, 0xFE][..])
        .unwrap();

    let llm = Arc::new(ScriptedLlm::new(vec![
        reply(
            vec![
                tool_use("d1", "download", json!({"path": "shot.png"})),
                tool_use("f1", "file_read", json!({"path": "shot.png"})),
                tool_use("d2", "download", json!({"path": "blob.bin"})),
                tool_use(
                    "b1",
                    "bash",
                    json!({"command": "echo $XDG_CACHE_HOME $npm_config_cache $(ulimit -c)"}),
                ),
            ],
            "tool_use",
        ),
        reply(vec![text("Seen.")], "end_turn"),
    ]));
    let transcript = dir.path().join("img.jsonl");
    let opts = SessionOptions {
        workspace: Some("pics".into()),
        template,
        max_turns: 5,
        transcript: Some(transcript.clone()),
        prompt: Some("look at shot.png".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report = run_session(&sc, llm.clone(), opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report.exit_code, 0, "{report:?}");
    let reqs = llm.requests();
    let blocks = reqs[1].last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(png)
    };
    for b in &blocks[..2] {
        let parts = b["content"].as_array().unwrap_or_else(|| panic!("{b}"));
        assert_eq!(parts[0]["type"], "text");
        assert!(!parts[0]["text"].as_str().unwrap().contains(&b64));
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], b64);
    }
    let blob: Value = serde_json::from_str(blocks[2]["content"].as_str().unwrap()).unwrap();
    assert!(blob.get("content_base64").is_none(), "{blob}");
    assert_eq!(blob["size"], 4);
    assert!(blob["sha256"].is_string(), "{blob}");
    let bash: Value = serde_json::from_str(blocks[3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        bash["stdout"].as_str().unwrap().trim(),
        "/workspace/.cache /workspace/.cache/npm 0"
    );
    // The transcript keeps the shape but not the image bytes.
    let t = std::fs::read_to_string(&transcript).unwrap();
    assert!(!t.contains(&b64));
    assert!(t.contains("base64 chars elided"));
    sc.shutdown().await.unwrap();
}
