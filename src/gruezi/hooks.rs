use crate::gruezi::ha::{HaDecisionReason, HaState};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::{process::Command, time::timeout};
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaHooks {
    pub on_promote: Option<String>,
    pub on_demote: Option<String>,
    pub on_backup: Option<String>,
    pub on_fault: Option<String>,
    pub timeout_ms: u64,
}

impl Default for HaHooks {
    fn default() -> Self {
        Self {
            on_promote: None,
            on_demote: None,
            on_backup: None,
            on_fault: None,
            timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Promote,
    Demote,
    Backup,
    Fault,
}

impl HookEvent {
    #[must_use]
    pub const fn as_env_value(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Demote => "demote",
            Self::Backup => "backup",
            Self::Fault => "fault",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookContext {
    pub node_id: String,
    pub group_id: String,
    pub interface: String,
    pub state: HaState,
    pub previous_state: HaState,
    pub reason: Option<HaDecisionReason>,
    pub priority: u8,
    pub peer_id: Option<String>,
    pub peer_state: Option<HaState>,
    pub peer_priority: Option<u8>,
    pub last_peer_seen_ms_ago: Option<u64>,
}

impl HaHooks {
    #[must_use]
    pub fn script_for(&self, event: HookEvent) -> Option<&str> {
        match event {
            HookEvent::Promote => self.on_promote.as_deref(),
            HookEvent::Demote => self.on_demote.as_deref(),
            HookEvent::Backup => self.on_backup.as_deref(),
            HookEvent::Fault => self.on_fault.as_deref(),
        }
    }

    /// Execute the configured hook for a state transition if one exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the hook process fails, times out, or exits unsuccessfully.
    pub async fn run(&self, event: HookEvent, context: HookContext) -> Result<()> {
        let Some(script) = self.script_for(event) else {
            return Ok(());
        };

        let output = timeout(
            Duration::from_millis(self.timeout_ms),
            hook_command(script, event, &context)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .with_context(|| format!("hook {script} timed out after {}ms", self.timeout_ms))?
        .with_context(|| format!("failed to execute hook {script}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("hook {script} failed with {}: {stderr}", output.status);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        info!(hook = script, event = event.as_env_value(), output = %stdout, "HA hook executed");

        Ok(())
    }
}

pub fn spawn_hook(hooks: HaHooks, event: HookEvent, context: HookContext) {
    tokio::spawn(async move {
        if let Err(error) = hooks.run(event, context).await {
            warn!(%error, event = event.as_env_value(), "HA hook execution failed");
        }
    });
}

fn hook_command(script: &str, event: HookEvent, context: &HookContext) -> Command {
    let mut command = Command::new(script);
    command
        .env("GRUEZI_EVENT", event.as_env_value())
        .env("GRUEZI_NODE_ID", &context.node_id)
        .env("GRUEZI_GROUP_ID", &context.group_id)
        .env("GRUEZI_INTERFACE", &context.interface)
        .env("GRUEZI_PRIORITY", context.priority.to_string())
        .env(
            "GRUEZI_STATE",
            format!("{:?}", context.state).to_ascii_uppercase(),
        )
        .env(
            "GRUEZI_PREVIOUS_STATE",
            format!("{:?}", context.previous_state).to_ascii_uppercase(),
        );

    if let Some(reason) = context.reason {
        command.env("GRUEZI_REASON", reason.as_str().to_ascii_uppercase());
    }

    if let Some(peer_id) = &context.peer_id {
        command.env("GRUEZI_PEER_ID", peer_id);
    }

    if let Some(peer_state) = context.peer_state {
        command.env(
            "GRUEZI_PEER_STATE",
            format!("{peer_state:?}").to_ascii_uppercase(),
        );
    }

    if let Some(peer_priority) = context.peer_priority {
        command.env("GRUEZI_PEER_PRIORITY", peer_priority.to_string());
    }

    if let Some(last_peer_seen_ms_ago) = context.last_peer_seen_ms_ago {
        command.env(
            "GRUEZI_LAST_PEER_SEEN_MS",
            last_peer_seen_ms_ago.to_string(),
        );
    }

    command
}
