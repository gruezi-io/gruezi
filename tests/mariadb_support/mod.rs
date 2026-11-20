//! Scripted-fake-`mariadb` harness for the MariaDB adapter integration tests.
//!
//! [`AdapterFixture`] builds a private DAC-correct layout (mode-0700
//! directories, mode-0600 option files carrying dummy credentials) and runs
//! `contrib/mariadb/gruezi-mariadb-adapter` against a fake client that logs
//! every argv and stdin to a call log. Assertions about credentials and about
//! read-only SQL are proven from that log.

use anyhow::{Result, anyhow};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

/// Path to the adapter under test (never copied; run in place).
pub const ADAPTER_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/contrib/mariadb/gruezi-mariadb-adapter"
);

const FAKE_MARIADB_SCRIPT: &str = include_str!("fake-mariadb.sh");

/// Credential planted in the local option file; must never reach argv, logs
/// or captured output.
pub const LOCAL_SECRET: &str = "LocalCredential-MustNeverAppear-zx91Q";
/// Credential planted in the peer option file.
pub const PEER_SECRET: &str = "PeerCredential-MustNeverAppear-wq42R";

/// The complete local fixture: server scalar row, three GTID vectors and a
/// `SHOW ALL SLAVES STATUS` block. Every field is individually overridable
/// through struct-update syntax so each test probes exactly one deviation.
pub struct LocalFixture {
    pub server_id: String,
    pub domain: String,
    pub strict: String,
    pub log_bin: String,
    pub log_slave_updates: String,
    pub binlog_format: String,
    pub slave_skip_errors: String,
    pub pid_file: String,
    pub binlog_pos: String,
    pub binlog_state: String,
    pub slave_pos: String,
    pub io_pos: String,
    pub io_running: String,
    pub sql_running: String,
    pub io_errno: String,
    pub sql_errno: String,
    pub using_gtid: String,
    pub connection_name: String,
    /// Value for the `Gtid_Slave_Pos` column: never evidence, so tests set it
    /// to lies on purpose.
    pub gtid_slave_pos_column: String,
    /// Value for `Seconds_Behind_Master`: banned as evidence.
    pub seconds_behind_master: String,
    /// Non-empty `Replicate_*` fields, e.g. `("Replicate_Do_DB", "lab")`.
    pub filters: Vec<(String, String)>,
    /// Emit a `*** 1. row ***` separator before the status fields.
    pub row_separator: bool,
}

impl LocalFixture {
    /// A healthy strict-pair replica: both domains present with their expected
    /// origins, both threads Yes, zero errnos, no relay backlog.
    ///
    /// `pid_file` is the value of `@@global.pid_file`; point it at a file
    /// holding a real pid to get a derived DB incarnation.
    #[must_use]
    pub fn healthy(pid_file: &str) -> Self {
        Self {
            server_id: "1".to_owned(),
            domain: "0".to_owned(),
            strict: "1".to_owned(),
            log_bin: "1".to_owned(),
            log_slave_updates: "1".to_owned(),
            binlog_format: "ROW".to_owned(),
            slave_skip_errors: "OFF".to_owned(),
            pid_file: pid_file.to_owned(),
            binlog_pos: "0-1-10,1-2-20".to_owned(),
            binlog_state: "0-1-1-10,1-2-1-20".to_owned(),
            slave_pos: "0-1-10,1-2-20".to_owned(),
            io_pos: "0-1-10,1-2-20".to_owned(),
            io_running: "Yes".to_owned(),
            sql_running: "Yes".to_owned(),
            io_errno: "0".to_owned(),
            sql_errno: "0".to_owned(),
            using_gtid: "Slave_Pos".to_owned(),
            connection_name: String::new(),
            gtid_slave_pos_column: String::new(),
            seconds_behind_master: "0".to_owned(),
            filters: Vec::new(),
            row_separator: true,
        }
    }

    /// Render scalar values and named vertical fields. The fake client applies
    /// the invocation's column-name option, just as MariaDB 11.8 does.
    #[must_use]
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n{}\n{}\n{}\n",
            self.server_id,
            self.domain,
            self.strict,
            self.log_bin,
            self.log_slave_updates,
            self.binlog_format,
            self.slave_skip_errors,
            self.pid_file,
            self.binlog_pos,
            self.binlog_state,
            self.slave_pos
        ));
        if self.row_separator {
            parts.push(
                "*************************** 1. row ***************************\n".to_owned(),
            );
        }
        parts.push(format!(
            "             Connection_name: {}\n",
            self.connection_name
        ));
        parts.push(format!(
            "             Slave_IO_Running: {}\n",
            self.io_running
        ));
        parts.push(format!(
            "            Slave_SQL_Running: {}\n",
            self.sql_running
        ));
        parts.push(format!("              Last_IO_Errno: {}\n", self.io_errno));
        parts.push(format!("             Last_SQL_Errno: {}\n", self.sql_errno));
        parts.push(format!("               Using_Gtid: {}\n", self.using_gtid));
        parts.push(format!("              Gtid_IO_Pos: {}\n", self.io_pos));
        parts.push(format!(
            "            Gtid_Slave_Pos: {}\n",
            self.gtid_slave_pos_column
        ));
        parts.push(format!(
            "       Seconds_Behind_Master: {}\n",
            self.seconds_behind_master
        ));
        for (field, value) in &self.filters {
            parts.push(format!("        {field}: {value}\n"));
        }
        parts.join("")
    }
}

/// Peer-side fixture for `internal-peer-vectors` (server identity + vectors).
pub struct PeerFixture {
    pub server_id: String,
    pub domain: String,
    pub binlog_pos: String,
    pub binlog_state: String,
    pub slave_pos: String,
}

impl PeerFixture {
    /// The healthy counterpart of [`LocalFixture::healthy`].
    #[must_use]
    pub fn healthy() -> Self {
        Self {
            server_id: "2".to_owned(),
            domain: "1".to_owned(),
            binlog_pos: "0-1-10,1-2-20".to_owned(),
            binlog_state: "0-1-1-10,1-2-1-20".to_owned(),
            slave_pos: "0-1-10,1-2-20".to_owned(),
        }
    }

    /// Render the peer read output.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{}\t{}\n{}\n{}\n{}\n",
            self.server_id, self.domain, self.binlog_pos, self.binlog_state, self.slave_pos
        )
    }
}

/// A complete private environment: DAC-correct config/state layout, the fake
/// client first on PATH, and a call log capturing every client invocation.
pub struct AdapterFixture {
    pub root: PathBuf,
    pub conf_path: PathBuf,
    pub state_dir: PathBuf,
    bin_dir: PathBuf,
    fixture_dir: PathBuf,
    call_log: PathBuf,
    pub local_option: PathBuf,
    peer_option: PathBuf,
    pub pid_file: PathBuf,
}

impl AdapterFixture {
    /// Build the fixture. Dirs are mode 0700, option files mode 0600 with
    /// planted credentials, and the config is the full closed key set.
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while building the fixture.
    pub fn new(name: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut attempts = 0_u32;
        let root = loop {
            let path = std::env::temp_dir().join(format!(
                "gruezi-mariadb-adapter-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempts += 1;
                    if attempts >= 16 {
                        return Err(error.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        let fixture = Self {
            conf_path: root.join("conf").join("adapter.conf"),
            state_dir: root.join("state"),
            bin_dir: root.join("bin"),
            fixture_dir: root.join("client-fixture"),
            call_log: root.join("call.log"),
            local_option: root.join("conf").join("local.cnf"),
            peer_option: root.join("conf").join("peer.cnf"),
            pid_file: root.join("db.pid"),
            root,
        };
        let conf_dir = fixture.root.join("conf");
        fs::create_dir_all(&conf_dir)?;
        fs::create_dir_all(&fixture.state_dir)?;
        fs::create_dir_all(&fixture.bin_dir)?;
        fs::create_dir_all(&fixture.fixture_dir)?;
        fs::set_permissions(&conf_dir, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&fixture.state_dir, fs::Permissions::from_mode(0o700))?;
        fs::write(
            &fixture.local_option,
            format!("[client]\nuser=actuator\npassword={LOCAL_SECRET}\nhost=127.0.0.1\n"),
        )?;
        fs::write(
            &fixture.peer_option,
            format!("[client]\nuser=monitor\npassword={PEER_SECRET}\nhost=127.0.0.2\n"),
        )?;
        fs::set_permissions(&fixture.local_option, fs::Permissions::from_mode(0o600))?;
        fs::set_permissions(&fixture.peer_option, fs::Permissions::from_mode(0o600))?;
        let fake = fixture.bin_dir.join("mariadb");
        fs::write(&fake, FAKE_MARIADB_SCRIPT)?;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755))?;
        fs::write(&fixture.call_log, "")?;
        let config = fixture.default_config();
        fixture.write_config(&config)?;
        Ok(fixture)
    }

    /// The full closed key set pointing at this fixture's paths.
    #[must_use]
    pub fn default_config(&self) -> String {
        format!(
            "OWN_SERVER_ID=1\nPEER_SERVER_ID=2\nOWN_DOMAIN=0\nPEER_DOMAIN=1\n\
             LOCAL_OPTION_FILE={}\nPEER_OPTION_FILE={}\nSTATE_DIR={}\n\
             CONNECT_TIMEOUT_S=2\nSTATEMENT_TIMEOUT_S=5\nCOMMAND_TIMEOUT_S=8\n\
             MAX_APPLY_WAIT_S=10\n",
            self.local_option.display(),
            self.peer_option.display(),
            self.state_dir.display()
        )
    }

    /// Replace the adapter config with arbitrary text (mode 0600).
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while writing the config.
    pub fn write_config(&self, text: &str) -> Result<()> {
        fs::write(&self.conf_path, text)?;
        fs::set_permissions(&self.conf_path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Install the local client response for `check`.
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while writing the fixture output.
    pub fn set_local_output(&self, text: &str) -> Result<()> {
        self.write_fixture_file("out.local.cnf", text)
    }

    /// Install the peer client response for `internal-peer-vectors`.
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while writing the fixture output.
    pub fn set_peer_output(&self, text: &str) -> Result<()> {
        self.write_fixture_file("out.peer.cnf", text)
    }

    /// Set the fake-client mode: "normal", "hang" or "fail".
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while writing the mode file.
    pub fn set_mode(&self, mode: &str) -> Result<()> {
        self.write_fixture_file("mode", mode)
    }

    fn write_fixture_file(&self, name: &str, text: &str) -> Result<()> {
        fs::write(self.fixture_dir.join(name), format!("{text}\n"))?;
        Ok(())
    }

    /// Run the adapter with the test guard enabled.
    ///
    /// # Errors
    ///
    /// Returns errors from spawning the adapter or decoding its output.
    pub fn run(&self, args: &[&str]) -> Result<Output> {
        self.run_inner(args, true, &[])
    }

    /// Run the adapter WITHOUT `GRUEZI_ADAPTER_TEST`: internal commands must
    /// be refused in this mode.
    ///
    /// # Errors
    ///
    /// Returns errors from spawning the adapter or decoding its output.
    pub fn run_unguarded(&self, args: &[&str]) -> Result<Output> {
        self.run_inner(args, false, &[])
    }

    /// Run a lifecycle event with the complete pinned environment; overrides
    /// replace individual fields to exercise malformed and stale inputs.
    pub fn event(
        &self,
        event: &str,
        generation: &str,
        overrides: &[(&str, &str)],
    ) -> Result<Output> {
        let mut vars = vec![("GRUEZI_EVENT", event)];
        vars.extend_from_slice(overrides);
        self.event_args(&[], generation, &vars)
    }

    /// Run explicit argv, or the packaged no-argument entrypoint with an event override.
    pub fn event_args(
        &self,
        args: &[&str],
        generation: &str,
        overrides: &[(&str, &str)],
    ) -> Result<Output> {
        let event = args.first().copied().unwrap_or("");
        let mut vars = vec![
            (
                "GRUEZI_EVENT",
                if event == "declare-authoritative" {
                    "activate"
                } else {
                    event
                },
            ),
            ("GRUEZI_GENERATION", generation),
            ("GRUEZI_INSTANCE_ID", "11111111111111111111111111111111"),
            ("GRUEZI_PEER_NONCE", "22222222222222222222222222222222"),
            ("GRUEZI_TRANSFER_ID", "9"),
            ("GRUEZI_AUTHORITY_SOURCE", "peer"),
            ("GRUEZI_AUTHORITY_TERM", "1"),
            ("GRUEZI_TIMEOUT_MS", "15000"),
            ("GRUEZI_ACTIVATION_MODE", "handoff"),
            ("GRUEZI_DEACTIVATE_MODE", "handoff_release"),
        ];
        vars.extend_from_slice(overrides);
        self.run_inner(args, !args.is_empty(), &vars)
    }

    /// Persist one fake-server control, independent of the adapter environment.
    pub fn control(&self, name: &str, value: &str) -> Result<()> {
        self.write_fixture_file(name, value)
    }

    /// Observe a fake-server value after a transition or compensation.
    pub fn control_value(&self, name: &str) -> Result<String> {
        Ok(fs::read_to_string(self.fixture_dir.join(name))?
            .trim()
            .to_owned())
    }

    /// Enable stateful fake SQL behavior for real lifecycle command tests.
    pub fn lifecycle(&self) -> Result<()> {
        self.control("lifecycle", "1")
    }

    /// Clear only the transcript, preserving all fake-server and adapter state.
    pub fn clear_log(&self) -> Result<()> {
        fs::write(&self.call_log, "")?;
        Ok(())
    }

    fn run_inner(&self, args: &[&str], guard: bool, vars: &[(&str, &str)]) -> Result<Output> {
        let mut path = self.bin_dir.display().to_string();
        if let Ok(old) = std::env::var("PATH") {
            path.push(':');
            path.push_str(&old);
        }
        let mut command = Command::new(ADAPTER_PATH);
        command
            .args(args)
            .env("GRUEZI_MARIADB_ADAPTER_CONF", &self.conf_path)
            .env("GRUEZI_FAKE_MARIADB_LOG", &self.call_log)
            .env("GRUEZI_FAKE_MARIADB_FIXTURE", &self.fixture_dir)
            .env("GRUEZI_FAKE_STATE_DIR", &self.state_dir)
            .env("PATH", &path)
            .env_remove("GRUEZI_EVENT")
            .env_remove("GRUEZI_INSTANCE_ID")
            .env_remove("GRUEZI_GENERATION");
        if guard {
            command.env("GRUEZI_ADAPTER_TEST", "1");
        } else {
            command.env_remove("GRUEZI_ADAPTER_TEST");
        }
        command.envs(vars.iter().copied());
        let output = command.output()?;
        Ok(output)
    }

    /// The accumulated fake-client call log (every argv + stdin).
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while reading the log.
    pub fn call_log(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.call_log)?)
    }
}

impl Drop for AdapterFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Exit code of a finished adapter run (missing code maps to -1).
#[must_use]
pub fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// Stdout of a finished run as UTF-8.
///
/// # Errors
///
/// Returns an error when stdout is not valid UTF-8.
pub fn stdout_str(output: &Output) -> Result<String> {
    Ok(std::str::from_utf8(&output.stdout)?.to_owned())
}

/// Stderr of a finished run as UTF-8.
///
/// # Errors
///
/// Returns an error when stderr is not valid UTF-8.
pub fn stderr_str(output: &Output) -> Result<String> {
    Ok(std::str::from_utf8(&output.stderr)?.to_owned())
}

/// Assert that no fake-client invocation carried a write statement in stdin.
/// `check` must be provably read-only from the call log alone.
///
/// # Errors
///
/// Returns an error naming the first offending statement.
pub fn assert_no_write_sql(call_log: &str) -> Result<()> {
    const FORBIDDEN: [&str; 12] = [
        "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT",
        "REVOKE", "RESET", "FLUSH",
    ];
    for (index, line) in call_log.lines().enumerate() {
        let statement = line.trim();
        for word in FORBIDDEN {
            // Word-boundary match so SHOW ... columns never trip the scan.
            let uppercase = statement.to_ascii_uppercase();
            if uppercase
                .split_whitespace()
                .any(|token| token.trim_matches(|c: char| !c.is_alphanumeric()) == word)
            {
                return Err(anyhow!(
                    "call log line {} contains write statement {}: {}",
                    index + 1,
                    word,
                    statement
                ));
            }
        }
    }
    Ok(())
}

/// Assert that no captured text contains either planted credential.
///
/// # Errors
///
/// Returns an error naming where the secret surfaced.
pub fn assert_no_secret(text: &str, label: &str) -> Result<()> {
    if text.contains(LOCAL_SECRET) {
        return Err(anyhow!("{label} leaked the local credential"));
    }
    if text.contains(PEER_SECRET) {
        return Err(anyhow!("{label} leaked the peer credential"));
    }
    Ok(())
}

/// sha256 hex digest of `input`, computed with the same `sha256sum` the
/// adapter uses.
///
/// # Errors
///
/// Returns errors from running `sha256sum` or parsing its output.
pub fn sha256_hex(input: &str) -> Result<String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    stdin.write_all(input.as_bytes())?;
    drop(stdin);
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let digest = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("sha256sum printed nothing"))?;
    Ok(digest.to_owned())
}

/// The single-row release receipt shape (parse + verify primitives only; the
/// write side belongs to the task that implements the barrier).
pub struct Receipt {
    pub transfer_id: String,
    pub server_id: String,
    pub gruezi_instance_id: String,
    pub generation: String,
    pub db_incarnation: String,
    pub target_binlog_state: String,
    pub target_binlog_pos: String,
    pub target_slave_pos: String,
    pub release_nonce: String,
    pub op_seq: String,
}

impl Receipt {
    /// A valid sample bound to the healthy pair identities.
    #[must_use]
    pub fn sample() -> Self {
        Self {
            transfer_id: "transfer-0001".to_owned(),
            server_id: "2".to_owned(),
            gruezi_instance_id: "instance-aaaa".to_owned(),
            generation: "7".to_owned(),
            db_incarnation: "2222-333333".to_owned(),
            target_binlog_state: "0-1-1-10,1-2-1-20".to_owned(),
            target_binlog_pos: "0-1-10,1-2-20".to_owned(),
            target_slave_pos: "0-1-10,1-2-20".to_owned(),
            release_nonce: "nonce-550e".to_owned(),
            op_seq: "5".to_owned(),
        }
    }

    /// The canonical field tuple: each field as `key=value\n` in fixed order.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "transfer_id={}\nserver_id={}\ngruezi_instance_id={}\ngeneration={}\n\
             db_incarnation={}\ntarget_binlog_state={}\ntarget_binlog_pos={}\n\
             target_slave_pos={}\nrelease_nonce={}\nop_seq={}\n",
            self.transfer_id,
            self.server_id,
            self.gruezi_instance_id,
            self.generation,
            self.db_incarnation,
            self.target_binlog_state,
            self.target_binlog_pos,
            self.target_slave_pos,
            self.release_nonce,
            self.op_seq
        )
    }

    /// The record digest over the canonical field tuple.
    ///
    /// # Errors
    ///
    /// Returns errors from `sha256sum`.
    pub fn digest(&self) -> Result<String> {
        sha256_hex(&self.canonical())
    }

    /// Write the receipt file; `digest_override` tampers with the digest
    /// while leaving the fields intact.
    ///
    /// # Errors
    ///
    /// Returns filesystem or digest errors.
    pub fn write(&self, path: &Path, digest_override: Option<&str>) -> Result<()> {
        let digest = match digest_override {
            Some(value) => value.to_owned(),
            None => self.digest()?,
        };
        fs::write(
            path,
            format!("{}record_digest={}\n", self.canonical(), digest),
        )?;
        Ok(())
    }
}
