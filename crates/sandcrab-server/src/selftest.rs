//! `server.selftest` (design §8): the SDK self-test of one named template on this server's
//! backend, one run at a time per template, the last report kept per template and sandbox
//! user for `server.capabilities`.
//!
//! The sandboxes of a run use the calling peer's uid:gid, the user that peer's own sandboxes
//! get by default, so a root server tests the setup its clients get (not 0:0).
//!
//! The report never claims more than was shown. A self-test that cannot start (no read-write
//! mount root for its scratch directory, the template is missing or unpinned, the backend
//! refused) is a report with `ok: false` that says isolation is not proven and why. A run
//! that passes [`SELFTEST_DEADLINE`] is inconclusive, never ok.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_sdk::UserSpec;
use sandcrab_sdk::protocol::ProtocolError;
use sandcrab_sdk::protocol::api::{ApiError, SelftestParams, SelftestReport, SelftestSummary};
use sandcrab_sdk::selftest::{self, SelftestConfig};

use crate::Shared;
use crate::peer::Peer;

/// A whole run (two sandboxes, fifteen probes of at most 5 s each, the teardown) must end
/// within this, or it is reported inconclusive.
pub(crate) const SELFTEST_DEADLINE: Duration = Duration::from_secs(180);

/// A report and when it was made.
#[derive(Clone)]
struct Kept {
    report: SelftestReport,
    at: Instant,
}

/// A report is about a template run as one sandbox user: another user may get another answer
/// (what it may write through a mount, its capabilities).
type Key = (String, UserSpec);

/// Per template: a lock that serialises runs. Per template and user: the last report.
#[derive(Default)]
pub(crate) struct Selftests {
    running: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    last: Mutex<HashMap<Key, Kept>>,
}

impl Selftests {
    fn lock_of(&self, template: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(m.entry(template.to_owned()).or_default())
    }

    fn last(&self, template: &str, user: UserSpec) -> Option<Kept> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(template.to_owned(), user))
            .cloned()
    }

    /// The last report of `template` for `peer`'s sandbox user, in short; `None` when it never
    /// ran for that user.
    pub(crate) fn summary(&self, template: &str, peer: Peer) -> Option<SelftestSummary> {
        self.last(template, user_of(peer)).map(|k| SelftestSummary {
            template: k.report.template,
            ok: k.report.ok,
            reason: k.report.reason,
            age_secs: k.at.elapsed().as_secs(),
        })
    }
}

/// The sandbox user a peer's sandboxes get when the request names none (as `sandbox.create`).
fn user_of(peer: Peer) -> UserSpec {
    UserSpec::new(peer.uid, peer.gid)
}

/// `server.selftest`: the template named (an empty name means `default_template`), as the
/// peer's uid:gid.
pub(crate) async fn call(
    shared: &Shared,
    peer: Peer,
    p: SelftestParams,
) -> Result<SelftestReport, ApiError> {
    let template = if p.template.is_empty() {
        shared.config.default_template.clone().ok_or_else(|| {
            ApiError::Protocol(ProtocolError::InvalidParams(
                "`template` is empty and the server has no `default_template`".into(),
            ))
        })?
    } else {
        p.template
    };
    Ok(run(
        shared,
        &template,
        user_of(peer),
        p.max_age_secs.map(Duration::from_secs),
    )
    .await)
}

/// Run the self-test of `template` as `user`, or return the last report for that user when
/// it is younger than `max_age`. A caller that waited for another run of the same template
/// and user gets that run's report when `max_age` allows.
pub(crate) async fn run(
    shared: &Shared,
    template: &str,
    user: UserSpec,
    max_age: Option<Duration>,
) -> SelftestReport {
    let lock = shared.selftests.lock_of(template);
    let _one_at_a_time = lock.lock().await;
    if let Some(max) = max_age
        && let Some(k) = shared.selftests.last(template, user)
        && k.at.elapsed() <= max
    {
        return k.report;
    }

    let started_at = chrono::Utc::now();
    let start = Instant::now();
    if (user.uid == 0 || user.gid == 0) && !shared.config.allow_root_user {
        // The peer could not create such a sandbox either; nothing to claim for it.
        return not_proven(
            shared,
            template,
            started_at,
            start.elapsed(),
            format!(
                "isolation not proven: sandbox user {user} has uid or gid 0, which needs \
                 `allow_root_user = true` in the server config"
            ),
        );
    }

    let mut config = SelftestConfig::new(template);
    config.user = Some(user);
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

    tracing::info!(template, %user, "self-test started");
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
            (template.to_owned(), user),
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

/// `selftest_on_start`: run each in turn, in the background, as the user the expected client
/// gets: the server's own uid:gid, or on a root server the first admitted non-root uid with
/// its primary group. A failure only shows in the reports (and the log).
pub(crate) async fn on_start(shared: Arc<Shared>) {
    let user = start_user(&shared.allow_uids);
    for t in shared.config.selftest_on_start.clone() {
        let r = run(&shared, &t, user, None).await;
        if !r.ok {
            tracing::warn!(
                template = %t,
                reason = r.reason.as_deref().unwrap_or(""),
                "self-test at start: isolation not proven"
            );
        }
    }
}

/// The sandbox user of the self-tests at start (see [`on_start`]).
fn start_user(allow_uids: &[u32]) -> UserSpec {
    let own = sandcrab_sdk::core::private_dir::current_uid();
    if own != 0 {
        return UserSpec::new(own, sandcrab_sdk::core::private_dir::current_gid());
    }
    match allow_uids.iter().copied().find(|u| *u != 0) {
        Some(uid) => {
            let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
            UserSpec::new(uid, primary_gid(&passwd, uid).unwrap_or(uid))
        }
        None => UserSpec::new(0, 0),
    }
}

/// The primary gid of `uid` in a passwd file (`name:pw:uid:gid:...`).
fn primary_gid(passwd: &str, uid: u32) -> Option<u32> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 4 && f[2].parse() == Ok(uid))
            .then(|| f[3].parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_gid_reads_the_fourth_field() {
        let passwd =
            "root:x:0:0:root:/root:/bin/sh\n# comment\ndev:x:1500:1600::/home/dev:/bin/sh\n";
        assert_eq!(primary_gid(passwd, 1500), Some(1600));
        assert_eq!(primary_gid(passwd, 0), Some(0));
        assert_eq!(primary_gid(passwd, 7), None);
    }

    #[test]
    fn the_cache_is_per_user() {
        let s = Selftests::default();
        let report = SelftestReport {
            ok: true,
            reason: None,
            backend: "fake".into(),
            template: "base".into(),
            started_at: String::new(),
            duration_ms: 0,
            lines: Vec::new(),
        };
        s.last.lock().unwrap().insert(
            ("base".into(), UserSpec::new(1500, 1500)),
            Kept {
                report,
                at: Instant::now(),
            },
        );
        let peer = |uid| Peer { uid, gid: uid };
        assert!(s.summary("base", peer(1500)).is_some());
        assert!(s.summary("base", peer(1501)).is_none());
    }
}
