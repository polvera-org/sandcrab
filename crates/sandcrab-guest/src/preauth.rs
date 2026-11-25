//! Pre-authentication budget shared by `crabd serve` and `crabd relay`.
//!
//! A connection that has not authenticated yet must not hold one of the server's connection
//! slots: an unauthenticated peer could otherwise open `max_connections` idle connections and
//! starve the real client until they time out. So fresh connections are tracked in a small,
//! separate [`PendingQueue`] instead, with a short deadline, and only take a real slot once
//! they have authenticated.
//!
//! When the queue is full, a new connection **evicts the oldest pending one** rather than
//! being refused. The real host sends its `auth` frame in its first write and is through in
//! well under a millisecond on a local socket, so to keep it out an attacker would have to
//! open more than [`PendingQueue::capacity`] connections inside that window, every time.
//! Refusing new connections instead would let a flood that refills the queue keep the host
//! out for good. Memory and fds stay bounded either way: pending connections are capped by
//! the queue and authenticated ones by the caller's slots.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Default number of connections that may be authenticating at once.
pub const DEFAULT_PENDING: usize = 16;
/// Default time a connection may stay unauthenticated.
pub const DEFAULT_PENDING_TIMEOUT: Duration = Duration::from_secs(2);

/// Bounded set of not-yet-authenticated connections (see the module docs).
#[derive(Debug)]
pub struct PendingQueue {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    /// Oldest first. Dropping a sender evicts that connection.
    pending: VecDeque<(u64, oneshot::Sender<()>)>,
}

/// Why a pending connection ended before its work finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingEnd {
    /// A newer connection needed the slot.
    Evicted,
    /// The deadline passed.
    TimedOut,
}

impl PendingQueue {
    /// # Panics
    /// If `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        assert!(capacity > 0, "pending capacity must be at least 1");
        Arc::new(Self {
            capacity,
            inner: Mutex::new(Inner::default()),
        })
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Connections pending right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Register a fresh connection, evicting the oldest pending one when full. Returns `true`
    /// in the second field when a connection was evicted.
    pub fn admit(self: &Arc<Self>) -> (PendingTicket, bool) {
        let (tx, rx) = oneshot::channel();
        let mut g = self.lock();
        let mut evicted = false;
        while g.pending.len() >= self.capacity {
            // Dropping the sender wakes that connection's `run` with `Evicted`.
            g.pending.pop_front();
            evicted = true;
        }
        let id = g.next_id;
        g.next_id += 1;
        g.pending.push_back((id, tx));
        drop(g);
        (
            PendingTicket {
                id,
                queue: Arc::clone(self),
                evicted: rx,
            },
            evicted,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One connection's place in a [`PendingQueue`]; leaves the queue when dropped.
#[derive(Debug)]
pub struct PendingTicket {
    id: u64,
    queue: Arc<PendingQueue>,
    evicted: oneshot::Receiver<()>,
}

impl PendingTicket {
    /// Run `fut` (the authentication) unless the connection is evicted or `timeout` passes
    /// first. The ticket should be dropped right after, to free its place.
    pub async fn run<F: Future>(
        &mut self,
        timeout: Duration,
        fut: F,
    ) -> Result<F::Output, PendingEnd> {
        tokio::select! {
            biased;
            _ = &mut self.evicted => Err(PendingEnd::Evicted),
            r = tokio::time::timeout(timeout, fut) => r.map_err(|_| PendingEnd::TimedOut),
        }
    }
}

impl Drop for PendingTicket {
    fn drop(&mut self) {
        let mut g = self.queue.lock();
        if let Some(i) = g.pending.iter().position(|(id, _)| *id == self.id) {
            g.pending.remove(i);
        }
    }
}

/// `warn!` at most once per `every`, counting what was suppressed in between (`debug!` for
/// those). Keeps a flood of refused connections from flooding the log too.
#[derive(Debug)]
pub struct RateLimitedLog {
    every: Duration,
    last: Mutex<Option<Instant>>,
    suppressed: AtomicU64,
}

impl RateLimitedLog {
    #[must_use]
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            last: Mutex::new(None),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Log `msg` about `peer` (rate limited). Returns whether it was logged at `warn`.
    pub fn warn(&self, peer: &str, msg: &str) -> bool {
        let now = Instant::now();
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| now.duration_since(t) < self.every) {
            drop(last);
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, "{msg}");
            return false;
        }
        *last = Some(now);
        drop(last);
        let suppressed = self.suppressed.swap(0, Ordering::Relaxed);
        warn!(%peer, suppressed, "{msg}");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_queue_evicts_the_oldest() {
        let q = PendingQueue::new(2);
        let (mut a, ev) = q.admit();
        assert!(!ev);
        let (_b, ev) = q.admit();
        assert!(!ev);
        assert_eq!(q.len(), 2);
        let (_c, ev) = q.admit();
        assert!(ev, "third admission evicts");
        assert_eq!(q.len(), 2);
        let r = a
            .run(Duration::from_secs(5), std::future::pending::<()>())
            .await;
        assert_eq!(r, Err(PendingEnd::Evicted));
    }

    #[tokio::test]
    async fn finished_tickets_leave_the_queue() {
        let q = PendingQueue::new(2);
        let (mut a, _) = q.admit();
        assert_eq!(a.run(Duration::from_secs(1), async { 7 }).await, Ok(7));
        drop(a);
        assert!(q.is_empty());
        let (mut b, _) = q.admit();
        let r = b
            .run(Duration::from_millis(20), std::future::pending::<()>())
            .await;
        assert_eq!(r, Err(PendingEnd::TimedOut));
    }

    #[test]
    fn log_is_rate_limited() {
        let l = RateLimitedLog::new(Duration::from_secs(3600));
        assert!(l.warn("p", "refused"));
        assert!(!l.warn("p", "refused"));
        assert_eq!(l.suppressed.load(Ordering::Relaxed), 1);
    }
}
