//! `sandbox.destroyed` for sandboxes the server did not destroy on request (design §5.3).
//!
//! The manager's TTL reaper destroys expired sandboxes on its own and has no hook. The
//! server therefore looks: every [`WATCH_INTERVAL`] (and on every `sandbox.list`) it records
//! each live sandbox's expiry, and a sandbox it owns that is gone after its recorded expiry
//! is announced with reason `ttl` to its owner's control connections. A sandbox that is gone
//! before its expiry was destroyed on request (that path announces it itself) and is
//! forgotten silently.
//!
//! Reason `backend_exit` is never sent: the core keeps a sandbox whose container or VM died
//! until it is destroyed (TTL, request or stop), so there is nothing to observe yet.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_sdk::SandboxId;
use sandcrab_sdk::protocol::api::{self, DestroyReason, SandboxDestroyed};

use crate::Shared;

/// How often the server looks for sandboxes the TTL reaper destroyed.
pub(crate) const WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// A sandbox gone this close to its recorded expiry counts as expired: the expiry is
/// recorded from a duration, so it can be off by a little.
const SLACK: Duration = Duration::from_secs(1);

/// The last seen expiry of every live sandbox.
#[derive(Default)]
pub(crate) struct Expiries(Mutex<HashMap<SandboxId, Instant>>);

impl Expiries {
    /// Record the live sandboxes' expiries; forget owners of sandboxes that are gone, and
    /// announce those whose expiry had passed.
    pub(crate) fn sweep(&self, shared: &Shared) {
        // One sweep at a time (the watcher and `sandbox.list`), so an expiry is seen by the
        // sweep that forgets the owner. Owners first: a sandbox owned before the list is in
        // it unless it is gone (one created during the sweep is not looked at).
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let owned = shared.owned_ids();
        let now = Instant::now();
        let live = shared.sandcrab.list_sandboxes();
        let live_ids: HashSet<SandboxId> = live.iter().map(|i| i.id).collect();
        for i in &live {
            match i.expires_in {
                Some(d) => {
                    seen.insert(i.id, now + d);
                }
                None => {
                    seen.remove(&i.id);
                }
            }
        }
        let expired: Vec<SandboxId> = seen
            .iter()
            .filter(|(id, at)| !live_ids.contains(id) && **at <= now + SLACK)
            .map(|(id, _)| *id)
            .collect();
        seen.retain(|id, _| live_ids.contains(id));
        let gone: Vec<SandboxId> = owned
            .into_iter()
            .filter(|id| !live_ids.contains(id))
            .collect();
        for (id, uid) in shared.take_owners(&gone) {
            shared.hub.forget(id);
            if expired.contains(&id) {
                tracing::info!(uid, sandbox = %id, "sandbox expired");
                shared.hub.notify(
                    Some(uid),
                    api::NOTIFY_SANDBOX_DESTROYED,
                    &SandboxDestroyed {
                        sandbox_id: id.to_string(),
                        reason: DestroyReason::Ttl,
                    },
                );
            }
        }
    }
}

/// Sweep every [`WATCH_INTERVAL`] until the task is aborted.
pub(crate) async fn watch(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(WATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        shared.expiries.sweep(&shared);
    }
}
