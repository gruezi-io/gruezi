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
            "Gruezi provides distributed service discovery with RAFT consensus,\n\
             a key-value store backed by RocksDB, and DNS-based service discovery.",
        )
        .styles(styles)
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Show verbose output with cron expression")
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
        .subcommand(
            Command::new("start")
                .about("Start the gruezi service")
                .arg(
                    Arg::new("bind")
                        .short('b')
                        .long("bind")
                        .help("Address to bind to")
                        .value_name("ADDRESS")
                        .default_value("0.0.0.0:8080"),
                )
                .arg(
                    Arg::new("peers")
                        .short('p')
                        .long("peers")
                        .help("Comma-separated list of peer addresses")
                        .value_name("PEERS"),
                )
                .arg(
                    Arg::new("node-id")
                        .short('n')
                        .long("node-id")
                        .help("Unique node identifier")
                        .value_name("ID"),
                ),
        )
        .subcommand(
            Command::new("status").about("Show cluster status").arg(
                Arg::new("node")
                    .short('n')
                    .long("node")
                    .help("Query specific node")
                    .value_name("ADDRESS"),
            ),
        )
        .subcommand(
            Command::new("peers").about("List cluster peers").arg(
                Arg::new("format")
                    .short('f')
                    .long("format")
                    .help("Output format")
                    .value_name("FORMAT")
                    .value_parser(["table", "json", "yaml"])
                    .default_value("table"),
            ),
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
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
    fn test_status_command() {
        let app = new();
        let matches = app.try_get_matches_from(vec!["gruezi", "status"]);
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
