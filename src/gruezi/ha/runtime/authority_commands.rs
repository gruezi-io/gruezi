//! Bounded command work runs beside the coordinator, never across an authority
//! deadline. Dropping the `JoinSet` aborts every command; `kill_on_drop` then kills
//! any child process. No command result or fence proof survives a restart.

use super::super::config::HaRuntimeConfig;
use crate::gruezi::ownership::{FencingState, OwnershipController};
use std::{process::Stdio, time::Instant as StdInstant};
use tokio::{
    process::Command,
    task::JoinSet,
    time::{Duration, Instant, timeout},
};

#[derive(Clone, Default)]
pub(super) enum AuthorityCommandRunner {
    #[default]
    System,
    #[cfg(test)]
    Scripted(std::sync::Arc<std::sync::Mutex<ScriptedAuthority>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Check,
    Fence,
}

struct Completion {
    kind: Kind,
    started: Instant,
    peer_seen: Option<StdInstant>,
    success: bool,
}

pub(super) struct AuthorityCommands {
    runner: AuthorityCommandRunner,
    tasks: JoinSet<Completion>,
    next_check: Instant,
    next_fence: Instant,
    last_check_result: Option<Instant>,
    fencing: bool,
    stopped: bool,
}

impl AuthorityCommands {
    pub(super) fn new(runner: AuthorityCommandRunner) -> Self {
        Self {
            runner,
            tasks: JoinSet::new(),
            next_check: Instant::now(),
            next_fence: Instant::now(),
            last_check_result: None,
            fencing: false,
            stopped: false,
        }
    }

    pub(super) fn deadline(
        &self,
        runtime: &HaRuntimeConfig,
        ownership: &OwnershipController,
    ) -> Option<Instant> {
        runtime.authority.as_ref()?;
        if self.stopped {
            return None;
        }
        let check = Some(self.next_check);
        let fence = (!self.fencing && ownership.needs_fencing()).then_some(self.next_fence);
        check.into_iter().chain(fence).min()
    }

    pub(super) fn start_due(
        &mut self,
        runtime: &HaRuntimeConfig,
        ownership: &mut OwnershipController,
        peer_seen: Option<StdInstant>,
    ) {
        let Some(config) = &runtime.authority else {
            return;
        };
        if self.stopped {
            return;
        }
        let now = Instant::now();
        let interval =
            Duration::from_millis(config.interval_ms.unwrap_or(runtime.advert_interval_ms));
        if now >= self.next_check {
            self.next_check = now + interval;
            self.spawn(
                runtime,
                Kind::Check,
                config.check_command.clone(),
                peer_seen,
            );
        }
        if !self.fencing && now >= self.next_fence && ownership.needs_fencing() {
            ownership.set_fencing(FencingState::Requested);
            self.next_fence = now + interval;
            if let Some(command) = &config.fence_command {
                self.fencing = true;
                self.spawn(runtime, Kind::Fence, command.clone(), peer_seen);
            }
        }
    }

    pub(super) async fn stop(&mut self) {
        self.stopped = true;
        self.tasks.shutdown().await;
    }

    fn spawn(
        &mut self,
        runtime: &HaRuntimeConfig,
        kind: Kind,
        path: String,
        peer_seen: Option<StdInstant>,
    ) {
        let runner = self.runner.clone();
        let peer_id = (kind == Kind::Fence).then(|| runtime.peer_id.clone());
        let limit = Duration::from_millis(runtime.command_timeout_ms);
        // The lease starts here, before the command task is scheduled.
        let started = Instant::now();
        self.tasks.spawn(async move {
            let success = timeout(limit, runner.run(path, peer_id))
                .await
                .unwrap_or(false);
            Completion {
                kind,
                started,
                peer_seen,
                success,
            }
        });
    }

    pub(super) async fn complete(
        &mut self,
        runtime: &HaRuntimeConfig,
        ownership: &mut OwnershipController,
        peer_seen: Option<StdInstant>,
    ) {
        let Some(result) = self.tasks.join_next().await else {
            std::future::pending::<()>().await;
            return;
        };
        // A command task contains no fallible join operations or panicking
        // paths. Still fail closed if the executor cancels it unexpectedly.
        let Ok(result) = result else {
            ownership.observe_external_grant(None);
            ownership.set_fencing(FencingState::Requested);
            return;
        };
        match result.kind {
            Kind::Check => {
                // Checks start every interval, even when a prior invocation
                // is still bounded by its timeout. A late older result must
                // never replace a newer grant or a newer refusal.
                if self
                    .last_check_result
                    .is_some_and(|started| started > result.started)
                {
                    return;
                }
                self.last_check_result = Some(result.started);
                let expiry = runtime.authority.as_ref().and_then(|config| {
                    // Begin withdrawal early enough to spend its entire
                    // deadline, with the configured safety margin left over.
                    let margin = Duration::from_millis(runtime.withdraw_timeout_ms)
                        + Duration::from_millis(u64::from(runtime.watchdog.safety_margin_ms));
                    let lifetime =
                        Duration::from_millis(config.grant_ttl_ms).saturating_sub(margin);
                    result.success.then_some(result.started + lifetime)
                });
                ownership.observe_external_grant(expiry);
            }
            Kind::Fence => {
                self.fencing = false;
                if result.success && result.peer_seen == peer_seen && ownership.needs_fencing() {
                    ownership.set_fencing(FencingState::Confirmed);
                }
            }
        }
    }
}

impl AuthorityCommandRunner {
    async fn run(self, path: String, peer_id: Option<String>) -> bool {
        match self {
            Self::System => {
                let mut command = Command::new(path);
                if let Some(peer_id) = peer_id {
                    command.arg(peer_id);
                }
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .status()
                    .await
                    .is_ok_and(|status| status.success())
            }
            #[cfg(test)]
            Self::Scripted(script) => run_scripted(script, path, peer_id).await,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(super) enum ScriptedOutcome {
    Exit(bool),
    Delay(Duration, bool),
    Timeout,
}

#[cfg(test)]
pub(super) struct ScriptedAuthority {
    pub(super) check: ScriptedOutcome,
    pub(super) fence: ScriptedOutcome,
    pub(super) calls: Vec<(String, Option<String>, Instant)>,
    pub(super) fenced_kernel:
        Option<std::sync::Arc<std::sync::Mutex<crate::gruezi::addresses::SimulatedKernel>>>,
}

#[cfg(test)]
impl Default for ScriptedAuthority {
    fn default() -> Self {
        Self {
            check: ScriptedOutcome::Exit(false),
            fence: ScriptedOutcome::Exit(false),
            calls: Vec::new(),
            fenced_kernel: None,
        }
    }
}

#[cfg(test)]
async fn run_scripted(
    script: std::sync::Arc<std::sync::Mutex<ScriptedAuthority>>,
    path: String,
    peer_id: Option<String>,
) -> bool {
    let (outcome, kernel) = {
        let mut script = script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = if peer_id.is_some() {
            script.fence
        } else {
            script.check
        };
        script.calls.push((path, peer_id.clone(), Instant::now()));
        (outcome, script.fenced_kernel.clone())
    };
    let success = match outcome {
        ScriptedOutcome::Exit(success) => success,
        ScriptedOutcome::Delay(delay, success) => {
            tokio::time::sleep(delay).await;
            success
        }
        ScriptedOutcome::Timeout => std::future::pending().await,
    };
    if success
        && peer_id.is_some()
        && let Some(kernel) = kernel
    {
        kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .clear();
    }
    success
}

#[cfg(test)]
mod tests {
    use super::AuthorityCommandRunner;

    /// These are real, immediate processes with a running clock. Timer tests
    /// use the scripted runner and never execute a subprocess while paused.
    #[cfg(unix)]
    #[tokio::test]
    async fn external_system_commands_use_exit_status_and_missing_paths() {
        assert!(
            AuthorityCommandRunner::System
                .run("/bin/true".to_owned(), None)
                .await
        );
        assert!(
            !AuthorityCommandRunner::System
                .run("/bin/false".to_owned(), None)
                .await
        );
        assert!(
            !AuthorityCommandRunner::System
                .run("/nonexistent-gruezi-authority-command".to_owned(), None,)
                .await
        );
    }
}
