use crate::config::DEFAULT_HA_BIND;
use clap::{
    Arg, ArgAction, Command,
    builder::styling::{AnsiColor, Effects, Styles},
};

pub mod built_info {
    #![allow(clippy::doc_markdown)]
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// Creates and configures the CLI command structure
#[must_use]
pub fn new() -> Command {
    let styles = Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default());

    let git_hash = built_info::GIT_COMMIT_HASH.unwrap_or("unknown");
    let long_version: &'static str =
        Box::leak(format!("{} - {}", env!("CARGO_PKG_VERSION"), git_hash).into_boxed_str());

    Command::new("gruezi")
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(long_version)
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .long_about(
            "Gruezi provides two operational modes:\n\
             \n\
             - `ha`: 2-node high availability using a UDP-based failover protocol on port 9375\n\
             - `kv`: 3+ node consensus-backed key-value service with API traffic on port 9376 and peer traffic on port 9377\n\
             \n\
             Configuration is loaded from `--config`, then `GRUEZI_CONFIG`, then `/etc/gruezi/gruezi.yaml` if present.\n\
             \n\
             The CLI is intended to expose both local control and remote management workflows over time.",
        )
        .styles(styles)
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Enable verbose CLI output")
                .long_help(
                    "Enable verbose logging for debugging purposes.\n\n\
                     Can be specified multiple times to increase verbosity:\n  \
                     -v    = INFO level\n  \
                     -vv   = DEBUG level\n  \
                     -vvv  = TRACE level\n\n\
                     Note: Verbose output is sent to stderr via the RUST_LOG environment variable.",
                )
                .action(ArgAction::Count),
        )
        .subcommand(start_command())
        .subcommand(status_command())
        .subcommand(peers_command())
        .subcommand_required(true)
        .arg_required_else_help(true)
}

fn start_command() -> Command {
    Command::new("start")
        .about("Start the gruezi service")
        .long_about(
            "Start `gruezi` using either YAML configuration or the direct HA CLI flags.\n\
             \n\
             Preferred path:\n\
             - use `--config` with a YAML file\n\
             - or rely on `GRUEZI_CONFIG`\n\
             - or place the default file at `/etc/gruezi/gruezi.yaml`\n\
             \n\
             Direct flags remain available as an incremental HA-only path. In that case,\n\
             `--bind` defaults to the HA control port on `0.0.0.0:9375`.",
        )
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .help("Path to a YAML configuration file")
                .long_help(
                    "Path to a YAML configuration file.\n\n\
                     When this is provided, `gruezi` ignores the direct `--bind`, `--peers`, and `--node-id` flags.\n\
                     The config file can describe either `mode: ha` or `mode: kv`.",
                )
                .value_name("FILE")
                .conflicts_with_all(["bind", "peers", "node-id"]),
        )
        .arg(
            Arg::new("bind")
                .short('b')
                .long("bind")
                .help("Address to bind the HA listener to")
                .long_help(
                    "Address to bind the HA listener to when using the direct CLI path.\n\n\
                     This is primarily for `mode: ha` style startup without a YAML file.\n\
                     The default is `0.0.0.0:9375`, which is the draft HA peer port.",
                )
                .value_name("ADDRESS")
                .default_value(DEFAULT_HA_BIND),
        )
        .arg(
            Arg::new("peers")
                .short('p')
                .long("peers")
                .help("Comma-separated list of peer addresses")
                .long_help(
                    "Comma-separated list of peer addresses for the direct CLI startup path.\n\n\
                     For HA mode this should identify the remote peer using the HA control port, for example `10.0.0.2:9375`.",
                )
                .value_name("PEERS"),
        )
        .arg(
            Arg::new("node-id")
                .short('n')
                .long("node-id")
                .help("Unique node identifier")
                .long_help(
                    "Unique node identifier for the local node when using the direct CLI startup path.\n\n\
                     YAML-based HA configuration requires `node.id` explicitly.",
                )
                .value_name("ID"),
        )
}

fn status_command() -> Command {
    Command::new("status")
        .about("Show cluster status")
        .long_about(
            "Show status for the local node or a remote node.\n\n\
             Over time this command is expected to target the common API and management port on `9376/tcp` rather than the HA or Raft peer ports directly.",
        )
        .arg(
            Arg::new("node")
                .short('n')
                .long("node")
                .help("Query a specific node or API endpoint")
                .long_help(
                    "Query a specific node or API endpoint.\n\n\
                     This should point to the management/API address, not the HA UDP peer port.",
                )
                .value_name("ADDRESS"),
        )
        .arg(
            Arg::new("watch")
                .short('w')
                .long("watch")
                .help("Refresh status continuously until interrupted")
                .long_help(
                    "Refresh status continuously until interrupted.\n\n\
                     This is intended for live HA observation and correlation with packet captures.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("interval-ms")
                .long("interval-ms")
                .help("Polling interval in milliseconds for --watch")
                .long_help(
                    "Polling interval in milliseconds for `--watch`.\n\n\
                     The default is 1000ms.",
                )
                .value_name("MILLISECONDS")
                .value_parser(clap::value_parser!(u64).range(1..))
                .default_value("1000"),
        )
}

fn peers_command() -> Command {
    Command::new("peers")
        .about("List cluster peers")
        .long_about(
            "List peers known to the current node.\n\n\
             This command is intended for both HA and KV workflows and should eventually reflect peer information gathered through the shared management/API layer.",
        )
        .arg(
            Arg::new("format")
                .short('f')
                .long("format")
                .help("Render output as table, json, or yaml")
                .long_help(
                    "Render peer output in one of the supported formats.\n\n\
                     `table` is intended for operators, while `json` and `yaml` are better for automation.",
                )
                .value_name("FORMAT")
                .value_parser(["table", "json", "yaml"])
                .default_value("table"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn verify_cli() {
        new().debug_assert();
    }

    #[test]
    fn test_start_command() {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "start"]);
        assert!(matches.is_ok());
    }

    #[test]
    fn test_start_command_with_config() {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "start", "--config", "gruezi.yml"]);
        assert!(matches.is_ok());
    }

    #[test]
    fn test_status_command() {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "status"]);
        assert!(matches.is_ok());
    }

    #[test]
    fn test_status_watch_command() {
        let app = new();
        let matches =
            app.try_get_matches_from(vec!["gruezi", "status", "--watch", "--interval-ms", "500"]);
        assert!(matches.is_ok());
    }

    #[test]
    fn test_peers_command() {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "peers"]);
        assert!(matches.is_ok());
    }

    #[test]
    fn test_verbose_flag() -> Result<()> {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "-vvv", "status"])?;
        assert_eq!(matches.get_count("verbose"), 3);
        Ok(())
    }
}
