use crate::gruezi::ownership::{ObservedPresence, ResourceState};
use anyhow::{Context, Result, bail};
use std::net::IpAddr;
use tokio::{
    process::Command,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Duration, Instant, timeout},
};
use tracing::warn;

pub const COMMAND_TIMEOUT_MS: u64 = 3000;
pub const WITHDRAW_TIMEOUT_MS: u64 = 5000;

/// Bound the worker's per-address add and announcement commands plus read-back.
pub(crate) const fn acquisition_budget_ms(address_count: usize) -> u128 {
    (2 * address_count as u128 + 1) * COMMAND_TIMEOUT_MS as u128
}

#[cfg(test)]
pub(crate) struct CommandGate {
    pub(crate) entered: tokio::sync::oneshot::Sender<()>,
    pub(crate) release: tokio::sync::oneshot::Receiver<Result<()>>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KernelAddressEntry {
    pub(crate) interface: String,
    pub(crate) address: String,
    pub(crate) flags: String,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct SimulatedKernel {
    pub(crate) entries: Vec<KernelAddressEntry>,
    pub(crate) calls: Vec<Vec<String>>,
    pub(crate) fail_add_addresses: Vec<String>,
    pub(crate) fail_del_addresses: Vec<String>,
    pub(crate) fail_all_commands: bool,
    pub(crate) fail_show: bool,
    pub(crate) default_add_flags: Vec<(String, String)>,
    pub(crate) gate_next_add: Option<CommandGate>,
    pub(crate) gate_after_add: Option<CommandGate>,
    pub(crate) gate_next_del: Option<CommandGate>,
}

#[cfg(test)]
impl SimulatedKernel {
    pub(crate) fn new_shared() -> std::sync::Arc<std::sync::Mutex<Self>> {
        std::sync::Arc::new(std::sync::Mutex::new(Self::default()))
    }

    pub(crate) fn table_snapshot(shared: &std::sync::Arc<std::sync::Mutex<Self>>) -> Vec<String> {
        let guard = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entries
            .iter()
            .map(|entry| {
                if entry.flags.is_empty() {
                    format!("{} {}", entry.interface, entry.address)
                } else {
                    format!("{} {} ({})", entry.interface, entry.address, entry.flags)
                }
            })
            .collect()
    }

    pub(crate) fn calls_snapshot(
        shared: &std::sync::Arc<std::sync::Mutex<Self>>,
    ) -> Vec<Vec<String>> {
        let guard = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.calls.clone()
    }

    pub(crate) fn insert_address(
        shared: &std::sync::Arc<std::sync::Mutex<Self>>,
        interface: &str,
        address: &str,
        flags: &str,
    ) {
        let mut guard = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entries
            .retain(|entry| !(entry.interface == interface && entry.address == address));
        guard.entries.push(KernelAddressEntry {
            interface: interface.to_owned(),
            address: address.to_owned(),
            flags: flags.to_owned(),
        });
    }

    pub(crate) fn remove_address(
        shared: &std::sync::Arc<std::sync::Mutex<Self>>,
        interface: &str,
        address: &str,
    ) {
        let mut guard = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entries
            .retain(|entry| !(entry.interface == interface && entry.address == address));
    }
}

pub(crate) enum CommandRunner {
    System,
    #[cfg(test)]
    Scripted {
        results: std::collections::VecDeque<Option<Result<String>>>,
        calls: Vec<Vec<String>>,
    },
    #[cfg(test)]
    Simulated(std::sync::Arc<std::sync::Mutex<SimulatedKernel>>),
}

impl CommandRunner {
    async fn run(&mut self, command: &str, args: &[&str]) -> Result<String> {
        if command.trim().is_empty() {
            bail!("command path cannot be empty");
        }
        timeout(Duration::from_millis(COMMAND_TIMEOUT_MS), async {
            match self {
                Self::System => {
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
                #[cfg(test)]
                Self::Scripted { results, calls } => {
                    calls.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                    match results.pop_front().context("script exhausted")? {
                        Some(result) => result,
                        None => std::future::pending().await,
                    }
                }
                #[cfg(test)]
                Self::Simulated(kernel) => run_simulated_command(kernel, command, args).await,
            }
        })
        .await
        .with_context(|| format!("{command} {args:?} timed out after {COMMAND_TIMEOUT_MS} ms"))?
    }
}

#[cfg(test)]
async fn wait_on_gate(gate: Option<CommandGate>) -> Result<()> {
    if let Some(gate) = gate {
        let _ = gate.entered.send(());
        match gate.release.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => std::future::pending::<()>().await,
        }
    }
    Ok(())
}

#[cfg(test)]
fn simulated_show(
    kernel: &std::sync::Arc<std::sync::Mutex<SimulatedKernel>>,
    interface: &str,
) -> Result<String> {
    use std::fmt::Write as _;

    let guard = kernel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.fail_show {
        bail!("simulated ip address show failure");
    }
    let mut output = String::new();
    for entry in &guard.entries {
        if entry.interface == interface {
            let family = if entry.address.contains(':') {
                "inet6"
            } else {
                "inet"
            };
            let _ = writeln!(
                output,
                "2: {interface}    {family} {} scope global {} {interface}",
                entry.address, entry.flags
            );
        }
    }
    Ok(output)
}

#[cfg(test)]
async fn simulated_add(
    kernel: &std::sync::Arc<std::sync::Mutex<SimulatedKernel>>,
    interface: &str,
    address: &str,
) -> Result<String> {
    let before_gate = {
        let mut guard = kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.gate_next_add.take()
    };
    wait_on_gate(before_gate).await?;

    let after_gate = {
        let mut guard = kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.fail_add_addresses.iter().any(|item| item == address) {
            bail!("RTNETLINK answers: injected add failure for {address}");
        }
        if guard
            .entries
            .iter()
            .any(|entry| entry.interface == interface && entry.address == address)
        {
            bail!("RTNETLINK answers: File exists");
        }
        let flags = guard
            .default_add_flags
            .iter()
            .find(|(addr, _)| addr == address)
            .map_or_else(String::new, |(_, flags)| flags.clone());
        guard.entries.push(KernelAddressEntry {
            interface: interface.to_owned(),
            address: address.to_owned(),
            flags,
        });
        guard.gate_after_add.take()
    };

    wait_on_gate(after_gate).await?;
    Ok(String::new())
}

#[cfg(test)]
async fn simulated_del(
    kernel: &std::sync::Arc<std::sync::Mutex<SimulatedKernel>>,
    interface: &str,
    address: &str,
) -> Result<String> {
    let before_gate = {
        let mut guard = kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.gate_next_del.take()
    };
    wait_on_gate(before_gate).await?;

    let mut guard = kernel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.fail_del_addresses.iter().any(|item| item == address) {
        bail!("RTNETLINK answers: injected del failure for {address}");
    }
    let initial_len = guard.entries.len();
    guard
        .entries
        .retain(|entry| !(entry.interface == interface && entry.address == address));
    if guard.entries.len() == initial_len {
        bail!("RTNETLINK answers: Cannot assign requested address");
    }
    Ok(String::new())
}

#[cfg(test)]
async fn run_simulated_command(
    kernel: &std::sync::Arc<std::sync::Mutex<SimulatedKernel>>,
    command: &str,
    args: &[&str],
) -> Result<String> {
    {
        let mut guard = kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut call = Vec::with_capacity(args.len() + 1);
        call.push(command.to_owned());
        call.extend(args.iter().map(|arg| (*arg).to_owned()));
        guard.calls.push(call);
        if guard.fail_all_commands {
            bail!("unexpected command execution: {command} {args:?}");
        }
    }

    if args.first() == Some(&"-o")
        && matches!(args.get(1), Some(&"address" | &"addr"))
        && args.get(2) == Some(&"show")
        && args.get(3) == Some(&"dev")
        && let Some(&interface) = args.get(4)
    {
        return simulated_show(kernel, interface);
    }

    if args.first() == Some(&"address")
        && let Some(&operation) = args.get(1)
        && let Some(&address) = args.get(2)
        && args.get(3) == Some(&"dev")
        && let Some(&interface) = args.get(4)
    {
        if operation == "add" {
            return simulated_add(kernel, interface, address).await;
        } else if operation == "del" {
            return simulated_del(kernel, interface, address).await;
        }
    }

    Ok(String::new())
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

    fn evaluate_read_back(
        &self,
        observed: &[AddressObservation],
        action: AddressAction,
    ) -> (ObservedPresence, Vec<String>) {
        let mut ready_count = 0usize;
        let mut any_match_count = 0usize;
        let mut missing_errors = Vec::new();

        for address in &self.addresses {
            match parse_address(address) {
                Ok((ip, prefix)) => {
                    let matching: Vec<_> = observed
                        .iter()
                        .filter(|entry| {
                            entry.interface == self.interface
                                && entry.ip == ip
                                && entry.prefix == prefix
                        })
                        .collect();
                    if !matching.is_empty() {
                        any_match_count = any_match_count.saturating_add(1);
                    }
                    let is_ready = matching.iter().any(|entry| entry.ready);
                    if is_ready {
                        ready_count = ready_count.saturating_add(1);
                    }
                    let satisfied = match action {
                        AddressAction::Add => is_ready,
                        AddressAction::Remove => matching.is_empty(),
                    };
                    if !satisfied {
                        missing_errors.push(format!(
                            "read-back did not confirm {} {address} on {}",
                            action.ip_subcommand(),
                            self.interface
                        ));
                    }
                }
                Err(error) => {
                    missing_errors.push(format!("{address}: {error:#}"));
                }
            }
        }

        let presence = if ready_count == self.addresses.len() {
            ObservedPresence::Present
        } else if any_match_count == 0 {
            ObservedPresence::Absent
        } else {
            ObservedPresence::Partial
        };

        (presence, missing_errors)
    }
}

/// Serialized command sent from the ownership controller to the single resource worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceCommand {
    Acquire(u64),
    Withdraw(u64),
    Reconcile,
}

/// Live desired ownership target published by the ownership controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredOwnership {
    Authorized,
    Revoked,
    Shutdown,
}

/// Generation-tagged directive inspected between every per-address operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerDirective {
    pub generation: u64,
    pub desired: DesiredOwnership,
}

/// Outcome category reported by the single resource worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceReportKind {
    Acquired,
    Withdrawn,
    Reconciled,
    CompensatedWithdrawal,
    RejectedWhileWithdrawing,
    Fault,
}

/// Result of a serialized resource worker operation or reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceReport {
    pub generation: u64,
    pub kind: ResourceReportKind,
    pub resource_state: ResourceState,
    pub observed_at: Instant,
    pub error: Option<String>,
}

/// Handle owned by the coordinator to drive and join the single resource worker task.
pub struct ResourceWorkerHandle {
    command_tx: mpsc::UnboundedSender<ResourceCommand>,
    directive_tx: watch::Sender<WorkerDirective>,
    report_rx: mpsc::UnboundedReceiver<ResourceReport>,
    join_handle: Option<JoinHandle<ResourceState>>,
}

impl ResourceWorkerHandle {
    /// Spawn the single resource worker task using the system command runner.
    #[must_use]
    pub fn spawn(manager: AddressManager) -> Self {
        Self::spawn_with_runner(manager, CommandRunner::System)
    }

    pub(crate) fn spawn_with_runner(manager: AddressManager, runner: CommandRunner) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (report_tx, report_rx) = mpsc::unbounded_channel();
        let initial_directive = WorkerDirective {
            generation: 0,
            desired: DesiredOwnership::Revoked,
        };
        let (directive_tx, directive_rx) = watch::channel(initial_directive);
        let worker = ResourceWorker {
            manager,
            runner,
            command_rx,
            directive_rx,
            report_tx,
            sticky_withdraw: false,
            has_attempted_acquire: false,
        };
        let join_handle = tokio::spawn(worker.run());
        Self {
            command_tx,
            directive_tx,
            report_rx,
            join_handle: Some(join_handle),
        }
    }

    /// Update the live generation and desired ownership state inspected between operations.
    pub fn set_directive(&self, generation: u64, desired: DesiredOwnership) {
        self.directive_tx.send_replace(WorkerDirective {
            generation,
            desired,
        });
    }

    /// Enqueue a serialized resource command for the worker.
    pub fn send_command(&self, command: ResourceCommand) {
        let _ = self.command_tx.send(command);
    }

    /// Receive the next worker report asynchronously.
    pub async fn recv_report(&mut self) -> Option<ResourceReport> {
        self.report_rx.recv().await
    }

    /// Poll for a pending worker report without waiting.
    pub fn try_recv_report(&mut self) -> Option<ResourceReport> {
        self.report_rx.try_recv().ok()
    }

    /// Request sticky shutdown withdrawal and join the worker within the withdrawal deadline.
    pub async fn shutdown(&mut self, generation: u64) -> ResourceReport {
        self.set_directive(generation, DesiredOwnership::Shutdown);
        self.send_command(ResourceCommand::Withdraw(generation));

        let Some(mut join_handle) = self.join_handle.take() else {
            return ResourceReport {
                generation,
                kind: ResourceReportKind::Withdrawn,
                resource_state: ResourceState::Absent,
                observed_at: Instant::now(),
                error: None,
            };
        };

        let deadline = Duration::from_millis(WITHDRAW_TIMEOUT_MS);
        if let Ok(joined) = timeout(deadline, &mut join_handle).await {
            let mut latest_report = None;
            while let Ok(report) = self.report_rx.try_recv() {
                latest_report = Some(report);
            }
            if let Some(report) = latest_report {
                return report;
            }
            let final_state = joined.unwrap_or(ResourceState::Uncertain {
                last_observation: ObservedPresence::Unknown,
                last_error: Some("resource worker task failed to join".to_owned()),
            });
            let kind = if final_state.is_confirmed_absent_or_unmanaged() {
                ResourceReportKind::Withdrawn
            } else {
                ResourceReportKind::Fault
            };
            return ResourceReport {
                generation,
                kind,
                resource_state: final_state,
                observed_at: Instant::now(),
                error: None,
            };
        }

        join_handle.abort();
        let _ = join_handle.await;
        let error = format!("resource withdrawal timed out after {WITHDRAW_TIMEOUT_MS} ms");
        ResourceReport {
            generation,
            kind: ResourceReportKind::Fault,
            resource_state: ResourceState::Uncertain {
                last_observation: ObservedPresence::Unknown,
                last_error: Some(error.clone()),
            },
            observed_at: Instant::now(),
            error: Some(error),
        }
    }
}

struct ResourceWorker {
    manager: AddressManager,
    runner: CommandRunner,
    command_rx: mpsc::UnboundedReceiver<ResourceCommand>,
    directive_rx: watch::Receiver<WorkerDirective>,
    report_tx: mpsc::UnboundedSender<ResourceReport>,
    sticky_withdraw: bool,
    has_attempted_acquire: bool,
}

impl ResourceWorker {
    async fn run(mut self) -> ResourceState {
        let mut last_state = if self.manager.addresses.is_empty() {
            ResourceState::Unmanaged
        } else {
            ResourceState::Absent
        };

        while let Some(first_command) = self.command_rx.recv().await {
            let command = self.coalesce_queued_commands(first_command);
            let report = match command {
                ResourceCommand::Acquire(generation) => self.handle_acquire(generation).await,
                ResourceCommand::Withdraw(generation) => {
                    let is_revocation = self.has_attempted_acquire
                        || self.directive_rx.borrow().desired == DesiredOwnership::Shutdown;
                    self.handle_withdraw(generation, ResourceReportKind::Withdrawn, is_revocation)
                        .await
                }
                ResourceCommand::Reconcile => self.handle_reconcile().await,
            };
            last_state = report.resource_state.clone();
            let _ = self.report_tx.send(report);

            if self.directive_rx.borrow().desired == DesiredOwnership::Shutdown {
                break;
            }
        }

        last_state
    }

    fn coalesce_queued_commands(&mut self, first: ResourceCommand) -> ResourceCommand {
        let mut selected = first;
        if let ResourceCommand::Withdraw(_) = selected {
            self.sticky_withdraw = true;
        }

        while let Ok(next) = self.command_rx.try_recv() {
            match next {
                ResourceCommand::Withdraw(generation) => {
                    self.sticky_withdraw = true;
                    // Sticky Withdraw drops any queued Acquire commands.
                    selected = ResourceCommand::Withdraw(generation);
                }
                ResourceCommand::Acquire(generation) => {
                    if !self.sticky_withdraw
                        && self.directive_rx.borrow().desired == DesiredOwnership::Authorized
                        && !matches!(selected, ResourceCommand::Withdraw(_))
                    {
                        selected = ResourceCommand::Acquire(generation);
                    }
                }
                ResourceCommand::Reconcile => {
                    if !matches!(
                        selected,
                        ResourceCommand::Acquire(_) | ResourceCommand::Withdraw(_)
                    ) {
                        selected = ResourceCommand::Reconcile;
                    }
                }
            }
        }

        if self.directive_rx.borrow().desired == DesiredOwnership::Shutdown {
            let generation = self.directive_rx.borrow().generation;
            self.sticky_withdraw = true;
            return ResourceCommand::Withdraw(generation);
        }

        selected
    }

    fn is_generation_still_authorized(&self, generation: u64) -> bool {
        let directive = *self.directive_rx.borrow();
        !self.sticky_withdraw
            && directive.desired == DesiredOwnership::Authorized
            && directive.generation == generation
    }

    fn drain_queued_acquires_on_abort(&mut self, fallback_generation: u64) -> u64 {
        let mut target_generation = self
            .directive_rx
            .borrow()
            .generation
            .max(fallback_generation);
        while let Ok(command) = self.command_rx.try_recv() {
            if let ResourceCommand::Withdraw(withdraw_gen) = command {
                target_generation = target_generation.max(withdraw_gen);
            }
        }
        target_generation
    }

    async fn compensate_stale_acquire(&mut self, generation: u64) -> ResourceReport {
        let withdraw_generation = self.drain_queued_acquires_on_abort(generation);
        self.sticky_withdraw = true;
        self.handle_withdraw(
            withdraw_generation,
            ResourceReportKind::CompensatedWithdrawal,
            true,
        )
        .await
    }

    async fn add_configured_addresses(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Vec<String>, ResourceReport> {
        let mut errors = Vec::new();
        for address in &self.manager.addresses.clone() {
            if !self.is_generation_still_authorized(generation) {
                return Err(self.compensate_stale_acquire(generation).await);
            }
            if let Err(error) = self
                .runner
                .run(
                    &self.manager.ip_command,
                    &["address", "add", address, "dev", &self.manager.interface],
                )
                .await
            {
                errors.push(format!("{error:#}"));
            }
            if !self.is_generation_still_authorized(generation) {
                return Err(self.compensate_stale_acquire(generation).await);
            }
        }
        Ok(errors)
    }

    async fn announce_acquired_addresses(&mut self, generation: u64) -> Option<ResourceReport> {
        for address in &self.manager.addresses.clone() {
            if !self.is_generation_still_authorized(generation) {
                return Some(self.compensate_stale_acquire(generation).await);
            }
            if let Err(error) = announce_address(
                &mut self.runner,
                &self.manager.arping_command,
                &self.manager.ndsend_command,
                &self.manager.interface,
                address,
            )
            .await
            {
                warn!(%error, address, "HA neighbor announcement failed");
            }
        }
        if !self.is_generation_still_authorized(generation) {
            return Some(self.compensate_stale_acquire(generation).await);
        }
        None
    }

    async fn handle_acquire(&mut self, generation: u64) -> ResourceReport {
        if self.manager.addresses.is_empty() {
            return ResourceReport {
                generation,
                kind: ResourceReportKind::Acquired,
                resource_state: ResourceState::Unmanaged,
                observed_at: Instant::now(),
                error: None,
            };
        }

        if self.sticky_withdraw {
            let error = "acquisition rejected while previous withdrawal is unconfirmed".to_owned();
            return ResourceReport {
                generation,
                kind: ResourceReportKind::RejectedWhileWithdrawing,
                resource_state: ResourceState::Uncertain {
                    last_observation: ObservedPresence::Unknown,
                    last_error: Some(error.clone()),
                },
                observed_at: Instant::now(),
                error: Some(error),
            };
        }

        if !self.is_generation_still_authorized(generation) {
            return self.compensate_stale_acquire(generation).await;
        }

        self.has_attempted_acquire = true;
        let mut errors = match self.add_configured_addresses(generation).await {
            Ok(errors) => errors,
            Err(compensated_report) => return compensated_report,
        };

        let observation = self
            .runner
            .run(
                &self.manager.ip_command,
                &["-o", "address", "show", "dev", &self.manager.interface],
            )
            .await
            .and_then(|output| AddressObservation::parse(&output));

        if !self.is_generation_still_authorized(generation) {
            return self.compensate_stale_acquire(generation).await;
        }

        let observed_at = Instant::now();
        let last_observation = match observation {
            Ok(observed) => {
                let (presence, read_back_errors) = self
                    .manager
                    .evaluate_read_back(&observed, AddressAction::Add);
                if presence == ObservedPresence::Present && read_back_errors.is_empty() {
                    for error in errors {
                        warn!(%error, "HA address command failed but read-back confirmed state");
                    }
                    if let Some(compensated) = self.announce_acquired_addresses(generation).await {
                        return compensated;
                    }
                    return ResourceReport {
                        generation,
                        kind: ResourceReportKind::Acquired,
                        resource_state: ResourceState::Present { generation },
                        observed_at: Instant::now(),
                        error: None,
                    };
                }
                errors.extend(read_back_errors);
                presence
            }
            Err(error) => {
                errors.push(format!("read-back failed: {error:#}"));
                ObservedPresence::Unknown
            }
        };

        let combined_error = errors.join("; ");
        ResourceReport {
            generation,
            kind: ResourceReportKind::Fault,
            resource_state: ResourceState::Uncertain {
                last_observation,
                last_error: Some(combined_error.clone()),
            },
            observed_at,
            error: Some(combined_error),
        }
    }

    async fn handle_withdraw(
        &mut self,
        generation: u64,
        success_kind: ResourceReportKind,
        latch_sticky_on_failure: bool,
    ) -> ResourceReport {
        if self.manager.addresses.is_empty() {
            self.sticky_withdraw = false;
            return ResourceReport {
                generation,
                kind: success_kind,
                resource_state: ResourceState::Unmanaged,
                observed_at: Instant::now(),
                error: None,
            };
        }

        if latch_sticky_on_failure {
            self.sticky_withdraw = true;
        }

        let mut errors = Vec::new();
        for address in &self.manager.addresses.clone() {
            if let Err(error) = self
                .runner
                .run(
                    &self.manager.ip_command,
                    &["address", "del", address, "dev", &self.manager.interface],
                )
                .await
            {
                errors.push(format!("{error:#}"));
            }
        }

        let observation = self
            .runner
            .run(
                &self.manager.ip_command,
                &["-o", "address", "show", "dev", &self.manager.interface],
            )
            .await
            .and_then(|output| AddressObservation::parse(&output));
        let observed_at = Instant::now();

        let last_observation = match observation {
            Ok(observed) => {
                let (presence, read_back_errors) = self
                    .manager
                    .evaluate_read_back(&observed, AddressAction::Remove);
                if presence == ObservedPresence::Absent && read_back_errors.is_empty() {
                    for error in errors {
                        warn!(%error, "HA address command failed but read-back confirmed state");
                    }
                    self.sticky_withdraw = false;
                    return ResourceReport {
                        generation,
                        kind: success_kind,
                        resource_state: ResourceState::Absent,
                        observed_at,
                        error: None,
                    };
                }
                errors.extend(read_back_errors);
                presence
            }
            Err(error) => {
                errors.push(format!("read-back failed: {error:#}"));
                ObservedPresence::Unknown
            }
        };

        self.sticky_withdraw = latch_sticky_on_failure;
        let combined_error = errors.join("; ");
        ResourceReport {
            generation,
            kind: ResourceReportKind::Fault,
            resource_state: ResourceState::Uncertain {
                last_observation,
                last_error: Some(combined_error.clone()),
            },
            observed_at,
            error: Some(combined_error),
        }
    }

    async fn handle_reconcile(&mut self) -> ResourceReport {
        let directive = *self.directive_rx.borrow();
        let generation = directive.generation;

        if self.manager.addresses.is_empty() {
            self.sticky_withdraw = false;
            return ResourceReport {
                generation,
                kind: ResourceReportKind::Reconciled,
                resource_state: ResourceState::Unmanaged,
                observed_at: Instant::now(),
                error: None,
            };
        }

        let observation = self
            .runner
            .run(
                &self.manager.ip_command,
                &["-o", "address", "show", "dev", &self.manager.interface],
            )
            .await
            .and_then(|output| AddressObservation::parse(&output));
        let observed_at = Instant::now();

        let observed = match observation {
            Ok(observed) => observed,
            Err(error) => {
                let message = format!("read-back failed: {error:#}");
                return ResourceReport {
                    generation,
                    kind: ResourceReportKind::Fault,
                    resource_state: ResourceState::Uncertain {
                        last_observation: ObservedPresence::Unknown,
                        last_error: Some(message.clone()),
                    },
                    observed_at,
                    error: Some(message),
                };
            }
        };

        let should_hold =
            directive.desired == DesiredOwnership::Authorized && !self.sticky_withdraw;

        if should_hold {
            let (presence, _) = self
                .manager
                .evaluate_read_back(&observed, AddressAction::Add);
            if presence == ObservedPresence::Present {
                return ResourceReport {
                    generation,
                    kind: ResourceReportKind::Reconciled,
                    resource_state: ResourceState::Present { generation },
                    observed_at,
                    error: None,
                };
            }
            // Missing or partially present VIP while authority is valid: re-acquire.
            return self.handle_acquire(generation).await;
        }

        let (presence, _) = self
            .manager
            .evaluate_read_back(&observed, AddressAction::Remove);
        if presence == ObservedPresence::Absent {
            self.sticky_withdraw = false;
            return ResourceReport {
                generation,
                kind: ResourceReportKind::Reconciled,
                resource_state: ResourceState::Absent,
                observed_at,
                error: None,
            };
        }

        // Residual managed VIP without authority: withdraw and confirm absence.
        self.handle_withdraw(
            generation,
            ResourceReportKind::Reconciled,
            self.has_attempted_acquire,
        )
        .await
    }
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
