//! Self-check of the e2e harness: sandboxes are torn down even when a test body panics.
//! See `tests/common/mod.rs` for how to run the suite.

mod common;

use std::sync::{Arc, Mutex};

use common::{E2eBackend, docker_ps, run, sh};
use sandcrab_sdk::{SandboxConfig, SandboxId};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn harness_destroys_sandboxes_when_the_body_panics() {
    let spawned: Arc<Mutex<Option<SandboxId>>> = Arc::default();
    let slot = spawned.clone();
    let res = tokio::spawn(run("harness-panic", move |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        *slot.lock().unwrap() = Some(sb.id());
        assert_eq!(sh(&sb, "echo alive").await, "alive\n");
        // Leak the handle on purpose: only the harness can clean this up.
        let _ = sb.detach();
        panic!("intentional failure");
    }))
    .await;

    if E2eBackend::from_env() != E2eBackend::Docker {
        return; // skipped by the harness
    }
    let err = res.expect_err("body panic must propagate");
    assert!(err.is_panic());
    let id = spawned.lock().unwrap().expect("sandbox was spawned");
    assert!(
        docker_ps(&format!("sandcrab.sandbox_id={id}")).is_empty(),
        "sandbox {id} leaked after a failing test"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn harness_cleans_up_a_sandbox_dropped_by_a_panicking_body() {
    let spawned: Arc<Mutex<Option<SandboxId>>> = Arc::default();
    let slot = spawned.clone();
    let res = tokio::spawn(run("harness-drop", move |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        *slot.lock().unwrap() = Some(sb.id());
        // `sb` is dropped while unwinding → SDK background destroy races the harness.
        assert_eq!(sh(&sb, "echo alive").await, "dead\n", "intentional failure");
    }))
    .await;

    if E2eBackend::from_env() != E2eBackend::Docker {
        return;
    }
    assert!(res.expect_err("body panic must propagate").is_panic());
    let id = spawned.lock().unwrap().expect("sandbox was spawned");
    assert!(
        docker_ps(&format!("sandcrab.sandbox_id={id}")).is_empty(),
        "sandbox {id} leaked after a failing test"
    );
}
