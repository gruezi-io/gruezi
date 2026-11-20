//! Named integration tests for the MariaDB adapter foundation
//! (`contrib/mariadb/gruezi-mariadb-adapter`).
//!
//! Every test runs the real adapter against a scripted fake `mariadb` client
//! that logs each argv and stdin, so the safety claims (read-only `check`, no
//! credentials on argv) are proven from the call log rather than from prose.
//! GTID parsing is exercised through the adapter's parser itself under the
//! awk found on this host (mawk 1.3.4; the lab re-runs under gawk).

mod mariadb_support;

use anyhow::{Result, anyhow};
use mariadb_support::{
    ADAPTER_PATH, AdapterFixture, LocalFixture, PeerFixture, Receipt, assert_no_secret,
    assert_no_write_sql, exit_code, sha256_hex, stderr_str, stdout_str,
};
use std::{
    fmt::Write as _, fs, os::unix::fs::PermissionsExt, path::Path, process::Output, time::Instant,
};

fn combined(output: &Output) -> Result<String> {
    Ok(format!("{}{}", stdout_str(output)?, stderr_str(output)?))
}

/// Assert the exit code and return the combined output for reason checks.
fn expect_exit(output: &Output, want: i32) -> Result<String> {
    let got = exit_code(output);
    let text = combined(output)?;
    assert_eq!(got, want, "unexpected exit code; combined output:\n{text}");
    Ok(text)
}

fn healthy_local(fx: &AdapterFixture) -> LocalFixture {
    LocalFixture::healthy(&fx.pid_file.display().to_string())
}

fn install_healthy(fx: &AdapterFixture) -> Result<()> {
    fs::write(&fx.pid_file, format!("{}\n", std::process::id()))?;
    fx.set_local_output(&healthy_local(fx).render())?;
    fx.set_peer_output(&PeerFixture::healthy().render())?;
    Ok(())
}

fn assert_reason(text: &str, reason: &str) {
    assert!(
        text.contains(&format!("reason={reason}")),
        "expected reason={reason}; output:\n{text}"
    );
}

fn state_names(fx: &AdapterFixture) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(&fx.state_dir)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn receipt_verify(
    fx: &AdapterFixture,
    path: &Path,
    expect: &Receipt,
    mode: &str,
) -> Result<Output> {
    let file = format!("--file={}", path.display());
    fx.run(&[
        "internal-receipt-verify",
        &file,
        &format!("--expect-transfer-id={}", expect.transfer_id),
        &format!("--expect-server-id={}", expect.server_id),
        &format!("--expect-instance={}", expect.gruezi_instance_id),
        &format!("--expect-generation={}", expect.generation),
        &format!("--expect-incarnation={}", expect.db_incarnation),
        &format!("--expect-nonce={}", expect.release_nonce),
        &format!("--mode={mode}"),
    ])
}

#[test]
fn gtid_parse_boundaries() -> Result<()> {
    let fx = AdapterFixture::new("gtid-parse")?;
    // 2^64-1 accepted, 2^64 rejected: sequence bounds checked digit-wise.
    let out = fx.run(&[
        "internal-gtid-parse",
        "position",
        "1-2-18446744073709551615",
    ])?;
    let text = expect_exit(&out, 0)?;
    assert!(text.contains("vector=1-2-18446744073709551615-18446744073709551615"));
    expect_exit(
        &fx.run(&[
            "internal-gtid-parse",
            "position",
            "1-2-18446744073709551616",
        ])?,
        13,
    )?;
    // Leading zeros are non-canonical and rejected; empty is a valid empty set.
    expect_exit(&fx.run(&["internal-gtid-parse", "position", "01-2-3"])?, 13)?;
    expect_exit(
        &fx.run(&["internal-gtid-parse", "position", "1-2-003"])?,
        13,
    )?;
    let text = expect_exit(&fx.run(&["internal-gtid-parse", "position", ""])?, 0)?;
    assert!(text.contains("vector="));
    // Duplicate domains are parse errors in position vectors only.
    expect_exit(
        &fx.run(&["internal-gtid-parse", "position", "1-2-5,1-3-6"])?,
        13,
    )?;
    // gtid_binlog_state multi-origin structure parses; duplicated
    // (domain, server_id) pairs do not.
    let out = fx.run(&["internal-gtid-parse", "state", "0-1-1-10,0-2-1-5"])?;
    assert!(expect_exit(&out, 0)?.contains("vector=0-1-1-10,0-2-1-5"));
    expect_exit(
        &fx.run(&["internal-gtid-parse", "state", "0-1-1-10,0-1-12-15"])?,
        13,
    )?;
    // Bad separators and field counts.
    for bad in ["1:2-3", "1-2", "1-2-3-4-5", "1--2-3", "1-2-3,"] {
        expect_exit(&fx.run(&["internal-gtid-parse", "position", bad])?, 13)?;
    }
    // Domain and server_id bounds at 2^32-1 / 2^32.
    expect_exit(
        &fx.run(&["internal-gtid-parse", "position", "4294967295-4294967295-1"])?,
        0,
    )?;
    expect_exit(
        &fx.run(&["internal-gtid-parse", "position", "4294967296-1-1"])?,
        13,
    )?;
    expect_exit(
        &fx.run(&["internal-gtid-parse", "position", "1-4294967296-1"])?,
        13,
    )?;
    // Internal commands are refused without the test guard.
    let out = fx.run_unguarded(&["internal-gtid-parse", "position", "1-2-3"])?;
    expect_exit(&out, 64)?;
    Ok(())
}

#[test]
fn gtid_covers_cases() -> Result<()> {
    let fx = AdapterFixture::new("gtid-covers")?;
    let cases: [(&str, &str, &str); 7] = [
        // local, target, verdict
        ("1-2-5", "1-2-5", "covered=yes"),       // equal
        ("1-2-5", "1-2-6", "covered=no"),        // local behind target
        ("1-2-7", "1-2-5", "covered=yes"),       // local ahead of target
        ("0-1-5", "1-2-5", "covered=no"),        // missing target domain
        ("0-1-5,1-2-5", "1-2-5", "covered=yes"), // extra local domain
        ("1-3-5", "1-2-5", "covered=no"),        // wrong origin never covers
        ("1-2-1-5:7-9", "1-2-8", "covered=yes"),
    ];
    for (local, target, verdict) in cases {
        let out = fx.run(&[
            "internal-gtid-covers",
            "position",
            local,
            "position",
            target,
        ])?;
        let text = expect_exit(&out, 0)?;
        assert!(text.contains(verdict), "covers {local} vs {target}: {text}");
    }
    Ok(())
}

#[test]
fn check_requires_every_server_variable() -> Result<()> {
    let fx = AdapterFixture::new("check-vars")?;
    install_healthy(&fx)?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    let base = healthy_local(&fx);
    // Every scalar variable is required and must be parseable; pid_file may be
    // empty (incarnation UNKNOWN) but the other seven may not.
    let variants = vec![
        LocalFixture {
            server_id: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            domain: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            strict: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            log_bin: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            log_slave_updates: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            binlog_format: String::new(),
            ..healthy_local(&fx)
        },
        LocalFixture {
            slave_skip_errors: String::new(),
            ..healthy_local(&fx)
        },
    ];
    for variant in variants {
        fx.set_local_output(&variant.render())?;
        let out = fx.run(&["check"])?;
        let text = expect_exit(&out, 13)?;
        assert_reason(&text, "output_unparseable");
    }
    // A physically short scalar row is refused.
    fx.set_local_output("1\t0\tON\tON\tON\tROW\tOFF\n0-1-10,1-2-20\n")?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    // A missing vector line is refused.
    let mut lines: Vec<String> = base.render().lines().map(str::to_owned).collect();
    lines.remove(2);
    fx.set_local_output(&format!("{}\n", lines.join("\n")))?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    // An empty pid_file is legal and yields UNKNOWN, never an approximation.
    let no_pid = LocalFixture {
        pid_file: String::new(),
        ..healthy_local(&fx)
    };
    fx.set_local_output(&no_pid.render())?;
    let text = expect_exit(&fx.run(&["check"])?, 0)?;
    assert!(text.contains("db_incarnation=UNKNOWN"));
    Ok(())
}

#[test]
fn real_format_show_all_slaves_status_field_names() -> Result<()> {
    let fx = AdapterFixture::new("real-field-names")?;
    install_healthy(&fx)?;
    let local = LocalFixture {
        strict: "ON".to_owned(),
        log_bin: "ON".to_owned(),
        log_slave_updates: "ON".to_owned(),
        seconds_behind_master: "NULL".to_owned(),
        ..healthy_local(&fx)
    };
    fx.set_local_output(&local.render())?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    assert!(fx.call_log()?.contains("--column-names"));
    assert_no_write_sql(&fx.call_log()?)?;
    Ok(())
}

#[test]
fn real_format_numeric_booleans_accepted() -> Result<()> {
    for (strict, log_bin, updates, code, reason) in [
        ("1", "1", "1", 0, ""),
        ("0", "1", "1", 11, "gtid_strict_mode_violation"),
        ("1", "0", "1", 10, "log_bin_off"),
        ("1", "1", "0", 10, "log_slave_updates_off"),
        ("ON", "ON", "ON", 0, ""),
        ("OFF", "ON", "ON", 11, "gtid_strict_mode_violation"),
        ("ON", "OFF", "ON", 10, "log_bin_off"),
        ("ON", "ON", "OFF", 10, "log_slave_updates_off"),
    ] {
        let fx = AdapterFixture::new("real-booleans")?;
        install_healthy(&fx)?;
        fx.set_local_output(
            &LocalFixture {
                strict: strict.to_owned(),
                log_bin: log_bin.to_owned(),
                log_slave_updates: updates.to_owned(),
                ..healthy_local(&fx)
            }
            .render(),
        )?;
        let text = expect_exit(&fx.run(&["check"])?, code)?;
        if !reason.is_empty() {
            assert_reason(&text, reason);
        }
        let fx = lifecycle_fixture("real-snapshot-booleans")?;
        fx.control("snapshot_strict", strict)?;
        fx.control("snapshot_log_bin", log_bin)?;
        fx.control("snapshot_log_slave_updates", updates)?;
        expect_exit(&fx.event("activate", "1", &[])?, code)?;
    }
    Ok(())
}

#[test]
fn numeric_boolean_garbage_rejected() -> Result<()> {
    for bad in [
        "2", "-1", "01", "00", "on", "off", "true", "false", "yes", "no", "y", "n", "t", "f",
        "enabled", "disabled", "True", "YES", "NULL", "1 ", "",
    ] {
        for field in ["strict", "log_bin", "log_slave_updates"] {
            let fx = AdapterFixture::new("bad-boolean")?;
            install_healthy(&fx)?;
            let mut local = healthy_local(&fx);
            match field {
                "strict" => local.strict = bad.to_owned(),
                "log_bin" => local.log_bin = bad.to_owned(),
                _ => local.log_slave_updates = bad.to_owned(),
            }
            fx.set_local_output(&local.render())?;
            expect_exit(&fx.run(&["check"])?, 13)?;
            let fx = lifecycle_fixture("bad-snapshot-boolean")?;
            fx.control(&format!("snapshot_{field}"), bad)?;
            expect_exit(&fx.event("activate", "1", &[])?, 13)?;
            assert_no_enable(&fx)?;
        }
    }
    Ok(())
}

#[test]
fn real_client_session_statement_timeout_accepted() -> Result<()> {
    let fx = lifecycle_fixture("real-client-timeout")?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    let log = fx.call_log()?;
    let invocations: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("ARGV:"))
        .collect();
    assert_ne!(invocations.len(), 0);
    assert!(
        invocations
            .iter()
            .all(|line| line.contains("<--init-command=SET SESSION max_statement_time=5>"))
    );
    assert!(!log.contains("--max-statement-time="));
    Ok(())
}

#[test]
fn check_requires_both_threads_and_zero_errors() -> Result<()> {
    let fx = AdapterFixture::new("check-threads")?;
    install_healthy(&fx)?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    fx.set_local_output(
        &LocalFixture {
            sql_running: "No".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 10)?,
        "replica_sql_thread_not_running",
    );
    fx.set_local_output(
        &LocalFixture {
            io_running: "No".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 10)?,
        "replica_io_thread_not_running",
    );
    // Transient IO trouble is ineligible (exit 10), NOT a permanent latch:
    // healing must return to eligible with no operator clear (row 12 recovery).
    fx.set_local_output(
        &LocalFixture {
            io_errno: "5".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 10)?,
        "replication_error_io",
    );
    assert!(!fx.state_dir.join("quarantine.record").exists());
    install_healthy(&fx)?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    // A SQL error is permanent evidence and latches quarantine.
    fx.set_local_output(
        &LocalFixture {
            sql_errno: "5".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 11)?,
        "replication_error_sql",
    );
    assert!(fx.state_dir.join("quarantine.record").exists());
    Ok(())
}

#[test]
fn check_never_trusts_seconds_behind_master() -> Result<()> {
    let fx = AdapterFixture::new("check-sbm")?;
    install_healthy(&fx)?;
    // A huge Seconds_Behind_Master and a lying Gtid_Slave_Pos column change
    // nothing when the actual evidence is healthy.
    let lying = LocalFixture {
        seconds_behind_master: "99999".to_owned(),
        gtid_slave_pos_column: "1-2-1".to_owned(),
        ..healthy_local(&fx)
    };
    fx.set_local_output(&lying.render())?;
    let text = expect_exit(&fx.run(&["check"])?, 0)?;
    assert!(!text.contains("seconds_behind"));
    // A real relay backlog with Seconds_Behind_Master claiming 0 is still a
    // backlog: the verdict comes from Gtid_IO_Pos vs gtid_slave_pos.
    let backlog = LocalFixture {
        seconds_behind_master: "0".to_owned(),
        io_pos: "0-1-10,1-2-25".to_owned(),
        ..healthy_local(&fx)
    };
    fx.set_local_output(&backlog.render())?;
    let text = expect_exit(&fx.run(&["check"])?, 10)?;
    assert_reason(&text, "relay_backlog");
    assert!(text.contains("gtid_slave_pos=0-1-10,1-2-20"));
    Ok(())
}

#[test]
fn check_unparseable_output_is_exit_13() -> Result<()> {
    let fx = AdapterFixture::new("check-garbage")?;
    install_healthy(&fx)?;
    fx.set_local_output("banana\nnot a row at all\n")?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 13)?,
        "output_unparseable",
    );
    fx.set_local_output(
        &LocalFixture {
            binlog_pos: "1-2-3-".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 13)?, "gtid_parse_error");
    fx.set_mode("fail")?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    Ok(())
}

#[test]
fn check_timeout_is_exit_12_within_bound() -> Result<()> {
    let fx = AdapterFixture::new("check-timeout")?;
    let config = fx
        .default_config()
        .replace("COMMAND_TIMEOUT_S=8", "COMMAND_TIMEOUT_S=1");
    fx.write_config(&config)?;
    install_healthy(&fx)?;
    fx.set_mode("hang")?;
    let started = Instant::now();
    let out = fx.run(&["check"])?;
    let elapsed = started.elapsed();
    let text = expect_exit(&out, 12)?;
    assert_reason(&text, "mariadb_timeout");
    assert!(
        elapsed.as_secs() < 5,
        "timeout path took {elapsed:?}, must stay within the configured bound"
    );
    Ok(())
}

#[test]
fn quarantine_persists_across_invocations() -> Result<()> {
    let fx = AdapterFixture::new("quarantine-persist")?;
    install_healthy(&fx)?;
    // Multi-origin gtid_binlog_state is positive origin-conflict evidence.
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,0-2-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 11)?, "origin_conflict");
    let record_path = fx.state_dir.join("quarantine.record");
    let first = fs::read_to_string(&record_path)?;
    // A later healthy observation never clears the latch.
    install_healthy(&fx)?;
    let text = expect_exit(&fx.run(&["check"])?, 11)?;
    assert_reason(&text, "quarantine_latched");
    assert_eq!(fs::read_to_string(&record_path)?, first);
    Ok(())
}

#[test]
fn quarantine_clear_requires_flag_and_audits() -> Result<()> {
    let fx = AdapterFixture::new("quarantine-clear")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,0-2-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    expect_exit(&fx.run(&["check"])?, 11)?;
    let record_path = fx.state_dir.join("quarantine.record");
    // Missing flag, missing operator or extra junk: usage error, no mutation.
    for args in [
        vec!["quarantine-clear"],
        vec!["quarantine-clear", "--i-have-reconciled-history"],
        vec!["quarantine-clear", "--operator", "ops-42"],
        vec![
            "quarantine-clear",
            "--i-have-reconciled-history",
            "--operator",
            "ops-42",
            "--force",
        ],
    ] {
        expect_exit(&fx.run(&args)?, 64)?;
        assert!(record_path.exists());
    }
    // The only clear path: explicit flag AND audited operator identity.
    let out = fx.run(&[
        "quarantine-clear",
        "--i-have-reconciled-history",
        "--operator",
        "ops-42",
    ])?;
    expect_exit(&out, 0)?;
    assert!(!record_path.exists());
    let names = state_names(&fx)?;
    assert!(
        names.iter().any(|n| n.starts_with("quarantine.cleared.")),
        "rotated record missing; state: {names:?}"
    );
    let audit = fs::read_to_string(fx.state_dir.join("quarantine.audit"))?;
    assert!(audit.contains("event=cleared"));
    assert!(audit.contains("operator=ops-42"));
    assert!(audit.contains("prior_reason=origin_conflict"));
    expect_exit(&fx.run(&["quarantine-status"])?, 0)?;
    install_healthy(&fx)?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    Ok(())
}

#[test]
fn credentials_never_in_argv() -> Result<()> {
    let fx = AdapterFixture::new("credentials")?;
    install_healthy(&fx)?;
    let check = fx.run(&["check"])?;
    expect_exit(&check, 0)?;
    let peer = fx.run(&["internal-peer-vectors"])?;
    expect_exit(&peer, 0)?;
    let log = fx.call_log()?;
    assert!(log.contains("<--defaults-extra-file="));
    assert!(!log.contains("--password"));
    assert_no_secret(&log, "call log")?;
    assert_no_secret(&combined(&check)?, "check output")?;
    assert_no_secret(&combined(&peer)?, "peer output")?;
    // Read-only proof for `check`: the call log shows only SELECT/SHOW SQL.
    assert_no_write_sql(&log)?;
    Ok(())
}

#[test]
fn unreadable_or_world_readable_option_file_refused() -> Result<()> {
    let fx = AdapterFixture::new("option-mode")?;
    install_healthy(&fx)?;
    fs::set_permissions(&fx.local_option, fs::Permissions::from_mode(0o644))?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 13)?, "option_file_mode");
    fs::set_permissions(&fx.local_option, fs::Permissions::from_mode(0o000))?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 13)?, "option_file_mode");
    fs::set_permissions(&fx.local_option, fs::Permissions::from_mode(0o600))?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    Ok(())
}

#[test]
fn option_file_symlink_wrong_owner_or_group_access_refused() -> Result<()> {
    let fx = AdapterFixture::new("option-symlink")?;
    install_healthy(&fx)?;
    // A symlink to a perfectly good 0600 file is still refused.
    let real = fx.root.join("real.cnf");
    fs::write(&real, "[client]\n")?;
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600))?;
    fs::remove_file(&fx.local_option)?;
    std::os::unix::fs::symlink(&real, &fx.local_option)?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 13)?,
        "option_file_symlink",
    );
    fs::remove_file(&fx.local_option)?;
    fs::copy(&real, &fx.local_option)?;
    fs::set_permissions(&fx.local_option, fs::Permissions::from_mode(0o600))?;
    // Wrong owner: /etc/passwd is root-owned while the adapter runs as the
    // unprivileged test UID (the adapter refuses root outright).
    let config = fx.default_config().replace(
        &format!("LOCAL_OPTION_FILE={}", fx.local_option.display()),
        "LOCAL_OPTION_FILE=/etc/passwd",
    );
    fx.write_config(&config)?;
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 13)?,
        "option_file_owner_mismatch",
    );
    // Group access is refused even without a symlink or owner problem.
    fx.write_config(&fx.default_config())?;
    fs::set_permissions(&fx.local_option, fs::Permissions::from_mode(0o640))?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 13)?, "option_file_mode");
    Ok(())
}

#[test]
fn ordinary_replica_status_uses_separate_applied_position_query() -> Result<()> {
    let fx = AdapterFixture::new("separate-applied")?;
    install_healthy(&fx)?;
    // The Gtid_Slave_Pos column says "behind" while the separate
    // SELECT @@global.gtid_slave_pos says the truth (caught up): the verdict
    // must follow the separate query.
    fx.set_local_output(
        &LocalFixture {
            gtid_slave_pos_column: "1-2-1".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    let text = expect_exit(&fx.run(&["check"])?, 0)?;
    assert!(text.contains("gtid_slave_pos=0-1-10,1-2-20"));
    let log = fx.call_log()?;
    assert!(log.contains("SELECT @@global.gtid_slave_pos"));
    assert!(log.contains("SHOW ALL SLAVES STATUS"));
    Ok(())
}

#[test]
fn baseline_cleanup_preserves_unreplicated_dirty_history() -> Result<()> {
    let fx = AdapterFixture::new("dirty-baseline")?;
    install_healthy(&fx)?;
    let out = fx.run(&[
        "internal-dirty-open",
        "--id=d1",
        "--reason=enable",
        "--target-slave-pos=1-2-5",
    ])?;
    expect_exit(&out, 0)?;
    let out = fx.run(&["internal-baseline-cleanup"])?;
    let text = expect_exit(&out, 0)?;
    assert!(text.contains("preserved_dirty=1"));
    let record = fx.state_dir.join("dirty.d1.record");
    assert!(record.exists());
    // An open dirty record is latch-class evidence and survives evaluation.
    assert_reason(
        &expect_exit(&fx.run(&["check"])?, 11)?,
        "dirty_history_open",
    );
    assert!(record.exists());
    Ok(())
}

#[test]
fn gruezi_restart_same_db_keeps_dirty_history() -> Result<()> {
    let fx = AdapterFixture::new("dirty-restart")?;
    install_healthy(&fx)?;
    expect_exit(
        &fx.run(&[
            "internal-dirty-open",
            "--id=r1",
            "--reason=enable",
            "--target-slave-pos=1-2-5",
        ])?,
        0,
    )?;
    let first = expect_exit(&fx.run(&["check"])?, 11)?;
    // Gruezi-only restart: baseline cleanup + fresh invocations, same DB.
    expect_exit(&fx.run(&["internal-baseline-cleanup"])?, 0)?;
    let second = expect_exit(&fx.run(&["check"])?, 11)?;
    let incarnation = |text: &str| -> String {
        text.lines()
            .find(|line| line.starts_with("db_incarnation="))
            .unwrap_or("db_incarnation=MISSING")
            .to_owned()
    };
    let (a, b) = (incarnation(&first), incarnation(&second));
    assert_eq!(
        a, b,
        "DB incarnation must be stable within one server process"
    );
    assert!(first.contains("dirty_history=open"));
    assert!(second.contains("dirty_history=open"));
    assert!(fx.state_dir.join("dirty.r1.record").exists());
    Ok(())
}

#[test]
fn converged_vectors_never_clear_latched_overlap() -> Result<()> {
    let fx = AdapterFixture::new("converged")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,0-2-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    expect_exit(&fx.run(&["check"])?, 11)?;
    let record_path = fx.state_dir.join("quarantine.record");
    let latched = fs::read_to_string(&record_path)?;
    // Vectors converge to perfect equality: the latch still stands and the
    // record is untouched - GTID equality is not reconciliation.
    install_healthy(&fx)?;
    expect_exit(&fx.run(&["check"])?, 11)?;
    assert_eq!(fs::read_to_string(&record_path)?, latched);
    assert!(
        !state_names(&fx)?
            .iter()
            .any(|n| n.starts_with("quarantine.cleared."))
    );
    Ok(())
}

#[test]
fn configuration_is_data_not_shell_code() -> Result<()> {
    let fx = AdapterFixture::new("config-injection")?;
    install_healthy(&fx)?;
    let sentinel = fx.root.join("sentinel-pwned").display().to_string();
    let evil_values = [
        format!("OWN_SERVER_ID=$(touch {sentinel})"),
        format!("OWN_SERVER_ID=`touch {sentinel}`"),
        format!("OWN_SERVER_ID=1;touch {sentinel}"),
        format!("OWN_SERVER_ID=1 | touch {sentinel}"),
    ];
    for evil in evil_values {
        fx.write_config(&format!("{}\n{evil}\n", fx.default_config()))?;
        expect_exit(&fx.run(&["check"])?, 13)?;
        assert!(
            !Path::new(&sentinel).exists(),
            "config content executed: {evil}"
        );
    }
    // Unknown key, duplicate key and missing key are all refused.
    fx.write_config(&format!("{}EVIL_KEY=1\n", fx.default_config()))?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    fx.write_config(&format!("{}OWN_SERVER_ID=9\n", fx.default_config()))?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    let missing = fx.default_config().replace("PEER_DOMAIN=1\n", "");
    fx.write_config(&missing)?;
    expect_exit(&fx.run(&["check"])?, 13)?;
    // A symlinked config is refused even when its content is valid.
    let real = fx.root.join("real-adapter.conf");
    fs::write(&real, fx.default_config())?;
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600))?;
    fs::remove_file(&fx.conf_path)?;
    std::os::unix::fs::symlink(&real, &fx.conf_path)?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 13)?, "config_symlink");
    // Shell syntax inside a comment is inert data: config still loads.
    fs::remove_file(&fx.conf_path)?;
    fx.write_config(&format!("# $(touch {sentinel})\n{}", fx.default_config()))?;
    expect_exit(&fx.run(&["check"])?, 0)?;
    assert!(!Path::new(&sentinel).exists());
    Ok(())
}

#[test]
fn gtid_binlog_state_origin_conflict_quarantines() -> Result<()> {
    let fx = AdapterFixture::new("origin-conflict")?;
    install_healthy(&fx)?;
    // Same domain under two different server_ids: legitimate state structure,
    // but positive origin-conflict evidence for the strict pair model.
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,0-2-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 11)?, "origin_conflict");
    assert!(fx.state_dir.join("quarantine.record").exists());
    // A domain outside {OWN_DOMAIN, PEER_DOMAIN} is foreign history.
    let fx = AdapterFixture::new("origin-foreign")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,7-9-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    assert_reason(&expect_exit(&fx.run(&["check"])?, 11)?, "origin_conflict");
    // Wrong server_id inside a known domain.
    let fx = AdapterFixture::new("origin-wrong-server")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            binlog_state: "0-3-1-10,1-2-1-20".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    let text = expect_exit(&fx.run(&["check"])?, 11)?;
    assert_reason(&text, "origin_conflict");
    Ok(())
}

#[test]
fn log_slave_updates_off_is_flagged() -> Result<()> {
    let fx = AdapterFixture::new("log-slave-updates")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            log_slave_updates: "OFF".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    let text = expect_exit(&fx.run(&["check"])?, 10)?;
    assert!(text.contains("log_slave_updates=OFF"));
    assert!(text.contains("rpo0=unavailable"));
    assert_reason(&text, "log_slave_updates_off");
    // RPO-0 unavailability is a marker, not divergence evidence: no latch.
    assert!(!fx.state_dir.join("quarantine.record").exists());
    Ok(())
}

#[test]
fn gtid_strict_mode_off_is_reported_not_silently_accepted() -> Result<()> {
    let fx = AdapterFixture::new("strict-off")?;
    install_healthy(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            strict: "OFF".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    let text = expect_exit(&fx.run(&["check"])?, 11)?;
    assert!(text.contains("gtid_strict_mode=OFF"));
    assert_reason(&text, "gtid_strict_mode_violation");
    assert!(fx.state_dir.join("quarantine.record").exists());
    Ok(())
}

#[test]
fn receipt_digest_mismatch_refuses() -> Result<()> {
    let fx = AdapterFixture::new("receipt-digest")?;
    let path = fx.root.join("receipt.txt");
    let sample = Receipt::sample();
    let out = receipt_verify(&fx, &path, &sample, "read")?;
    expect_exit(&out, 13)?; // missing file is unparseable, not verified
    sample.write(
        &path,
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
    )?;
    assert_reason(
        &expect_exit(&receipt_verify(&fx, &path, &sample, "read")?, 13)?,
        "receipt_digest_mismatch",
    );
    // Control: the untampered receipt verifies.
    sample.write(&path, None)?;
    let text = expect_exit(&receipt_verify(&fx, &path, &sample, "read")?, 0)?;
    assert!(text.contains("receipt=verified"));
    // The production SQL carrier has the same integrity requirement.
    let fx = lifecycle_fixture("carrier-digest")?;
    let valid = carrier_row(&[])?;
    let (fields, _) = valid
        .rsplit_once('\t')
        .ok_or_else(|| anyhow!("no digest"))?;
    fx.control("receipt.peer.cnf", &format!("{fields}\t{}", "0".repeat(64)))?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)?;
    Ok(())
}

#[test]
fn receipt_rejects_wrong_identity_fields() -> Result<()> {
    let fx = AdapterFixture::new("receipt-identity")?;
    let path = fx.root.join("receipt.txt");
    let sample = Receipt::sample();
    sample.write(&path, None)?;
    let wrong = [
        ("transfer_id", "transfer-0002"),
        ("server_id", "3"),
        ("gruezi_instance_id", "instance-bbbb"),
        ("generation", "8"),
        ("db_incarnation", "9999-88888"),
        ("release_nonce", "nonce-other"),
    ];
    for (field, value) in wrong {
        let mut expect = Receipt::sample();
        match field {
            "transfer_id" => expect.transfer_id = value.to_owned(),
            "server_id" => expect.server_id = value.to_owned(),
            "gruezi_instance_id" => expect.gruezi_instance_id = value.to_owned(),
            "generation" => expect.generation = value.to_owned(),
            "db_incarnation" => expect.db_incarnation = value.to_owned(),
            "release_nonce" => expect.release_nonce = value.to_owned(),
            _ => return Err(anyhow!("unexpected receipt field {field}")),
        }
        let text = expect_exit(&receipt_verify(&fx, &path, &expect, "read")?, 13)?;
        assert_reason(&text, "receipt_identity_mismatch");
    }
    // An UNKNOWN incarnation cannot bind identity even with a valid digest.
    let unknown = Receipt {
        db_incarnation: "UNKNOWN".to_owned(),
        ..Receipt::sample()
    };
    unknown.write(&path, None)?;
    assert_reason(
        &expect_exit(&receipt_verify(&fx, &path, &unknown, "read")?, 13)?,
        "receipt_incarnation_unknown",
    );
    Ok(())
}

#[test]
fn receipt_op_seq_mutation_single_use_read_idempotent() -> Result<()> {
    let fx = AdapterFixture::new("receipt-opseq")?;
    let path = fx.root.join("receipt.txt");
    let sample = Receipt::sample();
    sample.write(&path, None)?;
    // Mutating consumption is strictly single-use.
    expect_exit(&receipt_verify(&fx, &path, &sample, "mutate")?, 0)?;
    assert_reason(
        &expect_exit(&receipt_verify(&fx, &path, &sample, "mutate")?, 13)?,
        "receipt_op_seq_stale",
    );
    // A strictly greater op_seq consumes again.
    let next = Receipt {
        op_seq: "6".to_owned(),
        ..Receipt::sample()
    };
    next.write(&path, None)?;
    expect_exit(&receipt_verify(&fx, &path, &next, "mutate")?, 0)?;
    // Read-only verification at the active operation generation is idempotent.
    expect_exit(&receipt_verify(&fx, &path, &next, "read")?, 0)?;
    // At the same op_seq but a different generation the read is stale.
    let other_generation = Receipt {
        generation: "8".to_owned(),
        op_seq: "6".to_owned(),
        ..Receipt::sample()
    };
    other_generation.write(&path, None)?;
    assert_reason(
        &expect_exit(&receipt_verify(&fx, &path, &other_generation, "read")?, 13)?,
        "receipt_op_seq_stale",
    );
    Ok(())
}

fn incarnation_line(text: &str) -> String {
    text.lines()
        .find(|line| line.starts_with("db_incarnation="))
        .unwrap_or("db_incarnation=MISSING")
        .to_owned()
}

#[test]
fn incarnation_unknown_is_unknown_not_approximated() -> Result<()> {
    let fx = AdapterFixture::new("incarnation")?;
    install_healthy(&fx)?;
    // A pid without a /proc entry: the identity is unavailable, so the result
    // is literally UNKNOWN - never a now-minus-uptime approximation.
    fs::write(&fx.pid_file, "4294967295\n")?;
    let text = expect_exit(&fx.run(&["check"])?, 0)?;
    assert_eq!(incarnation_line(&text), "db_incarnation=UNKNOWN");
    assert!(!text.contains("uptime"));
    // A live pid yields pid-starttime, stable across invocations.
    fs::write(&fx.pid_file, format!("{}\n", std::process::id()))?;
    let first = incarnation_line(&expect_exit(&fx.run(&["check"])?, 0)?);
    let second = incarnation_line(&expect_exit(&fx.run(&["check"])?, 0)?);
    assert_eq!(first, second);
    assert!(first.starts_with("db_incarnation="));
    let value = first.trim_start_matches("db_incarnation=");
    let mut parts = value.split('-');
    let pid = parts.next().unwrap_or("");
    let ticks = parts.next().unwrap_or("");
    assert!(!pid.is_empty() && !ticks.is_empty() && pid.chars().all(|c| c.is_ascii_digit()));
    assert!(ticks.chars().all(|c| c.is_ascii_digit()));
    // A different live pid is a different identity (what a restart changes).
    fs::write(&fx.pid_file, "1\n")?;
    let other = incarnation_line(&expect_exit(&fx.run(&["check"])?, 0)?);
    assert_ne!(first, other);
    Ok(())
}

/// Manual-QA observable: the healthy pair prints `eligible=yes` with every
/// verified variable; each failure fixture prints its specific nonzero exit
/// code and named reason; the call log proves `check` issued no write SQL.
#[test]
fn qa_eligibility_report_and_failure_reasons() -> Result<()> {
    let healthy = AdapterFixture::new("qa-healthy")?;
    install_healthy(&healthy)?;
    let out = healthy.run(&["check"])?;
    let text = expect_exit(&out, 0)?;
    println!(
        "== healthy pair check (exit {}) ==\n{text}",
        exit_code(&out)
    );
    assert!(text.contains("eligible=yes"));
    for needle in [
        "server_id=1",
        "gtid_domain_id=0",
        "gtid_strict_mode=ON",
        "log_slave_updates=ON",
        "binlog_format=ROW",
        "replica_io_running=Yes",
        "replica_sql_running=Yes",
        "rpo0=available",
    ] {
        assert!(text.contains(needle), "missing {needle} in report:\n{text}");
    }

    let stopped = AdapterFixture::new("qa-stopped")?;
    install_healthy(&stopped)?;
    stopped.set_local_output(
        &LocalFixture {
            sql_running: "No".to_owned(),
            ..healthy_local(&stopped)
        }
        .render(),
    )?;
    let out = stopped.run(&["check"])?;
    let text = expect_exit(&out, 10)?;
    println!(
        "== stopped SQL thread (exit {}) ==\n{text}",
        exit_code(&out)
    );
    assert_reason(&text, "replica_sql_thread_not_running");

    let filter = AdapterFixture::new("qa-filter")?;
    install_healthy(&filter)?;
    filter.set_local_output(
        &LocalFixture {
            filters: vec![("Replicate_Do_DB".to_owned(), "lab".to_owned())],
            ..healthy_local(&filter)
        }
        .render(),
    )?;
    let out = filter.run(&["check"])?;
    let text = expect_exit(&out, 10)?;
    println!(
        "== replication filter (exit {}) ==\n{text}",
        exit_code(&out)
    );
    assert_reason(&text, "replication_filter_set");

    let wrong = AdapterFixture::new("qa-wrong-domain")?;
    install_healthy(&wrong)?;
    wrong.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-10,0-2-1-5,1-2-1-20".to_owned(),
            ..healthy_local(&wrong)
        }
        .render(),
    )?;
    let out = wrong.run(&["check"])?;
    let text = expect_exit(&out, 11)?;
    println!(
        "== wrong/multi origin (exit {}) ==\n{text}",
        exit_code(&out)
    );
    assert_reason(&text, "origin_conflict");

    let malformed = AdapterFixture::new("qa-malformed")?;
    install_healthy(&malformed)?;
    malformed.set_local_output(
        &LocalFixture {
            binlog_state: "0-1-1-".to_owned(),
            ..healthy_local(&malformed)
        }
        .render(),
    )?;
    let out = malformed.run(&["check"])?;
    let text = expect_exit(&out, 13)?;
    println!("== malformed GTID (exit {}) ==\n{text}", exit_code(&out));
    assert_reason(&text, "gtid_parse_error");

    for fixture in [&healthy, &stopped, &filter, &wrong, &malformed] {
        let log = fixture.call_log()?;
        assert_no_write_sql(&log)?;
        assert_no_secret(&log, "call log")?;
        assert_no_secret(
            &combined(&fixture.run(&["quarantine-status"])?)?,
            "status output",
        )?;
    }
    Ok(())
}

fn carrier_row(overrides: &[(&str, &str)]) -> Result<String> {
    let state = "0-1-10,1-2-20";
    let incarnation = format!("2:1:90:{}", sha256_hex(state)?);
    let mut fields = vec![
        ("singleton_id", "1"),
        ("transfer_id", "9"),
        ("server_id", "2"),
        ("gruezi_instance_id", "22222222222222222222222222222222"),
        ("generation", "73"),
        ("db_incarnation", &incarnation),
        ("target_gtid_binlog_pos", "0-1-10,1-2-20"),
        ("target_gtid_current_pos", "0-1-10,1-2-20"),
        ("target_gtid_slave_pos", "0-1-10,1-2-20"),
        ("target_gtid_binlog_state", state),
        ("release_nonce", "33333333333333333333333333333333"),
        ("released_at_utc", "2026-11-19T12:00:00Z"),
    ];
    for (key, value) in &mut fields {
        if let Some((_, replacement)) = overrides.iter().find(|(name, _)| name == key) {
            *value = replacement;
        }
    }
    let mut canonical = String::new();
    for (key, value) in &fields {
        writeln!(canonical, "{key}={value}")?;
    }
    let mut values: Vec<&str> = fields.iter().map(|(_, value)| *value).collect();
    let digest = sha256_hex(&canonical)?;
    values.push(&digest);
    Ok(values.join("\t"))
}

fn lifecycle_fixture(name: &str) -> Result<AdapterFixture> {
    let fx = AdapterFixture::new(name)?;
    install_healthy(&fx)?;
    fx.lifecycle()?;
    fx.control("receipt.peer.cnf", &carrier_row(&[])?)?;
    Ok(fx)
}

fn activated(name: &str) -> Result<AdapterFixture> {
    let fx = lifecycle_fixture(name)?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    Ok(fx)
}

fn assert_no_enable(fx: &AdapterFixture) -> Result<()> {
    assert!(!fx.call_log()?.contains("SET GLOBAL read_only=0"));
    Ok(())
}

fn assert_dirty(fx: &AdapterFixture) -> Result<()> {
    assert!(
        state_names(fx)?
            .iter()
            .any(|name| name.starts_with("dirty.") && name.ends_with(".record"))
    );
    Ok(())
}

fn ordered(log: &str, needles: &[&str]) {
    let mut rest = log;
    for needle in needles {
        let position = rest.find(needle).unwrap_or_else(|| {
            // Assertion failure keeps the entire transcript visible.
            assert!(rest.contains(needle), "missing ordered {needle} in:\n{log}");
            0
        });
        rest = rest.get(position + needle.len()..).unwrap_or("");
    }
}

fn verify_active(fx: &AdapterFixture, incarnation: &str) -> Result<Output> {
    fx.event("verify", "1", &[("GRUEZI_DB_INCARNATION", incarnation)])
}

fn active_incarnation(fx: &AdapterFixture) -> Result<String> {
    let record = fs::read_to_string(fx.state_dir.join("active.record"))?;
    Ok(record
        .lines()
        .find_map(|line| line.strip_prefix("proc_incarnation="))
        .ok_or_else(|| anyhow!("no active incarnation"))?
        .to_owned())
}

#[test]
fn activate_sql_order_is_checks_then_wait_then_epoch_then_enable_then_readback() -> Result<()> {
    let fx = activated("activate-order")?;
    let log = fx.call_log()?;
    ordered(
        &log,
        &[
            "gtid_strict_mode",
            "FROM gruezi_ha.handoff_release",
            "MASTER_GTID_WAIT('1-2-20',10)",
            "SELECT CONNECTION_ID()",
            "EPOCH_BEFORE_ENABLE",
            "SET GLOBAL read_only=0",
            "SELECT 'enabled'",
            "SELECT @@global.server_id,@@global.gtid_domain_id,@@global.read_only",
        ],
    );
    assert!(log.contains("RECEIPT_CONSUMED_BEFORE_ENABLE"));
    assert_dirty(&fx)?;
    Ok(())
}

#[test]
fn activate_refuses_when_quarantined() -> Result<()> {
    let fx = lifecycle_fixture("activate-latch")?;
    fs::write(
        fx.state_dir.join("quarantine.record"),
        "reason=origin_conflict\n",
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 11)?;
    assert_no_enable(&fx)
}

#[test]
fn activate_refuses_unclosed_dirty_epoch_not_covered_by_peer() -> Result<()> {
    let fx = lifecycle_fixture("activate-dirty")?;
    expect_exit(
        &fx.run(&[
            "internal-dirty-open",
            "--id=unclosed",
            "--reason=enable",
            "--target-slave-pos=1-2-99",
        ])?,
        0,
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 11)?;
    assert_dirty(&fx)?;
    assert_no_enable(&fx)
}

#[test]
fn activate_peer_unreachable_or_writable_fails_closed() -> Result<()> {
    for (control, code) in [("peer_unreachable", 13), ("peer_writable", 10)] {
        let fx = lifecycle_fixture(control)?;
        fx.control(control, "1")?;
        expect_exit(&fx.event("activate", "1", &[])?, code)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn activate_wait_minus_one_or_null_fails_closed() -> Result<()> {
    for result in ["-1", "NULL", "0\n0", "garbage"] {
        let fx = lifecycle_fixture("wait-refuse")?;
        fx.control("wait_result", result)?;
        expect_exit(&fx.event("activate", "1", &[])?, 13)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn activate_missing_domain_or_unexpected_origin_quarantines() -> Result<()> {
    for state in ["0-1-1-10,1-3-1-20", "0-1-1-10,1-2-1-20,2-9-1-3"] {
        let fx = lifecycle_fixture("activate-origin")?;
        fx.set_local_output(
            &LocalFixture {
                binlog_state: state.to_owned(),
                ..healthy_local(&fx)
            }
            .render(),
        )?;
        expect_exit(&fx.event("activate", "1", &[])?, 11)?;
        assert!(fx.state_dir.join("quarantine.record").exists());
        assert_no_enable(&fx)?;
    }
    // Wait returning 0 cannot manufacture a missing domain.
    let fx = lifecycle_fixture("activate-missing-after-wait")?;
    fx.control(
        "after_wait.local",
        &LocalFixture {
            binlog_state: "0-1-1-10".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 11)?;
    assert_no_enable(&fx)
}

#[test]
fn activate_readback_mismatch_compensates_and_fails() -> Result<()> {
    let fx = lifecycle_fixture("activate-readback")?;
    fx.control("readback_mismatch", "1")?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    ordered(
        &fx.call_log()?,
        &[
            "SET GLOBAL read_only=0",
            "SELECT 'enabled'",
            "SET GLOBAL read_only=1",
        ],
    );
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("active.record").exists());
    Ok(())
}

#[test]
fn deactivate_kills_connections_not_just_queries() -> Result<()> {
    let fx = lifecycle_fixture("deactivate-kill")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 0)?;
    let log = fx.call_log()?;
    ordered(
        &log,
        &[
            "KILL CONNECTION 41",
            "SET GLOBAL read_only=1",
            "KILL CONNECTION 42",
            "INNODB_TRX",
            "XA RECOVER",
            "STOP REPLICA",
        ],
    );
    assert!(!log.contains("KILL QUERY"));
    Ok(())
}

#[test]
fn deactivate_open_transaction_blocks_success() -> Result<()> {
    let fx = activated("deactivate-trx")?;
    fx.control("transactions", "1")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 13)?;
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    assert!(!fx.call_log()?.contains("STOP REPLICA"));
    Ok(())
}

#[test]
fn deactivate_xa_blocks_success() -> Result<()> {
    let fx = activated("deactivate-xa")?;
    fx.control("xa", "123\t8\t8\tprepared")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 13)?;
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    Ok(())
}

#[test]
fn deactivate_dirty_epoch_closed_only_after_readback() -> Result<()> {
    let fx = activated("deactivate-close")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 0)?;
    assert!(
        !state_names(&fx)?
            .iter()
            .any(|name| name.starts_with("dirty."))
    );
    let log = fx.call_log()?;
    ordered(
        &log,
        &[
            "STOP REPLICA",
            "REPLACE INTO",
            "SELECT singleton_id",
            "GLOBAL_STATUS",
            "SELECT @@global.server_id,@@global.gtid_domain_id,@@global.read_only",
        ],
    );
    Ok(())
}

#[test]
fn fault_injection_refused_without_test_guard() -> Result<()> {
    let fx = lifecycle_fixture("guard")?;
    expect_exit(
        &fx.run_unguarded(&["internal-dirty-open", "--id=bad", "--reason=bad"])?,
        64,
    )?;
    assert_no_enable(&fx)
}

fn authorize(fx: &AdapterFixture) -> Result<()> {
    expect_exit(
        &fx.event_args(
            &[
                "declare-authoritative",
                "--i-accept-possible-data-loss",
                "--incident-id",
                "INC-19",
                "--reason",
                "Peer network unavailable",
                "--operator",
                "ops-42",
            ],
            "1",
            &[],
        )?,
        0,
    )?;
    Ok(())
}

#[test]
fn declare_authoritative_is_one_shot_and_audited() -> Result<()> {
    let fx = lifecycle_fixture("override-audit")?;
    authorize(&fx)?;
    fx.control("peer_unreachable", "1")?;
    let text = expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    assert!(text.contains("target=unknown") && text.contains("possible_data_loss=yes"));
    assert!(!fx.state_dir.join("authority.pending").exists());
    let audit = fs::read_to_string(fx.state_dir.join("quarantine.audit"))?;
    assert!(
        audit.contains("operator=ops-42")
            && audit.contains("incident=INC-19")
            && audit.contains("event=authority-consumed")
    );
    Ok(())
}

#[test]
fn override_never_bypasses_authority_quarantine_dirty_epoch_or_incarnation() -> Result<()> {
    for condition in [
        "authority",
        "quarantine",
        "dirty",
        "incarnation",
        "writable",
    ] {
        let fx = lifecycle_fixture(condition)?;
        authorize(&fx)?;
        fx.control("peer_unreachable", "1")?;
        let mut env = Vec::new();
        let expected = match condition {
            "authority" => {
                env.push(("GRUEZI_AUTHORITY_SOURCE", "none"));
                10
            }
            "quarantine" => {
                fs::write(
                    fx.state_dir.join("quarantine.record"),
                    "reason=origin_conflict\n",
                )?;
                11
            }
            "dirty" => {
                fs::write(fx.state_dir.join("dirty.prior.record"), "status=open\n")?;
                11
            }
            "incarnation" => {
                fs::write(&fx.pid_file, "1\n")?;
                13
            }
            "writable" => {
                fx.control("read_only", "0")?;
                10
            }
            _ => return Err(anyhow!("bad condition")),
        };
        expect_exit(&fx.event("activate", "1", &env)?, expected)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn override_consumed_before_enable_and_not_replayed() -> Result<()> {
    let fx = lifecycle_fixture("override-consume")?;
    authorize(&fx)?;
    fx.control("peer_unreachable", "1")?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    assert!(fx.call_log()?.contains("OVERRIDE_CONSUMED_BEFORE_ENABLE"));
    expect_exit(&fx.event("activate", "1", &[])?, 64)?;
    Ok(())
}

#[test]
fn deactivate_cancels_obsolete_adapter_sql_sessions() -> Result<()> {
    let fx = lifecycle_fixture("obsolete-session")?;
    expect_exit(
        &fx.event("deactivate", "1", &[("GRUEZI_DEACTIVATE_MODE", "revoke")])?,
        0,
    )?;
    ordered(
        &fx.call_log()?,
        &[
            "USER=SUBSTRING_INDEX(CURRENT_USER()",
            "KILL CONNECTION 41",
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST",
            "SET GLOBAL read_only=1",
        ],
    );
    Ok(())
}

#[test]
fn cooperative_handoff_preserves_last_acknowledged_token() -> Result<()> {
    let fx = activated("last-token")?;
    assert!(fx.call_log()?.contains("MASTER_GTID_WAIT('1-2-20',10)"));
    let text = fs::read_to_string(
        fx.state_dir
            .join("dirty.11111111111111111111111111111111.1.record"),
    )?;
    assert!(text.contains("target=0-1-10,1-2-20"));
    expect_exit(&fx.event("deactivate", "2", &[])?, 0)?;
    let receipt = fs::read_to_string(fx.state_dir.join("last_release.record"))?;
    assert!(receipt.contains("target_gtid_binlog_state=0-1-10,1-2-20"));
    Ok(())
}

#[test]
fn late_replication_invalidates_unproved_final_watermark() -> Result<()> {
    let fx = lifecycle_fixture("late-peer")?;
    fx.control("late_peer", "1")?;
    assert_reason(
        &expect_exit(&fx.event("activate", "1", &[])?, 13)?,
        "receipt_live_state_mismatch",
    );
    assert_no_enable(&fx)
}

#[test]
fn receipt_requires_live_state_equality() -> Result<()> {
    let fx = lifecycle_fixture("receipt-samples")?;
    fx.control(
        "receipt.peer.cnf",
        &carrier_row(&[("target_gtid_current_pos", "0-1-11,1-2-20")])?,
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn verify_requires_full_env_contract() -> Result<()> {
    let fx = activated("verify-env")?;
    let incarnation = active_incarnation(&fx)?;
    for name in [
        "GRUEZI_EVENT",
        "GRUEZI_GENERATION",
        "GRUEZI_INSTANCE_ID",
        "GRUEZI_TRANSFER_ID",
        "GRUEZI_AUTHORITY_SOURCE",
        "GRUEZI_AUTHORITY_TERM",
        "GRUEZI_TIMEOUT_MS",
        "GRUEZI_DB_INCARNATION",
    ] {
        expect_exit(
            &fx.event(
                "verify",
                "1",
                &[("GRUEZI_DB_INCARNATION", &incarnation), (name, "")],
            )?,
            64,
        )?;
    }
    Ok(())
}

#[test]
fn verify_call_log_contains_no_write_statement() -> Result<()> {
    let fx = activated("verify-readonly")?;
    let incarnation = active_incarnation(&fx)?;
    fx.clear_log()?;
    let text = expect_exit(&verify_active(&fx, &incarnation)?, 0)?;
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("db_incarnation="))
            .count(),
        1
    );
    let log = fx.call_log()?;
    assert_no_write_sql(&log)?;
    // The real client sets a session timeout via initialization; the old fake
    // accepted an unsupported flag. Exempt only that exact argv option, keeping
    // every SQL-body SET and every other session/global SET forbidden.
    let log = log.replace("<--init-command=SET SESSION max_statement_time=5>", "");
    for forbidden in [
        "SET ",
        "KILL ",
        "START ",
        "STOP ",
        "MASTER_GTID_WAIT",
        "peer.cnf",
    ] {
        assert!(!log.contains(forbidden), "verify issued {forbidden}");
    }
    Ok(())
}

#[test]
fn verify_incarnation_mismatch_exit_13() -> Result<()> {
    let fx = activated("verify-mismatch")?;
    expect_exit(&verify_active(&fx, "1-999999")?, 13)?;
    Ok(())
}

#[test]
fn verify_unknown_incarnation_exit_13() -> Result<()> {
    let fx = activated("verify-unknown")?;
    let incarnation = active_incarnation(&fx)?;
    fs::write(&fx.pid_file, "4294967295\n")?;
    expect_exit(&verify_active(&fx, &incarnation)?, 13)?;
    Ok(())
}

#[test]
fn verify_read_only_change_fails() -> Result<()> {
    let fx = activated("verify-read-only")?;
    let incarnation = active_incarnation(&fx)?;
    fx.control("read_only", "1")?;
    expect_exit(&verify_active(&fx, &incarnation)?, 10)?;
    Ok(())
}

#[test]
fn verify_timeout_exit_12_within_bound() -> Result<()> {
    let fx = activated("verify-timeout")?;
    let incarnation = active_incarnation(&fx)?;
    fx.set_mode("hang")?;
    let started = Instant::now();
    expect_exit(
        &fx.event(
            "verify",
            "1",
            &[
                ("GRUEZI_DB_INCARNATION", &incarnation),
                ("GRUEZI_TIMEOUT_MS", "150"),
            ],
        )?,
        12,
    )?;
    assert!(started.elapsed().as_secs() < 3);
    Ok(())
}

#[test]
fn verify_at_active_generation_accepted() -> Result<()> {
    let fx = activated("verify-idempotent")?;
    let incarnation = active_incarnation(&fx)?;
    expect_exit(&verify_active(&fx, &incarnation)?, 0)?;
    expect_exit(&verify_active(&fx, &incarnation)?, 0)?;
    Ok(())
}

#[test]
fn verify_runs_through_configured_activate_command() -> Result<()> {
    let fx = activated("verify-activate-command")?;
    let incarnation = active_incarnation(&fx)?;
    fx.clear_log()?;
    expect_exit(
        &fx.event_args(
            &["activate"],
            "1",
            &[
                ("GRUEZI_EVENT", "verify"),
                ("GRUEZI_DB_INCARNATION", &incarnation),
            ],
        )?,
        0,
    )?;
    let log = fx.call_log()?;
    // Permit only the real client's exact session-timeout initialization.
    let log = log.replace("<--init-command=SET SESSION max_statement_time=5>", "");
    assert!(!log.contains("SET ") && !log.contains("peer.cnf"));
    assert_no_write_sql(&log)
}

#[test]
fn stale_same_instance_generation_refused() -> Result<()> {
    let fx = activated("stale-generation")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 64)?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 0)?;
    Ok(())
}

#[test]
fn operation_record_binds_instance_generation_incarnation_and_conn_id() -> Result<()> {
    let fx = activated("operation-binding")?;
    let record = fs::read_to_string(fx.state_dir.join("operation.record"))?;
    assert!(record.contains("instance=11111111111111111111111111111111\n"));
    assert!(record.contains("generation=1\n"));
    assert!(record.contains(&format!("proc_incarnation={}\n", active_incarnation(&fx)?)));
    assert!(record.contains("connection_id=77\n"));
    Ok(())
}

#[test]
fn deactivate_lock_order_blocks_delayed_enable() -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let fx = lifecycle_fixture("operation-lock")?;
    let mut holder = Command::new("flock")
        .arg("-x")
        .arg(fx.state_dir.join("operation.lock"))
        .args(["bash", "-c", "printf 'READY\\n'; read -r release"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = holder
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no lock output"))?;
    let mut ready = String::new();
    BufReader::new(stdout).read_line(&mut ready)?;
    assert_eq!(ready, "READY\n");
    let out = fx.event("deactivate", "1", &[("GRUEZI_TIMEOUT_MS", "100")])?;
    // Release and reap even before asserting, so a failed assertion cannot
    // strand the operation lock.
    drop(holder.stdin.take());
    let _ = holder.wait()?;
    expect_exit(&out, 12)?;
    assert_eq!(fx.call_log()?, "");
    Ok(())
}

#[test]
fn delayed_enable_cannot_survive_deactivate() -> Result<()> {
    let fx = lifecycle_fixture("delayed-enable")?;
    fs::write(
        fx.state_dir.join("operation.record"),
        "instance=11111111111111111111111111111111\ngeneration=1\nconnection_id=41\n",
    )?;
    expect_exit(
        &fx.event("deactivate", "2", &[("GRUEZI_DEACTIVATE_MODE", "revoke")])?,
        0,
    )?;
    ordered(
        &fx.call_log()?,
        &[
            "KILL CONNECTION 41",
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST",
            "SET GLOBAL read_only=1",
            "INNODB_TRX",
            "XA RECOVER",
            "SELECT @@global.read_only",
        ],
    );
    assert_no_enable(&fx)
}

#[test]
fn cold_bootstrap_refuses_with_dirty_history() -> Result<()> {
    let fx = lifecycle_fixture("cold-dirty")?;
    fs::write(fx.state_dir.join("dirty.prior.record"), "status=open\n")?;
    let text = expect_exit(
        &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", "cold")])?,
        11,
    )?;
    assert!(!text.contains("event=enable_readback_verified"));
    assert_no_enable(&fx)
}

#[test]
fn start_replica_resumes_only_in_baseline_without_quarantine() -> Result<()> {
    for mode in ["baseline", "revoke", "handoff_release", "latched"] {
        let fx = lifecycle_fixture(mode)?;
        fx.control("threads", "No")?;
        if mode == "latched" {
            fs::write(
                fx.state_dir.join("quarantine.record"),
                "reason=origin_conflict\n",
            )?;
        }
        expect_exit(
            &fx.event(
                "deactivate",
                "1",
                &[(
                    "GRUEZI_DEACTIVATE_MODE",
                    if mode == "latched" { "baseline" } else { mode },
                )],
            )?,
            0,
        )?;
        assert_eq!(fx.call_log()?.contains("START REPLICA"), mode == "baseline");
    }
    Ok(())
}

#[test]
fn closed_epoch_clears_pending_dirty_marker_not_quarantine() -> Result<()> {
    let fx = activated("close-pending")?;
    expect_exit(&fx.run(&["check"])?, 11)?;
    assert!(fx.state_dir.join("dirty.pending").exists());
    fs::write(
        fx.state_dir.join("quarantine.record"),
        "reason=origin_conflict\n",
    )?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 0)?;
    assert!(!fx.state_dir.join("dirty.pending").exists());
    assert!(fx.state_dir.join("quarantine.record").exists());
    Ok(())
}

#[test]
fn unverified_closure_keeps_latch() -> Result<()> {
    let fx = activated("unverified-closure")?;
    expect_exit(&fx.run(&["check"])?, 11)?;
    fx.control("transactions", "1")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 13)?;
    assert!(fx.state_dir.join("dirty.pending").exists());
    assert_dirty(&fx)
}

#[test]
fn row_field_count_must_be_exact() -> Result<()> {
    for count in [7, 9] {
        let fx = AdapterFixture::new("row-fields")?;
        install_healthy(&fx)?;
        let original = healthy_local(&fx).render();
        let (_, remaining) = original
            .split_once('\n')
            .ok_or_else(|| anyhow!("no scalar row"))?;
        let scalar = if count == 7 {
            "1\t0\tON\tON\tON\tROW\tOFF".to_owned()
        } else {
            format!(
                "1\t0\tON\tON\tON\tROW\tOFF\t{}\textra",
                fx.pid_file.display()
            )
        };
        fx.set_local_output(&format!("{scalar}\n{remaining}"))?;
        expect_exit(&fx.run(&["check"])?, 13)?;
    }
    Ok(())
}

#[test]
fn receipt_rejects_consumed_instance_generation() -> Result<()> {
    let fx = lifecycle_fixture("receipt-replay")?;
    fs::write(
        fx.state_dir.join("consumed.keys"),
        "22222222222222222222222222222222:73\n",
    )?;
    assert_reason(
        &expect_exit(&fx.event("activate", "1", &[])?, 13)?,
        "receipt_already_consumed",
    );
    assert_no_enable(&fx)
}

#[test]
fn receipt_accepts_valid_predecessor_generation_independent_of_local() -> Result<()> {
    let fx = lifecycle_fixture("receipt-independent-generation")?;
    expect_exit(&fx.event("activate", "999", &[])?, 0)?;
    assert_eq!(
        fs::read_to_string(fx.state_dir.join("consumed.keys"))?,
        "22222222222222222222222222222222:73\n"
    );
    Ok(())
}

#[test]
fn receipt_binlog_state_column_drives_coverage() -> Result<()> {
    let fx = lifecycle_fixture("receipt-state-coverage")?;
    // All sampled positions still match. Only the origin-aware state frontier
    // is now beyond the successor, and MASTER_GTID_WAIT still returns zero.
    let target = "0-1-10,1-2-21";
    fx.set_peer_output(
        &PeerFixture {
            binlog_state: target.to_owned(),
            ..PeerFixture::healthy()
        }
        .render(),
    )?;
    let incarnation = format!("2:1:90:{}", sha256_hex(target)?);
    fx.control(
        "receipt.peer.cnf",
        &carrier_row(&[
            ("target_gtid_binlog_state", target),
            ("db_incarnation", &incarnation),
        ])?,
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 11)?;
    assert_no_enable(&fx)
}

#[test]
fn qa_activate_then_deactivate_ordered_sql_cycle() -> Result<()> {
    let fx = activated("qa-cycle")?;
    let output = fx.event("deactivate", "2", &[])?;
    println!(
        "== full activate then deactivate (exit {}) ==\n{}\n== exact ordered SQL transcript ==\n{}",
        exit_code(&output),
        combined(&output)?,
        fx.call_log()?
    );
    expect_exit(&output, 0)?;
    for fault in ["transactions", "xa", "readback_mismatch"] {
        let fx = lifecycle_fixture(fault)?;
        if fault == "readback_mismatch" {
            fx.control(fault, "1")?;
            let output = fx.event("activate", "1", &[])?;
            println!(
                "== {fault} exit {} ==\n{}",
                exit_code(&output),
                combined(&output)?
            );
            expect_exit(&output, 13)?;
        } else {
            expect_exit(&fx.event("activate", "1", &[])?, 0)?;
            fx.control(fault, "1")?;
            let output = fx.event("deactivate", "2", &[])?;
            println!(
                "== {fault} exit {} ==\n{}",
                exit_code(&output),
                combined(&output)?
            );
            expect_exit(&output, 13)?;
        }
        assert_dirty(&fx)?;
        assert!(!fx.state_dir.join("last_release.record").exists());
        assert_eq!(fx.control_value("read_only")?, "1");
        println!("failure={fault} read_only=1 dirty_epoch=retained release=withheld");
    }
    Ok(())
}

#[test]
fn override_refuses_observed_writable_peer() -> Result<()> {
    let fx = lifecycle_fixture("override-writable-peer")?;
    authorize(&fx)?;
    fx.control("peer_writable", "1")?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert!(fx.state_dir.join("authority.pending").exists());
    assert_no_enable(&fx)
}

#[test]
fn normal_handoff_never_consumes_operator_authorization() -> Result<()> {
    let fx = lifecycle_fixture("normal-with-override")?;
    authorize(&fx)?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    assert!(fx.state_dir.join("authority.pending").exists());
    assert!(!fx.state_dir.join("authority.consumed").exists());
    assert!(fx.call_log()?.contains("MASTER_GTID_WAIT"));
    Ok(())
}

#[test]
fn override_expiry_and_restart_identity_mismatch_refuse() -> Result<()> {
    for condition in ["expiry", "restart"] {
        let fx = lifecycle_fixture(condition)?;
        authorize(&fx)?;
        fx.control("peer_unreachable", "1")?;
        let path = fx.state_dir.join("authority.pending");
        let pending = fs::read_to_string(&path)?;
        let replacement = if condition == "expiry" {
            pending
                .lines()
                .map(|line| {
                    if line.starts_with("expires_ms=") {
                        "expires_ms=0"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            pending.replace(
                "11111111111111111111111111111111",
                "44444444444444444444444444444444",
            )
        };
        fs::write(path, replacement)?;
        expect_exit(&fx.event("activate", "1", &[])?, 13)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn override_after_consumption_failure_requires_new_authorization() -> Result<()> {
    let fx = lifecycle_fixture("override-cancelled-enable")?;
    authorize(&fx)?;
    fx.control("peer_unreachable", "1")?;
    fx.control("readback_mismatch", "1")?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert!(!fx.state_dir.join("authority.pending").exists());
    assert!(fx.state_dir.join("authority.consumed").exists());
    assert_dirty(&fx)?;
    expect_exit(&fx.event("activate", "2", &[])?, 11)?;
    Ok(())
}

#[test]
fn override_never_bypasses_sql_errors_or_configuration() -> Result<()> {
    let fx = lifecycle_fixture("override-sql-error")?;
    authorize(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            sql_errno: "123".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 11)?;
    assert_no_enable(&fx)?;
    let fx = lifecycle_fixture("override-config-error")?;
    authorize(&fx)?;
    fx.write_config(&format!("{}UNKNOWN_KEY=1\n", fx.default_config()))?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn permission_failure_never_counts_as_empty_success() -> Result<()> {
    for statement in ["PROCESSLIST", "INNODB_TRX", "XA RECOVER", "STOP REPLICA"] {
        let fx = lifecycle_fixture("permission-denied")?;
        fx.control("deny_sql", statement)?;
        expect_exit(&fx.event("deactivate", "1", &[])?, 13)?;
        assert!(!fx.state_dir.join("last_release.record").exists());
    }
    let fx = lifecycle_fixture("override-permission-denied")?;
    authorize(&fx)?;
    fx.control(
        "deny_sql",
        "SELECT @@global.server_id,@@global.gtid_domain_id,@@global.read_only;",
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn receipt_carrier_requires_exact_columns_and_one_row() -> Result<()> {
    let row = carrier_row(&[])?;
    for invalid in [
        row.split('\t').take(12).collect::<Vec<_>>().join("\t"),
        format!("{row}\textra"),
        format!("{row}\n{row}"),
        String::new(),
    ] {
        let fx = lifecycle_fixture("carrier-shape")?;
        fx.control("receipt.peer.cnf", &invalid)?;
        expect_exit(&fx.event("activate", "1", &[])?, 13)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn receipt_rejects_wrong_instance_generation_or_incarnation() -> Result<()> {
    for (key, value) in [
        ("gruezi_instance_id", "44444444444444444444444444444444"),
        ("generation", "0"),
        ("generation", "18446744073709551616"),
        ("db_incarnation", "2:1:1000:unknown"),
        ("transfer_id", "10"),
        ("server_id", "3"),
    ] {
        let fx = lifecycle_fixture("receipt-bindings")?;
        fx.control("receipt.peer.cnf", &carrier_row(&[(key, value)])?)?;
        expect_exit(&fx.event("activate", "1", &[])?, 13)?;
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn peer_restart_invalidates_incarnation_binding() -> Result<()> {
    let fx = lifecycle_fixture("peer-restart")?;
    fx.control("peer_uptime", "1")?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn peer_read_only_without_applier_quiescence_is_not_final() -> Result<()> {
    let fx = lifecycle_fixture("peer-applier-live")?;
    fx.control("peer_threads", "Yes")?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn release_barrier_stops_appliers_and_blocks_late_commit() -> Result<()> {
    let fx = activated("release-appliers")?;
    fx.clear_log()?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 0)?;
    ordered(
        &fx.call_log()?,
        &[
            "SET GLOBAL read_only=1",
            "INNODB_TRX",
            "XA RECOVER",
            "STOP REPLICA",
            "GLOBAL_STATUS",
            "REPLACE INTO",
        ],
    );
    Ok(())
}

#[test]
fn release_barrier_readback_precedes_capture() -> Result<()> {
    let fx = lifecycle_fixture("barrier-readback")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 0)?;
    ordered(
        &fx.call_log()?,
        &[
            "SET GLOBAL read_only=1",
            "SELECT @@global.read_only",
            "XA RECOVER",
            "SELECT @@global.read_only",
            "STOP REPLICA",
            "SELECT @@global.gtid_binlog_pos",
        ],
    );
    Ok(())
}

#[test]
fn release_never_changes_event_scheduler() -> Result<()> {
    let fx = activated("event-scheduler")?;
    fx.control("event_scheduler", "ON")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 10)?;
    assert!(!fx.call_log()?.contains("SET GLOBAL event_scheduler"));
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    Ok(())
}

#[test]
fn cold_bootstrap_requires_no_prior_write_episode_and_converged_pair() -> Result<()> {
    let fx = lifecycle_fixture("cold-new-pair")?;
    fx.control("receipt.peer.cnf", "")?;
    let text = expect_exit(
        &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", "cold")])?,
        0,
    )?;
    assert!(text.contains("target=unknown") && text.contains("rpo=unknown"));
    let diagnostic = text
        .lines()
        .find(|line| line.starts_with("event=enable_readback_verified "))
        .ok_or_else(|| anyhow!("missing verified enable readback diagnostic"))?;
    assert!(diagnostic.contains("read_only=0 incarnation_matched=yes"));
    let timestamp = diagnostic
        .split_whitespace()
        .find_map(|field| field.strip_prefix("realtime_ns="))
        .ok_or_else(|| anyhow!("missing readback timestamp"))?;
    assert_eq!(timestamp.len(), 19);
    assert!(timestamp.bytes().all(|byte| byte.is_ascii_digit()));
    let fx = lifecycle_fixture("cold-prior-episode")?;
    fx.control("receipt.peer.cnf", "")?;
    fs::write(
        fx.state_dir.join("last_release.record"),
        "prior_write=yes\n",
    )?;
    expect_exit(
        &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", "cold")])?,
        10,
    )?;
    assert_no_enable(&fx)?;
    let fx = lifecycle_fixture("cold-unconverged")?;
    fx.control("receipt.peer.cnf", "")?;
    fx.set_peer_output(
        &PeerFixture {
            binlog_state: "0-1-1-10,1-2-1-21".to_owned(),
            ..PeerFixture::healthy()
        }
        .render(),
    )?;
    expect_exit(
        &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", "cold")])?,
        10,
    )?;
    assert_no_enable(&fx)
}

#[test]
fn baseline_and_revoke_preserve_dirty_epoch() -> Result<()> {
    for mode in ["baseline", "revoke"] {
        let fx = activated(mode)?;
        expect_exit(
            &fx.event("deactivate", "2", &[("GRUEZI_DEACTIVATE_MODE", mode)])?,
            0,
        )?;
        assert_dirty(&fx)?;
        assert!(!fx.state_dir.join("last_release.record").exists());
    }
    Ok(())
}

#[test]
fn baseline_deactivate_accepts_generation_zero() -> Result<()> {
    const BASELINE: &[(&str, &str)] = &[("GRUEZI_DEACTIVATE_MODE", "baseline")];
    // Startup baseline runs before any ownership generation exists.
    let fx = activated("baseline-gen-zero")?;
    let ledger = fx.state_dir.join("generation.record");
    let before = fs::read(&ledger)?;
    expect_exit(&fx.event("deactivate", "0", BASELINE)?, 0)?;
    // Full baseline cleanup still ran; dirty history stays and no receipt exists.
    assert!(fx.call_log()?.contains("SET GLOBAL read_only=1"));
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    // Generation 0 never touches the strictly-increasing ledger.
    assert_eq!(before, fs::read(&ledger)?);
    // Replaying the already-recorded generation 1 is still refused.
    let replay = fx.event("activate", "1", &[])?;
    assert_reason(&expect_exit(&replay, 64)?, "stale_generation");
    // A fresh state stays ledger-less after the baseline call.
    let fresh = lifecycle_fixture("baseline-gen-zero-fresh")?;
    expect_exit(&fresh.event("deactivate", "0", BASELINE)?, 0)?;
    assert!(!fresh.state_dir.join("generation.record").exists());
    Ok(())
}

#[test]
fn activate_refuses_generation_zero() -> Result<()> {
    for mode in ["handoff", "cold"] {
        let fx = lifecycle_fixture("activate-gen-zero")?;
        let out = fx.event("activate", "0", &[("GRUEZI_ACTIVATION_MODE", mode)])?;
        assert_reason(&expect_exit(&out, 64)?, "generation_invalid");
        assert_no_enable(&fx)?;
    }
    Ok(())
}

#[test]
fn handoff_release_deactivate_refuses_generation_zero() -> Result<()> {
    let fx = activated("release-gen-zero")?;
    let out = fx.event("deactivate", "0", &[])?;
    assert_reason(&expect_exit(&out, 64)?, "generation_invalid");
    Ok(())
}

#[test]
fn revoke_deactivate_refuses_generation_zero() -> Result<()> {
    let fx = activated("revoke-gen-zero")?;
    let out = fx.event("deactivate", "0", &[("GRUEZI_DEACTIVATE_MODE", "revoke")])?;
    assert_reason(&expect_exit(&out, 64)?, "generation_invalid");
    Ok(())
}

#[test]
fn verify_refuses_generation_zero() -> Result<()> {
    let fx = activated("verify-gen-zero")?;
    let incarnation = active_incarnation(&fx)?;
    let out = fx.event("verify", "0", &[("GRUEZI_DB_INCARNATION", &incarnation)])?;
    assert_reason(&expect_exit(&out, 64)?, "generation_invalid");
    Ok(())
}

#[test]
fn generation_after_baseline_zero_still_accepted() -> Result<()> {
    let fx = lifecycle_fixture("after-baseline-zero")?;
    expect_exit(
        &fx.event("deactivate", "0", &[("GRUEZI_DEACTIVATE_MODE", "baseline")])?,
        0,
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    Ok(())
}

#[test]
fn obsolete_sessions_must_be_observed_absent() -> Result<()> {
    let fx = lifecycle_fixture("obsolete-remains")?;
    fx.control("sessions_remaining", "1")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 13)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    assert!(fx.call_log()?.contains("SET GLOBAL read_only=1"));
    Ok(())
}

#[test]
fn failed_receipt_readback_withholds_receipt_and_retains_epoch() -> Result<()> {
    let fx = activated("receipt-publish-failure")?;
    fx.control("receipt_readback_corrupt", "1")?;
    expect_exit(&fx.event("deactivate", "2", &[])?, 13)?;
    assert_dirty(&fx)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    ordered(
        &fx.call_log()?,
        &[
            "REPLACE INTO",
            "SELECT singleton_id",
            "DELETE FROM gruezi_ha.handoff_release",
        ],
    );
    Ok(())
}

#[test]
fn cold_empty_pair_can_form_without_inventing_history() -> Result<()> {
    let fx = lifecycle_fixture("cold-empty")?;
    fx.control("receipt.peer.cnf", "")?;
    fx.set_local_output(
        &LocalFixture {
            binlog_state: String::new(),
            binlog_pos: String::new(),
            slave_pos: String::new(),
            io_pos: String::new(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    fx.set_peer_output(
        &PeerFixture {
            binlog_state: String::new(),
            binlog_pos: String::new(),
            slave_pos: String::new(),
            ..PeerFixture::healthy()
        }
        .render(),
    )?;
    let text = expect_exit(
        &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", "cold")])?,
        0,
    )?;
    assert!(text.contains("target=unknown") && text.contains("rpo=unknown"));
    assert!(!fx.call_log()?.contains("MASTER_GTID_WAIT"));
    Ok(())
}

#[test]
fn post_wait_missing_fields_cannot_reuse_prior_replica_state() -> Result<()> {
    let fx = lifecycle_fixture("post-wait-missing")?;
    let malformed = healthy_local(&fx)
        .render()
        .lines()
        .filter(|line| !line.contains("Slave_SQL_Running"))
        .collect::<Vec<_>>()
        .join("\n");
    fx.control("after_wait.local", &malformed)?;
    expect_exit(&fx.event("activate", "1", &[])?, 13)?;
    assert_no_enable(&fx)
}

#[test]
fn override_can_bypass_only_clean_sql_thread_io_connectivity() -> Result<()> {
    let fx = lifecycle_fixture("override-io-only")?;
    authorize(&fx)?;
    fx.set_local_output(
        &LocalFixture {
            io_running: "Connecting".to_owned(),
            io_errno: "2003".to_owned(),
            ..healthy_local(&fx)
        }
        .render(),
    )?;
    expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    assert!(fx.call_log()?.contains("OVERRIDE_CONSUMED_BEFORE_ENABLE"));
    assert!(!fx.call_log()?.contains("MASTER_GTID_WAIT"));
    Ok(())
}

#[test]
fn override_unavailable_target_wait_never_claims_coverage() -> Result<()> {
    let fx = lifecycle_fixture("override-wait")?;
    authorize(&fx)?;
    fx.control("wait_result", "-1")?;
    let text = expect_exit(&fx.event("activate", "1", &[])?, 0)?;
    assert!(text.contains("target=unknown") && text.contains("rpo=unknown"));
    assert!(fx.state_dir.join("authority.consumed").exists());
    assert!(!fx.state_dir.join("consumed.keys").exists());
    Ok(())
}

#[test]
fn adapter_file_is_executable() -> Result<()> {
    // Tests launch through `bash <path>`, which hides a lost executable bit;
    // the packaged asset is mode 0755, so pin it directly.
    let mode = fs::metadata(ADAPTER_PATH)?.permissions().mode();
    assert_eq!(mode & 0o755, 0o755, "adapter mode is {mode:o}");
    Ok(())
}

#[test]
fn deactivate_exempts_readonly_monitor_sessions() -> Result<()> {
    let fx = lifecycle_fixture("monitor-exempt")?;
    fx.control("monitor_session", "1")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 0)?;
    let log = fx.call_log()?;
    assert!(
        log.contains("USER<>'monitor'"),
        "app kill must name the exemption"
    );
    assert!(
        !log.contains("KILL CONNECTION 43"),
        "monitor session was killed"
    );
    // A write-capable session that cannot be proven absent still fails closed.
    let fx = lifecycle_fixture("monitor-exempt-remaining")?;
    fx.control("monitor_session", "1")?;
    fx.control("sessions_remaining", "1")?;
    expect_exit(&fx.event("deactivate", "1", &[])?, 13)?;
    assert!(!fx.state_dir.join("last_release.record").exists());
    Ok(())
}

#[test]
fn verify_rejects_non_active_generation() -> Result<()> {
    let fx = activated("verify-wrong-generation")?;
    let incarnation = active_incarnation(&fx)?;
    for generation in ["2", "999"] {
        let out = fx.event(
            "verify",
            generation,
            &[("GRUEZI_DB_INCARNATION", &incarnation)],
        )?;
        assert_reason(&expect_exit(&out, 64)?, "stale_generation");
    }
    expect_exit(&verify_active(&fx, &incarnation)?, 0)?;
    Ok(())
}
