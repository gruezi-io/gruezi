//! Lifecycle-only serial execution. The legacy detached hook path is untouched.

use super::{HaHooks, HookContext, HookEvent};
use crate::config::LifecycleConfig;
pub use crate::gruezi::ownership::AuthoritySource;
use anyhow::{Context, Result, bail};
use std::{
    collections::VecDeque,
    ffi::OsString,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time::Instant,
};
use tracing::warn;

mod process;
use process::{Identities, execute};

pub const CANCEL_GRACE_MS: u64 = 250;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    Activate,
    Deactivate,
    Verify,
    Notify(HookEvent),
}

impl LifecycleKind {
    pub(super) const fn as_env_value(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Deactivate => "deactivate",
            Self::Verify => "verify",
            Self::Notify(event) => event.as_env_value(),
        }
    }

    const fn critical(self) -> bool {
        !matches!(self, Self::Notify(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationMode {
    Handoff,
    Cold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivationMode {
    Baseline,
    HandoffRelease,
    Revoke,
}

/// Submission carries the operation's semantics separately from its event kind.
/// Mutating operations cannot be constructed without an explicit mode, while
/// verification and notification cannot accidentally inherit a mutation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleOperation {
    Activate(ActivationMode),
    Deactivate(DeactivationMode),
    Verify,
    Notify(HookEvent),
}

impl LifecycleOperation {
    const fn kind(self) -> LifecycleKind {
        match self {
            Self::Activate(_) => LifecycleKind::Activate,
            Self::Deactivate(_) => LifecycleKind::Deactivate,
            Self::Verify => LifecycleKind::Verify,
            Self::Notify(event) => LifecycleKind::Notify(event),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LifecycleContext {
    pub authority_source: Option<AuthoritySource>,
    pub authority_term: u64,
    pub transfer_id: Option<u64>,
    pub peer_nonce: Option<[u8; 16]>,
    /// Absolute remaining withdrawal budget, supplied by the coordinator.
    /// Only Deactivate uses this cap; it can shorten, never extend, its timeout.
    pub deactivate_deadline: Option<Instant>,
    pub db_incarnation: Option<String>,
    pub notification: Option<HookContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleOutcome {
    Succeeded,
    Failed,
    TimedOut,
    SpawnError,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    pub kind: LifecycleKind,
    pub generation: u64,
    pub outcome: LifecycleOutcome,
    pub elapsed: Duration,
    /// Job start, before spawn. Verification cadence and freshness deadlines
    /// must be anchored here, never at the time the consumer sees completion.
    /// Queued jobs cancelled before execution have no start time.
    pub started: Option<Instant>,
    /// Exit code, or the negated terminating signal; absent on spawn failure.
    pub exit_code_or_signal: Option<i32>,
    pub stderr_tail: String,
    pub db_incarnation: Option<String>,
}

impl LifecycleEvent {
    /// State consumers must still record failures, but must never credit an old success.
    #[must_use]
    pub const fn is_current_success(&self, current_generation: u64) -> bool {
        self.generation == current_generation && matches!(self.outcome, LifecycleOutcome::Succeeded)
    }
}

struct Job {
    kind: LifecycleKind,
    operation: LifecycleOperation,
    generation: u64,
    context: LifecycleContext,
}

impl Job {
    fn event(&self, outcome: LifecycleOutcome) -> LifecycleEvent {
        LifecycleEvent {
            kind: self.kind,
            generation: self.generation,
            outcome,
            elapsed: Duration::ZERO,
            started: None,
            exit_code_or_signal: None,
            stderr_tail: String::new(),
            db_incarnation: None,
        }
    }
}

struct Running {
    kind: LifecycleKind,
    generation: u64,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct Queue {
    generation: u64,
    jobs: VecDeque<Job>,
    running: Option<Running>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Notify,
    events: mpsc::UnboundedSender<LifecycleEvent>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: LifecycleEvent) {
        if matches!(event.kind, LifecycleKind::Notify(_))
            && event.outcome != LifecycleOutcome::Succeeded
        {
            warn!(event = event.kind.as_env_value(), outcome = ?event.outcome,
                stderr = %event.stderr_tail, "HA hook execution failed");
        }
        // A dropped consumer does not change process cleanup or notification semantics.
        let _ = self.events.send(event);
    }

    fn close(&self) {
        let mut queue = self.queue();
        queue.closed = true;
        for job in queue.jobs.drain(..) {
            self.emit(job.event(LifecycleOutcome::Cancelled));
        }
        if let Some(running) = &queue.running {
            running.cancel.send_replace(true);
        }
        self.wake.notify_one();
    }
}

struct Settings {
    lifecycle: LifecycleConfig,
    hooks: HaHooks,
    node_id: String,
    group_id: String,
    instance_id: String,
    adapter_conf: Option<OsString>,
    identities: Identities,
}

/// A lifecycle deployment owns exactly one of these handles. Submission never
/// awaits a script, so address withdrawal and authority timers remain independent.
/// Dropping the handle cancels the worker; `shutdown` also waits for reaping.
pub struct LifecycleExecutor {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl LifecycleExecutor {
    /// Start the lifecycle-only executor, validating the packaged OS identities.
    /// The coordinator must use the existing detached hooks when lifecycle is absent.
    ///
    /// # Errors
    ///
    /// Fails closed if the OS identities are absent, privileged, aliased to each
    /// other or the daemon.
    ///
    /// `boot_nonce` must be the same boot nonce used by the wire sender, not a
    /// separately generated identity. Runtime integration supplies it once.
    pub fn spawn(
        lifecycle: LifecycleConfig,
        hooks: HaHooks,
        node_id: String,
        group_id: String,
        boot_nonce: [u8; 16],
    ) -> Result<(Self, mpsc::UnboundedReceiver<LifecycleEvent>)> {
        Ok(Self::start(
            lifecycle,
            hooks,
            node_id,
            group_id,
            boot_nonce,
            Identities::resolve()?,
        ))
    }

    #[cfg(test)]
    fn spawn_for_unit_tests(
        lifecycle: LifecycleConfig,
        hooks: HaHooks,
        node_id: String,
        group_id: String,
        boot_nonce: [u8; 16],
    ) -> (Self, mpsc::UnboundedReceiver<LifecycleEvent>) {
        Self::start(
            lifecycle,
            hooks,
            node_id,
            group_id,
            boot_nonce,
            Identities::for_unit_tests(),
        )
    }

    fn start(
        lifecycle: LifecycleConfig,
        hooks: HaHooks,
        node_id: String,
        group_id: String,
        boot_nonce: [u8; 16],
        identities: Identities,
    ) -> (Self, mpsc::UnboundedReceiver<LifecycleEvent>) {
        let settings = Settings {
            lifecycle,
            hooks,
            node_id,
            group_id,
            instance_id: nonce_hex(boot_nonce),
            adapter_conf: std::env::var_os("GRUEZI_MARIADB_ADAPTER_CONF"),
            identities,
        };
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Notify::new(),
            events,
        });
        let worker = Arc::clone(&shared);
        let task = tokio::spawn(async move { work(worker, settings).await });
        (Self { shared, task }, receiver)
    }

    /// Submit a generation-tagged job with an explicit operation mode.
    /// Returns false for an already obsolete
    /// submission. Verify is coalesced; critical jobs precede notifications.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown, or for a notification without its context.
    pub fn submit(
        &self,
        operation: LifecycleOperation,
        generation: u64,
        context: LifecycleContext,
    ) -> Result<bool> {
        let kind = operation.kind();
        if matches!(kind, LifecycleKind::Notify(_)) && context.notification.is_none() {
            bail!("notification requires hook context");
        }
        let mut queue = self.shared.queue();
        if queue.closed {
            bail!("lifecycle executor closed");
        }
        if generation < queue.generation {
            return Ok(false);
        }
        queue.generation = generation;
        let mut retained = VecDeque::new();
        for job in queue.jobs.drain(..) {
            let discard = job.generation != generation
                || (kind == LifecycleKind::Deactivate
                    && matches!(job.kind, LifecycleKind::Activate | LifecycleKind::Verify))
                || (kind == LifecycleKind::Verify && job.kind == LifecycleKind::Verify);
            if discard {
                self.shared.emit(job.event(LifecycleOutcome::Cancelled));
            } else {
                retained.push_back(job);
            }
        }
        queue.jobs = retained;
        if let Some(running) = &queue.running
            && (running.generation != generation
                || (kind == LifecycleKind::Deactivate
                    && matches!(
                        running.kind,
                        LifecycleKind::Activate | LifecycleKind::Verify
                    ))
                || (kind.critical() && !running.kind.critical()))
        {
            running.cancel.send_replace(true);
        }
        queue.jobs.push_back(Job {
            kind,
            operation,
            generation,
            context,
        });
        self.shared.wake.notify_one();
        Ok(true)
    }

    /// Cancel pending work and wait until the active process group is reaped.
    ///
    /// # Errors
    ///
    /// Returns an error if the executor task panics.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shared.close();
        (&mut self.task)
            .await
            .context("lifecycle executor task failed")
    }
}

impl Drop for LifecycleExecutor {
    fn drop(&mut self) {
        self.shared.close();
    }
}

async fn work(shared: Arc<Shared>, settings: Settings) {
    loop {
        let notified = shared.wake.notified();
        let next = {
            let mut queue = shared.queue();
            if queue.closed {
                return;
            }
            let index = queue
                .jobs
                .iter()
                .position(|job| job.kind.critical())
                .unwrap_or(0);
            queue.jobs.remove(index).map(|job| {
                let (cancel, receiver) = watch::channel(false);
                queue.running = Some(Running {
                    kind: job.kind,
                    generation: job.generation,
                    cancel,
                });
                (job, receiver)
            })
        };
        let Some((job, mut cancel)) = next else {
            notified.await;
            continue;
        };
        let started = Instant::now();
        let (mut event, quiescent) = execute(&settings, &job, &mut cancel, started).await;
        event.elapsed = started.elapsed();
        // Linearize completion with submit: even a just-exited child cannot be
        // credited after a caller synchronously revoked its generation.
        let mut queue = shared.queue();
        if *cancel.borrow() || job.generation != queue.generation {
            event.outcome = LifecycleOutcome::Cancelled;
            event.db_incarnation = None;
        }
        queue.running = None;
        shared.emit(event);
        if !quiescent {
            // Never start an opposing command after an unproven termination.
            queue.closed = true;
            for pending in queue.jobs.drain(..) {
                shared.emit(pending.event(LifecycleOutcome::Cancelled));
            }
        }
    }
}

fn nonce_hex(nonce: [u8; 16]) -> String {
    format!("{:032x}", u128::from_be_bytes(nonce))
}
