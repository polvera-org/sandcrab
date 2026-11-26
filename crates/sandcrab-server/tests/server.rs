//! The server over a real Unix socket, on core's `FakeBackend`: the socket rules, the first
//! frame, `server.hello`, every sandbox call, mounts, bad frames, limits, shutdown and the
//! lifeline. A raw client speaks the frames, so nothing here depends on an SDK.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

use common::*;
use sandcrab_core::testing::{FakeBackend, FakeEvent};
use sandcrab_server::{BackendName, MountRootConfig, ServerError};
use serde_json::{Value, json};

#[tokio::test]
async fn hello_and_the_first_frame() {
    let h = Harness::new(|_, _| {}).await;

    let mut c = Client::connect(&h.socket).await;
    let r = c.ok("server.hello", hello("1.0")).await;
    assert_eq!(r["api_version"], "1.0");
    assert_eq!(r["backend"], "docker");
    assert_eq!(r["features"], json!(["forwards", "mounts"]));
    assert_eq!(r["limits"]["max_sandboxes"], 64);
    assert_eq!(r["limits"]["max_mounts"], 16);
    assert_eq!(r["limits"]["default_ttl_secs"], 1800);
    // hello is answered once per connection.
    assert_eq!(code(&c.err("server.hello", hello("1.0")).await), -32600);

    // Another major, or a malformed version: UNSUPPORTED_VERSION, then close.
    for v in ["2.0", "0.9", "1", "x.y"] {
        let mut c = Client::connect(&h.socket).await;
        let e = c.err("server.hello", hello(v)).await;
        assert_eq!(code(&e), -32009, "{v}: {e}");
        assert_eq!(e["data"], json!({"server": "1.0", "client": v}));
        assert!(c.closed().await);
    }
    // Unknown params are refused.
    let mut c = Client::connect(&h.socket).await;
    let e = c
        .err(
            "server.hello",
            json!({"client": "t", "api_version": "1.0", "x": 1}),
        )
        .await;
    assert_eq!(code(&e), -32602);

    // Other first frames.
    let mut c = Client::connect(&h.socket).await;
    assert_eq!(code(&c.err("sandbox.list", json!({})).await), -32600);
    assert!(c.closed().await);
    // Process and forward connections with bad params: one error, then close.
    for m in ["process.spawn", "forward.attach"] {
        let mut c = Client::connect(&h.socket).await;
        assert_eq!(code(&c.err(m, json!({})).await), -32602, "{m}");
        assert!(c.closed().await);
    }
    let mut c = Client::connect(&h.socket).await;
    c.raw(&[0, 0, 0, 3]).await;
    c.raw(b"{x:").await;
    let r = c.recv().await.unwrap();
    assert_eq!(code(&r["error"]), -32700);
    assert_eq!(r["id"], Value::Null);
    assert!(c.closed().await);

    // No first frame: closed after the timeout.
    let mut c = Client::connect(&h.socket).await;
    let t = std::time::Instant::now();
    assert!(c.closed().await);
    assert!(
        t.elapsed() >= Duration::from_millis(1900),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn a_peer_outside_the_allow_list_is_refused_even_on_an_open_socket() {
    let h = Harness::new(|c, _| {
        c.allow_uids = Some(vec![me().wrapping_add(1)]);
        c.socket_mode = 0o666;
    })
    .await;
    let mode = std::fs::metadata(&h.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666);
    let mut c = Client::connect(&h.socket).await;
    // The answer comes before any frame is read.
    let r = c.recv().await.expect("one UNAUTHORIZED frame");
    assert_eq!(code(&r["error"]), -32008);
    assert_eq!(r["error"]["data"], json!({}));
    assert_eq!(r["id"], Value::Null);
    assert!(c.closed().await);

    // A client that writes its hello after the answer was sent (as an SDK whose connect
    // callback runs late does) still reads it: the server does not close under that write,
    // which would fail it with EPIPE and lose the answer.
    let mut c = Client::connect(&h.socket).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.send(&json!({"jsonrpc": "2.0", "id": 0, "method": "server.hello", "params": hello("1.0")}))
        .await;
    let r = c
        .recv()
        .await
        .expect("one UNAUTHORIZED frame after a late hello");
    assert_eq!(code(&r["error"]), -32008);
    assert!(c.closed().await);
}

#[tokio::test]
async fn default_allow_list_is_the_own_uid_and_mode_and_group_apply() {
    let gid = my_gid();
    let h = Harness::new(|c, _| {
        c.socket_mode = 0o640;
        c.socket_group = Some(gid.to_string());
    })
    .await;
    let m = std::fs::metadata(&h.socket).unwrap();
    assert_eq!(m.permissions().mode() & 0o777, 0o640);
    assert_eq!(m.gid(), gid);
    // The directory was created for the group: traversable, not listable.
    let d = std::fs::metadata(h.socket.parent().unwrap()).unwrap();
    assert_eq!(d.permissions().mode() & 0o777, 0o710);
    h.client().await;
}

#[tokio::test]
async fn a_bad_socket_directory_or_a_live_socket_stops_the_start() {
    let tmp = tempfile::tempdir().unwrap();
    let open = tmp.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let mut config = base_config(tmp.path());
    config.socket = Some(open.join("server.sock"));
    let err = try_start(config, &FakeBackend::shared_kernel())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("other users"), "{err}");

    // A second server on a live socket refuses; once the first stopped, it starts.
    let mut h = Harness::new(|_, _| {}).await;
    let mut config = base_config(h.tmp.path());
    config.state_dir = Some(h.tmp.path().join("state2"));
    let err = try_start(config.clone(), &FakeBackend::shared_kernel())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("another server"), "{err}");
    h.stop().await.unwrap();
    assert!(!h.socket.exists());
    let (server, _tx, _rx) = try_start(config, &FakeBackend::shared_kernel())
        .await
        .unwrap();
    assert!(server.socket_path().exists());
}

#[tokio::test]
async fn the_configured_backend_must_be_the_one_built() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = base_config(tmp.path());
    config.backend = Some(BackendName::Libkrun);
    let err = try_start(config, &FakeBackend::shared_kernel())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("libkrun"), "{err}");
    // No backend at all: a config error, never a pick.
    let mut config = base_config(tmp.path());
    config.backend = None;
    assert!(matches!(
        try_start(config, &FakeBackend::shared_kernel()).await,
        Err(ServerError::Config(_))
    ));
}

#[tokio::test]
async fn every_sandbox_call() {
    let h = Harness::new(|c, _| c.limits.max_ttl_secs = 7200).await;
    let mut c = h.client().await;

    let a = c
        .ok(
            "sandbox.create",
            json!({"template": "base", "labels": {"run": "r1"}}),
        )
        .await;
    let id_a = a["sandbox_id"].as_str().unwrap().to_owned();
    assert_eq!(a["template"], "base");
    assert_eq!(a["state"], "running");
    assert_eq!(a["backend"], "docker");
    assert_eq!(a["labels"], json!({"run": "r1"}));
    assert!(a["expires_at"].is_string(), "default TTL: {a}");
    // The sandbox user defaults to the peer's uid:gid.
    let spec = h.backend.last_spec().unwrap();
    let user = spec.user.unwrap();
    assert_eq!((user.uid, user.gid), (me(), my_gid()));
    assert_eq!(
        spec.network.policy,
        sandcrab_sdk::NetworkPolicy::Disabled,
        "network defaults to disabled"
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    let b = c
        .ok(
            "sandbox.create",
            json!({"template": "base", "labels": {"run": "r1"}, "ttl_secs": null,
                   "user": "1234:1234", "resources": {"pids_max": 128}}),
        )
        .await;
    let id_b = b["sandbox_id"].as_str().unwrap().to_owned();
    assert!(b["expires_at"].is_null());
    let spec = h.backend.last_spec().unwrap();
    assert_eq!(spec.user.unwrap().to_string(), "1234:1234");
    assert_eq!(spec.pids_max, Some(128));
    let other = c
        .ok(
            "sandbox.create",
            json!({"template": "base", "labels": {"run": "r2"}}),
        )
        .await;

    // get (the expiry is computed at each answer)
    let mut got = c.ok("sandbox.get", json!({"sandbox_id": id_a})).await;
    let mut want = a.clone();
    assert!(got["expires_at"].is_string());
    got["expires_at"] = Value::Null;
    want["expires_at"] = Value::Null;
    assert_eq!(got, want);
    let e = c.err("sandbox.get", json!({"sandbox_id": "nope"})).await;
    assert_eq!(
        (code(&e), &e["data"]),
        (-32102, &json!({"sandbox_id": "nope"}))
    );

    // list: all, by label, since a time.
    let ids = |v: &Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|i| i["sandbox_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&c.ok("sandbox.list", json!({})).await).len(), 3);
    assert_eq!(
        ids(&c.ok("sandbox.list", json!({"labels": {"run": "r1"}})).await),
        [id_a.as_str(), id_b.as_str()]
    );
    let since = b["created_at"].as_str().unwrap();
    assert_eq!(
        ids(&c
            .ok(
                "sandbox.list",
                json!({"labels": {"run": "r1"}, "created_after": since})
            )
            .await),
        [id_b.as_str()]
    );
    let e = c
        .err("sandbox.list", json!({"created_after": "yesterday"}))
        .await;
    assert_eq!(code(&e), -32602);

    // set_ttl: renew, remove, refuse zero and above the ceiling.
    let r = c
        .ok(
            "sandbox.set_ttl",
            json!({"sandbox_id": id_b, "ttl_secs": 3600}),
        )
        .await;
    assert!(r["expires_at"].is_string());
    let r = c
        .ok(
            "sandbox.set_ttl",
            json!({"sandbox_id": id_b, "ttl_secs": null}),
        )
        .await;
    assert!(r["expires_at"].is_null());
    let e = c
        .err(
            "sandbox.set_ttl",
            json!({"sandbox_id": id_b, "ttl_secs": 0}),
        )
        .await;
    assert_eq!(code(&e), -32602);
    let e = c
        .err(
            "sandbox.set_ttl",
            json!({"sandbox_id": id_b, "ttl_secs": 7201}),
        )
        .await;
    assert_eq!(code(&e), -32104);
    assert_eq!(e["data"], json!({"what": "ttl_secs", "limit": 7200}));
    let e = c.err("sandbox.set_ttl", json!({"sandbox_id": id_b})).await;
    assert_eq!(code(&e), -32602, "ttl_secs is required: {e}");

    // destroy: true once, then false; then not found.
    let r = c.ok("sandbox.destroy", json!({"sandbox_id": id_a})).await;
    assert_eq!(r, json!({"destroyed": true}));
    let r = c.ok("sandbox.destroy", json!({"sandbox_id": id_a})).await;
    assert_eq!(r, json!({"destroyed": false}));
    let e = c.err("sandbox.get", json!({"sandbox_id": id_a})).await;
    assert_eq!(code(&e), -32102);

    // destroy_by_label
    let e = c
        .err("sandbox.destroy_by_label", json!({"labels": {}}))
        .await;
    assert_eq!(code(&e), -32602);
    let r = c
        .ok("sandbox.destroy_by_label", json!({"labels": {"run": "r1"}}))
        .await;
    assert_eq!(
        r,
        json!({"matched": 1, "destroyed": 1, "remaining": [], "empty": true})
    );
    assert_eq!(
        ids(&c.ok("sandbox.list", json!({})).await),
        [other["sandbox_id"].as_str().unwrap()]
    );

    // capabilities
    let caps = c.ok("server.capabilities", json!({})).await;
    assert_eq!(caps["backend"], "docker");
    assert_eq!(caps["hardware_isolated"], false);
    assert_eq!(caps["multi_tenant_safe"], false);
    assert_eq!(caps["nproc_scope"], "host_uid");
    assert_eq!(caps["max_mounts"], 16);
    assert_eq!(caps["selftest"], Value::Null);

    // Not yet served on this step.
    assert_eq!(code(&c.err("server.selftest", json!({})).await), -32007);
    for m in ["process.cancel", "forward.serve", "forward.reject"] {
        assert_eq!(code(&c.err(m, json!({})).await), -32602, "{m}");
    }
}

#[tokio::test]
async fn create_refusals_and_the_sandbox_limit() {
    let h = Harness::new(|c, _| c.limits.max_sandboxes = 2).await;
    let mut c = h.client().await;
    let cases = [
        (json!({"template": "nope"}), -32106),
        (json!({"template": ""}), -32602),
        (json!({"template": "base", "extra": 1}), -32602),
        (
            json!({"template": "base", "labels": {"sandcrab.x": "1"}}),
            -32602,
        ),
        (json!({"template": "base", "user": "0:0"}), -32105),
        (json!({"template": "base", "user": "1000:0"}), -32105),
        (json!({"template": "base", "user": "alice"}), -32602),
        (json!({"template": "base", "ttl_secs": 0}), -32602),
        (json!({"template": "base", "ttl_secs": 90000}), -32104),
        (
            json!({"template": "base", "resources": {"vcpu": 9}}),
            -32104,
        ),
        (
            json!({"template": "base", "resources": {"memory_mb": 20000}}),
            -32104,
        ),
        (
            json!({"template": "base", "resources": {"pids_max": 9000}}),
            -32104,
        ),
        (
            json!({"template": "base", "resources": {"nofile": 2000000}}),
            -32104,
        ),
        // No mount roots configured: every mount is refused.
        (
            json!({"template": "base", "mounts": [{"path": "/tmp", "mode": "ro"}]}),
            -32103,
        ),
    ];
    for (params, want) in cases {
        let e = c.err("sandbox.create", params.clone()).await;
        assert_eq!(code(&e), want, "{params}: {e}");
    }
    let e = c.err("sandbox.create", json!({"template": "nope"})).await;
    assert_eq!(e["data"], json!({"kind": "not_found", "template": "nope"}));

    c.ok("sandbox.create", json!({"template": "base"})).await;
    c.ok("sandbox.create", json!({"template": "base"})).await;
    let e = c.err("sandbox.create", json!({"template": "base"})).await;
    assert_eq!(code(&e), -32104);
    assert_eq!(e["data"], json!({"what": "sandboxes", "limit": 2}));
}

#[tokio::test]
async fn the_default_template_and_a_root_user_when_allowed() {
    let h = Harness::new(|c, _| {
        c.default_template = Some("base".into());
        c.allow_root_user = true;
    })
    .await;
    let mut c = h.client().await;
    let r = c
        .ok("sandbox.create", json!({"template": "", "user": "0:0"}))
        .await;
    assert_eq!(r["template"], "base");
    assert_eq!(h.backend.last_spec().unwrap().user.unwrap().uid, 0);
}

#[tokio::test]
async fn mounts_under_the_roots_and_never_the_servers_own_directories() {
    let h = Harness::new(|c, tmp| {
        let root = tmp.join("root");
        for d in ["data/a", "data/b", "tools", "many", "real/sock"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for i in 0..17 {
            std::fs::create_dir_all(root.join(format!("many/{i}"))).unwrap();
        }
        // The socket is named through a symlink: a second path to `root/real/sock`.
        std::os::unix::fs::symlink(root.join("real"), tmp.join("alias")).unwrap();
        std::os::unix::fs::symlink(root.join("data/a"), root.join("link")).unwrap();
        std::fs::set_permissions(
            root.join("real/sock"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        c.socket = Some(tmp.join("alias/sock/server.sock"));
        c.state_dir = Some(root.join("state"));
        c.mount_roots = vec![
            MountRootConfig {
                path: root.clone(),
                mode: sandcrab_sdk::protocol::api::MountMode::ReadWrite,
            },
            MountRootConfig {
                path: root.join("tools"),
                mode: sandcrab_sdk::protocol::api::MountMode::ReadOnly,
            },
        ];
    })
    .await;
    let root = std::fs::canonicalize(h.tmp.path().join("root")).unwrap();
    let p = |s: &str| root.join(s).display().to_string();
    let mut c = h.client().await;

    let r = c
        .ok(
            "sandbox.create",
            json!({"template": "base", "mounts": [
                {"path": p("data/a"), "mode": "rw"}, {"path": p("tools"), "mode": "ro"}]}),
        )
        .await;
    assert_eq!(
        r["mounts"],
        json!([{"path": p("data/a"), "mode": "rw"}, {"path": p("tools"), "mode": "ro"}])
    );
    let spec = h.backend.last_spec().unwrap();
    assert_eq!(spec.host_mounts.len(), 2);
    assert_eq!(spec.host_mounts[0].guest_path, root.join("data/a"));
    // A second sandbox may mount the same folder.
    c.ok(
        "sandbox.create",
        json!({"template": "base", "mounts": [{"path": p("data/a"), "mode": "ro"}]}),
    )
    .await;

    let refused = |path: String, mode: &str| json!({"template": "base", "mounts": [{"path": path, "mode": mode}]});
    let cases = [
        // The socket's directory, by its real path, and a folder that contains it.
        refused(p("real/sock"), "ro"),
        refused(p("real"), "rw"),
        // The state directory (created by the server under the root).
        refused(p("state"), "ro"),
        // Outside every root; above the root's mode; through a symlink; missing.
        refused("/etc".into(), "ro"),
        refused(p("tools"), "rw"),
        refused(p("link"), "ro"),
        refused(p("data/missing"), "ro"),
        // Nested in one sandbox.
        json!({"template": "base", "mounts": [
            {"path": p("data"), "mode": "ro"}, {"path": p("data/b"), "mode": "ro"}]}),
    ];
    for params in cases {
        let e = c.err("sandbox.create", params.clone()).await;
        assert_eq!(code(&e), -32103, "{params}: {e}");
        assert!(e["data"]["reason"].is_string(), "{e}");
    }
    // Over the cap: the error names it.
    let many: Vec<Value> = (0..17)
        .map(|i| json!({"path": p(&format!("many/{i}")), "mode": "ro"}))
        .collect();
    let e = c
        .err(
            "sandbox.create",
            json!({"template": "base", "mounts": many}),
        )
        .await;
    assert_eq!(code(&e), -32103);
    assert_eq!(e["data"]["cap"], 16, "{e}");
}

#[tokio::test]
async fn bad_frames_never_take_the_server_down() {
    let h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    // Malformed JSON and a non-request: answered, the connection stays.
    c.raw(&[0, 0, 0, 5]).await;
    c.raw(b"nope!").await;
    assert_eq!(code(&c.recv().await.unwrap()["error"]), -32700);
    c.send(&json!([1, 2])).await;
    assert_eq!(code(&c.recv().await.unwrap()["error"]), -32600);
    c.send(&json!({"jsonrpc": "2.0", "id": 9, "method": 5}))
        .await;
    assert_eq!(code(&c.recv().await.unwrap()["error"]), -32600);
    assert_eq!(code(&c.err("no.such", json!({})).await), -32601);
    assert_eq!(
        code(
            &c.err("sandbox.get", json!({"sandbox_id": "x", "y": 1}))
                .await
        ),
        -32602
    );
    // A notification gets no answer; the next call still works.
    c.send(&json!({"jsonrpc": "2.0", "method": "sandbox.list"}))
        .await;
    c.ok("sandbox.list", json!({})).await;

    // A frame over 16 MiB: one error, then close.
    c.raw(&(16 * 1024 * 1024 + 1u32).to_be_bytes()).await;
    let r = c.recv().await.unwrap();
    assert_eq!(code(&r["error"]), -32006, "{r}");
    assert!(c.closed().await);
    // A zero-length frame.
    let mut c = h.client().await;
    c.raw(&[0, 0, 0, 0]).await;
    assert!(c.recv().await.unwrap()["error"].is_object());
    assert!(c.closed().await);
    // A frame cut short, then a close.
    let mut c = h.client().await;
    c.raw(&[0, 0, 0, 50, b'{']).await;
    drop(c);

    // The server still serves.
    h.client().await.ok("sandbox.list", json!({})).await;
}

#[tokio::test]
async fn disconnect_keeps_sandboxes_and_shutdown_destroys_them() {
    let mut h = Harness::new(|_, _| {}).await;
    let mut c = h.client().await;
    let id = c.ok("sandbox.create", json!({"template": "base"})).await["sandbox_id"].clone();
    c.ok("sandbox.create", json!({"template": "base"})).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.destroyed(), 0, "a disconnect destroys nothing");
    let mut c = h.client().await;
    let got = c.ok("sandbox.get", json!({"sandbox_id": id})).await;
    assert_eq!(got["sandbox_id"], id);

    h.stop().await.unwrap();
    assert_eq!(h.destroyed(), 2);
    assert!(!h.socket.exists(), "the socket is removed");
    // The open connection was told, then closed with the server.
    assert_eq!(c.recv().await.unwrap()["method"], "server.shutdown");
    assert!(c.closed().await);
}

#[tokio::test]
async fn the_lifeline_stops_the_server_and_its_sandboxes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = base_config(tmp.path());
    let socket = config.socket.clone().unwrap();
    let backend = FakeBackend::shared_kernel();
    let (server, _tx, _rx) = try_start(config, &backend).await.unwrap();
    let (keep, end) = std::os::unix::net::UnixStream::pair().unwrap();
    let task = tokio::spawn(server.run(sandcrab_server::lifeline(end)));

    let mut c = Client::connect(&socket).await;
    c.ok("server.hello", hello("1.0")).await;
    c.ok("sandbox.create", json!({"template": "base"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "the lifeline is still held");

    drop(keep);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the server stops when its lifeline closes")
        .unwrap()
        .unwrap();
    assert!(
        backend.events().contains(&FakeEvent::Destroyed(
            backend
                .events()
                .iter()
                .find_map(|e| match e {
                    FakeEvent::Spawned(id) => Some(*id),
                    FakeEvent::Destroyed(_) => None,
                })
                .unwrap()
        ))
    );
    assert!(!socket.exists());
}
