use crate::gruezi::{
    ha::HaDecisionReason,
    hooks::{HaHooks, HookContext, HookEvent},
};
use anyhow::{Context, Result, bail};
use std::net::IpAddr;
use tokio::process::Command;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressManager {
    pub ip_command: String,
    pub arping_command: String,
    pub ndsend_command: String,
    pub interface: String,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressAction {
    Add,
    Remove,
}

impl AddressAction {
    #[must_use]
    pub const fn ip_subcommand(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "del",
        }
    }
}

impl AddressManager {
    /// Apply the configured VIP addresses using the requested action.
    ///
    /// # Errors
    ///
    /// Returns an error if the address command fails with a non-ignorable error.
    pub async fn apply(&self, action: AddressAction) -> Result<()> {
        for address in &self.addresses {
            run_address_command(&self.ip_command, &self.interface, address, action).await?;
            if action == AddressAction::Add
                && let Err(error) = announce_address(
                    &self.arping_command,
                    &self.ndsend_command,
                    &self.interface,
                    address,
                )
                .await
            {
                warn!(%error, address, "HA neighbor announcement failed");
            }
        }

        Ok(())
    }
}

pub fn spawn_address_action(
    manager: AddressManager,
    action: AddressAction,
    fault_hook: Option<(HaHooks, HookContext)>,
) {
    tokio::spawn(async move {
        if let Err(error) = manager.apply(action).await {
            warn!(%error, action = action.ip_subcommand(), "HA address action failed");
            if let Some((hooks, mut context)) = fault_hook {
                context.reason = Some(HaDecisionReason::AddressActionFailed);
                if let Err(hook_error) = hooks.run(HookEvent::Fault, context).await {
                    warn!(%hook_error, "HA fault hook execution failed after address action failure");
                }
            }
        }
    });
}

async fn run_address_command(
    ip_command: &str,
    interface: &str,
    address: &str,
    action: AddressAction,
) -> Result<()> {
    if ip_command.trim().is_empty() {
        bail!("ip command path cannot be empty");
    }

    let output = Command::new(ip_command)
        .args(["address", action.ip_subcommand(), address, "dev", interface])
        .output()
        .await
        .with_context(|| format!("failed to execute {ip_command} for {address}"))?;

    if output.status.success() || is_ignorable_ip_error(action, &output.stderr) {
        info!(
            command = %ip_command,
            interface,
            address,
            action = action.ip_subcommand(),
            "HA address action applied"
        );
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "address action {ip_command} {} {address} failed: {stderr}",
        action.ip_subcommand()
    );
}

fn is_ignorable_ip_error(action: AddressAction, stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);

    match action {
        AddressAction::Add => stderr.contains("File exists"),
        AddressAction::Remove => stderr.contains("Cannot assign requested address"),
    }
}

async fn announce_address(
    arping_command: &str,
    ndsend_command: &str,
    interface: &str,
    address: &str,
) -> Result<()> {
    let ip = parse_address_ip(address)?;

    match ip {
        IpAddr::V4(ip) => {
            let ipv4 = ip.to_string();
            run_announcement_command(arping_command, ["-U", "-I", interface, "-c", "1", &ipv4])
                .await
        }
        IpAddr::V6(ip) => {
            let ipv6 = ip.to_string();
            run_announcement_command(ndsend_command, [&ipv6, interface]).await
        }
    }
}

async fn run_announcement_command<const N: usize>(command: &str, args: [&str; N]) -> Result<()> {
    if command.trim().is_empty() {
        bail!("announcement command path cannot be empty");
    }

    let output = Command::new(command)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to execute announcement command {command}"))?;

    if output.status.success() {
        info!(command, "HA neighbor announcement sent");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("announcement command {command} failed: {stderr}");
}

fn parse_address_ip(address: &str) -> Result<IpAddr> {
    let ip = address
        .split('/')
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid address {address}"))?;

    ip.parse::<IpAddr>()
        .with_context(|| format!("invalid IP address in {address}"))
}

#[cfg(test)]
mod tests {
    use super::{AddressAction, is_ignorable_ip_error, parse_address_ip};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn ignores_existing_address_on_add() {
        assert!(is_ignorable_ip_error(
            AddressAction::Add,
            b"RTNETLINK answers: File exists"
        ));
    }

    #[test]
    fn ignores_missing_address_on_remove() {
        assert!(is_ignorable_ip_error(
            AddressAction::Remove,
            b"RTNETLINK answers: Cannot assign requested address"
        ));
    }

    #[test]
    fn parses_ipv4_address_with_prefix() {
        let parsed = parse_address_ip("10.0.0.10/24");
        assert!(parsed.is_ok());
        assert_eq!(parsed.ok(), Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 10))));
    }

    #[test]
    fn parses_ipv6_address_with_prefix() {
        let parsed = parse_address_ip("fd00::10/64");
        assert!(parsed.is_ok());
        assert_eq!(
            parsed.ok(),
            Some(IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x10)))
        );
    }
}
