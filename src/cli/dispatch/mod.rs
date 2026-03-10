use crate::cli::actions::Action;
use crate::config::DEFAULT_HA_BIND;
use anyhow::Result;
use clap::ArgMatches;

/// Convert `ArgMatches` into an Action
///
/// # Errors
///
/// Returns an error when the provided command is invalid
pub fn handler(matches: &ArgMatches) -> Result<Action> {
    // Determine verbosity level
    let verbose = matches.get_count("verbose") > 0;

    // Route to the appropriate action based on subcommand
    match matches.subcommand() {
        Some(("start", sub_matches)) => {
            let config = sub_matches.get_one::<String>("config").cloned();

            let bind = sub_matches
                .get_one::<String>("bind")
                .map_or_else(|| DEFAULT_HA_BIND.to_owned(), String::from);

            let peers = sub_matches.get_one::<String>("peers").cloned();

            let node_id = sub_matches.get_one::<String>("node-id").cloned();

            Ok(Action::Start {
                config,
                bind,
                peers,
                node_id,
                verbose,
            })
        }
        Some(("status", sub_matches)) => {
            let node = sub_matches.get_one::<String>("node").cloned();
            let watch = sub_matches.get_flag("watch");
            let interval_ms = sub_matches
                .get_one::<u64>("interval-ms")
                .copied()
                .unwrap_or(1_000);

            Ok(Action::Status {
                node,
                verbose,
                watch,
                interval_ms,
            })
        }
        Some(("peers", sub_matches)) => {
            let format = sub_matches
                .get_one::<String>("format")
                .map_or_else(|| "table".to_owned(), String::from);

            Ok(Action::Peers { format, verbose })
        }
        _ => {
            anyhow::bail!("Invalid command. Run with --help for usage information")
        }
    }
}
