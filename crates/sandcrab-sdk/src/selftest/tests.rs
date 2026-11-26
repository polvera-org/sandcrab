use sandcrab_core::testing::{FakeBackend, FakeEvent};
use sandcrab_core::{MountPolicy, MountRoot};

use super::*;
use crate::test_support::Env;

fn attempts(results: &[(&str, &str)]) -> Value {
    let list: Vec<Value> = results
        .iter()
        .map(|(t, r)| json!({ "target": t, "result": r, "ms": 0 }))
        .collect();
    json!({ "attempts": list })
}

fn facts() -> HostFacts {
    HostFacts {
        forward_nonce: "n1".into(),
        target_got: Some("n1".into()),
        stray_connections: 0,
        canary: "canary-x".into(),
        rw_nonce: "n2".into(),
        rw_on_host: Some("n2".into()),
        other_listening: true,
        env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
        tree_clean: Some(true),
        leftover: Some(ProbeRun::Answer(json!({ "visible": [] }))),
    }
}

fn status(caps: &str, nnp: &str) -> Value {
    json!({ "status": {
        "CapInh": caps, "CapPrm": caps, "CapEff": caps, "CapBnd": caps, "CapAmb": caps,
        "NoNewPrivs": nnp,
    }})
}

fn fs(absent: &str, ro: &str) -> Value {
    json!({
        "absent": [{ "path": "/x/canary-outside", "result": absent }],
        "read": { "path": "/x/ro/canary", "result": "ok", "content": "canary-x" },
        "ro_write": { "path": "/x/ro/.probe-ro", "result": ro },
        "rw_write": { "path": "/x/rw/probe-rw", "result": "ok" },
    })
}

/// A passing answer and a failing one (the wall removed) for every line.
fn cases() -> Vec<(&'static str, Value, Value)> {
    let fwd = |result: &str, reply: &str, end: &str| {
        json!({ "attempt": { "target": "tcp 127.0.0.1:1", "result": result },
                "reply": reply, "bytes": reply.len(), "end": end })
    };
    let other = |result: &str, visible: Value| json!({ "attempt": { "target": "tcp 127.0.0.1:2", "result": result }, "visible": visible });
    vec![
        (
            "caps-empty",
            status("0000000000000000", "1"),
            status("00000000a80425fb", "1"),
        ),
        (
            "no-new-privs",
            status("0000000000000000", "1"),
            status("0000000000000000", "0"),
        ),
        (
            "userns-refused",
            json!({ "result": "EPERM" }),
            json!({ "result": "ok" }),
        ),
        (
            "net-loopback-only",
            json!({ "interfaces": ["lo"] }),
            json!({ "interfaces": ["lo", "eth0"] }),
        ),
        (
            "net-external-refused",
            attempts(&[
                ("tcp 1.1.1.1:443", "ENETUNREACH"),
                ("tcp 8.8.8.8:443", "EHOSTUNREACH"),
            ]),
            attempts(&[
                ("tcp 1.1.1.1:443", "ENETUNREACH"),
                ("tcp 8.8.8.8:443", "ok"),
            ]),
        ),
        (
            "dns-refused",
            attempts(&[
                ("udp 127.0.0.53:53", "ECONNREFUSED"),
                ("udp [2001:db8::1]:53", "EADDRNOTAVAIL"),
                ("udp 1.1.1.1:53", "ENETUNREACH"),
            ]),
            // ECONNREFUSED from a non-loopback resolver means it was reached.
            attempts(&[("udp 10.0.0.2:53", "ECONNREFUSED")]),
        ),
        (
            "gateway-refused",
            attempts(&[("tcp 172.17.0.1:80", "ENETUNREACH")]),
            attempts(&[("tcp 172.17.0.1:80", "ECONNREFUSED")]),
        ),
        (
            "metadata-refused",
            attempts(&[("tcp 169.254.169.254:80", "ENETUNREACH")]),
            attempts(&[("tcp 169.254.169.254:80", "ok")]),
        ),
        (
            "forward-reaches-target",
            fwd("ok", "ack:n1", "eof"),
            fwd("ECONNREFUSED", "", "eof"),
        ),
        (
            "forward-only-declared",
            fwd("ECONNREFUSED", "", ""),
            fwd("ok", "", ""),
        ),
        (
            "forward-fails-closed",
            fwd("ok", "", "eof"),
            fwd("ok", "ack:n3", "eof"),
        ),
        (
            "fs-outside-mounts",
            fs("ENOENT", "EROFS"),
            fs("ok", "EROFS"),
        ),
        (
            "other-sandbox-unreachable",
            other("ECONNREFUSED", json!([])),
            other("ok", json!([])),
        ),
        (
            "exec-env-exact",
            json!({ "env": { "A": "1" } }),
            json!({ "env": { "A": "1", "PATH": "/bin" } }),
        ),
        (
            "tree-kill",
            json!({ "ready": true, "child_pid": 7 }),
            json!({ "ready": true, "child_pid": 7 }),
        ),
    ]
}

#[test]
fn every_line_has_a_case() {
    let ids: Vec<&str> = cases().iter().map(|c| c.0).collect();
    assert_eq!(ids, LINES);
}

#[test]
fn each_line_passes_fails_and_times_out() {
    for (id, good, bad) in cases() {
        let f = facts();
        let l = evaluate(id, &ProbeRun::Answer(good.clone()), &f);
        assert_eq!(l.status, Pass, "{id}: {l:?}");
        assert!(l.required);
        assert_eq!(
            l.evidence["probe"], good,
            "{id}: the answer is the evidence"
        );

        // tree-kill's wall is on the host side: the bad answer is the same, the facts differ.
        let f = if id == "tree-kill" {
            HostFacts {
                tree_clean: Some(false),
                ..facts()
            }
        } else {
            facts()
        };
        let l = evaluate(id, &ProbeRun::Answer(bad), &f);
        assert_eq!(l.status, Fail, "{id}: {l:?}");

        let l = evaluate(id, &ProbeRun::Timeout, &facts());
        assert_eq!(l.status, Inconclusive, "{id}: {l:?}");
        assert_eq!(l.evidence["probe"], "timeout");

        let l = evaluate(id, &ProbeRun::NoAnswer("ENOENT".into()), &facts());
        assert_eq!(l.status, Inconclusive, "{id}: {l:?}");

        // An answer that says nothing is never a pass.
        let l = evaluate(id, &ProbeRun::Answer(json!({})), &facts());
        assert_ne!(l.status, Pass, "{id}: {l:?}");
    }
}

#[test]
fn an_attempt_that_timed_out_is_inconclusive_and_a_fail_wins() {
    let v = attempts(&[
        ("tcp 1.1.1.1:443", "ENETUNREACH"),
        ("tcp 8.8.8.8:443", "timeout"),
    ]);
    let l = evaluate("net-external-refused", &ProbeRun::Answer(v), &facts());
    assert_eq!(l.status, Inconclusive);

    let v = attempts(&[("tcp 1.1.1.1:443", "timeout"), ("tcp 8.8.8.8:443", "ok")]);
    let l = evaluate("net-external-refused", &ProbeRun::Answer(v), &facts());
    assert_eq!(l.status, Fail);

    // A forward exchange that hung.
    let v = json!({ "attempt": { "result": "ok" }, "reply": "", "bytes": 0, "end": "timeout" });
    let l = evaluate("forward-fails-closed", &ProbeRun::Answer(v), &facts());
    assert_eq!(l.status, Inconclusive);
}

#[test]
fn host_facts_can_fail_a_line_the_probe_passed() {
    let pass = |id: &str, f: &HostFacts| {
        let good = cases().into_iter().find(|c| c.0 == id).unwrap().1;
        evaluate(id, &ProbeRun::Answer(good), f).status
    };
    let f = HostFacts {
        target_got: None,
        ..facts()
    };
    assert_eq!(pass("forward-reaches-target", &f), Fail);
    let f = HostFacts {
        stray_connections: 1,
        ..facts()
    };
    assert_eq!(pass("forward-only-declared", &f), Fail);
    let f = HostFacts {
        rw_on_host: None,
        ..facts()
    };
    assert_eq!(pass("fs-outside-mounts", &f), Fail);
    let f = HostFacts {
        other_listening: false,
        ..facts()
    };
    assert_eq!(pass("other-sandbox-unreachable", &f), Inconclusive);
    let f = HostFacts {
        leftover: Some(ProbeRun::Answer(json!({ "visible": [12] }))),
        ..facts()
    };
    assert_eq!(pass("tree-kill", &f), Fail);
    let f = HostFacts {
        tree_clean: None,
        ..facts()
    };
    assert_eq!(pass("tree-kill", &f), Inconclusive);
    let f = HostFacts {
        leftover: Some(ProbeRun::Timeout),
        ..facts()
    };
    assert_eq!(pass("tree-kill", &f), Inconclusive);
}

fn report(statuses: &[SelftestStatus]) -> SelftestReport {
    let lines = statuses
        .iter()
        .zip(LINES)
        .map(|(s, id)| line(id, *s, "why", Value::Null))
        .collect();
    assemble(
        "docker".into(),
        "base".into(),
        "2026-11-26T00:00:00.000Z".into(),
        Duration::from_millis(1234),
        lines,
    )
}

#[test]
fn report_is_ok_only_when_every_required_line_passes() {
    let r = report(&[Pass, Pass, Pass]);
    assert!(r.ok && r.reason.is_none(), "{r:?}");
    assert_eq!((r.duration_ms, r.backend.as_str()), (1234, "docker"));

    let r = report(&[Pass, Inconclusive, Fail]);
    assert!(!r.ok);
    assert_eq!(
        r.reason.as_deref(),
        Some("no-new-privs is inconclusive: why")
    );

    let r = report(&[]);
    assert!(
        !r.ok && r.reason.is_some(),
        "an empty report proves nothing"
    );

    // A line that is not required does not decide `ok`.
    let mut r = report(&[Pass, Fail]);
    r.lines[1].required = false;
    let r = assemble(r.backend, r.template, r.started_at, Duration::ZERO, r.lines);
    assert!(r.ok, "{r:?}");

    let wire = serde_json::to_value(report(&[Inconclusive])).unwrap();
    assert_eq!(wire["lines"][0]["status"], "inconclusive");
}

/// On the fake no probe can run (`crabd probe` is not one of its scripted programs): every
/// line is inconclusive, the report is not ok, and both sandboxes and the scratch directory
/// are gone afterwards.
#[tokio::test]
async fn on_the_fake_nothing_answers_so_nothing_passes_and_all_is_cleaned_up() {
    let env = Env::new();
    let root = env.tmp.path().join("mounts");
    std::fs::create_dir(&root).unwrap();
    let backend = FakeBackend::new();
    let policy = MountPolicy::new(
        vec![MountRoot {
            path: root.clone(),
            max_mode: MountMode::ReadWrite,
        }],
        Vec::new(),
    )
    .unwrap();
    let sc = env
        .builder()
        .backend(backend.clone())
        .mount_policy(policy)
        .build()
        .unwrap();

    let r = run(&sc, SelftestConfig::new("base")).await.unwrap();
    assert!(!r.ok, "{r:?}");
    assert_eq!(r.template, "base");
    let ids: Vec<&str> = r.lines.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(ids, LINES);
    for l in &r.lines {
        assert_eq!(l.status, Inconclusive, "{l:?}");
    }
    assert!(r.reason.unwrap().starts_with("caps-empty is inconclusive"));

    let events = backend.events();
    let spawned = events
        .iter()
        .filter(|e| matches!(e, FakeEvent::Spawned(_)))
        .count();
    let destroyed = events
        .iter()
        .filter(|e| matches!(e, FakeEvent::Destroyed(_)))
        .count();
    assert_eq!((spawned, destroyed), (2, 2), "{events:?}");
    assert!(sc.list_sandboxes().is_empty());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "scratch left");
}

#[tokio::test]
async fn without_a_read_write_root_it_refuses_before_spawning() {
    let env = Env::new();
    let backend = FakeBackend::new();
    let sc = env.sandcrab(&backend);
    let err = run(&sc, SelftestConfig::new("base")).await.unwrap_err();
    assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    assert!(backend.events().is_empty());
}

#[tokio::test]
async fn an_unknown_template_is_an_error_and_leaves_nothing() {
    let env = Env::new();
    let root = env.tmp.path().join("mounts");
    std::fs::create_dir(&root).unwrap();
    let backend = FakeBackend::new();
    let mut config = SelftestConfig::new("no-such-template");
    config.scratch_root = Some(root.clone());
    let policy = MountPolicy::new(
        vec![MountRoot {
            path: root.clone(),
            max_mode: MountMode::ReadWrite,
        }],
        Vec::new(),
    )
    .unwrap();
    let sc = env
        .builder()
        .backend(backend.clone())
        .mount_policy(policy)
        .build()
        .unwrap();
    assert!(run(&sc, config).await.is_err());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}
