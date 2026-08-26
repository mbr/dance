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
    /// Reports deployed systems and pending configuration changes.
    Status(WorkflowArgs),
    /// Reviews and deploys outdated systems.
    Deploy(DeployArgs),
    /// Runs configured verifiers without deploying.
    Verify(VerifyArgs),
}

/// Configures system comparison shared by status and deployment.
#[derive(Args, Debug)]
pub(crate) struct WorkflowArgs {
    /// Updates selected flakes before comparing systems.
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
    /// Configures the hosts and inputs to compare.
    #[command(flatten)]
    pub(crate) workflow: WorkflowArgs,
    /// Commits changed flake locks after a successful deployment.
    #[arg(short, long, requires = "update")]
    pub(crate) commit: bool,
    /// Updates flakes and commits changed locks after successful deployment.
    #[arg(short = 'g', long, conflicts_with_all = ["update", "commit"])]
    pub(crate) git: bool,
    /// Deploys without requesting confirmation.
    #[arg(short, long)]
    pub(crate) yes: bool,
    /// Skips configured flake verifiers.
    #[arg(short, long)]
    pub(crate) skip_verifier: bool,
    /// Leaves a failed test or verification in place instead of restoring it.
    #[arg(short = 'R', long)]
    pub(crate) no_rollback: bool,
    /// Retains this many system generations after deployment.
    #[arg(short, long, default_value = "2")]
    pub(crate) keep_generations: NonZeroU32,
    /// Skips post-deployment garbage collection.
    #[arg(short = 'G', long, conflicts_with = "keep_generations")]
    pub(crate) no_gc: bool,
}

impl DeployArgs {
    /// Returns whether selected flakes should be updated.
    pub(crate) fn update_flakes(&self) -> bool {
        self.workflow.update || self.git
    }

    /// Returns whether changed lock files should be committed.
    pub(crate) fn commit_flakes(&self) -> bool {
        self.commit || self.git
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    /// Checks that lock commits cannot be requested without updating inputs.
    #[test]
    fn committing_requires_an_update() {
        assert!(Cli::try_parse_from(["dance", "deploy", "--commit"]).is_err());
        let cli = Cli::try_parse_from(["dance", "deploy", "--update", "--commit"])
            .expect("commit with update should parse");
        assert!(matches!(cli.command, Command::Deploy(args) if args.commit));
    }

    /// Checks the combined Git update shorthand and its explicit-option conflicts.
    #[test]
    fn git_enables_update_and_commit() {
        let cli =
            Cli::try_parse_from(["dance", "deploy", "--git"]).expect("Git shorthand should parse");
        let Command::Deploy(args) = cli.command else {
            panic!("deploy command should parse");
        };
        assert!(args.update_flakes());
        assert!(args.commit_flakes());
        assert!(Cli::try_parse_from(["dance", "deploy", "--git", "--update"]).is_err());
        assert!(Cli::try_parse_from(["dance", "deploy", "--git", "--commit"]).is_err());
    }
}
