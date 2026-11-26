//! Process and forward connections over a real Unix socket, on core's `FakeBackend` (its mock
//! `crabd` scripts the programs: see `sandcrab_core::testing::MockProcesses`). A raw client
//! speaks the frames and records.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn sandbox(c: &mut Client, extra: Value) -> Value {
    let mut p = json!({"template": "base"});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    c.ok("sandbox.create", p).await["sandbox_id"].clone()
}

/// Wait until `cond` holds, at most 5 s.
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn one_mib_passes_both_ways_in_order_and_exit_is_last() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;

    // Out: 1 MiB from the program, in 64 KiB records, then EXIT, then close.
    let mut p = Client::spawn_ok(
        &h.socket,
        &spawn_params(&sb, &["emit", "1048576"], json!({})),
    )
    .await;
    let (out, err, exit) = p.drain().await;
    assert_eq!(out, pattern(1 << 20));
    assert!(err.is_empty());
    let exit = exit.expect("an EXIT record");
    assert_eq!(exit["code"], 0);
    assert_eq!(exit["tree_clean"], true);

    // Both ways: 1 MiB of stdin echoed back, written while it is read.
    let params = spawn_params(&sb, &["echo"], json!({"stdin": "pipe"}));
    let p = Client::spawn_ok(&h.socket, &params).await;
    let (mut rd, mut wr) = p.s.into_split();
    let data = pattern(1 << 20);
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        for chunk in sent.chunks(40_000) {
            let mut rec = vec![STDIN];
            rec.extend_from_slice(&u32::try_from(chunk.len()).unwrap().to_be_bytes());
            rec.extend_from_slice(chunk);
            wr.write_all(&rec).await.unwrap();
        }
        wr.write_all(&[STDIN_EOF, 0, 0, 0, 0]).await.unwrap();
        wr
    });
    let mut got = Vec::new();
    let exit = loop {
        let mut hd = [0u8; 5];
        rd.read_exact(&mut hd).await.unwrap();
        let mut payload = vec![0u8; u32::from_be_bytes([hd[1], hd[2], hd[3], hd[4]]) as usize];
        rd.read_exact(&mut payload).await.unwrap();
        match hd[0] {
            STDOUT => got.extend_from_slice(&payload),
            EXIT => break serde_json::from_slice::<Value>(&payload).unwrap(),
            k => panic!("record {k:#x}"),
        }
    };
    let _wr = writer.await.unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data, "the bytes arrive unchanged and in order");
    assert_eq!(exit["code"], 0);
    let mut rest = [0u8; 1];
    assert_eq!(rd.read(&mut rest).await.unwrap(), 0, "closed after EXIT");

    // stdout and stderr records keep their order and their tags.
    let params = spawn_params(&sb, &["mixed"], json!({"stderr": "stream"}));
    let mut p = Client::spawn_ok(&h.socket, &params).await;
    let mut kinds = Vec::new();
    while let Some((k, d)) = p.next_record().await {
        kinds.push((k, d));
    }
    let tags: Vec<(u8, &[u8])> = kinds.iter().map(|(k, d)| (*k, d.as_slice())).collect();
    assert_eq!(
        &tags[..3],
        &[(STDOUT, &b"a"[..]), (STDERR, b"b"), (STDOUT, b"c")]
    );
    assert_eq!(tags[3].0, EXIT);
    assert_eq!(tags.len(), 4);
}

#[tokio::test]
async fn stdin_stays_open_until_eof_and_null_stdin_refuses_records() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let m = mock(&h, &sb);

    let mut p = Client::spawn_ok(
        &h.socket,
        &spawn_params(&sb, &["echo"], json!({"stdin": "pipe"})),
    )
    .await;
    p.record(STDIN, b"one").await;
    assert_eq!(p.next_record().await, Some((STDOUT, b"one".to_vec())));
    // stdin open: nothing ends by itself.
    let quiet = tokio::time::timeout(Duration::from_millis(200), p.next_record()).await;
    assert!(quiet.is_err(), "the program waits for more stdin");
    p.record(STDIN, b"two").await;
    assert_eq!(p.next_record().await, Some((STDOUT, b"two".to_vec())));
    p.record(STDIN_EOF, b"").await;
    let (out, _, exit) = p.drain().await;
    assert!(out.is_empty());
    assert_eq!(exit.unwrap()["code"], 0);

    // A record after STDIN_EOF closes the connection, which stops the tree.
    let mut p = Client::spawn_ok(
        &h.socket,
        &spawn_params(&sb, &["sleep"], json!({"stdin": "pipe"})),
    )
    .await;
    p.record(STDIN_EOF, b"").await;
    p.record(STDIN, b"late").await;
    assert_eq!(p.drain().await.2, None, "closed without EXIT");

    // With stdin "null" any stdin record closes the connection.
    let before = m.processes.closed_by_host.load(Ordering::SeqCst);
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
    p.record(STDIN, b"x").await;
    assert_eq!(p.drain().await.2, None);
    until("the guest connection closed", || {
        m.processes.closed_by_host.load(Ordering::SeqCst) > before && m.processes.live() == 0
    })
    .await;
    // An unknown record kind too.
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
    p.record(0x7e, b"").await;
    assert_eq!(p.drain().await.2, None);
    until("no program left", || m.processes.live() == 0).await;
}

#[tokio::test]
async fn a_program_that_exits_without_reading_its_stdin_still_reports_its_exit() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let chunk = vec![b'x'; 65536];
    for _ in 0..5 {
        let mut p = Client::spawn_ok(
            &h.socket,
            &spawn_params(&sb, &["exit", "0"], json!({"stdin": "pipe"})),
        )
        .await;
        // More than the connections buffer: the guest is gone before the last write.
        for _ in 0..64 {
            p.record(STDIN, &chunk).await;
        }
        p.record(STDIN_EOF, b"").await;
        let (_, _, exit) = p.drain().await;
        assert_eq!(exit.expect("EXIT, not a bare close")["code"], 0);
    }
}

#[tokio::test]
async fn a_client_that_stops_reading_stops_the_program_and_memory_stays_bounded() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let m = mock(&h, &sb);
    const TOTAL: usize = 32 << 20;
    let mut p = Client::spawn_ok(
        &h.socket,
        &spawn_params(&sb, &["emit", &TOTAL.to_string()], json!({})),
    )
    .await;

    // Read nothing: the program's writes stall.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let first = m.processes.stdout_written.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = m.processes.stdout_written.load(Ordering::SeqCst);
    assert_eq!(first, second, "a paused reader stalls the program");
    // What is in flight: the client socket's buffers, one record in the server, the
    // guest transport. Far below what the program wanted to write.
    assert!(second < 4 << 20, "in flight: {second} bytes");
    println!("in flight with a paused reader: {second} bytes of {TOTAL}");

    // Read on: everything arrives.
    let (out, _, exit) = p.drain().await;
    assert_eq!(out.len(), TOTAL);
    assert!(out == pattern(TOTAL));
    assert_eq!(exit.unwrap()["code"], 0);
}

#[tokio::test]
async fn every_outcome_has_its_own_shape() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let m = mock(&h, &sb);

    let refused = |r: Option<Value>| r.expect("an error frame")["error"].clone();
    // Could not start: SPAWN_FAILED with errno and what, from crabd, unchanged.
    let (mut p, r) = Client::spawn(
        &h.socket,
        &spawn_params(&sb, &["no-such-program"], json!({})),
    )
    .await;
    let e = refused(r);
    assert_eq!(code(&e), -32010);
    assert_eq!(e["data"], json!({"errno": "ENOENT", "what": "program"}));
    assert!(p.closed().await);
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["no-cwd"], json!({}))).await;
    assert_eq!(refused(r)["data"]["what"], "cwd");
    // crabd's own limit passes through.
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["limit"], json!({}))).await;
    assert_eq!(code(&refused(r)), -32011);

    // Rejected by the server: bad params, no such sandbox, a destroyed one.
    let mut bad = spawn_params(&sb, &["exit"], json!({}));
    bad["process_id"] = json!("not-a-uuid");
    let (mut p, r) = Client::spawn(&h.socket, &bad).await;
    assert_eq!(code(&refused(r)), -32602);
    assert!(p.closed().await);
    let mut bad = spawn_params(&sb, &["exit"], json!({}));
    bad["extra"] = json!(1);
    let (_, r) = Client::spawn(&h.socket, &bad).await;
    assert_eq!(code(&refused(r)), -32602, "unknown fields are refused");
    let other = json!(new_id());
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&other, &["exit"], json!({}))).await;
    let e = refused(r);
    assert_eq!(code(&e), -32102);
    assert_eq!(e["data"]["sandbox_id"], other);

    // Unavailable: nothing could be sent to the guest.
    h.backend
        .connect_failures
        .store(1_000_000, Ordering::SeqCst);
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["exit"], json!({}))).await;
    let e = refused(r);
    h.backend.connect_failures.store(0, Ordering::SeqCst);
    assert_eq!(code(&e), -32101, "{e}");
    assert!(e["data"]["detail"].is_string());

    // Lost before the ack: the guest closed after the request. No answer at all.
    let (_, r) = Client::spawn(
        &h.socket,
        &spawn_params(&sb, &["hangup-before-ack"], json!({})),
    )
    .await;
    assert_eq!(r, None, "lost: closed without an answer");
    // Lost after the ack: no EXIT.
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["hangup"], json!({}))).await;
    assert_eq!(p.drain().await.2, None, "lost: closed without EXIT");

    // An exit code, a timeout, an output cap: all in the EXIT record.
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["exit", "3"], json!({}))).await;
    let exit = p.drain().await.2.unwrap();
    assert_eq!(
        (exit["code"].clone(), exit["stderr_tail"].clone()),
        (json!(3), json!("bye"))
    );
    let params = spawn_params(&sb, &["sleep"], json!({"timeout_ms": 50}));
    let exit = Client::spawn_ok(&h.socket, &params)
        .await
        .drain()
        .await
        .2
        .unwrap();
    assert_eq!(exit["timed_out"], true);
    let cap = json!({"output_cap": {"max_bytes": 1000, "on_overflow": "kill"}});
    let (out, _, exit) = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["emit", "100000"], cap))
        .await
        .drain()
        .await;
    assert_eq!(out.len(), 1000);
    assert_eq!(exit.unwrap()["overflow"], true);

    // A process_id used twice while the first lives: CONFLICT.
    let params = spawn_params(&sb, &["sleep"], json!({}));
    let first = Client::spawn_ok(&h.socket, &params).await;
    let (_, r) = Client::spawn(&h.socket, &params).await;
    assert_eq!(code(&refused(r)), -32108);
    drop(first);
    until("the first one stopped", || m.processes.live() == 0).await;
}

#[tokio::test]
async fn processes_per_sandbox_are_limited() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let mut live = Vec::new();
    for _ in 0..64 {
        live.push(Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await);
    }
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
    let e = r.unwrap()["error"].clone();
    assert_eq!(code(&e), -32104);
    assert_eq!(e["data"], json!({"what": "processes", "limit": 64}));
    // Another sandbox has its own budget.
    let other = sandbox(&mut c, json!({})).await;
    let _p = Client::spawn_ok(&h.socket, &spawn_params(&other, &["sleep"], json!({}))).await;
    // One ends: room again.
    drop(live.pop());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
}

#[tokio::test]
async fn a_cancel_before_with_or_after_the_spawn_always_ends_the_process() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let m = mock(&h, &sb);

    // Before: the server has not seen the id. Remembered; the spawn is CANCELLED and nothing
    // reaches the guest.
    let params = spawn_params(&sb, &["sleep"], json!({}));
    let r = c
        .ok(
            "process.cancel",
            json!({"process_id": params["process_id"]}),
        )
        .await;
    assert_eq!(r, json!({"was_running": false, "tree_clean": true}));
    let opens = m.processes.open_requests.load(Ordering::SeqCst);
    let (mut p, r) = Client::spawn(&h.socket, &params).await;
    assert_eq!(code(&r.unwrap()["error"]), -32012);
    assert!(p.closed().await);
    assert_eq!(
        m.processes.open_requests.load(Ordering::SeqCst),
        opens,
        "nothing was opened"
    );

    // With: the guest has the request but has not read it yet. crabd remembers the cancel and
    // answers the open CANCELLED.
    m.first_frame_delay_ms.store(300, Ordering::SeqCst);
    let params = spawn_params(&sb, &["sleep"], json!({}));
    let spawn = tokio::spawn({
        let (path, params) = (h.socket.clone(), params.clone());
        async move { Client::spawn(&path, &params).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = c
        .ok(
            "process.cancel",
            json!({"process_id": params["process_id"], "grace_ms": 0}),
        )
        .await;
    assert_eq!(r["was_running"], false);
    let (_, r) = spawn.await.unwrap();
    assert_eq!(code(&r.unwrap()["error"]), -32012);
    m.first_frame_delay_ms.store(0, Ordering::SeqCst);

    // Sent at the same moment, many times: each ends as CANCELLED or as an EXIT that says
    // cancelled, never as a running program.
    for _ in 0..20 {
        let params = spawn_params(&sb, &["sleep"], json!({}));
        let spawn = tokio::spawn({
            let (path, params) = (h.socket.clone(), params.clone());
            async move {
                let (mut p, r) = Client::spawn(&path, &params).await;
                let r = r.unwrap();
                if r.get("error").is_some() {
                    return code(&r["error"]);
                }
                p.drain()
                    .await
                    .2
                    .map_or(0, |e| i64::from(e["cancelled"] == true))
            }
        });
        let mut answer = c
            .ok(
                "process.cancel",
                json!({"process_id": params["process_id"], "grace_ms": 0}),
            )
            .await;
        // The SDK's rule: a cancel answered `was_running: false` is sent once more after a
        // successful spawn. Here: once more after a short wait.
        if answer["was_running"] == false {
            tokio::time::sleep(Duration::from_millis(20)).await;
            answer = c
                .ok(
                    "process.cancel",
                    json!({"process_id": params["process_id"], "grace_ms": 0}),
                )
                .await;
        }
        let end = tokio::time::timeout(Duration::from_secs(5), spawn)
            .await
            .expect("the process ended")
            .unwrap();
        assert!(end == -32012 || end == 1, "ended as {end} ({answer})");
    }
    until("no program left", || m.processes.live() == 0).await;

    // After: was_running, EXIT with cancelled, close. A second cancel is fine.
    let params = spawn_params(&sb, &["sleep"], json!({}));
    let mut p = Client::spawn_ok(&h.socket, &params).await;
    let r = c
        .ok(
            "process.cancel",
            json!({"process_id": params["process_id"], "grace_ms": 0}),
        )
        .await;
    assert_eq!(r, json!({"was_running": true, "tree_clean": true}));
    let exit = p.drain().await.2.unwrap();
    assert_eq!(exit["cancelled"], true);
    let r = c
        .ok(
            "process.cancel",
            json!({"process_id": params["process_id"]}),
        )
        .await;
    assert_eq!(r["was_running"], false);
    // Bad params.
    assert_eq!(
        code(&c.err("process.cancel", json!({"x": 1})).await),
        -32602
    );
}

#[tokio::test]
async fn a_closed_process_connection_stops_the_tree_a_lost_control_connection_does_not() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let m = mock(&h, &sb);

    // A process connection closed: the guest connection closes.
    let p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
    until("one running", || m.processes.live() == 1).await;
    drop(p);
    until("stopped", || m.processes.live() == 0).await;

    // Also while a stdin write waits on a program that does not read (echo blocks on its
    // stdout, which this client does not read).
    let p = Client::spawn_ok(
        &h.socket,
        &spawn_params(&sb, &["echo"], json!({"stdin": "pipe"})),
    )
    .await;
    let (rd, mut wr) = p.s.into_split();
    let writer = tokio::spawn(async move {
        let chunk = vec![7u8; 65536];
        loop {
            let mut rec = vec![STDIN, 0, 1, 0, 0];
            rec.extend_from_slice(&chunk);
            if wr.write_all(&rec).await.is_err() {
                return;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(m.processes.live(), 1);
    writer.abort();
    let _ = writer.await;
    drop(rd);
    until("stopped although its stdin was blocked", || {
        m.processes.live() == 0
    })
    .await;

    // A lost control connection stops nothing.
    let params = spawn_params(&sb, &["sleep"], json!({}));
    let mut p = Client::spawn_ok(&h.socket, &params).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(m.processes.live(), 1, "the process runs on");
    let mut c = h.client().await;
    c.ok(
        "process.cancel",
        json!({"process_id": params["process_id"], "grace_ms": 0}),
    )
    .await;
    assert_eq!(p.drain().await.2.unwrap()["cancelled"], true);
}

// ---------------------------------------------------------------------------------------------
// Forwards
// ---------------------------------------------------------------------------------------------

async fn forward_sandbox(
    h: &Harness,
    c: &mut Client,
    forwards: Value,
) -> (Value, sandcrab_core::testing::MockCrabd) {
    let info = c
        .ok(
            "sandbox.create",
            json!({"template": "base", "forwards": forwards}),
        )
        .await;
    assert_eq!(
        info["forwards_served"], true,
        "the creating connection serves: {info}"
    );
    let sb = info["sandbox_id"].clone();
    let m = mock(h, &sb);
    until("the forwards listen", || m.forwards.has_listener()).await;
    (sb, m)
}

async fn attach(h: &Harness, conn_id: &Value) -> Client {
    let mut a = Client::connect(&h.socket).await;
    let r = a.call("forward.attach", json!({"conn_id": conn_id})).await;
    assert_eq!(r["result"], json!({}), "{r}");
    a
}

#[tokio::test]
async fn a_forwarded_connection_reaches_the_serving_client_and_pipes_both_ways() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let (sb, m) = forward_sandbox(&h, &mut c, json!([{"guest_port": 8080}])).await;

    let mut program = m.forwards.connect(8080).unwrap();
    let note = c.notification("forward.incoming").await;
    assert_eq!(note["sandbox_id"], sb);
    assert_eq!(note["guest_port"], 8080);
    let id = note["conn_id"].as_str().unwrap();
    assert_eq!(id.len(), 32, "128 bits, hex");
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    let a = attach(&h, &note["conn_id"]).await;

    // Raw bytes both ways, 1 MiB each, concurrently.
    let data = pattern(1 << 20);
    let (mut ard, mut awr) = a.s.into_split();
    let (mut prd, mut pwr) = tokio::io::split(&mut program);
    let to_client = async {
        pwr.write_all(&data).await.unwrap();
        pwr.shutdown().await.unwrap();
    };
    let at_client = async {
        let mut got = Vec::new();
        ard.read_to_end(&mut got).await.unwrap();
        got
    };
    let from_client = async {
        awr.write_all(&data).await.unwrap();
        // A half-close is the end of this direction.
        awr.shutdown().await.unwrap();
    };
    let at_program = async {
        let mut got = Vec::new();
        prd.read_to_end(&mut got).await.unwrap();
        got
    };
    let ((), got_client, (), got_program) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(to_client, at_client, from_client, at_program)
    })
    .await
    .expect("both directions end");
    assert!(got_client == data, "program → client");
    assert!(got_program == data, "client → program");

    let info = c.ok("sandbox.get", json!({"sandbox_id": sb})).await;
    assert_eq!(info["forwards"][0]["guest_port"], 8080);
    assert_eq!(info["forwards"][0]["accepted"], 1, "{info}");
    assert_eq!(info["forwards_served"], true);
}

#[tokio::test]
async fn unserved_rejected_unknown_and_capped_connections_fail_closed() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let (sb, m) = forward_sandbox(
        &h,
        &mut c,
        json!([{"guest_port": 9000, "max_connections": 1}]),
    )
    .await;
    let refused = |m: &sandcrab_core::testing::MockCrabd| m.forwards.stats_of(9000).refused;

    // forward.reject: the program's connection closes; counted.
    let mut program = m.forwards.connect(9000).unwrap();
    let note = c.notification("forward.incoming").await;
    c.ok(
        "forward.reject",
        json!({"conn_id": note["conn_id"], "reason": "target down"}),
    )
    .await;
    let mut buf = Vec::new();
    program.read_to_end(&mut buf).await.unwrap();
    until("rejected counted", || refused(&m).rejected == 1).await;
    // The conn_id is gone: an attach now is -32004, then close.
    let mut a = Client::connect(&h.socket).await;
    let e = a
        .err("forward.attach", json!({"conn_id": note["conn_id"]}))
        .await;
    assert_eq!(code(&e), -32004);
    assert!(a.closed().await);
    // Unknown fields.
    let mut a = Client::connect(&h.socket).await;
    let e = a
        .err("forward.attach", json!({"conn_id": "x", "y": 1}))
        .await;
    assert_eq!(code(&e), -32602);
    // forward.reject of an unknown id is fine.
    c.ok("forward.reject", json!({"conn_id": "nope"})).await;

    // Over the forward's cap: refused at once.
    let mut first = m.forwards.connect(9000).unwrap();
    let note = c.notification("forward.incoming").await;
    let mut a = attach(&h, &note["conn_id"]).await;
    let mut second = m.forwards.connect(9000).unwrap();
    let mut buf = Vec::new();
    second.read_to_end(&mut buf).await.unwrap();
    until("capped counted", || refused(&m).rejected == 2).await;
    first.write_all(b"still here").await.unwrap();
    let mut got = [0u8; 10];
    a.s.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"still here");
    drop(first);
    drop(a);

    // The serving connection is gone: nobody serves, connections are refused.
    drop(c);
    let mut c = h.client().await;
    let info = c.ok("sandbox.get", json!({"sandbox_id": sb})).await;
    assert_eq!(info["forwards_served"], false);
    // Let the relay of the first connection end and free its slot.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut program = m.forwards.connect(9000).unwrap();
    let mut buf = Vec::new();
    program.read_to_end(&mut buf).await.unwrap();
    until("unserved counted", || refused(&m).rejected == 3).await;
    let quiet = tokio::time::timeout(Duration::from_millis(200), c.recv()).await;
    assert!(
        quiet.is_err(),
        "no notification on a connection that does not serve"
    );

    // Reconnect: forward.serve, and the next connection goes to the new connection.
    let r = c.ok("forward.serve", json!({"sandbox_id": sb})).await;
    assert_eq!(r, json!({"ports": [9000]}));
    let info = c.ok("sandbox.get", json!({"sandbox_id": sb})).await;
    assert_eq!(info["forwards_served"], true);
    let mut program = m.forwards.connect(9000).unwrap();
    let note = c.notification("forward.incoming").await;
    let mut a = attach(&h, &note["conn_id"]).await;
    a.raw(b"hello").await;
    let mut got = [0u8; 5];
    program.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"hello");
    drop(a);
    drop(program);

    // Counters in sandbox.get.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let info = c.ok("sandbox.get", json!({"sandbox_id": sb})).await;
    let f = &info["forwards"][0];
    assert_eq!(f["accepted"], 2, "{info}");
    assert_eq!(f["refused"]["rejected"], 3, "{info}");
    // forward.serve of a sandbox that is not the caller's.
    let e = c
        .err("forward.serve", json!({"sandbox_id": new_id()}))
        .await;
    assert_eq!(code(&e), -32102);
}

#[tokio::test]
async fn an_announced_connection_not_attached_in_time_is_refused() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let (_sb, m) = forward_sandbox(&h, &mut c, json!([{"guest_port": 7000}])).await;
    let mut program = m.forwards.connect(7000).unwrap();
    let note = c.notification("forward.incoming").await;
    let t = Instant::now();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), program.read_to_end(&mut buf))
        .await
        .expect("closed in time")
        .unwrap();
    assert!(t.elapsed() >= Duration::from_secs(8), "{:?}", t.elapsed());
    let r = m.forwards.stats_of(7000).refused;
    assert_eq!(r.rejected + r.unclaimed, 1, "{r:?}");
    // Too late to attach.
    let mut a = Client::connect(&h.socket).await;
    assert_eq!(
        code(
            &a.err("forward.attach", json!({"conn_id": note["conn_id"]}))
                .await
        ),
        -32004
    );
}

#[tokio::test]
async fn shutdown_notifies_and_cancels_processes() {
    let mut h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    let mut p = Client::spawn_ok(&h.socket, &spawn_params(&sb, &["sleep"], json!({}))).await;
    let stop = tokio::spawn(async move {
        let r = h.stop().await;
        (h, r)
    });
    let note = c.notification("server.shutdown").await;
    assert_eq!(note, json!({"grace_ms": 5000}));
    let exit = p.drain().await.2.expect("the process gets its EXIT");
    assert_eq!(exit["cancelled"], true);
    let (h, r) = stop.await.unwrap();
    r.unwrap();
    assert_eq!(h.destroyed(), 1);
    assert!(c.closed().await);
}

#[tokio::test]
async fn destroy_notifies_the_owners_control_connections() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let mut other = h.client().await;
    let sb = sandbox(&mut c, json!({})).await;
    c.ok("sandbox.destroy", json!({"sandbox_id": sb})).await;
    let note = other.notification("sandbox.destroyed").await;
    assert_eq!(note, json!({"sandbox_id": sb, "reason": "destroyed"}));
    // A process in a destroyed sandbox: not found.
    let (_, r) = Client::spawn(&h.socket, &spawn_params(&sb, &["exit"], json!({}))).await;
    assert_eq!(code(&r.unwrap()["error"]), -32102);
}
