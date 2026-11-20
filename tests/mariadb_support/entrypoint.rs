//! The packaged script receives no argv from the daemon.

use super::*;

#[test]
fn packaged_entrypoint_accepts_existing_lifecycle_modes() -> Result<()> {
    for activation in ["cold", "handoff"] {
        for deactivation in ["baseline", "handoff_release", "revoke"] {
            let fx = lifecycle_fixture("packaged-lifecycle")?;
            expect_exit(
                &fx.event("deactivate", "0", &[("GRUEZI_DEACTIVATE_MODE", "baseline")])?,
                0,
            )?;
            if activation == "cold" {
                fx.control("receipt.peer.cnf", "")?;
            }
            let text = expect_exit(
                &fx.event("activate", "1", &[("GRUEZI_ACTIVATION_MODE", activation)])?,
                0,
            )?;
            assert!(text.contains("active=yes"));
            assert_eq!(fx.control_value("read_only")?, "0");
            let incarnation = active_incarnation(&fx)?;
            fx.clear_log()?;
            expect_exit(&verify_active(&fx, &incarnation)?, 0)?;
            assert_no_write_sql(&fx.call_log()?)?;
            expect_exit(
                &fx.event(
                    "deactivate",
                    "2",
                    &[("GRUEZI_DEACTIVATE_MODE", deactivation)],
                )?,
                0,
            )?;
            assert_eq!(fx.control_value("read_only")?, "1");
            assert_eq!(
                fx.state_dir.join("last_release.record").exists(),
                deactivation == "handoff_release"
            );
        }
    }
    Ok(())
}

#[test]
fn packaged_entrypoint_rejects_missing_invalid_and_non_lifecycle_events() -> Result<()> {
    let fx = lifecycle_fixture("packaged-events")?;
    // Missing means absent from the environment, rather than an empty override.
    expect_exit(&fx.run_unguarded(&[])?, 64)?;
    for event in [
        "",
        "invalid",
        "Activate",
        "help",
        "check",
        "quarantine-status",
        "quarantine-clear",
        "declare-authoritative",
        "internal-gtid-parse",
        "active",
        "standby",
    ] {
        expect_exit(&fx.event(event, "1", &[])?, 64)?;
        assert_eq!(fx.call_log()?, "");
        assert_eq!(state_names(&fx)?, Vec::<String>::new());
    }
    Ok(())
}

#[test]
fn packaged_entrypoint_preserves_mode_and_identity_validation() -> Result<()> {
    for (event, field, value, reason) in [
        (
            "activate",
            "GRUEZI_ACTIVATION_MODE",
            "",
            "activation_mode_invalid",
        ),
        (
            "activate",
            "GRUEZI_ACTIVATION_MODE",
            "revoke",
            "activation_mode_invalid",
        ),
        (
            "deactivate",
            "GRUEZI_DEACTIVATE_MODE",
            "",
            "deactivate_mode_invalid",
        ),
        (
            "deactivate",
            "GRUEZI_DEACTIVATE_MODE",
            "cold",
            "deactivate_mode_invalid",
        ),
        (
            "activate",
            "GRUEZI_GENERATION",
            "invalid",
            "generation_invalid",
        ),
        ("activate", "GRUEZI_INSTANCE_ID", "", "instance_invalid"),
        (
            "verify",
            "GRUEZI_DB_INCARNATION",
            "",
            "incarnation_env_invalid",
        ),
    ] {
        let fx = lifecycle_fixture("packaged-validation")?;
        let text = expect_exit(&fx.event(event, "1", &[(field, value)])?, 64)?;
        assert_reason(&text, reason);
        assert_eq!(fx.call_log()?, "");
    }
    Ok(())
}

#[test]
fn positional_lifecycle_invocations_remain_supported() -> Result<()> {
    let fx = lifecycle_fixture("positional-lifecycle")?;
    expect_exit(&fx.event_args(&["activate"], "1", &[])?, 0)?;
    let incarnation = active_incarnation(&fx)?;
    expect_exit(
        &fx.event_args(&["verify"], "1", &[("GRUEZI_DB_INCARNATION", &incarnation)])?,
        0,
    )?;
    expect_exit(&fx.event_args(&["deactivate"], "2", &[])?, 0)?;
    assert_eq!(fx.control_value("read_only")?, "1");
    Ok(())
}
