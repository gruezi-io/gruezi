use crate::gruezi::{
    ha::HaDecisionReason,
    hooks::{HaHooks, HookContext, HookEvent},
};
use anyhow::{Context, Result, bail};
use std::net::IpAddr;
#[cfg(target_os = "linux")]
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::warn;

const COMMAND_TIMEOUT_MS: u64 = 3000;

enum CommandRunner {
    System,
    #[cfg(test)]
    Scripted {
        results: std::collections::VecDeque<Option<Result<String>>>,
        calls: Vec<Vec<String>>,
    },
}

impl CommandRunner {
    async fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
        if command.trim().is_empty() {
            bail!("command path cannot be empty");
        }
        timeout(Duration::from_millis(COMMAND_TIMEOUT_MS), async {
            match self {
                Self::System => {
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = args;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "HA address commands require Linux",
                        )
                        .into())
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let output = Command::new(command)
                            .args(args)
                            .kill_on_drop(true)
                            .output()
                            .await
                            .with_context(|| format!("failed to execute {command}"))?;
                        if !output.status.success() {
                            bail!(
                                "{command} {args:?} failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                        }
                        String::from_utf8(output.stdout).context("command output is not UTF-8")
                    }
                }
                #[cfg(test)]
                Self::Scripted { results, calls } => {
                    calls.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                    match results.pop_front().context("script exhausted")? {
                        Some(result) => result,
                        None => std::future::pending().await,
                    }
                }
            }
        })
        .await
        .with_context(|| format!("{command} {args:?} timed out after {COMMAND_TIMEOUT_MS} ms"))?
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AddressObservation {
    interface: String,
    ip: IpAddr,
    prefix: u8,
    ready: bool,
}

impl AddressObservation {
    fn parse(output: &str) -> Result<Vec<Self>> {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                let interface = fields
                    .get(1)
                    .context("missing address interface")?
                    .trim_end_matches(':');
                let family = fields.get(2).context("missing address family")?;
                if !matches!(*family, "inet" | "inet6") {
                    bail!("unsupported address family {family}");
                }
                let (ip, prefix) = parse_address(fields.get(3).context("missing address")?)?;
                let ready = ip.is_ipv4()
                    || !(fields.contains(&"tentative") || fields.contains(&"dadfailed"));
                Ok(Self {
                    interface: interface.to_owned(),
                    ip,
                    prefix,
                    ready,
                })
            })
            .collect()
    }
}

fn parse_address(address: &str) -> Result<(IpAddr, u8)> {
    let (ip, prefix) = address
        .split_once('/')
        .context("address requires a prefix")?;
    let ip: IpAddr = ip.parse().context("invalid address IP")?;
    let prefix: u8 = prefix.parse().context("invalid address prefix")?;
    if prefix > if ip.is_ipv4() { 32 } else { 128 } {
        bail!("address prefix out of range");
    }
    Ok((ip, prefix))
}

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
    /// Returns aggregated command diagnostics if read-back cannot confirm the
    /// requested state. Announcement failures are logged separately.
    pub async fn apply(&self, action: AddressAction) -> Result<()> {
        self.apply_with_runner(action, &mut CommandRunner::System)
            .await
    }

    async fn apply_with_runner(
        &self,
        action: AddressAction,
        runner: &mut CommandRunner,
    ) -> Result<()> {
        if self.addresses.is_empty() {
            return Ok(());
        }
        let mut errors = Vec::new();
        for address in &self.addresses {
            if let Err(error) = runner
                .run(
                    &self.ip_command,
                    &[
                        "address",
                        action.ip_subcommand(),
                        address,
                        "dev",
                        &self.interface,
                    ],
                )
                .await
            {
                errors.push(format!("{error:#}"));
            }
        }
        let observation = runner
            .run(
                &self.ip_command,
                &["-o", "address", "show", "dev", &self.interface],
            )
            .await
            .and_then(|output| AddressObservation::parse(&output));
        let mut confirmed = true;
        match observation {
            Ok(observed) => {
                for address in &self.addresses {
                    let correct = match parse_address(address) {
                        Ok((ip, prefix)) => {
                            let mut entries = observed.iter().filter(|entry| {
                                entry.interface == self.interface
                                    && entry.ip == ip
                                    && entry.prefix == prefix
                            });
                            match action {
                                AddressAction::Add => entries.any(|entry| entry.ready),
                                AddressAction::Remove => entries.next().is_none(),
                            }
                        }
                        Err(error) => {
                            errors.push(format!("{address}: {error:#}"));
                            false
                        }
                    };
                    if !correct {
                        confirmed = false;
                        errors.push(format!(
                            "read-back did not confirm {} {address} on {}",
                            action.ip_subcommand(),
                            self.interface
                        ));
                    }
                }
            }
            Err(error) => {
                confirmed = false;
                errors.push(format!("read-back failed: {error:#}"));
            }
        }
        if !confirmed {
            bail!("{}", errors.join("; "));
        }
        // A duplicate add or missing delete can fail even when the desired
        // state already holds. Only read-back decides whether it holds.
        for error in errors {
            warn!(%error, "HA address command failed but read-back confirmed state");
        }
        if action == AddressAction::Add {
            for address in &self.addresses {
                if let Err(error) = announce_address(
                    runner,
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

async fn announce_address(
    runner: &mut CommandRunner,
    arping_command: &str,
    ndsend_command: &str,
    interface: &str,
    address: &str,
) -> Result<()> {
    let ip = parse_address_ip(address)?;

    match ip {
        IpAddr::V4(ip) => {
            let ipv4 = ip.to_string();
            runner
                .run(arping_command, &["-U", "-I", interface, "-c", "1", &ipv4])
                .await
                .map(|_| ())
        }
        IpAddr::V6(ip) => {
            let ipv6 = ip.to_string();
            runner
                .run(ndsend_command, &[&ipv6, interface])
                .await
                .map(|_| ())
        }
    }
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
    use super::{
        AddressAction, AddressManager, AddressObservation, COMMAND_TIMEOUT_MS, CommandRunner,
        parse_address_ip,
    };
    use anyhow::{Context, Result, anyhow};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn rejects_tentative_and_dadfailed_ipv6_presence() -> Result<()> {
        for flag in ["tentative", "dadfailed"] {
            let observations = AddressObservation::parse(&format!(
                "2: eth0 inet6 fd00::10/64 scope global {flag}"
            ))?;
            assert!(observations.iter().all(|entry| !entry.ready));
        }
        Ok(())
    }

    #[test]
    fn parses_ipv4_and_ipv6_observations() -> Result<()> {
        let observations = AddressObservation::parse(
            "2: eth0 inet 10.0.0.10/24 scope global\n2: eth0 inet6 fd00::10/64 scope global",
        )?;
        assert_eq!(observations.len(), 2);
        assert!(observations.iter().all(|entry| entry.ready));
        assert!(AddressObservation::parse("garbage").is_err());
        Ok(())
    }

    fn manager() -> AddressManager {
        AddressManager {
            ip_command: "ip".to_owned(),
            arping_command: "arping".to_owned(),
            ndsend_command: "ndsend".to_owned(),
            interface: "eth0".to_owned(),
            addresses: vec!["10.0.0.10/24".to_owned(), "fd00::10/64".to_owned()],
        }
    }

    fn scripted(results: Vec<Option<Result<String>>>) -> CommandRunner {
        CommandRunner::Scripted {
            results: results.into(),
            calls: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_failure_attempts_remaining_addresses_and_aggregates() {
        let mut runner = scripted(vec![
            Some(Err(anyhow!("first failed"))),
            Some(Ok(String::new())),
            Some(Ok(String::new())),
        ]);
        let error = manager()
            .apply_with_runner(AddressAction::Add, &mut runner)
            .await
            .err();
        let message = error.map_or_else(String::new, |error| error.to_string());
        assert!(message.contains("first failed"));
        assert!(message.contains("10.0.0.10/24") && message.contains("fd00::10/64"));
        if let CommandRunner::Scripted { calls, .. } = runner {
            assert_eq!(calls.len(), 3);
            assert!(
                calls
                    .iter()
                    .any(|args| args.iter().any(|arg| arg == "fd00::10/64"))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn never_completing_command_times_out_at_command_timeout_ms() -> Result<()> {
        let mut runner = scripted(vec![None, Some(Ok(String::new())), Some(Ok(String::new()))]);
        let start = tokio::time::Instant::now();
        let error = manager()
            .apply_with_runner(AddressAction::Add, &mut runner)
            .await
            .err()
            .context("expected timeout")?;
        assert!(error.to_string().contains("timed out after 3000 ms"));
        assert_eq!(
            start.elapsed(),
            tokio::time::Duration::from_millis(COMMAND_TIMEOUT_MS)
        );
        if let CommandRunner::Scripted { calls, .. } = runner {
            assert_eq!(calls.len(), 3);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_interface_or_prefix_never_confirms_presence() {
        for row in [
            "2: other0 inet 10.0.0.10/24 scope global",
            "2: eth0 inet 10.0.0.10/32 scope global",
        ] {
            let mut config = manager();
            config.addresses.truncate(1);
            let mut runner = scripted(vec![
                Some(Err(anyhow!("duplicate address"))),
                Some(Ok(row.to_owned())),
            ]);
            assert!(
                config
                    .apply_with_runner(AddressAction::Add, &mut runner)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn tentative_and_dadfailed_ipv6_prevent_acquisition() {
        for flag in ["tentative", "dadfailed"] {
            let mut config = manager();
            config.addresses = vec!["fd00::10/64".to_owned()];
            let mut runner = scripted(vec![
                Some(Ok(String::new())),
                Some(Ok(format!("2: eth0 inet6 fd00::10/64 scope global {flag}"))),
            ]);
            assert!(
                config
                    .apply_with_runner(AddressAction::Add, &mut runner)
                    .await
                    .is_err()
            );
            if let CommandRunner::Scripted { calls, .. } = runner {
                assert_eq!(calls.len(), 2);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn read_back_timeout_never_confirms_command_success() -> Result<()> {
        let mut runner = scripted(vec![Some(Ok(String::new())), Some(Ok(String::new())), None]);
        let start = tokio::time::Instant::now();
        assert!(
            manager()
                .apply_with_runner(AddressAction::Add, &mut runner)
                .await
                .is_err()
        );
        assert_eq!(
            start.elapsed(),
            tokio::time::Duration::from_millis(COMMAND_TIMEOUT_MS)
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn announcements_timeout_without_changing_confirmed_presence() -> Result<()> {
        let table =
            "2: eth0 inet 10.0.0.10/24 scope global\n2: eth0 inet6 fd00::10/64 scope global";
        let mut runner = scripted(vec![
            Some(Ok(String::new())),
            Some(Ok(String::new())),
            Some(Ok(table.to_owned())),
            None,
            None,
        ]);
        let start = tokio::time::Instant::now();
        manager()
            .apply_with_runner(AddressAction::Add, &mut runner)
            .await?;
        assert_eq!(
            start.elapsed(),
            tokio::time::Duration::from_millis(2 * COMMAND_TIMEOUT_MS)
        );
        Ok(())
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
