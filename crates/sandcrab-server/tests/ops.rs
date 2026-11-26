//! Operations over a real Unix socket, on core's `FakeBackend`: `server.selftest` (a report
//! for every outcome, one run at a time per template, the last report kept),
//! `server.capabilities` with the self-test summary, `selftest_on_start`, and
//! `sandbox.destroyed` when a TTL ends a sandbox.
//!
//! On the fake no probe answers (`crabd probe` is not one of its scripted programs), so a
//! run's lines are all inconclusive and the report is never ok: exactly what a server must
//! report when it cannot show the walls.

mod common;

use std::time::{Duration, Instant};

use common::*;
use sandcrab_core::testing::FakeEvent;
use sandcrab_sdk::protocol::api::MountMode;
use sandcrab_server::MountRootConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A read-write mount root (the self-test's scratch directory goes there).
fn with_root(c: &mut sandcrab_server::ServerConfig, tmp: &std::path::Path) {
    let root = tmp.join("mounts");
    std::fs::create_dir_all(&root).unwrap();
    c.mount_roots = vec![MountRootConfig {
        path: root,
        mode: MountMode::ReadWrite,
    }];
}

/// A call whose answer may take longer than the common client's 10 s.
async fn slow_call(c: &mut Client, method: &str, params: Value) -> Value {
    let id = c.next;
    c.next += 1;
    let body = serde_json::to_vec(
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    )
    .unwrap();
    c.s.write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    c.s.write_all(&body).await.unwrap();
    let read = async {
        loop {
            let mut h = [0u8; 4];
            c.s.read_exact(&mut h).await.unwrap();
            let mut b = vec![0u8; u32::from_be_bytes(h) as usize];
            c.s.read_exact(&mut b).await.unwrap();
            let v: Value = serde_json::from_slice(&b).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    };
    let r = tokio::time::timeout(Duration::from_secs(120), read)
        .await
        .expect("an answer in time");
    r.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{method}: {r}"))
}

fn spawned(h: &Harness) -> usize {
    h.backend
        .events()
        .iter()
        .filter(|e| matches!(e, FakeEvent::Spawned(_)))
        .count()
}

#[tokio::test]
async fn a_selftest_that_cannot_start_is_a_report_that_proves_nothing() {
    // No mount roots: no place for the scratch directory.
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;

    let caps = c
        .ok("server.capabilities", json!({"template": "base"}))
        .await;
    assert_eq!(caps["selftest"], Value::Null, "never run: {caps}");

    let r = slow_call(&mut c, "server.selftest", json!({"template": "base"})).await;
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["template"], "base");
    assert_eq!(r["backend"], "docker");
    assert_eq!(r["lines"], json!([]));
    let reason = r["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("isolation not proven:") && reason.contains("read-write mount root"),
        "{reason}"
    );
    assert_eq!(spawned(&h), 0);

    // The summary is the last report, for that template only.
    let caps = c
        .ok("server.capabilities", json!({"template": "base"}))
        .await;
    let s = &caps["selftest"];
    assert_eq!(
        (&s["template"], &s["ok"], &s["reason"]),
        (&json!("base"), &json!(false), &json!(reason))
    );
    assert!(s["age_secs"].as_u64().unwrap() < 5, "{s}");
    let caps = c.ok("server.capabilities", json!({})).await;
    assert_eq!(caps["selftest"], Value::Null);
    let caps = c
        .ok("server.capabilities", json!({"template": "other"}))
        .await;
    assert_eq!(caps["selftest"], Value::Null);

    // An unknown template: also a report, never an error.
    let r = slow_call(&mut c, "server.selftest", json!({"template": "nope"})).await;
    assert_eq!(r["ok"], false);
    assert!(
        r["reason"]
            .as_str()
            .unwrap()
            .starts_with("isolation not proven:")
    );
    // No template and no default: invalid params. Unknown params too.
    assert_eq!(
        code(&c.err("server.selftest", json!({"template": ""})).await),
        -32602
    );
    assert_eq!(
        code(
            &c.err("server.selftest", json!({"template": "base", "x": 1}))
                .await
        ),
        -32602
    );
}

#[tokio::test]
async fn one_selftest_at_a_time_per_template_and_the_last_report_is_kept() {
    let h = Harness::new(with_root).await;
    let (mut a, mut b) = (h.client().await, h.client().await);

    // Two callers at once, both content with a report of the last ten minutes: one run.
    let p = json!({"template": "base", "max_age_secs": 600});
    let (ra, rb) = tokio::join!(
        slow_call(&mut a, "server.selftest", p.clone()),
        slow_call(&mut b, "server.selftest", p.clone()),
    );
    assert_eq!(ra, rb, "the second caller got the first run's report");
    assert_eq!(spawned(&h), 2, "one run: sandboxes A and B");
    assert_eq!(ra["ok"], false, "nothing answers on the fake: {ra}");
    let lines = ra["lines"].as_array().unwrap();
    assert_eq!(lines.len(), sandcrab_sdk::selftest::LINES.len());
    assert!(lines.iter().all(|l| l["status"] != "pass"), "{ra}");
    assert!(
        ra["reason"]
            .as_str()
            .unwrap()
            .starts_with("caps-empty is inconclusive")
    );

    // Within max_age: the kept report, no run.
    let again = slow_call(&mut a, "server.selftest", p).await;
    assert_eq!(again, ra);
    assert_eq!(spawned(&h), 2);

    // Without max_age: a fresh run, after the first.
    let fresh = slow_call(&mut a, "server.selftest", json!({"template": "base"})).await;
    assert_eq!(spawned(&h), 4);
    assert!(
        fresh["started_at"].as_str().unwrap() > ra["started_at"].as_str().unwrap(),
        "{fresh}"
    );
    // Two fresh runs at once are serialised: the second starts after the first ended.
    let p = json!({"template": "base"});
    let (r1, r2) = tokio::join!(
        slow_call(&mut a, "server.selftest", p.clone()),
        slow_call(&mut b, "server.selftest", p),
    );
    assert_eq!(spawned(&h), 8);
    let (first, second) = if r1["started_at"].as_str() < r2["started_at"].as_str() {
        (r1, r2)
    } else {
        (r2, r1)
    };
    let t = |v: &Value| {
        chrono::DateTime::parse_from_rfc3339(v["started_at"].as_str().unwrap()).unwrap()
    };
    let first_end = t(&first)
        + chrono::Duration::milliseconds(
            i64::try_from(first["duration_ms"].as_u64().unwrap()).unwrap(),
        );
    assert!(t(&second) >= first_end, "{first} / {second}");

    // Nothing left: both sandboxes of every run are gone, and so is the scratch directory.
    assert!(
        h.backend
            .events()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Destroyed(_)))
            .count()
            >= 8
    );
    let left = std::fs::read_dir(h.tmp.path().join("mounts"))
        .unwrap()
        .count();
    assert_eq!(left, 0, "scratch directories left");

    let caps = a
        .ok("server.capabilities", json!({"template": "base"}))
        .await;
    assert_eq!(caps["selftest"]["ok"], false);
    assert_eq!(caps["selftest"]["reason"], second["reason"]);
}

#[tokio::test]
async fn selftest_on_start_runs_in_the_background() {
    let h = Harness::new(|c, _| c.selftest_on_start = vec!["base".into()]).await;
    let mut c = h.client().await;
    let t = Instant::now();
    loop {
        let caps = c
            .ok("server.capabilities", json!({"template": "base"}))
            .await;
        if !caps["selftest"].is_null() {
            assert_eq!(caps["selftest"]["ok"], false);
            assert!(
                caps["selftest"]["reason"]
                    .as_str()
                    .unwrap()
                    .starts_with("isolation not proven:"),
                "{caps}"
            );
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no self-test at start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_ttl_reap_is_announced_once_to_the_owner() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    // Another control connection of the same uid hears every notification.
    let mut w = h.client().await;
    let sb = c
        .ok("sandbox.create", json!({"template": "base", "ttl_secs": 1}))
        .await["sandbox_id"]
        .clone();
    let note = w.notification("sandbox.destroyed").await;
    assert_eq!(note, json!({"sandbox_id": sb, "reason": "ttl"}));
    assert_eq!(c.ok("sandbox.list", json!({})).await, json!([]));
    // Gone: a destroy finds nothing, and announces nothing.
    assert_eq!(
        c.ok("sandbox.destroy", json!({"sandbox_id": sb})).await,
        json!({"destroyed": false})
    );
    // An explicit destroy is still `destroyed`; nothing else is announced.
    let sb2 = c.ok("sandbox.create", json!({"template": "base"})).await["sandbox_id"].clone();
    c.ok("sandbox.destroy", json!({"sandbox_id": sb2})).await;
    let mut notes = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        let mut h4 = [0u8; 4];
        let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(200), w.s.read_exact(&mut h4)).await
        else {
            continue;
        };
        let mut b = vec![0u8; u32::from_be_bytes(h4) as usize];
        w.s.read_exact(&mut b).await.unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap();
        notes.push((v["method"].clone(), v["params"].clone()));
    }
    assert_eq!(
        notes,
        [(
            json!("sandbox.destroyed"),
            json!({"sandbox_id": sb2, "reason": "destroyed"})
        )]
    );
}
