//! Sandbox lifecycle e2e: `destroy()` / `shutdown()` must not report success while the
//! sandbox is still alive on the backend. See `tests/common/mod.rs` for how to run.
//!
//! Regression: a second caller racing a drop-triggered background destroy used to return
//! `Ok` while the backend teardown was still in flight (leaking the container if the
//! runtime then exited). Teardown is now single-flight: every caller awaits the same one.

mod common;

use std::time::Duration;

use common::run;
use sandcrab_sdk::SandboxConfig;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn destroy_waits_for_in_flight_teardown() {
    run("destroy-race", |h| async move {
        // Sandbox::destroy on a cloned core handle.
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        let id = sb.id();
        let core = sb.core().clone();
        drop(sb); // SDK Drop → background destroy starts
        tokio::time::sleep(Duration::from_millis(20)).await;
        core.destroy().await.unwrap();
        assert!(
            !h.backend_has(id),
            "Sandbox::destroy returned Ok but {id} is still alive on the backend"
        );

        // Sandcrab::shutdown.
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        let id = sb.id();
        drop(sb);
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.sc.shutdown().await.unwrap();
        assert!(
            !h.backend_has(id),
            "Sandcrab::shutdown returned Ok but {id} is still alive on the backend"
        );
    })
    .await;
}
