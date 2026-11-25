//! Tool-level e2e scenarios through the SDK (AGENTS.md §1.3): `bash` timeout with
//! process-group cleanup, bounded output, background processes serving a dev server on a
//! published port, and the full `tool_dispatch` surface. See `tests/common/mod.rs`.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::{bash_t, run, sh, wait_for};
use sandcrab_sdk::{
    KillSignal, MountMode, NetworkPolicy, SandboxConfig, ToolError, is_tool_error, tool_definitions,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// crabd keeps at most this many bytes per stream (head + tail), plus a short
/// `[... N bytes truncated ...]` marker (`sandcrab-guest` `bash_max_output_bytes`).
const OUTPUT_LIMIT: usize = 32 * 1024;
const MARKER_SLACK: usize = 128;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn bash_timeout_kills_the_whole_process_group() {
    run("bash-timeout", |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        let started = Instant::now();
        let r = bash_t(&sb, "sleep 1000 & sleep 1000", 1_000).await;
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert!(r.timed_out, "{r:?}");
        assert_eq!(r.exit_code, None, "{r:?}");

        // Background child of the timed-out shell is gone too (killpg).
        let left = sb.bash("pgrep -a sleep || true").await.unwrap();
        assert_eq!(left.stdout.trim(), "", "leftover processes: {left:?}");
        let ps = sh(&sb, "ps -eo comm=").await;
        assert!(!ps.lines().any(|l| l.trim() == "sleep"), "{ps}");
        let procs = sb.process_list().await.unwrap();
        assert!(procs.processes.iter().all(|p| !p.running), "{procs:?}");

        // Sandbox still healthy after the timeout.
        assert_eq!(sh(&sb, "echo still-here").await, "still-here\n");
        sb.destroy().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn bash_output_is_truncated_and_bounded() {
    run("truncation", |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        let r = bash_t(&sb, "yes | head -c 10M", 60_000).await;
        assert_eq!(r.exit_code, Some(0), "{}", r.stderr);
        assert!(r.truncated, "10 MiB of output must be truncated");
        assert!(
            r.stdout.len() <= OUTPUT_LIMIT + MARKER_SLACK,
            "stdout {} bytes",
            r.stdout.len()
        );
        assert!(r.stdout.starts_with("y\ny\n"));
        assert!(r.stdout.contains("bytes truncated"), "no marker");

        let r = bash_t(&sb, "yes err | head -c 10M >&2", 60_000).await;
        assert!(r.truncated);
        assert!(r.stderr.len() <= OUTPUT_LIMIT + MARKER_SLACK);

        // Same bound on the LLM-facing path.
        let v = sb
            .tool_dispatch("bash", json!({"command": "yes | head -c 10M"}))
            .await
            .unwrap();
        assert_eq!(v["truncated"], true);
        let size = serde_json::to_vec(&v).unwrap().len();
        assert!(
            size <= 2 * (OUTPUT_LIMIT + MARKER_SLACK),
            "reply {size} bytes"
        );
        sb.destroy().await.unwrap();
    })
    .await;
}

const SERVER_JS: &str = r"
const http = require('http');
http.createServer((req, res) => res.end('hello from sandcrab ' + req.url))
  .listen(8080, '0.0.0.0', () => console.log('LISTENING 8080'));
";

async fn http_get(addr: std::net::SocketAddr, path: &str) -> Option<String> {
    let mut s = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    // HTTP/1.1 framed by Content-Length, like browsers and curl: the response is complete
    // without waiting for the server's close.
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    s.write_all(req.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text[..end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_owned())
                })
                .and_then(|v| v.parse::<usize>().ok())?;
            if buf.len() >= end + 4 + len {
                return text.starts_with("HTTP/1.").then_some(text);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let n = tokio::time::timeout(left, s.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn dev_server_via_process_tools_and_published_port() {
    run("dev-server", |h| async move {
        let ws = h.name("site");
        h.ws().create(&ws).unwrap();
        // Docker publishes through its own port mapping (needs a NIC, hence `unrestricted`);
        // the microVM backends forward over `tunnel.open`, so the same scenario runs there
        // (libkrun: `unrestricted` is its userspace netstack; never TSI).
        let config = SandboxConfig::from_template("ts-dev")
            .mount(ws.as_str(), "/workspace", MountMode::ReadWrite)
            .default_cwd("/workspace")
            .network(NetworkPolicy::Unrestricted)
            .port(8080);
        let sb = h.sc.spawn(config).await.unwrap();
        let addr = sb.host_addr(8080).expect("port 8080 published");
        assert!(addr.ip().is_loopback(), "published on {addr}");

        sb.file_write("server.js", SERVER_JS).await.unwrap();
        let proc = sb.process_start("node server.js").await.unwrap();
        assert!(proc.pid > 0);

        let mut out = String::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !out.contains("LISTENING") {
            assert!(Instant::now() < deadline, "server never listened: {out}");
            let p = sb.process_poll(proc.process_id.clone()).await.unwrap();
            out.push_str(&p.stdout);
            assert!(p.running, "server exited early: {p:?} (out: {out})");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let body = wait_for("GET via published port", Duration::from_secs(20), || {
            http_get(addr, "/ping")
        })
        .await;
        assert!(body.contains("hello from sandcrab /ping"), "{body}");

        let listed = sb.process_list().await.unwrap();
        assert!(
            listed
                .processes
                .iter()
                .any(|p| p.process_id == proc.process_id && p.running),
            "{listed:?}"
        );

        let killed = sb
            .process_kill(proc.process_id.clone(), KillSignal::Term)
            .await
            .unwrap();
        assert!(killed.was_running);
        let exited = wait_for("server to exit", Duration::from_secs(15), || {
            let sb = &sb;
            let id = proc.process_id.clone();
            async move {
                let p = sb.process_poll(id).await.unwrap();
                (!p.running).then_some(p)
            }
        })
        .await;
        assert!(
            exited.signal.is_some() || exited.exit_code.is_some(),
            "{exited:?}"
        );
        let left = sb.bash("pgrep -x node || true").await.unwrap();
        assert_eq!(left.stdout.trim(), "", "{left:?}");
        let listed = sb.process_list().await.unwrap();
        assert!(listed.processes.iter().all(|p| !p.running), "{listed:?}");
        assert!(
            http_get(addr, "/after").await.is_none(),
            "server still answering after kill"
        );
        sb.destroy().await.unwrap();
    })
    .await;
}

/// Unwrap a `tool_dispatch` reply that must succeed.
fn ok(name: &str, r: Result<Value, ToolError>) -> Value {
    let v = r.unwrap_or_else(|e| panic!("{name}: fatal {e}"));
    assert!(!is_tool_error(&v), "{name} failed: {v}");
    v
}

/// Unwrap a `tool_dispatch` reply that must be a structured, non-fatal tool error.
fn err(name: &str, input: &Value, r: Result<Value, ToolError>) -> String {
    let v = r.unwrap_or_else(|e| panic!("{name}({input}): fatal (transport-level) error {e}"));
    assert!(is_tool_error(&v), "{name}({input}) should fail: {v}");
    let code = v["error"]["code"].as_str().unwrap().to_owned();
    assert!(
        !matches!(
            code.as_str(),
            "transport_error" | "sandbox_unavailable" | "sandbox_error"
        ),
        "{name}: {v}"
    );
    assert!(!v["error"]["message"].as_str().unwrap().is_empty());
    code
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn tool_surface_via_dispatch() {
    run("tool-surface", |h| async move {
        let ws = h.name("ws");
        h.ws().create(&ws).unwrap();
        let sb =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .mount(ws.as_str(), "/workspace", MountMode::ReadWrite)
                    .default_cwd("/workspace"),
            )
            .await
            .unwrap();
        let defs: BTreeSet<&str> = sb.tool_definitions().iter().map(|d| d.name).collect();
        assert_eq!(defs, tool_definitions().iter().map(|d| d.name).collect());
        let mut called = BTreeSet::new();
        macro_rules! call {
            ($name:literal, $input:expr) => {{
                called.insert($name);
                ok($name, sb.tool_dispatch($name, $input).await)
            }};
        }

        // ---- every tool, valid input ------------------------------------------
        let w = call!(
            "file_write",
            json!({"path": "a.txt", "content": "hello world\n"})
        );
        assert_eq!(w["created"], true);
        let r = call!("file_read", json!({"path": "a.txt"}));
        assert!(
            r["content"].as_str().unwrap().contains("hello world"),
            "{r}"
        );
        let e = call!(
            "file_edit",
            json!({"path": "a.txt", "old_string": "world", "new_string": "crab"})
        );
        assert_eq!(e["replacements"], 1);
        let g = call!("glob", json!({"pattern": "*.txt"}));
        assert_eq!(g["paths"].as_array().unwrap().len(), 1, "{g}");
        let g = call!("grep", json!({"pattern": "hello cr.b"}));
        assert_eq!(g["matches"][0]["line_number"], 1, "{g}");
        let l = call!("list_dir", json!({}));
        // Image-backed workspaces (Firecracker) have an ext4 `lost+found` at their root; it is
        // never synced back (docs/protocol.md).
        let entries: Vec<&Value> = l["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| h.is_docker() || e["path"] != "lost+found")
            .collect();
        assert_eq!(entries.len(), 1, "{l}");
        call!("file_move", json!({"from": "a.txt", "to": "b.txt"}));
        let u = call!(
            "upload",
            json!({"path": "bin.dat", "content_base64": "AAEC/w=="})
        );
        assert_eq!(u["bytes_written"], 4);
        let d = call!("download", json!({"path": "bin.dat"}));
        assert_eq!(d["content_base64"], "AAEC/w==");
        call!("file_delete", json!({"path": "bin.dat"}));
        let b = call!("bash", json!({"command": "cat b.txt"}));
        assert_eq!(b["stdout"], "hello crab\n");
        let p = call!("process_start", json!({"command": "cat"}));
        let pid = p["process_id"].as_str().unwrap().to_owned();
        let s = call!(
            "process_stdin",
            json!({"process_id": pid, "data": "ping\n"})
        );
        assert_eq!(s["bytes_written"], 5);
        let mut seen = String::new();
        for _ in 0..50 {
            let o = call!("process_poll", json!({"process_id": pid}));
            seen.push_str(o["stdout"].as_str().unwrap());
            if seen.contains("ping") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(seen.contains("ping"), "echo from cat: {seen:?}");
        let pl = call!("process_list", json!({}));
        assert_eq!(pl["processes"].as_array().unwrap().len(), 1, "{pl}");
        let k = call!("process_kill", json!({"process_id": pid}));
        assert_eq!(k["was_running"], true);
        assert_eq!(called, defs, "every defined tool was exercised");

        // ---- every tool, invalid input → structured error --------------------------
        let mut bad_codes = Vec::new();
        for def in tool_definitions() {
            let input = json!({"__bogus__": true});
            let code = err(
                def.name,
                &input,
                sb.tool_dispatch(def.name, input.clone()).await,
            );
            assert_eq!(code, "invalid_params", "{}", def.name);
            bad_codes.push(def.name);
        }
        assert_eq!(bad_codes.len(), defs.len());
        let unknown = json!({});
        assert_eq!(
            err(
                "rm_rf",
                &unknown,
                sb.tool_dispatch("rm_rf", unknown.clone()).await
            ),
            "unknown_tool"
        );

        // ---- schema-valid calls that crabd itself rejects ----------------------------
        let crabd_side: &[(&str, Value)] = &[
            ("bash", json!({"command": "true", "cwd": "/no/such/dir"})),
            ("file_read", json!({"path": "missing.txt"})),
            (
                "file_write",
                json!({"path": "/proc/sandcrab-nope/x", "content": "x"}),
            ),
            (
                "file_edit",
                json!({"path": "b.txt", "old_string": "absent", "new_string": "x"}),
            ),
            ("file_delete", json!({"path": "missing.txt"})),
            ("file_move", json!({"from": "missing.txt", "to": "c.txt"})),
            ("list_dir", json!({"path": "missing-dir"})),
            ("glob", json!({"pattern": "*", "root": "missing-dir"})),
            ("grep", json!({"pattern": "(unclosed"})),
            ("process_poll", json!({"process_id": "no-such-proc"})),
            (
                "process_stdin",
                json!({"process_id": "no-such-proc", "data": "x"}),
            ),
            ("process_kill", json!({"process_id": "no-such-proc"})),
            (
                "upload",
                json!({"path": "x.bin", "content_base64": "!!not base64!!"}),
            ),
            ("download", json!({"path": "missing.bin"})),
        ];
        for (name, input) in crabd_side {
            let code = err(name, input, sb.tool_dispatch(name, input.clone()).await);
            eprintln!("  {name}: {code}");
        }
        // Errors never break the session.
        assert_eq!(sh(&sb, "echo ok").await, "ok\n");
        sb.destroy().await.unwrap();
    })
    .await;
}
