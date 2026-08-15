//! Command-line interface definitions.

use std::{num::NonZeroU32, path::PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::logs::LogMode;

/// Deploys NixOS hosts from an inventory.
#[derive(Debug, Parser)]
#[command(version)]
pub(crate) struct Cli {
    /// Selects the host inventory.
    #[arg(
        short,
        long,
        env = "DANCE_INVENTORY",
        default_value = "inventory.toml",
        global = true
    )]
    pub(crate) inventory: PathBuf,
    /// Selects when command logs are retained.
    #[arg(
        short,
        long,
        env = "DANCE_LOGS",
        value_enum,
        default_value = "on-error",
        global = true
    )]
    pub(crate) logs: LogMode,
    /// Overrides the command log directory.
    #[arg(short = 'L', long, env = "DANCE_LOGS_DIR", global = true)]
    pub(crate) logs_dir: Option<PathBuf>,
    /// Selects the operation to perform.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Selects a workflow and its valid options.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Compares deployed systems with their local configurations.
    Review(WorkflowArgs),
    /// Reviews and deploys outdated systems.
    Deploy(DeployArgs),
    /// Runs configured verifiers without deploying.
    Verify(VerifyArgs),
}

/// Configures the review phase shared by each workflow.
#[derive(Args, Debug)]
pub(crate) struct WorkflowArgs {
    /// Updates selected flakes during review.
    #[arg(short, long)]
    pub(crate) update: bool,
    /// Limits the workflow to these inventory IDs.
    #[arg(value_name = "ID")]
    pub(crate) ids: Vec<String>,
}

/// Selects hosts for verifier execution.
#[derive(Args, Debug)]
pub(crate) struct VerifyArgs {
    /// Limits verification to these inventory IDs.
    #[arg(value_name = "ID")]
    pub(crate) ids: Vec<String>,
}

/// Configures activation after the shared review phase.
#[derive(Args, Debug)]
pub(crate) struct DeployArgs {
    /// Configures the hosts and inputs to review.
    #[command(flatten)]
    pub(crate) workflow: WorkflowArgs,
    /// Deploys without requesting confirmation.
    #[arg(short, long)]
    pub(crate) yes: bool,
    /// Skips configured flake verifiers.
    #[arg(short, long)]
    pub(crate) skip_verifier: bool,
    /// Leaves a failed activation in place instead of rolling it back.
    #[arg(short = 'R', long)]
    pub(crate) no_rollback: bool,
    /// Retains this many system generations after deployment.
    #[arg(short, long, default_value = "2")]
    pub(crate) keep_generations: NonZeroU32,
    /// Skips post-deployment garbage collection.
    #[arg(short = 'G', long, conflicts_with = "keep_generations")]
    pub(crate) no_gc: bool,
}
