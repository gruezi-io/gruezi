//! Test-only job runner for the deterministic runtime matrix (design spec 8a).
//!
//! The real executor queue, coalescing, cancellation and generation rules stay
//! in force; only the subprocess is replaced by a scripted step that consumes
//! virtual time. Paused-clock tests cannot supervise real children: tokio would
//! auto-advance past every process deadline while the child is still running.

use super::{Job, LifecycleEvent, LifecycleKind, LifecycleOperation, LifecycleOutcome, Settings};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::{
    sync::watch,
    time::{Duration, Instant, sleep, sleep_until},
};

/// What one scripted job does once started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Exit(i32),
    Delay(Duration, i32),
    Hang,
    SpawnError,
}

/// One observed job, with its virtual start and end, for overlap and order checks.
#[derive(Debug, Clone)]
pub(crate) struct ScriptedCall {
    pub(crate) operation: LifecycleOperation,
    pub(crate) generation: u64,
    pub(crate) instance_id: String,
    pub(crate) transfer_id: Option<u64>,
    pub(crate) peer_nonce: Option<[u8; 16]>,
    pub(crate) deadline: Instant,
    pub(crate) started: Instant,
    pub(crate) finished: Option<Instant>,
    pub(crate) outcome: Option<LifecycleOutcome>,
}

pub(crate) struct ScriptedLifecycle {
    pub(crate) activate: Step,
    pub(crate) deactivate: Step,
    pub(crate) verify: Step,
    pub(crate) notify: Step,
    /// Reported by successful activate and verify jobs, as the adapter prints it.
    pub(crate) incarnation: String,
    pub(crate) calls: Vec<ScriptedCall>,
}

impl Default for ScriptedLifecycle {
    fn default() -> Self {
        Self {
            activate: Step::Exit(0),
            deactivate: Step::Exit(0),
            verify: Step::Exit(0),
            notify: Step::Exit(0),
            incarnation: "4242:1700000000".to_owned(),
            calls: Vec::new(),
        }
    }
}

impl ScriptedLifecycle {
    pub(crate) fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    const fn step(&self, kind: LifecycleKind) -> Step {
        match kind {
            LifecycleKind::Activate => self.activate,
            LifecycleKind::Deactivate => self.deactivate,
            LifecycleKind::Verify => self.verify,
            LifecycleKind::Notify(_) => self.notify,
        }
    }
}

/// Mirror `process::execute`: same deadline (including the Deactivate clamp),
/// cancellation and termination grace, and the same unconfigured-hook no-op.
pub(super) async fn execute(
    script: &Arc<Mutex<ScriptedLifecycle>>,
    settings: &Settings,
    job: &Job,
    cancel: &mut watch::Receiver<bool>,
    started: Instant,
) -> (LifecycleEvent, bool) {
    let mut event = job.event(LifecycleOutcome::Succeeded);
    event.started = Some(started);
    let mut deadline = started + Duration::from_millis(super::process::job_timeout(settings, job));
    if job.kind == LifecycleKind::Deactivate
        && let Some(withdraw_deadline) = job.context.deactivate_deadline
    {
        deadline = deadline.min(withdraw_deadline);
    }
    if Instant::now() >= deadline {
        event.outcome = LifecycleOutcome::TimedOut;
        return (event, true);
    }
    if *cancel.borrow() {
        event.outcome = LifecycleOutcome::Cancelled;
        return (event, true);
    }
    if let LifecycleKind::Notify(hook) = job.kind
        && settings.hooks.script_for(hook).is_none()
    {
        return (event, true);
    }
    let (step, incarnation, index) = {
        let mut script = script.lock().unwrap_or_else(PoisonError::into_inner);
        script.calls.push(ScriptedCall {
            operation: job.operation,
            generation: job.generation,
            instance_id: settings.instance_id.clone(),
            transfer_id: job.context.transfer_id,
            peer_nonce: job.context.peer_nonce,
            deadline,
            started,
            finished: None,
            outcome: None,
        });
        (
            script.step(job.kind),
            script.incarnation.clone(),
            script.calls.len() - 1,
        )
    };
    let grace = Duration::from_millis(super::CANCEL_GRACE_MS);
    let work = async {
        match step {
            Step::Exit(code) => Some(code),
            Step::Delay(delay, code) => {
                sleep(delay).await;
                Some(code)
            }
            Step::Hang => std::future::pending().await,
            Step::SpawnError => None,
        }
    };
    tokio::select! {
        biased;
        _ = cancel.changed() => {
            event.outcome = LifecycleOutcome::Cancelled;
            // SIGTERM, bounded grace, SIGKILL: the real termination cost.
            sleep(grace).await;
        }
        () = sleep_until(deadline) => {
            event.outcome = LifecycleOutcome::TimedOut;
            sleep(grace).await;
        }
        code = work => {
            if let Some(code) = code {
                event.exit_code_or_signal = Some(code);
                if code != 0 {
                    event.outcome = LifecycleOutcome::Failed;
                    event.stderr_tail = format!("scripted exit {code}");
                } else if matches!(job.kind, LifecycleKind::Activate | LifecycleKind::Verify) {
                    event.db_incarnation = Some(incarnation);
                }
            } else {
                event.outcome = LifecycleOutcome::SpawnError;
                "scripted spawn failure: No such file or directory".clone_into(&mut event.stderr_tail);
            }
        }
    }
    let mut script = script.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(call) = script.calls.get_mut(index) {
        call.finished = Some(Instant::now());
        call.outcome = Some(event.outcome);
    }
    (event, true)
}
