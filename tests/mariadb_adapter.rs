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
    AdapterFixture, LocalFixture, PeerFixture, Receipt, assert_no_secret, assert_no_write_sql,
    exit_code, stderr_str, stdout_str,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Output, time::Instant};

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
