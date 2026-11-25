//! Scripted fake LLM driving a real libkrun microVM (real crabd, no API call) with a host
//! directory mounted read-write over virtio-fs. Needs the signed `sandcrab-krun` helper and the
//! pinned libkrun artifacts (`sandcrab libkrun install`), a pinned template image and a musl
//! crabd:
//!
//! ```sh
//! cargo test -p sandcrab-agent --test libkrun_e2e -- --ignored
//! ```
//!
//! The helper, artifacts and template pins come from `$SANDCRAB_HOME` (default `~/.sandcrab`);
//! the workspace registry is a temporary directory, so nothing is registered in yours.
//! Env: `SANDCRAB_CRABD` (default `<target dir>/<arch>-unknown-linux-musl/release/crabd`),
//! `CRABAGENT_E2E_TEMPLATE` (default `base`).
#![cfg(all(
    feature = "libkrun",
    any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
))]

use std::path::PathBuf;
use std::sync::Arc;

use sandcrab_agent::agent::{Interrupt, TurnOutcome};
use sandcrab_agent::cli::HostMount;
use sandcrab_agent::llm::fake::{ScriptedLlm, reply, text, tool_use};
use sandcrab_agent::session::{SessionOptions, backend_line, run_session};
use sandcrab_sdk::{LibkrunConfig, MountMode, NetworkPolicy, Sandcrab};
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn crabd_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    sandcrab_core::crabd_build_path(&repo_root())
}

fn sandcrab(state: &std::path::Path) -> Sandcrab {
    let crabd = std::path::absolute(crabd_path()).unwrap();
    assert!(crabd.is_file(), "crabd not found at {}", crabd.display());
    let home = sandcrab_sdk::core::default_state_root().unwrap();
    let mut c = LibkrunConfig::with_home(&home, crabd);
    c.lib_dir = Some(sandcrab_sdk::libkrun::artifacts::default_dir(&home));
    Sandcrab::builder()
        .state_dir(state)
        .templates_dir(repo_root().join("templates"))
        .libkrun(c)
        .build()
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the libkrun helper + artifacts, a pinned template image and a musl crabd"]
async fn fake_llm_edits_a_host_dir_live_inside_a_libkrun_microvm() {
    let template = std::env::var("CRABAGENT_E2E_TEMPLATE").unwrap_or_else(|_| "base".into());
    let dir = tempfile::tempdir().unwrap();
    let host = dir.path().join("repo");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join("seed.txt"), "hello from the host\n").unwrap();
    let sc = sandcrab(&dir.path().join("state"));
    let caps = sc.backend_capabilities();
    assert!(caps.hardware_isolated && caps.supports_host_dir_mounts);
    let line = backend_line(&sc.backend_kind().to_string(), caps, true);
    assert!(
        line.starts_with("backend `libkrun`: hardware isolated"),
        "{line}"
    );

    let llm = Arc::new(ScriptedLlm::new(vec![
        reply(
            vec![
                tool_use(
                    "b1",
                    "bash",
                    json!({"command": "cat seed.txt && echo from-the-guest > out.txt && pwd"}),
                ),
                tool_use("r1", "file_read", json!({"path": "seed.txt"})),
            ],
            "tool_use",
        ),
        reply(vec![text("Done.")], "end_turn"),
    ]));
    let opts = SessionOptions {
        template,
        host_mounts: vec![HostMount {
            host: host.clone(),
            guest: PathBuf::from("/workspace"),
            mode: MountMode::ReadWrite,
        }],
        network: Some(NetworkPolicy::Disabled),
        max_turns: 5,
        transcript: Some(dir.path().join("s.jsonl")),
        prompt: Some("read seed.txt and write out.txt".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report = run_session(&sc, llm.clone(), opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report.outcome, Some(TurnOutcome::Done), "{report:?}");
    assert_eq!(report.exit_code, 0);

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 2);
    let results: Vec<(Value, bool)> = reqs[1].last().unwrap()["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                serde_json::from_str(b["content"].as_str().unwrap()).unwrap(),
                b["is_error"] == true,
            )
        })
        .collect();
    assert!(!results[0].1 && !results[1].1, "{results:?}");
    assert_eq!(results[0].0["exit_code"], 0, "{}", results[0].0);
    let stdout = results[0].0["stdout"].as_str().unwrap();
    assert!(stdout.contains("hello from the host"), "{stdout}");
    assert!(stdout.contains("/workspace"), "{stdout}");
    // The guest's write reached the host directory (virtio-fs).
    assert_eq!(
        std::fs::read_to_string(host.join("out.txt")).unwrap(),
        "from-the-guest\n"
    );
    assert!(sc.list_sandboxes().is_empty());
    sc.shutdown().await.unwrap();
}
