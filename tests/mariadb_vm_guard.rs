//! Safety boundary tests: all executable transports are local recorders.
mod mariadb_vm_guard_support;

use anyhow::Result;
use mariadb_vm_guard_support::{Fixture, INVENTORY, check};
use std::fs;

#[test]
fn guard_refuses_without_env() -> Result<()> {
    let fixture = Fixture::new()?;
    let text = check(&fixture.run(&[], &[])?, 2)?;
    assert!(text.contains("GRUEZI_ALLOW_VM_FAULTS=1 required"));
    assert_eq!(fixture.calls()?, "");
    Ok(())
}

#[test]
fn guard_refuses_host_not_in_allowlist() -> Result<()> {
    for allowlist in ["", "disposable-one 10.88.0.10\ndisposable-two 10.88.0.99\n"] {
        let fixture = Fixture::new()?;
        fs::write(fixture.root.join("ansible/vm-allowlist.txt"), allowlist)?;
        let text = check(&fixture.operate("firewall-block")?, 2)?;
        assert!(text.contains("host and address not in allowlist"));
        assert_eq!(fixture.calls()?, "");
    }
    Ok(())
}

#[test]
fn guard_refuses_reference_host_names_and_addresses() -> Result<()> {
    for forbidden in [
        "maria-a",
        "maria-b",
        "MARIA-A.example",
        "alias-maria-b",
        "192.168.255.50",
        "192.168.255.51",
        "192.168.255.52",
    ] {
        let fixture = Fixture::new()?;
        let inventory = if forbidden.starts_with("192.") {
            INVENTORY.replace("10.88.0.10", forbidden)
        } else {
            INVENTORY.replace("disposable-one", forbidden)
        };
        fixture.inventory(&inventory)?;
        let text = check(&fixture.operate("firewall-block")?, 2)?;
        assert!(text.contains("reference name or address"));
        assert_eq!(fixture.calls()?, "");
    }
    Ok(())
}

#[test]
fn guard_has_no_override_for_reference_hosts() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.inventory(&INVENTORY.replace("disposable-one", "maria-a"))?;
    let text = check(&fixture.run(&["--force", "--list"], &[])?, 2)?;
    assert!(text.contains("unsupported option"));
    let text = check(
        &fixture.run(
            &["--list"],
            &[
                ("GRUEZI_ALLOW_REFERENCE_VMS", "1"),
                ("GRUEZI_ALLOW_VM_FAULTS", "1"),
            ],
        )?,
        2,
    )?;
    assert!(text.contains("reference name or address"));
    assert_eq!(fixture.calls()?, "");
    Ok(())
}

#[test]
fn dry_run_makes_no_remote_call() -> Result<()> {
    for mode in ["--list", "--dry-run"] {
        let fixture = Fixture::new()?;
        let text = check(&fixture.run(&[mode], &[])?, 0)?;
        let rows: Vec<_> = text
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .collect();
        assert_eq!(
            rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "kernel-sigkill",
                "watchdog-reset",
                "watchdog-arm-failure",
                "firewall-block",
                "asymmetric-loss"
            ]
        );
        for (_, fields) in rows {
            assert!(fields.starts_with("NOT RUN\t"));
            assert!(fields.contains("GRUEZI_ALLOW_VM_FAULTS=1"));
        }
        assert!(text.contains("GRUEZI_ALLOW_WATCHDOG_RESET=1"));
        assert_eq!(fixture.calls()?, "");
        assert!(!fixture.root.join("artifacts").exists());
    }
    Ok(())
}

#[test]
fn unrun_rows_are_reported_not_run() -> Result<()> {
    let fixture = Fixture::new()?;
    let text = check(&fixture.operate("firewall-block")?, 0)?;
    assert!(text.contains("firewall-block\tEXECUTED"));
    for row in [
        "kernel-sigkill",
        "watchdog-reset",
        "watchdog-arm-failure",
        "asymmetric-loss",
    ] {
        assert!(text.contains(&format!("{row}\tNOT RUN")));
    }
    assert!(!text.contains("\tPASS"));
    let calls = fixture.calls()?;
    assert_eq!(calls.lines().count(), 5);
    assert!(!calls.contains("/ssh "));
    assert!(calls.contains("gruezi_drop_state=absent"));
    Ok(())
}

#[test]
fn unsupported_inventory_and_aliases_fail_before_transport() -> Result<()> {
    let stock = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ansible/inventory/lab.yml.example"),
    )?;
    for inventory in [
        stock,
        INVENTORY.replace("10.88.0.10", "maria-alias"),
        INVENTORY.replace("10.88.0.10", "010.88.0.10"),
        INVENTORY.replace("10.88.0.10", "8.8.8.8"),
        INVENTORY.replace("10.88.0.10", "0xc0a8ff32"),
        INVENTORY.replace("10.88.0.10", "3232292658"),
        INVENTORY.replace("eth0", "eth0;touch"),
        format!("{INVENTORY}[all:vars]\nansible_connection=local\n"),
        INVENTORY.replace(
            "gruezi_vip=10.88.0.20/32",
            "ansible_ssh_common_args=-Jproxy",
        ),
        format!("{INVENTORY}{}", INVENTORY.lines().nth(1).unwrap_or("")),
    ] {
        let fixture = Fixture::new()?;
        fixture.inventory(&inventory)?;
        check(&fixture.operate("firewall-block")?, 2)?;
        assert_eq!(fixture.calls()?, "");
    }
    Ok(())
}

#[test]
fn every_actual_operation_is_guarded() -> Result<()> {
    for row in [
        "kernel-sigkill",
        "watchdog-reset",
        "watchdog-arm-failure",
        "firewall-block",
        "asymmetric-loss",
    ] {
        let fixture = Fixture::new()?;
        let output = fixture.run(
            &[
                "--row",
                row,
                "--target",
                "disposable-one",
                "--output",
                "/tmp/unused-vm-output",
            ],
            &[],
        )?;
        check(&output, 2)?;
        assert_eq!(fixture.calls()?, "");
    }
    let fixture = Fixture::new()?;
    let text = check(
        &fixture.run(
            &[
                "--row",
                "watchdog-reset",
                "--target",
                "disposable-one",
                "--output",
                "/tmp/unused-vm-output",
            ],
            &[("GRUEZI_ALLOW_VM_FAULTS", "1")],
        )?,
        2,
    )?;
    assert!(text.contains("GRUEZI_ALLOW_WATCHDOG_RESET=1 required"));
    assert_eq!(fixture.calls()?, "");
    Ok(())
}

#[test]
fn bounded_catalogue_uses_only_authorized_hosts() -> Result<()> {
    for row in [
        "kernel-sigkill",
        "watchdog-reset",
        "watchdog-arm-failure",
        "asymmetric-loss",
    ] {
        let fixture = Fixture::new()?;
        check(&fixture.operate(row)?, 0)?;
        let calls = fixture.calls()?;
        assert!(!calls.contains("ansible-inventory"));
        assert!(!calls.contains("deploy-ha-lab"));
        assert!(
            calls.lines().all(
                |line| line.contains("ansible-playbook") || line.contains("ansible@10.88.0.10")
            )
        );
        assert!(calls.contains("ProxyCommand=none"));
        assert!(calls.contains("StrictHostKeyChecking=yes"));
        if row == "asymmetric-loss" {
            assert!(calls.contains("\"gruezi_netem_state\":\"absent\""));
        } else {
            assert_eq!(
                calls.lines().filter(|line| line.contains("/ssh ")).count(),
                1
            );
        }
    }
    Ok(())
}

#[test]
fn failed_fault_restores_before_exit() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(fixture.root.join("fail-apply"), "")?;
    check(&fixture.operate("firewall-block")?, 17)?;
    let calls = fixture.calls()?;
    assert_eq!(calls.lines().count(), 3);
    assert!(
        calls
            .lines()
            .last()
            .is_some_and(|line| line.contains("gruezi_drop_state=absent"))
    );
    Ok(())
}
