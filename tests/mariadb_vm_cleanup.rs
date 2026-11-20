//! Real Ansible evaluates the shipped playbooks; local fake SSH never executes
//! the module payload. These contracts were missed by executable-only fakes.
mod mariadb_vm_cleanup_support;
mod mariadb_vm_guard_support;

use anyhow::Result;
use mariadb_vm_cleanup_support::{CleanupFixture, Kind, transcript};
use mariadb_vm_guard_support::check;

#[test]
fn deletion_errors_are_fatal_for_both_playbooks() -> Result<()> {
    let mut codes = Vec::new();
    for kind in [Kind::Drop, Kind::Netem] {
        let fixture = CleanupFixture::new(kind, "delete-error")?;
        let output = fixture.playbook()?;
        let text = transcript(&output)?;
        println!("{kind:?} exit={:?}\n{text}", output.status.code());
        assert!(fixture.transport_log()?.contains("delete rc=42"));
        codes.push(output.status.code());
    }
    assert_eq!(codes, [Some(2), Some(2)]);
    Ok(())
}

#[test]
fn known_absent_is_idempotent_without_deletion() -> Result<()> {
    for kind in [Kind::Drop, Kind::Netem] {
        let fixture = CleanupFixture::new(kind, "known-absent")?;
        check(&fixture.playbook()?, 0)?;
        let log = fixture.transport_log()?;
        assert!(!log.contains("delete"), "{kind:?}: {log}");
        assert_eq!(
            log.lines().filter(|line| line.starts_with("query")).count(),
            2
        );
    }
    Ok(())
}

#[test]
fn successful_deletion_requires_observed_absence() -> Result<()> {
    for kind in [Kind::Drop, Kind::Netem] {
        let fixture = CleanupFixture::new(kind, "removed")?;
        check(&fixture.playbook()?, 0)?;
        let log = fixture.transport_log()?;
        assert_eq!(
            log.lines().filter(|line| line.starts_with("query")).count(),
            2
        );
        assert_eq!(log.lines().filter(|line| *line == "delete rc=0").count(), 1);
    }
    Ok(())
}

#[test]
fn persistent_impairment_fails_despite_successful_commands() -> Result<()> {
    let mut codes = Vec::new();
    for kind in [Kind::Drop, Kind::Netem] {
        let fixture = CleanupFixture::new(kind, "persistent")?;
        let output = fixture.playbook()?;
        let text = transcript(&output)?;
        println!("{kind:?} exit={:?}\n{text}", output.status.code());
        let log = fixture.transport_log()?;
        assert!(log.contains("delete rc=0"), "{kind:?}: {log}");
        assert!(!log.contains("rc=42") && !log.contains("rc=43"));
        codes.push(output.status.code());
    }
    assert_eq!(codes, [Some(2), Some(2)]);
    Ok(())
}

#[test]
fn observation_errors_and_unparseable_state_fail_closed() -> Result<()> {
    for kind in [Kind::Drop, Kind::Netem] {
        for scenario in [
            "observation-error",
            "after-observation-error",
            "malformed",
            "after-malformed",
            "shape-error",
            "after-shape-error",
        ] {
            let fixture = CleanupFixture::new(kind, scenario)?;
            let output = fixture.playbook()?;
            let text = transcript(&output)?;
            println!(
                "{kind:?}/{scenario} exit={:?}\n{text}",
                output.status.code()
            );
            assert_eq!(output.status.code(), Some(2));
            assert!(fixture.transport_log()?.contains("query"));
        }
    }
    Ok(())
}

#[test]
fn runner_surfaces_unconfirmed_cleanup_on_normal_and_exit_paths() -> Result<()> {
    for kind in [Kind::Drop, Kind::Netem] {
        for exit_path in [false, true] {
            let fixture = CleanupFixture::new(kind, "delete-error")?;
            let output = fixture.runner(exit_path)?;
            let text = check(&output, 2)?;
            assert!(text.contains("manual recovery required"), "{text}");
            assert!(text.contains("cleanup_exit=2"), "{text}");
            assert!(fixture.transport_log()?.contains("delete rc=42"));
            assert!(!text.contains("\tEXECUTED\ttransport_exit=0"));
        }
    }
    Ok(())
}
