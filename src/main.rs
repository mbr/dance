//! NixOS deployment orchestration from a local inventory.

mod deployment;
mod error;
mod inventory;
mod process;
mod status;
mod store_path;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use display_full_error::DisplayFullErrorExt;

use crate::{error::Error, inventory::Inventory};

/// Deploys NixOS hosts from an inventory.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Selects the host inventory.
    #[arg(long, env = "DANCE_INVENTORY", default_value = "inventory.toml")]
    inventory: PathBuf,
}

/// Runs the deployment workflow.
async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    deployment::deploy(Inventory::load(&cli.inventory)?).await
}

/// Reports whether the deployment workflow succeeded.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.display_full());
            ExitCode::FAILURE
        }
    }
}
