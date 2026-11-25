//! Lifecycle timing hooks feeding the benchmark (`docs/plans/phase-1-mvp.bench-spec.md`).
//!
//! The manager creates a [`LifecycleTimings`] at `spawn_requested` (bench `spawn_ready` t0,
//! just before `RuntimeBackend::spawn`) and hands a clone to the backend in the
//! `ResolvedSpec`. Backends mark `backend_started` and any backend-specific sub-phases
//! (`rootfs_ready`, `ws_attach_done`, `instance_start`, ...) with [`LifecycleTimings::mark_custom`].
//! The manager marks `ready` (first `hello` parsed), `first_call` (first tool response),
//! and `teardown_started` / `teardown_finished`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Standard lifecycle points shared by every backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    /// The backend has started the sandbox (container started / VMM `InstanceStart`).
    BackendStarted,
    /// First successful `hello` response parsed (bench `spawn_ready` t1).
    Ready,
    /// First tool-call response received (bench `first_call` t1).
    FirstCall,
    /// `destroy` entered (bench `teardown` t0).
    TeardownStarted,
    /// Backend reports all resources released (bench `teardown` t1).
    TeardownFinished,
}

/// Raw monotonic instants. Each standard phase records its **first** mark only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingMarks {
    pub spawn_requested: Instant,
    pub backend_started: Option<Instant>,
    pub ready: Option<Instant>,
    pub first_call: Option<Instant>,
    pub teardown_started: Option<Instant>,
    pub teardown_finished: Option<Instant>,
    /// Backend-specific sub-phases, in mark order.
    pub custom: Vec<(String, Instant)>,
}

impl TimingMarks {
    fn get(&self, phase: LifecyclePhase) -> Option<Instant> {
        match phase {
            LifecyclePhase::BackendStarted => self.backend_started,
            LifecyclePhase::Ready => self.ready,
            LifecyclePhase::FirstCall => self.first_call,
            LifecyclePhase::TeardownStarted => self.teardown_started,
            LifecyclePhase::TeardownFinished => self.teardown_finished,
        }
    }

    fn slot(&mut self, phase: LifecyclePhase) -> &mut Option<Instant> {
        match phase {
            LifecyclePhase::BackendStarted => &mut self.backend_started,
            LifecyclePhase::Ready => &mut self.ready,
            LifecyclePhase::FirstCall => &mut self.first_call,
            LifecyclePhase::TeardownStarted => &mut self.teardown_started,
            LifecyclePhase::TeardownFinished => &mut self.teardown_finished,
        }
    }

    /// `spawn_requested` → `ready`.
    #[must_use]
    pub fn spawn_ready(&self) -> Option<Duration> {
        self.ready.map(|t| t - self.spawn_requested)
    }

    /// `ready` → first tool response.
    #[must_use]
    pub fn first_call(&self) -> Option<Duration> {
        Some(self.first_call? - self.ready?)
    }

    /// `teardown_started` → `teardown_finished`.
    #[must_use]
    pub fn teardown(&self) -> Option<Duration> {
        Some(self.teardown_finished? - self.teardown_started?)
    }

    /// Serializable offsets from `spawn_requested`.
    #[must_use]
    pub fn report(&self) -> TimingReport {
        let off = |t: Option<Instant>| {
            t.map(|t| u64::try_from((t - self.spawn_requested).as_nanos()).unwrap_or(u64::MAX))
        };
        TimingReport {
            backend_started_ns: off(self.backend_started),
            ready_ns: off(self.ready),
            first_call_ns: off(self.first_call),
            teardown_started_ns: off(self.teardown_started),
            teardown_finished_ns: off(self.teardown_finished),
            custom: self
                .custom
                .iter()
                .map(|(name, t)| PhaseOffset {
                    name: name.clone(),
                    ns: off(Some(*t)).unwrap_or_default(),
                })
                .collect(),
        }
    }
}

/// A backend-specific phase offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseOffset {
    pub name: String,
    pub ns: u64,
}

/// [`TimingMarks`] as nanosecond offsets from `spawn_requested` (bench JSONL friendly).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingReport {
    pub backend_started_ns: Option<u64>,
    pub ready_ns: Option<u64>,
    pub first_call_ns: Option<u64>,
    pub teardown_started_ns: Option<u64>,
    pub teardown_finished_ns: Option<u64>,
    pub custom: Vec<PhaseOffset>,
}

/// Shared, thread-safe timing recorder. Clones share the same marks.
#[derive(Debug, Clone)]
pub struct LifecycleTimings(Arc<Mutex<TimingMarks>>);

impl LifecycleTimings {
    /// Start recording with `spawn_requested = now`.
    #[must_use]
    pub fn start() -> Self {
        Self::starting_at(Instant::now())
    }

    #[must_use]
    pub fn starting_at(spawn_requested: Instant) -> Self {
        Self(Arc::new(Mutex::new(TimingMarks {
            spawn_requested,
            backend_started: None,
            ready: None,
            first_call: None,
            teardown_started: None,
            teardown_finished: None,
            custom: Vec::new(),
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut TimingMarks) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Mark `phase` now (no-op if already marked). Returns the recorded instant.
    pub fn mark(&self, phase: LifecyclePhase) -> Instant {
        self.mark_at(phase, Instant::now())
    }

    /// Mark `phase` at `at` (no-op if already marked). Returns the recorded instant.
    pub fn mark_at(&self, phase: LifecyclePhase, at: Instant) -> Instant {
        self.with(|m| *m.slot(phase).get_or_insert(at))
    }

    /// Record a backend-specific sub-phase now.
    pub fn mark_custom(&self, name: impl Into<String>) -> Instant {
        let now = Instant::now();
        self.with(|m| m.custom.push((name.into(), now)));
        now
    }

    #[must_use]
    pub fn get(&self, phase: LifecyclePhase) -> Option<Instant> {
        self.with(|m| m.get(phase))
    }

    #[must_use]
    pub fn snapshot(&self) -> TimingMarks {
        self.with(|m| m.clone())
    }

    #[must_use]
    pub fn report(&self) -> TimingReport {
        self.with(|m| m.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_mark_wins_and_durations() {
        let t0 = Instant::now();
        let t = LifecycleTimings::starting_at(t0);
        let clone = t.clone();
        t.mark_at(LifecyclePhase::Ready, t0 + Duration::from_millis(50));
        clone.mark_at(LifecyclePhase::Ready, t0 + Duration::from_millis(99));
        t.mark_at(LifecyclePhase::FirstCall, t0 + Duration::from_millis(60));
        t.mark_at(
            LifecyclePhase::TeardownStarted,
            t0 + Duration::from_millis(100),
        );
        t.mark_at(
            LifecyclePhase::TeardownFinished,
            t0 + Duration::from_millis(130),
        );
        t.mark_custom("rootfs_ready");
        let s = clone.snapshot();
        assert_eq!(s.spawn_ready(), Some(Duration::from_millis(50)));
        assert_eq!(s.first_call(), Some(Duration::from_millis(10)));
        assert_eq!(s.teardown(), Some(Duration::from_millis(30)));
        let r = s.report();
        assert_eq!(r.ready_ns, Some(50_000_000));
        assert_eq!(r.backend_started_ns, None);
        assert_eq!(r.custom[0].name, "rootfs_ready");
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["teardown_finished_ns"], 130_000_000);
    }
}
