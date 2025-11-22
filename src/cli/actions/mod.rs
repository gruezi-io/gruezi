pub mod peers;
pub mod start;
pub mod status;

/// Represents all possible actions the CLI can perform
#[derive(Debug)]
pub enum Action {
    /// Start the gruezi service
    Start {
        bind: String,
        peers: Option<String>,
        node_id: Option<String>,
        verbose: bool,
    },
    /// Show cluster status
    Status {
        node: Option<String>,
        verbose: bool,
    },
    /// List cluster peers
    Peers {
        format: String,
        verbose: bool,
    },
}
