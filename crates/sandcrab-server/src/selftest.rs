//! `server.selftest` (design §8): the SDK self-test of one named template on this server's
//! backend, one run at a time per template, the last report kept per template for
//! `server.capabilities`.
//!
//! The report never claims more than was shown. A self-test that cannot start (no read-write
//! mount root for its scratch directory, the template is missing or unpinned, the backend
//! refused) is a report with `ok: false` that says isolation is not proven and why. A run
//! that passes [`SELFTEST_DEADLINE`] is inconclusive, never ok.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_sdk::protocol::ProtocolError;
use sandcrab_sdk::protocol::api::{ApiError, SelftestParams, SelftestReport, SelftestSummary};
use sandcrab_sdk::selftest::{self, SelftestConfig};

use crate::Shared;

/// A whole run (two sandboxes, fifteen probes of at most 5 s each, the teardown) must end
/// within this, or it is reported inconclusive.
pub(crate) const SELFTEST_DEADLINE: Duration = Duration::from_secs(180);

/// A report and when it was made.
#[derive(Clone)]
struct Kept {
    report: SelftestReport,
    at: Instant,
}

/// Per template: a lock that serialises runs, and the last report.
#[derive(Default)]
pub(crate) struct Selftests {
    running: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    last: Mutex<HashMap<String, Kept>>,
}

impl Selftests {
    fn lock_of(&self, template: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(m.entry(template.to_owned()).or_default())
    }

    fn last(&self, template: &str) -> Option<Kept> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(template)
            .cloned()
    }

    /// The last report of `template` in short; `None` when it never ran.
    pub(crate) fn summary(&self, template: &str) -> Option<SelftestSummary> {
        self.last(template).map(|k| SelftestSummary {
            template: k.report.template,
            ok: k.report.ok,
            reason: k.report.reason,
            age_secs: k.at.elapsed().as_secs(),
        })
    }
}

/// `server.selftest`: the template named (an empty name means `default_template`).
pub(crate) async fn call(shared: &Shared, p: SelftestParams) -> Result<SelftestReport, ApiError> {
    let template = if p.template.is_empty() {
        shared.config.default_template.clone().ok_or_else(|| {
            ApiError::Protocol(ProtocolError::InvalidParams(
                "`template` is empty and the server has no `default_template`".into(),
            ))
        })?
    } else {
        p.template
    };
    Ok(run(shared, &template, p.max_age_secs.map(Duration::from_secs)).await)
}

/// Run the self-test of `template`, or return the last report when it is younger than
/// `max_age`. A caller that waited for another run of the same template gets that run's
/// report when `max_age` allows.
pub(crate) async fn run(
    shared: &Shared,
    template: &str,
    max_age: Option<Duration>,
) -> SelftestReport {
    let lock = shared.selftests.lock_of(template);
    let _one_at_a_time = lock.lock().await;
    if let Some(max) = max_age
        && let Some(k) = shared.selftests.last(template)
        && k.at.elapsed() <= max
    {
        return k.report;
    }

    let mut config = SelftestConfig::new(template);
    // The server's own directories must not be reachable from a sandbox (the SDK adds the
    // state directory and the scratch canary).
    if let Ok(socket) = shared.config.socket_path() {
        config.hidden_paths.push(socket.to_path_buf());
        if let Some(dir) = socket.parent() {
            config.hidden_paths.push(dir.to_path_buf());
        }
    }
    if let Some(state) = &shared.config.state_dir {
        config.hidden_paths.push(state.clone());
    }

    let started_at = chrono::Utc::now();
    let start = Instant::now();
    tracing::info!(template, "self-test started");
    let report = match tokio::time::timeout(
        SELFTEST_DEADLINE,
        selftest::run(&shared.sandcrab, config),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => not_proven(
            shared,
            template,
            started_at,
            start.elapsed(),
            format!("isolation not proven: the self-test could not run: {e}"),
        ),
        Err(_) => not_proven(
            shared,
            template,
            started_at,
            start.elapsed(),
            format!(
                "isolation not proven: the self-test did not finish within {} s (inconclusive)",
                SELFTEST_DEADLINE.as_secs()
            ),
        ),
    };
    tracing::info!(
        template,
        ok = report.ok,
        reason = report.reason.as_deref().unwrap_or(""),
        "self-test done"
    );
    shared
        .selftests
        .last
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            template.to_owned(),
            Kept {
                report: report.clone(),
                at: Instant::now(),
            },
        );
    report
}

/// A report with no lines: nothing was shown.
fn not_proven(
    shared: &Shared,
    template: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    took: Duration,
    reason: String,
) -> SelftestReport {
    SelftestReport {
        ok: false,
        reason: Some(reason),
        backend: shared.backend.clone(),
        template: template.to_owned(),
        started_at: started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        lines: Vec::new(),
    }
}

/// `selftest_on_start`: run each in turn, in the background; a failure only shows in the
/// reports (and the log).
pub(crate) async fn on_start(shared: Arc<Shared>) {
    for t in shared.config.selftest_on_start.clone() {
        let r = run(&shared, &t, None).await;
        if !r.ok {
            tracing::warn!(
                template = %t,
                reason = r.reason.as_deref().unwrap_or(""),
                "self-test at start: isolation not proven"
            );
        }
    }
}
