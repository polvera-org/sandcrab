//! Agent loop end-to-end against core's fake backend + mock crabd (bash echoes the command,
//! file_read → not_found, other tools → unimplemented), driven by a scripted fake LLM.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sandcrab_agent::agent::{Agent, Interrupt, TurnOutcome};
use sandcrab_agent::llm::fake::{ScriptedLlm, reply, text, tool_use};
use sandcrab_agent::llm::{EventSink, LlmClient, LlmError, Request, Response};
use sandcrab_agent::session::{SessionOptions, run_session};
use sandcrab_agent::tools::anthropic_tools;
use sandcrab_agent::ui::Ui;
use sandcrab_core::testing::{FakeBackend, FakeEvent};
use sandcrab_sdk::{SandboxConfig, Sandcrab, WorkspaceRef};
use serde_json::{Value, json};

fn setup(dir: &Path) -> (Sandcrab, FakeBackend) {
    let t = dir.join("templates/base");
    std::fs::create_dir_all(&t).unwrap();
    std::fs::write(
        t.join("sandcrab.toml"),
        "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n\
         [resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n",
    )
    .unwrap();
    let backend = FakeBackend::new();
    let sc = Sandcrab::builder()
        .state_dir(dir.join("state"))
        .templates_dir(dir.join("templates"))
        .ready_timeout(Duration::from_secs(5))
        .backend(backend.clone())
        .build()
        .unwrap();
    (sc, backend)
}

fn last_user_blocks(messages: &[Value]) -> Vec<Value> {
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    last["content"].as_array().unwrap().clone()
}

fn result_json(block: &Value) -> Value {
    serde_json::from_str(block["content"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn one_shot_session_runs_tools_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let (sc, backend) = setup(dir.path());
    let llm = Arc::new(ScriptedLlm::new(vec![
        reply(
            vec![
                text("Creating the page."),
                tool_use(
                    "t1",
                    "file_write",
                    json!({"path": "index.html", "content": "<h1>hi</h1>"}),
                ),
            ],
            "tool_use",
        ),
        // Two calls in one turn: run concurrently, answered in one message, in order.
        reply(
            vec![
                tool_use("t2", "bash", json!({"command": "ls"})),
                tool_use("t3", "bash", json!({"command": "echo hi"})),
            ],
            "tool_use",
        ),
        reply(vec![text("Done.")], "end_turn"),
    ]));
    let transcript = dir.path().join("t.jsonl");
    let opts = SessionOptions {
        workspace: Some("site".into()),
        template: "base".into(),
        max_turns: 10,
        transcript: Some(transcript.clone()),
        prompt: Some("build a landing page".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report = run_session(&sc, llm.clone(), opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report.exit_code, 0);
    assert_eq!(report.outcome, Some(TurnOutcome::Done));
    assert_eq!(llm.remaining(), 0);

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[0].len(), 1);
    assert_eq!(reqs[0][0]["content"], "build a landing page");

    // mock crabd does not implement file_write → error result with is_error.
    let r1 = last_user_blocks(&reqs[1]);
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0]["type"], "tool_result");
    assert_eq!(r1[0]["tool_use_id"], "t1");
    assert_eq!(r1[0]["is_error"], true);
    assert_eq!(result_json(&r1[0])["error"]["code"], "unimplemented");

    let r2 = last_user_blocks(&reqs[2]);
    assert_eq!(r2.len(), 2);
    assert_eq!(r2[0]["tool_use_id"], "t2");
    assert_eq!(r2[1]["tool_use_id"], "t3");
    assert!(r2[0].get("is_error").is_none());
    assert_eq!(result_json(&r2[0])["stdout"], "ls");
    assert_eq!(result_json(&r2[1])["stdout"], "echo hi");
    // history is append-only: request N is a prefix of request N+1
    assert_eq!(reqs[1][..reqs[0].len()], reqs[0][..]);
    assert_eq!(reqs[2][..reqs[1].len()], reqs[1][..]);
    assert_eq!(reqs[2][1]["role"], "assistant");
    assert_eq!(reqs[2][1]["content"][0]["text"], "Creating the page.");

    // workspace created, sandbox destroyed
    sc.workspaces().get(WorkspaceRef::parse("site")).unwrap();
    assert!(sc.list_sandboxes().is_empty());
    let ev = backend.events();
    assert!(matches!(ev.last(), Some(FakeEvent::Destroyed(_))), "{ev:?}");

    // transcript
    let lines: Vec<Value> = std::fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types.first(), Some(&"session_start"));
    assert_eq!(types.last(), Some(&"session_end"));
    assert_eq!(types.iter().filter(|t| **t == "request").count(), 3);
    assert_eq!(types.iter().filter(|t| **t == "response").count(), 3);
    assert_eq!(types.iter().filter(|t| **t == "tool").count(), 3);
    let start = &lines[0];
    assert_eq!(start["tools"].as_array().unwrap().len(), 16);
    assert!(start["system"].as_str().unwrap().contains("/workspace"));
    assert_eq!(lines.last().unwrap()["requests"], 3);
}

async fn agent_with(sc: &Sandcrab, llm: Arc<dyn LlmClient>) -> (Agent, Arc<sandcrab_sdk::Sandbox>) {
    let sb = Arc::new(
        sc.spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap(),
    );
    let tools = anthropic_tools(&sb.tool_definitions());
    let agent = Agent::new(llm, sb.clone(), "sys".into(), tools).with_ui(Ui::quiet());
    (agent, sb)
}

#[tokio::test]
async fn invalid_unknown_truncated_and_refused_calls() {
    let dir = tempfile::tempdir().unwrap();
    let (sc, _b) = setup(dir.path());
    let mut bad_json = reply(vec![tool_use("t1", "bash", json!({}))], "tool_use");
    bad_json
        .invalid_tool_inputs
        .insert("t1".into(), "{\"command\": \"ls".into());
    let mut refusal = reply(
        vec![tool_use("t5", "bash", json!({"command": "x"}))],
        "refusal",
    );
    refusal.stop_details = Some(json!({"type": "refusal", "category": "cyber"}));
    let llm = Arc::new(ScriptedLlm::new(vec![
        bad_json,
        reply(vec![tool_use("t2", "rm_rf", json!({}))], "tool_use"),
        reply(
            vec![tool_use("t3", "bash", json!({"command": "cat > big"}))],
            "max_tokens",
        ),
        reply(vec![text("partial")], "max_tokens"),
        refusal,
    ]));
    let (mut agent, sb) = agent_with(&sc, llm.clone()).await;
    let out = agent.run("go", &Interrupt::default()).await;
    assert_eq!(out, TurnOutcome::Refused);
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 5);

    let r = last_user_blocks(&reqs[1]);
    assert_eq!(r[0]["is_error"], true);
    assert_eq!(result_json(&r[0])["error"]["code"], "INVALID_JSON");

    let r = last_user_blocks(&reqs[2]);
    assert_eq!(result_json(&r[0])["error"]["code"], "unknown_tool");

    let r = last_user_blocks(&reqs[3]);
    assert_eq!(result_json(&r[0])["error"]["code"], "max_tokens");

    // max_tokens without a tool call → a "continue" user message
    let last = reqs[4].last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].as_str().unwrap().contains("max_tokens"));

    // refusal: tool not run, but still answered so the history stays valid
    let r = last_user_blocks(agent.messages());
    assert_eq!(r[0]["tool_use_id"], "t5");
    assert_eq!(result_json(&r[0])["error"]["code"], "refused");
    assert_eq!(agent.usage.requests, 5);

    drop(agent);
    Arc::try_unwrap(sb).unwrap().destroy().await.unwrap();
}

#[tokio::test]
async fn max_turns_and_reset() {
    let dir = tempfile::tempdir().unwrap();
    let (sc, _b) = setup(dir.path());
    let looping: Vec<Response> = (0..3)
        .map(|i| {
            reply(
                vec![tool_use(
                    &format!("t{i}"),
                    "bash",
                    json!({"command": "true"}),
                )],
                "tool_use",
            )
        })
        .collect();
    let llm = Arc::new(ScriptedLlm::new(looping));
    let (mut agent, sb) = agent_with(&sc, llm.clone()).await;
    agent.max_turns = 2;
    assert_eq!(
        agent.run("loop", &Interrupt::default()).await,
        TurnOutcome::MaxTurns
    );
    assert_eq!(llm.requests().len(), 2);
    assert!(!agent.messages().is_empty());
    agent.reset();
    assert!(agent.messages().is_empty());
    drop(agent);
    Arc::try_unwrap(sb).unwrap().destroy().await.unwrap();
}

/// An LLM that never answers.
struct Hang;

#[async_trait]
impl LlmClient for Hang {
    fn model(&self) -> &str {
        "hang"
    }
    async fn send(&self, _r: &Request<'_>, _s: &mut dyn EventSink) -> Result<Response, LlmError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn ctrl_c_interrupts_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (sc, _b) = setup(dir.path());
    let (mut agent, sb) = agent_with(&sc, Arc::new(Hang)).await;
    let intr = Interrupt::default();
    let i2 = intr.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        i2.trigger();
    });
    let out = tokio::time::timeout(Duration::from_secs(5), agent.run("hi", &intr))
        .await
        .unwrap();
    assert_eq!(out, TurnOutcome::Interrupted);
    drop(agent);
    Arc::try_unwrap(sb).unwrap().destroy().await.unwrap();
}

#[tokio::test]
async fn api_failure_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (sc, _b) = setup(dir.path());
    let llm = Arc::new(ScriptedLlm::new(vec![])); // exhausted → error
    let opts = SessionOptions {
        template: "base".into(),
        max_turns: 3,
        transcript: Some(dir.path().join("t.jsonl")),
        prompt: Some("x".into()),
        quiet: true,
        ..SessionOptions::default()
    };
    let report = run_session(&sc, llm, opts, &Interrupt::default())
        .await
        .unwrap();
    assert_eq!(report.exit_code, 1);
    assert!(matches!(report.outcome, Some(TurnOutcome::Failed(_))));
    assert!(sc.list_sandboxes().is_empty());
}
