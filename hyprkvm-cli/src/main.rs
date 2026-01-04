//! HyprKVM CLI tool
//!
//! Separate CLI for querying daemon status and managing configuration.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hyprkvm-ctl")]
#[command(about = "HyprKVM control utility")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show daemon status
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// List connected peers
    Peers,

    /// Ping a peer
    Ping {
        /// Peer name or direction
        peer: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Status { json } => {
            // TODO: Connect to daemon and get status
            if json {
                println!("{{\"status\": \"not_implemented\"}}");
            } else {
                println!("HyprKVM Status: not implemented yet");
            }
        }
        Commands::Peers => {
            println!("Peer listing not implemented yet");
        }
        Commands::Ping { peer } => {
            println!("Ping {} not implemented yet", peer);
        }
    }

    Ok(())
}
