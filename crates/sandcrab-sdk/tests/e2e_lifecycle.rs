//! Sandbox lifecycle e2e: `destroy()` / `shutdown()` must not report success while the
//! sandbox is still alive on the backend. See `tests/common/mod.rs` for how to run.
//!
//! BUG (open): `SandboxShared::destroy` (`crates/sandcrab-core/src/manager.rs` ~L417)
//! takes the `live` slot and removes the sandbox from `active` *before* awaiting
//! `handle.destroy()`. A second caller — `Sandbox::destroy`, `SandboxManager::shutdown`,
//! `Sandcrab::shutdown` — racing a drop-triggered background destroy finds `live == None`
//! and returns `Ok` immediately, while the backend teardown is still in flight. If the
//! runtime then ends (test/CLI exit) the background task is aborted and the container
//! leaks. Observed: a failing e2e test's sandbox survived `Sandcrab::shutdown()`.

mod common;

use std::time::Duration;

use common::run;
use sandcrab_sdk::SandboxConfig;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "BUG: destroy()/shutdown() return Ok while a concurrent (drop-triggered) teardown \
            is still in flight — sandcrab-core manager.rs SandboxShared::destroy"]
async fn bug_destroy_waits_for_in_flight_teardown() {
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
