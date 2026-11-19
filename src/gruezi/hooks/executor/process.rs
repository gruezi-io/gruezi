use super::{
    ActivationMode, DeactivationMode, Job, LifecycleEvent, LifecycleKind, LifecycleOperation,
    LifecycleOutcome, Settings,
};
use crate::gruezi::hooks::hook_command;
use anyhow::{Result, bail};
use std::{
    collections::VecDeque,
    ffi::CStr,
    io,
    os::unix::process::ExitStatusExt,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    signal::unix::{Signal, SignalKind, signal},
    sync::watch,
    time::{Instant, sleep, sleep_until, timeout},
};
use tracing::{debug, warn};

const GRACE: Duration = Duration::from_millis(super::CANCEL_GRACE_MS);
const OUTPUT_LIMIT: usize = 8 * 1024;
const STDERR_LIMIT: usize = 1024;

#[derive(Clone, Copy)]
enum SpawnIdentity {
    Account {
        uid: libc::uid_t,
        gid: libc::gid_t,
    },
    // There is no environment/configuration switch and no production variant.
    #[cfg(test)]
    CurrentProcessForUnitTests,
}

pub(super) struct Identities {
    actuator: SpawnIdentity,
    notify: SpawnIdentity,
}

impl Identities {
    pub(super) fn resolve() -> Result<Self> {
        let actuator = account(c"gruezi-actuator")?;
        let notify = account(c"gruezi-notify")?;
        // SAFETY: these getters have no arguments or side effects.
        let daemon = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        if actuator.0 == 0
            || actuator.1 == 0
            || notify.0 == 0
            || notify.1 == 0
            || actuator.0 == notify.0
            || actuator.1 == notify.1
            || [daemon.0, daemon.1].contains(&actuator.0)
            || [daemon.0, daemon.1].contains(&notify.0)
            || [daemon.2, daemon.3].contains(&actuator.1)
            || [daemon.2, daemon.3].contains(&notify.1)
        {
            bail!("lifecycle identities must be distinct, non-root and separate from the daemon");
        }
        Ok(Self {
            actuator: SpawnIdentity::Account {
                uid: actuator.0,
                gid: actuator.1,
            },
            notify: SpawnIdentity::Account {
                uid: notify.0,
                gid: notify.1,
            },
        })
    }

    #[cfg(test)]
    pub(super) const fn for_unit_tests() -> Self {
        Self {
            actuator: SpawnIdentity::CurrentProcessForUnitTests,
            notify: SpawnIdentity::CurrentProcessForUnitTests,
        }
    }
}

fn account(name: &CStr) -> Result<(libc::uid_t, libc::gid_t)> {
    let mut user = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut user_result = std::ptr::null_mut();
    let mut group_result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: all buffers and out-pointers are live and correctly sized. NSS
    // resolution happens in the parent, never in the post-fork child.
    let result = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            user.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut user_result,
        )
    };
    if result != 0 || user_result.is_null() {
        bail!(
            "lifecycle account {} unavailable (NSS error {result})",
            name.to_string_lossy()
        );
    }
    // SAFETY: successful getpwnam_r initialized user. Only copy scalar fields
    // before the scratch buffer is reused for group resolution.
    let user = unsafe { user.assume_init() };
    let uid = user.pw_uid;
    let user_gid = user.pw_gid;
    // SAFETY: same valid scratch/out-pointer contract as getpwnam_r above.
    let result = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            group.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut group_result,
        )
    };
    if result != 0 || group_result.is_null() {
        bail!(
            "lifecycle group {} unavailable (NSS error {result})",
            name.to_string_lossy()
        );
    }
    // SAFETY: successful getgrnam_r initialized group.
    let gid = unsafe { group.assume_init() }.gr_gid;
    if user_gid != gid {
        bail!("lifecycle account primary group differs from packaged group");
    }
    Ok((uid, gid))
}

fn isolate(command: &mut Command, identity: SpawnIdentity, actuator: bool) {
    // SAFETY: after fork this closure uses only checked, async-signal-safe
    // syscalls and errno access; no allocation, locks, NSS or tracing. Clear
    // supplementary groups BEFORE dropping gid/uid, then assert the resulting
    // real/effective identity. All inherited fds >=3 become CLOEXEC, including
    // Rust's spawn error pipe (it stays open until successful exec).
    unsafe {
        command.pre_exec(move || {
            match identity {
                SpawnIdentity::Account { uid, gid } => {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getuid() != uid
                        || libc::geteuid() != uid
                        || libc::getgid() != gid
                        || libc::getegid() != gid
                    {
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                }
                #[cfg(test)]
                SpawnIdentity::CurrentProcessForUnitTests => {}
            }
            // Linux resets dumpability on ordinary execve. This pre-exec setting
            // does not establish a post-exec non-dumpability guarantee; an
            // actuator executable must reassert it for that stronger claim.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || (actuator && libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0)
                || libc::syscall(
                    libc::SYS_close_range,
                    3_u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn command(settings: &Settings, job: &Job) -> Option<Command> {
    let mut command = match job.kind {
        LifecycleKind::Activate | LifecycleKind::Verify => {
            Command::new(&settings.lifecycle.activate_command)
        }
        LifecycleKind::Deactivate => Command::new(&settings.lifecycle.deactivate_command),
        LifecycleKind::Notify(event) => hook_command(
            settings.hooks.script_for(event)?,
            event,
            job.context.notification.as_ref()?,
        ),
    };
    // hook_command adds only documented GRUEZI_* context fields. Preserve those
    // explicit additions, then remove every inherited environment variable.
    let context_env: Vec<_> = command
        .as_std()
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect();
    command
        .env_clear()
        .envs(context_env)
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", "/nonexistent")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("GRUEZI_EVENT", job.kind.as_env_value())
        .env("GRUEZI_GENERATION", job.generation.to_string())
        .env("GRUEZI_INSTANCE_ID", &settings.instance_id)
        .env(
            "GRUEZI_AUTHORITY_SOURCE",
            job.context
                .authority_source
                .map_or("none", super::AuthoritySource::as_str),
        )
        .env(
            "GRUEZI_AUTHORITY_TERM",
            job.context.authority_term.to_string(),
        )
        .env("GRUEZI_TIMEOUT_MS", job_timeout(settings, job).to_string())
        .env("GRUEZI_NODE_ID", &settings.node_id)
        .env("GRUEZI_GROUP_ID", &settings.group_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if job.kind == LifecycleKind::Verify
        && let Some(incarnation) = &job.context.db_incarnation
    {
        command.env("GRUEZI_DB_INCARNATION", incarnation);
    }
    match job.operation {
        LifecycleOperation::Activate(mode) => {
            command.env(
                "GRUEZI_ACTIVATION_MODE",
                match mode {
                    ActivationMode::Handoff => "handoff",
                    ActivationMode::Cold => "cold",
                },
            );
        }
        LifecycleOperation::Deactivate(mode) => {
            command.env(
                "GRUEZI_DEACTIVATE_MODE",
                match mode {
                    DeactivationMode::Baseline => "baseline",
                    DeactivationMode::HandoffRelease => "handoff_release",
                    DeactivationMode::Revoke => "revoke",
                },
            );
        }
        LifecycleOperation::Verify | LifecycleOperation::Notify(_) => {}
    }
    let actuator = job.kind.critical();
    if actuator {
        command
            .env(
                "GRUEZI_TRANSFER_ID",
                job.context.transfer_id.unwrap_or(0).to_string(),
            )
            .env(
                "GRUEZI_PEER_NONCE",
                job.context
                    .peer_nonce
                    .map_or_else(String::new, super::nonce_hex),
            );
        if let Some(conf) = &settings.adapter_conf {
            command.env("GRUEZI_MARIADB_ADAPTER_CONF", conf);
        }
    }
    isolate(
        &mut command,
        if actuator {
            settings.identities.actuator
        } else {
            settings.identities.notify
        },
        actuator,
    );
    Some(command)
}

fn job_timeout(settings: &Settings, job: &Job) -> u64 {
    if job.kind.critical() {
        settings.lifecycle.timeout_ms
    } else {
        settings.hooks.timeout_ms
    }
}

/// Only signal groups created by `process_group(0)`. ESRCH is an explicit,
/// observable "already absent" result, never a silently discarded error.
fn signal_group(pgid: libc::pid_t, signal: i32) -> io::Result<bool> {
    // SAFETY: pgid is a checked positive child PID; killpg has no pointer args.
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        debug!(pgid, signal, "lifecycle process group already absent");
        Ok(false)
    } else {
        Err(error)
    }
}

async fn terminate(child: &mut Child, pgid: libc::pid_t) -> io::Result<ExitStatus> {
    signal_group(pgid, libc::SIGTERM)?;
    // Always give descendants the same bounded grace, even if the leader exits
    // first. A leader's exit alone is not evidence that its group is gone.
    sleep(GRACE).await;
    signal_group(pgid, libc::SIGKILL)?;
    child.wait().await
}

async fn observe_exit(pgid: libc::pid_t, exited: &mut Signal) -> io::Result<()> {
    let id = libc::id_t::try_from(pgid).map_err(io::Error::other)?;
    loop {
        let finished = {
            // SAFETY: zero is valid for siginfo_t; waitid writes this live object.
            // WNOWAIT retains the leader's zombie until group cleanup: reaping
            // first would permit PID/PGID reuse and could signal a new group.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            // SAFETY: id is our positive child PID and info is correctly sized.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    id,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: waitid initialized si_pid, including zero when not exited.
            (unsafe { info.si_pid() }) != 0
        };
        if finished {
            return Ok(());
        }
        exited.recv().await.ok_or(io::ErrorKind::BrokenPipe)?;
    }
}

#[derive(Default)]
struct Capture {
    bytes: VecDeque<u8>,
    total: usize,
}

async fn capture<R: AsyncRead + Unpin>(mut pipe: R) -> io::Result<Capture> {
    let mut capture = Capture::default();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(capture);
        }
        capture.total = capture.total.saturating_add(read);
        for byte in buffer.iter().take(read) {
            if capture.bytes.len() == OUTPUT_LIMIT {
                capture.bytes.pop_front();
            }
            capture.bytes.push_back(*byte);
        }
    }
}

fn stderr_tail(bytes: impl IntoIterator<Item = u8>) -> String {
    let bytes: Vec<_> = bytes.into_iter().collect();
    let clean: String = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let mut size = 0;
    let tail: Vec<_> = clean
        .chars()
        .rev()
        .take_while(|c| {
            size += c.len_utf8();
            size <= STDERR_LIMIT
        })
        .collect();
    tail.into_iter().rev().collect()
}

fn incarnation(output: &Capture) -> Option<String> {
    // Truncated stdout cannot prove exactly one receipt line.
    if output.total > OUTPUT_LIMIT {
        return None;
    }
    let bytes: Vec<_> = output.bytes.iter().copied().collect();
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut receipts = text
        .lines()
        .filter_map(|line| line.strip_prefix("db_incarnation="));
    let token = receipts.next()?;
    if token.is_empty()
        || token.len() > 256
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        || receipts.next().is_some()
    {
        return None;
    }
    Some(token.to_owned())
}

pub(super) async fn execute(
    settings: &Settings,
    job: &Job,
    cancel: &mut watch::Receiver<bool>,
    started: Instant,
) -> (LifecycleEvent, bool) {
    let mut event = job.event(LifecycleOutcome::Succeeded);
    event.started = Some(started);
    let mut deadline = started + Duration::from_millis(job_timeout(settings, job));
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
    let Some(mut command) = command(settings, job) else {
        return (event, true);
    };
    // Subscribe before spawning, so an immediate exit cannot be missed.
    let mut exited = match signal(SignalKind::child()) {
        Ok(exited) => exited,
        Err(error) => {
            event.outcome = LifecycleOutcome::SpawnError;
            event.stderr_tail = stderr_tail(error.to_string().bytes());
            return (event, true);
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            event.outcome = LifecycleOutcome::SpawnError;
            event.stderr_tail = stderr_tail(error.to_string().bytes());
            return (event, true);
        }
    };
    let Some(pgid) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
        event.outcome = LifecycleOutcome::SpawnError;
        "child PID unavailable".clone_into(&mut event.stderr_tail);
        if let Err(error) = child.kill().await {
            warn!(%error, "failed to reap child with unavailable PID");
        }
        return (event, false);
    };
    let (status, out, err, mut quiescent) =
        supervise(&mut child, pgid, cancel, &mut exited, deadline, &mut event).await;
    match status {
        Some(Ok(status)) => {
            event.exit_code_or_signal = status
                .code()
                .or_else(|| status.signal().map(|signal| -signal));
            if event.outcome == LifecycleOutcome::Succeeded && !status.success() {
                event.outcome = LifecycleOutcome::Failed;
            }
        }
        Some(Err(error)) => {
            warn!(%error, pgid, "lifecycle wait or process group termination failed");
            event.outcome = LifecycleOutcome::Failed;
            event.stderr_tail = stderr_tail(error.to_string().bytes());
            quiescent = false;
        }
        None => quiescent = false,
    }
    match err {
        Some(Ok(Ok(err))) if event.stderr_tail.is_empty() => {
            event.stderr_tail = stderr_tail(err.bytes);
        }
        Some(Ok(Ok(_))) => {}
        _ if event.outcome == LifecycleOutcome::Succeeded => {
            event.outcome = LifecycleOutcome::Failed;
        }
        _ => {}
    }
    match out {
        Some(Ok(Ok(out)))
            if event.outcome == LifecycleOutcome::Succeeded
                && matches!(job.kind, LifecycleKind::Activate | LifecycleKind::Verify) =>
        {
            event.db_incarnation = incarnation(&out);
        }
        Some(Ok(Ok(_))) => {}
        _ if event.outcome == LifecycleOutcome::Succeeded => {
            event.outcome = LifecycleOutcome::Failed;
        }
        _ => {}
    }
    (event, quiescent)
}

type Captured = Option<Result<io::Result<Capture>, tokio::task::JoinError>>;

async fn supervise(
    child: &mut Child,
    pgid: libc::pid_t,
    cancel: &mut watch::Receiver<bool>,
    exited: &mut Signal,
    deadline: Instant,
    event: &mut LifecycleEvent,
) -> (Option<io::Result<ExitStatus>>, Captured, Captured, bool) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout = tokio::spawn(async move {
        match stdout {
            Some(pipe) => capture(pipe).await,
            None => Ok(Capture::default()),
        }
    });
    let mut stderr = tokio::spawn(async move {
        match stderr {
            Some(pipe) => capture(pipe).await,
            None => Ok(Capture::default()),
        }
    });
    let deadline = sleep_until(deadline);
    tokio::pin!(deadline);
    let mut observed = None;
    let mut out = None;
    let mut err = None;
    // Store every completed handle once. Cancellation may arrive after one pipe
    // reached EOF; polling an already-consumed JoinHandle again would panic.
    let completed = loop {
        if observed.is_some() && out.is_some() && err.is_some() {
            break true;
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => break false,
            () = &mut deadline => {
                event.outcome = LifecycleOutcome::TimedOut;
                break false;
            }
            result = observe_exit(pgid, exited), if observed.is_none() => observed = Some(result),
            result = &mut stdout, if out.is_none() => out = Some(result),
            result = &mut stderr, if err.is_none() => err = Some(result),
        }
    };
    let mut quiescent = true;
    let status;
    if completed {
        // The job has completed: kill any redirected background children before
        // reaping the leader. Its retained PID prevents PGID reuse during kill.
        // The cancellation/timeout path below gives a still-running job grace.
        status = Some(match observed {
            Some(Ok(())) => match signal_group(pgid, libc::SIGKILL) {
                Ok(_) => child.wait().await,
                Err(error) => Err(error),
            },
            Some(Err(error)) => Err(error),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        });
    } else {
        if *cancel.borrow() {
            event.outcome = LifecycleOutcome::Cancelled;
        }
        status = Some(terminate(child, pgid).await);
        // A killed group closes its pipes. Bound even pipe draining: a script
        // that escapes its process group cannot hold this executor indefinitely.
        let captured = timeout(GRACE, async {
            if out.is_none() {
                out = Some((&mut stdout).await);
            }
            if err.is_none() {
                err = Some((&mut stderr).await);
            }
        })
        .await;
        if captured.is_err() {
            stdout.abort();
            stderr.abort();
            // Abort acknowledgements must be joined before reporting completion.
            if out.is_none() {
                out = Some(stdout.await);
            }
            if err.is_none() {
                err = Some(stderr.await);
            }
            quiescent = false;
        }
    }
    (status, out, err, quiescent)
}
