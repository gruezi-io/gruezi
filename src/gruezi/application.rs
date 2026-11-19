//! Application evidence is independent of election, authority, and address presence.
//! These values contain no executor or resource side effects.

use super::hooks::{LifecycleEvent, LifecycleKind, LifecycleOutcome};
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationPhase {
    Disabled {
        generation: u64,
    },
    Inactive {
        generation: u64,
    },
    Activating {
        generation: u64,
    },
    Ready {
        generation: u64,
        db_incarnation: Option<String>,
        verified_at: Instant,
        next_verify_at: Instant,
    },
    Deactivating {
        generation: u64,
    },
    Uncertain {
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriticalFailure {
    pub kind: LifecycleKind,
    pub generation: u64,
    pub outcome: LifecycleOutcome,
    pub elapsed: Duration,
    pub exit_code_or_signal: Option<i32>,
    /// The executor supplies a bounded, sanitized tail.
    pub stderr_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationOperation {
    pub kind: LifecycleKind,
    pub generation: u64,
    pub submitted_at: Instant,
    pub deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationState {
    pub phase: ApplicationPhase,
    pub operation: Option<ApplicationOperation>,
    /// Successful operations never erase an earlier critical failure.
    pub last_critical_failure: Option<CriticalFailure>,
}

impl ApplicationState {
    #[must_use]
    pub const fn disabled(generation: u64) -> Self {
        Self {
            phase: ApplicationPhase::Disabled { generation },
            operation: None,
            last_critical_failure: None,
        }
    }

    #[must_use]
    pub const fn configured(&self) -> bool {
        !matches!(self.phase, ApplicationPhase::Disabled { .. })
    }

    #[must_use]
    pub const fn passive_confirmed(&self, current_generation: u64) -> bool {
        match self.phase {
            ApplicationPhase::Disabled { .. } => true,
            ApplicationPhase::Inactive { generation } => generation == current_generation,
            _ => false,
        }
    }

    #[must_use]
    pub const fn activation_confirmed(&self, current_generation: u64) -> bool {
        matches!(self.phase, ApplicationPhase::Ready { generation, .. } if generation == current_generation)
    }

    #[must_use]
    pub fn ready(&self, current_generation: u64, now: Instant, timeout: Option<Duration>) -> bool {
        match (&self.phase, timeout) {
            (ApplicationPhase::Disabled { .. }, _) => true,
            (
                ApplicationPhase::Ready {
                    generation,
                    next_verify_at,
                    ..
                },
                Some(timeout),
            ) => *generation == current_generation && now < *next_verify_at + timeout,
            _ => false,
        }
    }

    pub(crate) fn record_failure(&mut self, event: &LifecycleEvent) {
        self.last_critical_failure = Some(CriticalFailure {
            kind: event.kind,
            generation: event.generation,
            outcome: event.outcome,
            elapsed: event.elapsed,
            exit_code_or_signal: event.exit_code_or_signal,
            stderr_tail: event.stderr_tail.clone(),
        });
    }
}
