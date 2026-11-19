//! The coordinator owns this device. There is deliberately no feeding task
//! and no magic-close in Drop: losing the owner must leave a running fence.

#[cfg(any(target_os = "linux", test))]
use crate::config::WatchdogNowayout;
use crate::config::{WatchdogConfig, WatchdogMode};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::time::{Duration, Instant};

/// An open which fails partway through arming is not an unprotected success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceState {
    Unarmed,
    Armed,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct WatchdogError {
    state: DeviceState,
    message: String,
}

impl WatchdogError {
    fn new(state: DeviceState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }
}

impl fmt::Display for WatchdogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (device {:?})", self.message, self.state)
    }
}

impl std::error::Error for WatchdogError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogStatus {
    pub mode: String,
    pub armed: bool,
    pub effective_timeout_ms: Option<u64>,
    pub last_feed_ago_ms: Option<u64>,
    pub unprotected: bool,
}

/// Closed implementation set: production Linux device or the fake-clock
/// device in tests. Merely constructing either variant never opens a device.
#[derive(Debug)]
pub(crate) enum WatchdogDevice {
    Linux(LinuxWatchdog),
    #[cfg(test)]
    Fake(FakeWatchdog),
}

impl Default for WatchdogDevice {
    fn default() -> Self {
        Self::Linux(LinuxWatchdog::default())
    }
}

impl WatchdogDevice {
    fn state(&self) -> DeviceState {
        match self {
            Self::Linux(device) => device.state(),
            #[cfg(test)]
            Self::Fake(device) => device.lock().state,
        }
    }

    fn arm(&mut self, config: &WatchdogConfig) -> Result<Duration, WatchdogError> {
        match self {
            Self::Linux(device) => device.arm(config),
            #[cfg(test)]
            Self::Fake(device) => device.arm(config),
        }
    }

    fn keep_alive(&mut self) -> Result<(), WatchdogError> {
        match self {
            Self::Linux(device) => device.keep_alive(),
            #[cfg(test)]
            Self::Fake(device) => device.keep_alive(),
        }
    }

    fn disarm(&mut self) -> Result<(), WatchdogError> {
        match self {
            Self::Linux(device) => device.disarm(),
            #[cfg(test)]
            Self::Fake(device) => device.disarm(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct WatchdogManager {
    config: WatchdogConfig,
    device: WatchdogDevice,
    budget: Duration,
    effective_timeout: Option<Duration>,
    last_feed: Option<Instant>,
    generation: Option<u64>,
    unprotected: bool,
}

impl Default for WatchdogManager {
    fn default() -> Self {
        Self::new(
            WatchdogConfig::default(),
            WatchdogDevice::default(),
            Duration::ZERO,
        )
    }
}

impl WatchdogManager {
    pub(crate) fn new(config: WatchdogConfig, device: WatchdogDevice, budget: Duration) -> Self {
        Self {
            config,
            device,
            budget,
            effective_timeout: None,
            last_feed: None,
            generation: None,
            unprotected: false,
        }
    }

    pub(crate) fn status(&self, now: Instant) -> WatchdogStatus {
        WatchdogStatus {
            mode: match self.config.mode {
                WatchdogMode::Off => "off",
                WatchdogMode::Automatic => "automatic",
                WatchdogMode::Required => "required",
            }
            .to_owned(),
            armed: self.device.state() == DeviceState::Armed,
            effective_timeout_ms: self.effective_timeout.map(millis),
            last_feed_ago_ms: self
                .last_feed
                .map(|fed| millis(now.saturating_duration_since(fed))),
            unprotected: self.unprotected,
        }
    }

    pub(crate) fn ownership_allowed(&self) -> bool {
        self.config.mode != WatchdogMode::Required || self.device.state() == DeviceState::Armed
    }

    /// Only called after valid authority and a serialized absence proof.
    ///
    /// # Errors
    /// Blocks required acquisition on every failure, and automatic acquisition
    /// unless the failed arm conclusively left the device unarmed.
    pub(crate) fn arm(&mut self, generation: u64, reconciled: bool) -> Result<(), WatchdogError> {
        if self.config.mode == WatchdogMode::Off {
            return Ok(());
        }
        if !reconciled {
            return Err(WatchdogError::new(
                DeviceState::Unarmed,
                "startup reconciliation incomplete",
            ));
        }
        if self.device.state() != DeviceState::Unarmed {
            return Err(WatchdogError::new(
                self.device.state(),
                "previous watchdog arm has not been safely disarmed",
            ));
        }
        let armed_at = Instant::now();
        match self.device.arm(&self.config) {
            Ok(effective) => {
                self.effective_timeout = Some(effective);
                self.last_feed = Some(armed_at);
                if armed_at.elapsed().saturating_add(self.budget) >= effective {
                    // Keep the fd open and unfed. No error path sends V.
                    return Err(WatchdogError::new(
                        self.device.state(),
                        "effective timeout does not exceed A + J + U + S",
                    ));
                }
                self.generation = Some(generation);
                self.unprotected = false;
                Ok(())
            }
            Err(error)
                if error.state == DeviceState::Unarmed
                    && self.config.mode == WatchdogMode::Automatic =>
            {
                self.unprotected = true;
                tracing::warn!(%error, "watchdog acquisition proceeds unprotected");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn stop_feeding(&mut self) {
        self.generation = None;
    }

    /// Whether the armed device still leaves `reserve` before the feed budget
    /// (the same `elapsed + B >= W` rule as `feed`) is exhausted. A watchdog
    /// that is off, unprotected or not armed for ownership constrains nothing.
    pub(crate) fn covers(&self, now: Instant, reserve: Duration) -> bool {
        if self.generation.is_none() {
            return true;
        }
        self.last_feed
            .zip(self.effective_timeout)
            .is_some_and(|(last, timeout)| {
                now.saturating_duration_since(last)
                    .saturating_add(self.budget)
                    .saturating_add(reserve)
                    < timeout
            })
    }

    /// Called only by the coordinator, after it checks authority and the owned
    /// resource predicate (present or unmanaged). Armed generation and deadline
    /// budget remain mandatory for either resource state.
    ///
    /// # Errors
    /// Returns an error on a missed safety budget or a failed keepalive.
    pub(crate) fn feed(&mut self, generation: u64) -> Result<(), WatchdogError> {
        if self.generation != Some(generation) {
            return Ok(());
        }
        let now = Instant::now();
        if self
            .last_feed
            .zip(self.effective_timeout)
            .is_none_or(|(last, timeout)| {
                now.saturating_duration_since(last)
                    .saturating_add(self.budget)
                    >= timeout
            })
        {
            self.stop_feeding();
            return Err(WatchdogError::new(
                self.device.state(),
                "watchdog feed budget exhausted",
            ));
        }
        if let Err(error) = self.device.keep_alive() {
            self.stop_feeding();
            return Err(error);
        }
        self.last_feed = Some(now);
        Ok(())
    }

    /// Called only on a completed withdrawal with a current absence proof.
    ///
    /// # Errors
    /// Returns an error if disarm cannot be confirmed; the device stays unfed.
    pub(crate) fn disarm(&mut self) -> Result<(), WatchdogError> {
        self.stop_feeding();
        match self.device.state() {
            DeviceState::Unarmed => Ok(()),
            DeviceState::Armed => self.device.disarm(),
            DeviceState::Unknown => Err(WatchdogError::new(
                DeviceState::Unknown,
                "cannot magic-close a partially armed watchdog",
            )),
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(target_os = "linux")]
use linux::LinuxWatchdog;

#[cfg(target_os = "linux")]
mod linux {
    use super::{DeviceState, Duration, WatchdogConfig, WatchdogError, WatchdogNowayout};
    use std::{
        fs::{self, File, OpenOptions},
        io::Write,
        os::{
            fd::{AsRawFd, IntoRawFd},
            unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        },
    };

    // Linux UAPI <linux/watchdog.h>, using libc's request ABI on glibc/musl.
    const WDIOC_GETSUPPORT: libc::Ioctl = 0x8028_5700_u32 as libc::Ioctl;
    const WDIOC_SETTIMEOUT: libc::Ioctl = 0xc004_5706_u32 as libc::Ioctl;
    const WDIOC_GETTIMEOUT: libc::Ioctl = 0x8004_5707_u32 as libc::Ioctl;
    const WDIOC_KEEPALIVE: libc::Ioctl = 0x8004_5705_u32 as libc::Ioctl;
    const WDIOF_SETTIMEOUT: u32 = 0x0080;
    const WDIOF_MAGICCLOSE: u32 = 0x0100;

    #[repr(C)]
    #[derive(Default)]
    struct WatchdogInfo {
        options: u32,
        firmware_version: u32,
        identity: [u8; 32],
    }

    #[derive(Debug, Default)]
    pub(crate) struct LinuxWatchdog {
        file: Option<File>,
        capabilities: u32,
        nowayout: bool,
        verified: bool,
        close_uncertain: bool,
    }

    impl LinuxWatchdog {
        pub(super) fn state(&self) -> DeviceState {
            if self.close_uncertain {
                DeviceState::Unknown
            } else if self.file.is_none() {
                DeviceState::Unarmed
            } else if self.verified {
                DeviceState::Armed
            } else {
                DeviceState::Unknown
            }
        }

        fn error(&self, message: impl Into<String>) -> WatchdogError {
            WatchdogError::new(self.state(), message)
        }

        fn detect_nowayout(path: &str) -> Result<bool, WatchdogError> {
            // Resolve by device number, so a symlink or alternate node name
            // cannot accidentally read another watchdog's nowayout setting.
            let inspect = || -> std::io::Result<bool> {
                let metadata = fs::metadata(path)?;
                if !metadata.file_type().is_char_device() {
                    return Err(std::io::Error::other("watchdog is not a character device"));
                }
                let device = metadata.rdev();
                let sysfs = format!(
                    "/sys/dev/char/{}:{}/nowayout",
                    libc::major(device),
                    libc::minor(device)
                );
                match fs::read_to_string(sysfs)?.trim() {
                    "0" => Ok(false),
                    "1" => Ok(true),
                    _ => Err(std::io::Error::other(
                        "unrecognized watchdog nowayout value",
                    )),
                }
            };
            inspect().map_err(|error| {
                WatchdogError::new(DeviceState::Unarmed, format!("watchdog preflight: {error}"))
            })
        }

        pub(super) fn arm(&mut self, config: &WatchdogConfig) -> Result<Duration, WatchdogError> {
            self.nowayout = Self::detect_nowayout(&config.device)?;
            if self.nowayout && config.nowayout == WatchdogNowayout::Reject {
                return Err(self.error("nowayout watchdog rejected before open"));
            }
            self.file = Some(
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(&config.device)
                    .map_err(|error| self.error(format!("watchdog open: {error}")))?,
            );
            self.configure(config)
        }

        fn configure(&mut self, config: &WatchdogConfig) -> Result<Duration, WatchdogError> {
            let fd = self
                .file
                .as_ref()
                .ok_or_else(|| self.error("watchdog not open"))?
                .as_raw_fd();
            let mut info = WatchdogInfo::default();
            // SAFETY: fd is owned and open; the kernel writes exactly the
            // repr(C) watchdog_info object supplied for this ioctl.
            if unsafe { libc::ioctl(fd, WDIOC_GETSUPPORT, &raw mut info) } < 0 {
                return Err(self.error(format!(
                    "watchdog support: {}",
                    std::io::Error::last_os_error()
                )));
            }
            self.capabilities = info.options;
            if self.capabilities & WDIOF_SETTIMEOUT == 0
                || (self.capabilities & WDIOF_MAGICCLOSE == 0 && !self.nowayout)
            {
                return Err(self.error("watchdog lacks timeout or disarm capability"));
            }
            let mut seconds = i32::try_from(u64::from(config.timeout_ms).div_ceil(1000))
                .map_err(|error| self.error(error.to_string()))?;
            Self::ioctl_int(fd, WDIOC_SETTIMEOUT, &mut seconds)
                .map_err(|error| self.error(format!("watchdog set timeout: {error}")))?;
            Self::ioctl_int(fd, WDIOC_GETTIMEOUT, &mut seconds)
                .map_err(|error| self.error(format!("watchdog read timeout: {error}")))?;
            let seconds = u64::try_from(seconds)
                .ok()
                .filter(|seconds| *seconds > 0)
                .ok_or_else(|| self.error("watchdog reported a nonpositive timeout"))?;
            self.verified = true;
            if self.nowayout {
                tracing::warn!("nowayout watchdog armed: ownership loss will require self-reboot");
            }
            Ok(Duration::from_secs(seconds))
        }

        fn ioctl_int(
            fd: libc::c_int,
            request: libc::Ioctl,
            value: &mut i32,
        ) -> std::io::Result<()> {
            // SAFETY: each caller supplies an owned open fd and the int
            // buffer required by SETTIMEOUT, GETTIMEOUT or KEEPALIVE.
            if unsafe { libc::ioctl(fd, request, &raw mut *value) } < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn keep_alive(&mut self) -> Result<(), WatchdogError> {
            let fd = self
                .file
                .as_ref()
                .ok_or_else(|| self.error("watchdog not open"))?
                .as_raw_fd();
            Self::ioctl_int(fd, WDIOC_KEEPALIVE, &mut 0)
                .map_err(|error| self.error(format!("watchdog keepalive: {error}")))
        }

        pub(super) fn disarm(&mut self) -> Result<(), WatchdogError> {
            if self.nowayout || self.capabilities & WDIOF_MAGICCLOSE == 0 {
                return Err(
                    self.error("watchdog cannot disarm; accepted self-reboot remains armed")
                );
            }
            let Some(file) = &mut self.file else {
                return Ok(());
            };
            file.write_all(b"V").map_err(|error| {
                WatchdogError::new(DeviceState::Armed, format!("watchdog magic close: {error}"))
            })?;
            // Close explicitly so close errors cannot masquerade as absence
            // of a fence. Drop/error cleanup never writes the magic byte.
            if let Some(file) = self.file.take() {
                let fd = file.into_raw_fd();
                // SAFETY: into_raw_fd transferred sole ownership to us.
                if unsafe { libc::close(fd) } < 0 {
                    self.verified = false;
                    self.close_uncertain = true;
                    return Err(WatchdogError::new(
                        DeviceState::Unknown,
                        "watchdog close failed",
                    ));
                }
            }
            self.verified = false;
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default)]
pub(crate) struct LinuxWatchdog;

#[cfg(not(target_os = "linux"))]
impl LinuxWatchdog {
    fn state(&self) -> DeviceState {
        DeviceState::Unarmed
    }
    fn arm(&mut self, _: &WatchdogConfig) -> Result<Duration, WatchdogError> {
        Err(WatchdogError::new(
            DeviceState::Unarmed,
            "Linux watchdog unsupported",
        ))
    }
    fn keep_alive(&mut self) -> Result<(), WatchdogError> {
        Err(WatchdogError::new(
            DeviceState::Unarmed,
            "Linux watchdog unsupported",
        ))
    }
    fn disarm(&mut self) -> Result<(), WatchdogError> {
        Err(WatchdogError::new(
            DeviceState::Unarmed,
            "Linux watchdog unsupported",
        ))
    }
}

#[cfg(test)]
pub(crate) use fake::{FakeWatchdog, WatchdogCall, WatchdogFault};

#[cfg(test)]
mod fake {
    use super::*;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) enum WatchdogFault {
        #[default]
        None,
        Open,
        Capability,
        SetTimeout,
        Readback,
        EffectiveTimeout(u64),
        Write,
        Disarm,
        Nowayout,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum WatchdogCall {
        Preflight,
        Open,
        Support,
        SetTimeout(u32),
        GetTimeout,
        KeepAlive,
        Disarm,
        MagicClose,
        Close,
        Reset { deadline: Instant },
    }

    #[derive(Debug)]
    pub(crate) struct FakeState {
        pub(super) state: DeviceState,
        pub(crate) fault: WatchdogFault,
        pub(crate) calls: Vec<(Instant, WatchdogCall)>,
        pub(crate) reset_deadline: Option<Instant>,
        timeout: Duration,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct FakeWatchdog(Arc<Mutex<FakeState>>);

    impl FakeWatchdog {
        pub(crate) fn new(fault: WatchdogFault) -> Self {
            Self(Arc::new(Mutex::new(FakeState {
                state: DeviceState::Unarmed,
                fault,
                calls: Vec::new(),
                reset_deadline: None,
                timeout: Duration::from_secs(15),
            })))
        }

        pub(crate) fn lock(&self) -> MutexGuard<'_, FakeState> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Observe the hardware deadline without running a coordinator task.
        pub(crate) fn calls(&self) -> Vec<(Instant, WatchdogCall)> {
            let mut state = self.lock();
            state.observe_reset();
            state.calls.clone()
        }

        pub(super) fn arm(&mut self, config: &WatchdogConfig) -> Result<Duration, WatchdogError> {
            let mut state = self.lock();
            state.record(WatchdogCall::Preflight);
            if state.fault == WatchdogFault::Nowayout && config.nowayout == WatchdogNowayout::Reject
            {
                return Err(WatchdogError::new(
                    DeviceState::Unarmed,
                    "injected nowayout preflight",
                ));
            }
            state.record(WatchdogCall::Open);
            state.fail(WatchdogFault::Open)?;
            state.state = DeviceState::Unknown;
            state.reset_deadline = Some(Instant::now() + state.timeout);
            state.record(WatchdogCall::Support);
            state.fail(WatchdogFault::Capability)?;
            state.record(WatchdogCall::SetTimeout(config.timeout_ms));
            state.fail(WatchdogFault::SetTimeout)?;
            state.timeout = match state.fault {
                WatchdogFault::EffectiveTimeout(ms) => Duration::from_millis(ms),
                _ => Duration::from_secs(u64::from(config.timeout_ms).div_ceil(1000)),
            };
            state.reset_deadline = Some(Instant::now() + state.timeout);
            state.record(WatchdogCall::GetTimeout);
            state.fail(WatchdogFault::Readback)?;
            if state.timeout.is_zero() {
                return Err(WatchdogError::new(
                    DeviceState::Unknown,
                    "invalid zero readback",
                ));
            }
            state.state = DeviceState::Armed;
            Ok(state.timeout)
        }

        pub(super) fn keep_alive(&mut self) -> Result<(), WatchdogError> {
            let mut state = self.lock();
            state.observe_reset();
            state.record(WatchdogCall::KeepAlive);
            state.fail(WatchdogFault::Write)?;
            state.reset_deadline = Some(Instant::now() + state.timeout);
            Ok(())
        }

        pub(super) fn disarm(&mut self) -> Result<(), WatchdogError> {
            let mut state = self.lock();
            state.record(WatchdogCall::Disarm);
            state.fail(WatchdogFault::Disarm)?;
            state.fail(WatchdogFault::Nowayout)?;
            state.record(WatchdogCall::MagicClose);
            state.record(WatchdogCall::Close);
            state.state = DeviceState::Unarmed;
            state.reset_deadline = None;
            Ok(())
        }
    }

    impl FakeState {
        fn record(&mut self, call: WatchdogCall) {
            self.calls.push((Instant::now(), call));
        }

        fn fail(&self, fault: WatchdogFault) -> Result<(), WatchdogError> {
            if self.fault == fault {
                Err(WatchdogError::new(
                    self.state,
                    format!("injected {fault:?}"),
                ))
            } else {
                Ok(())
            }
        }

        fn observe_reset(&mut self) {
            if let Some(deadline) = self.reset_deadline
                && Instant::now() >= deadline
            {
                self.calls
                    .push((deadline, WatchdogCall::Reset { deadline }));
                self.reset_deadline = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn manager(mode: WatchdogMode, fault: WatchdogFault) -> (WatchdogManager, FakeWatchdog) {
        let fake = FakeWatchdog::new(fault);
        (
            WatchdogManager::new(
                WatchdogConfig {
                    mode,
                    ..WatchdogConfig::default()
                },
                WatchdogDevice::Fake(fake.clone()),
                Duration::from_secs(11),
            ),
            fake,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_open_failure_is_conclusively_unarmed() {
        let (mut required, fake) = manager(WatchdogMode::Required, WatchdogFault::Open);
        assert!(required.arm(1, true).is_err());
        assert!(!required.ownership_allowed());
        assert_eq!(fake.lock().state, DeviceState::Unarmed);
        let (mut automatic, _) = manager(WatchdogMode::Automatic, WatchdogFault::Open);
        assert!(automatic.arm(1, true).is_ok());
        assert!(automatic.status(Instant::now()).unprotected);
        println!(
            "WATCHDOG open failure required blocks; automatic unprotected: {:?}",
            fake.calls()
        );
    }

    fn assert_partial_failure(fault: WatchdogFault) {
        for mode in [WatchdogMode::Required, WatchdogMode::Automatic] {
            let (mut manager, fake) = manager(mode, fault);
            assert!(manager.arm(1, true).is_err());
            assert_eq!(fake.lock().state, DeviceState::Unknown);
            assert!(!manager.status(Instant::now()).unprotected);
            assert!(manager.disarm().is_err());
            assert!(!fake.calls().iter().any(|(_, call)| matches!(
                call,
                WatchdogCall::Disarm | WatchdogCall::MagicClose | WatchdogCall::Close
            )));
            println!(
                "WATCHDOG partial fault {fault:?} blocks acquisition: {:?}",
                fake.calls()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_capability_failure_blocks_automatic_and_required() {
        assert_partial_failure(WatchdogFault::Capability);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_partial_arm_timeout_set_failure_blocks_automatic_and_required() {
        assert_partial_failure(WatchdogFault::SetTimeout);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_readback_failure_blocks_automatic_and_required() {
        assert_partial_failure(WatchdogFault::Readback);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_nonpositive_readback_is_unknown() {
        assert_partial_failure(WatchdogFault::EffectiveTimeout(0));
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_effective_timeout_rounding_up_is_verified() -> Result<()> {
        let (mut manager, fake) = manager(
            WatchdogMode::Required,
            WatchdogFault::EffectiveTimeout(16_000),
        );
        manager.arm(1, true)?;
        assert_eq!(
            manager.status(Instant::now()).effective_timeout_ms,
            Some(16_000)
        );
        manager.feed(1)?;
        assert!(
            fake.calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::GetTimeout)
        );
        manager.disarm()?;
        println!(
            "WATCHDOG rounded effective W=16000 > A+J+U+S=11000: {:?}",
            fake.calls()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_unsafe_effective_timeout_blocks_both_modes() {
        for mode in [WatchdogMode::Required, WatchdogMode::Automatic] {
            let (mut manager, fake) = manager(mode, WatchdogFault::EffectiveTimeout(11_000));
            assert!(manager.arm(1, true).is_err());
            assert!(!manager.status(Instant::now()).unprotected);
            assert!(manager.feed(1).is_ok());
            assert!(
                !fake
                    .calls()
                    .iter()
                    .any(|(_, call)| *call == WatchdogCall::KeepAlive)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_failed_write_stops_future_feeds() -> Result<()> {
        let (mut manager, fake) = manager(WatchdogMode::Required, WatchdogFault::None);
        manager.arm(1, true)?;
        fake.lock().fault = WatchdogFault::Write;
        assert!(manager.feed(1).is_err());
        let calls = fake.calls();
        manager.feed(1)?;
        assert_eq!(fake.calls(), calls);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_failed_disarm_remains_armed_and_unfed() -> Result<()> {
        let (mut manager, fake) = manager(WatchdogMode::Required, WatchdogFault::None);
        manager.arm(1, true)?;
        fake.lock().fault = WatchdogFault::Disarm;
        assert!(manager.disarm().is_err());
        assert!(manager.status(Instant::now()).armed);
        manager.feed(1)?;
        tokio::time::advance(Duration::from_secs(15)).await;
        let calls = fake.calls();
        assert!(
            calls
                .iter()
                .any(|(_, call)| matches!(call, WatchdogCall::Reset { .. }))
        );
        assert!(
            !calls
                .iter()
                .any(|(_, call)| *call == WatchdogCall::MagicClose)
        );
        println!("WATCHDOG failed disarm: {calls:?}");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_nowayout_reject_and_explicit_reboot() -> Result<()> {
        let (mut required, fake) = manager(WatchdogMode::Required, WatchdogFault::Nowayout);
        assert!(required.arm(1, true).is_err());
        assert!(
            !fake
                .calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::Open)
        );
        let (mut automatic, _) = manager(WatchdogMode::Automatic, WatchdogFault::Nowayout);
        automatic.arm(1, true)?;
        assert!(automatic.status(Instant::now()).unprotected);
        required.config.nowayout = WatchdogNowayout::Reboot;
        required.arm(2, true)?;
        required.feed(2)?;
        assert!(required.disarm().is_err());
        assert!(required.status(Instant::now()).armed);
        tokio::time::advance(Duration::from_secs(15)).await;
        assert!(
            fake.calls()
                .iter()
                .any(|(_, call)| matches!(call, WatchdogCall::Reset { .. }))
        );
        println!(
            "WATCHDOG nowayout reboot explicitly accepted: {:?}",
            fake.calls()
        );
        Ok(())
    }

    /// W = 15 s, B = 11 s: a reserve fits only while elapsed + B + reserve < W.
    #[tokio::test(start_paused = true)]
    async fn watchdog_covers_reserve_with_feed_budget_boundaries() -> Result<()> {
        let (mut manager, _fake) = manager(WatchdogMode::Required, WatchdogFault::None);
        assert!(manager.covers(Instant::now(), Duration::from_secs(60)));
        manager.arm(3, true)?;
        let reserve = Duration::from_secs(3);
        assert!(manager.covers(Instant::now(), reserve));
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(manager.covers(Instant::now(), reserve));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            !manager.covers(Instant::now(), reserve),
            "equality is unsafe"
        );
        manager.feed(3)?;
        assert!(
            manager.covers(Instant::now(), reserve),
            "a feed restores it"
        );
        manager.stop_feeding();
        assert!(manager.covers(Instant::now(), Duration::from_secs(60)));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_generation_and_feed_budget_gate_keepalive() -> Result<()> {
        let (mut manager, fake) = manager(WatchdogMode::Required, WatchdogFault::None);
        manager.arm(7, true)?;
        manager.feed(6)?;
        assert!(
            !fake
                .calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::KeepAlive)
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(manager.feed(7).is_err(), "elapsed + budget == W is unsafe");
        manager.feed(7)?;
        assert!(
            !fake
                .calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::KeepAlive)
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_i16_does_not_open_before_reconciliation() {
        let (mut manager, fake) = manager(WatchdogMode::Required, WatchdogFault::None);
        assert!(manager.arm(1, false).is_err());
        assert_eq!(fake.calls(), Vec::new());
    }
}
