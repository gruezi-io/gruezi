mod common;

use anyhow::{Result, anyhow};
use common::FakeIp;
use std::{fs, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{Duration, timeout},
};

async fn run(fixture: &FakeIp, name: &str, args: &[&str]) -> Result<std::process::Output> {
    Ok(timeout(
        Duration::from_secs(5),
        Command::new(fixture.command(name))
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await??)
}

async fn change(fixture: &FakeIp, operation: &str, address: &str) -> Result<std::process::Output> {
    run(
        fixture,
        "ip",
        &["address", operation, address, "dev", "eth0"],
    )
    .await
}

async fn show(fixture: &FakeIp, interface: &str) -> Result<String> {
    let output = run(fixture, "ip", &["-o", "address", "show", "dev", interface]).await?;
    assert!(output.status.success());
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn address_table_reports_duplicates_missing_deletes_and_flags() -> Result<()> {
    let fixture = FakeIp::new()?;
    assert_eq!(
        run(&fixture, "ip", &["--invalid"]).await?.status.code(),
        Some(2)
    );
    assert!(
        change(&fixture, "add", "10.0.0.10/24")
            .await?
            .status
            .success()
    );
    let duplicate = change(&fixture, "add", "10.0.0.10/24").await?;
    assert!(!duplicate.status.success());
    assert!(String::from_utf8(duplicate.stderr)?.contains("File exists"));
    assert!(
        change(&fixture, "add", "fd00::10/64")
            .await?
            .status
            .success()
    );
    fs::write(
        fixture.dir.join("flags"),
        "eth0 fd00::10/64 tentative dadfailed\n",
    )?;
    let output = show(&fixture, "eth0").await?;
    let rows: Vec<Vec<&str>> = output
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row.get(2) == Some(&"inet") && row.get(3) == Some(&"10.0.0.10/24"))
    );
    assert!(rows.iter().any(|row| row.get(2) == Some(&"inet6")
        && row.get(3) == Some(&"fd00::10/64")
        && row.contains(&"tentative")
        && row.contains(&"dadfailed")));
    assert_eq!(show(&fixture, "other0").await?, "");
    for address in ["10.0.0.10/24", "fd00::10/64"] {
        assert!(change(&fixture, "del", address).await?.status.success());
        let missing = change(&fixture, "del", address).await?;
        assert!(!missing.status.success());
        assert!(String::from_utf8(missing.stderr)?.contains("Cannot assign requested address"));
    }
    assert_eq!(show(&fixture, "eth0").await?, "");
    Ok(())
}

#[tokio::test]
async fn failures_target_nth_or_every_mutation_without_changing_state() -> Result<()> {
    let fixture = FakeIp::new()?;
    for operation in ["add", "del"] {
        fs::write(fixture.dir.join(format!("fail-{operation}")), "2\n")?;
    }
    assert!(
        change(&fixture, "add", "10.0.0.10/24")
            .await?
            .status
            .success()
    );
    assert!(
        !change(&fixture, "add", "fd00::10/64")
            .await?
            .status
            .success()
    );
    assert!(
        change(&fixture, "add", "fd00::10/64")
            .await?
            .status
            .success()
    );
    assert!(
        change(&fixture, "del", "10.0.0.10/24")
            .await?
            .status
            .success()
    );
    assert!(
        !change(&fixture, "del", "fd00::10/64")
            .await?
            .status
            .success()
    );
    assert!(show(&fixture, "eth0").await?.contains("fd00::10/64"));
    for operation in ["add", "del"] {
        fs::write(fixture.dir.join(format!("fail-{operation}")), "always\n")?;
        for _ in 0..2 {
            let output = change(&fixture, operation, "fd00::10/64").await?;
            assert!(!output.status.success());
            assert!(String::from_utf8(output.stderr)?.contains("injected"));
        }
    }
    assert!(show(&fixture, "eth0").await?.contains("fd00::10/64"));
    Ok(())
}

#[tokio::test]
async fn fifo_gate_allows_late_release_after_observed_interleaving() -> Result<()> {
    let fixture = FakeIp::new()?;
    timeout(Duration::from_secs(5), async {
        for operation in ["add", "del"] {
            let gate = fixture.dir.join(format!("gate-{operation}"));
            assert!(
                Command::new("mkfifo")
                    .arg(&gate)
                    .kill_on_drop(true)
                    .status()
                    .await?
                    .success()
            );
            let mut child = Command::new(fixture.command("ip"))
                .args(["address", operation, "10.0.0.10/24", "dev", "eth0"])
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("missing stdout"))?;
            let mut lines = BufReader::new(stdout).lines();
            assert_eq!(lines.next_line().await?, Some(format!("GATED {operation}")));
            // Keep the mutation gated through real subprocess work. Release is
            // driven by completed observations, independent of scheduler timing.
            for _ in 0..32 {
                let present = show(&fixture, "eth0").await?.contains("10.0.0.10/24");
                assert_eq!(present, operation == "del");
                assert!(child.try_wait()?.is_none());
            }
            let mut writer = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&gate)
                .await?;
            writer.write_all(b"release\n").await?;
            assert!(child.wait().await?.success());
            fs::remove_file(gate)?;
            let present = show(&fixture, "eth0").await?.contains("10.0.0.10/24");
            assert_eq!(present, operation == "add");
        }
        anyhow::Ok(())
    })
    .await?
}

#[tokio::test]
async fn announcements_are_recorded_and_fixture_files_are_removed() -> Result<()> {
    let fixture = FakeIp::new()?;
    let dir = fixture.dir.clone();
    for (name, args) in [
        ("arping", vec!["-U", "-I", "eth0", "-c", "1", "10.0.0.10"]),
        ("ndsend", vec!["fd00::10", "eth0"]),
    ] {
        assert!(run(&fixture, name, &args).await?.status.success());
        let output = timeout(
            Duration::from_secs(5),
            Command::new("/bin/bash")
                .args(["-n"])
                .arg(fixture.command(name))
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert!(output.status.success());
    }
    assert!(
        timeout(
            Duration::from_secs(5),
            Command::new("/bin/bash")
                .arg("-n")
                .arg(fixture.command("ip"))
                .kill_on_drop(true)
                .status(),
        )
        .await??
        .success()
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("commands"))?,
        "arping -U -I eth0 -c 1 10.0.0.10\nndsend fd00::10 eth0\n"
    );
    drop(fixture);
    assert!(!dir.exists());
    Ok(())
}
