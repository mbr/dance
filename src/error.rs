//! Application error types.

use std::{
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use thiserror::Error;

use crate::{inventory::InventoryError, logs::LogError, store_path::StorePathError};

/// Associates an input/output failure with the affected path.
#[derive(Debug, Error)]
#[error("I/O error")]
pub(crate) struct PathIoError {
    /// Identifies the affected filesystem entry.
    path: PathBuf,
    /// Contains the input/output error.
    #[source]
    source: io::Error,
}

impl PathIoError {
    /// Associates an input/output failure with its path.
    pub(crate) fn new(path: PathBuf, source: io::Error) -> Self {
        Self { path, source }
    }

    /// Returns the affected path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Describes failures encountered while deploying hosts.
#[derive(Debug, Error)]
pub(crate) enum Error {
    /// Indicates that a command could not be started.
    #[error("failed to run `{command}`")]
    CommandSpawn {
        /// Shows the attempted command.
        command: String,
        /// Contains the operating-system error.
        #[source]
        source: io::Error,
    },
    /// Indicates that a command could not be awaited.
    #[error("failed to wait for `{command}`")]
    CommandWait {
        /// Shows the attempted command.
        command: String,
        /// Contains the operating-system error.
        #[source]
        source: io::Error,
    },
    /// Indicates that command output could not be read.
    #[error("failed to read output from `{command}`")]
    CommandOutputRead {
        /// Shows the attempted command.
        command: String,
        /// Contains the input/output error.
        #[source]
        source: io::Error,
    },
    /// Indicates that a command returned a failure status.
    #[error("`{command}` exited with {status}: {stderr}")]
    CommandFailed {
        /// Shows the attempted command.
        command: String,
        /// Contains the command's exit status.
        status: ExitStatus,
        /// Contains standard error from the command.
        stderr: String,
    },
    /// Indicates that a command exceeded its execution deadline.
    #[error("`{command}` timed out after {seconds} seconds")]
    CommandTimedOut {
        /// Shows the attempted command.
        command: String,
        /// Contains the configured deadline in seconds.
        seconds: u64,
    },
    /// Indicates that command output was not UTF-8.
    #[error("`{command}` returned non-UTF-8 output")]
    CommandOutput {
        /// Shows the attempted command.
        command: String,
        /// Contains the decoding error.
        #[source]
        source: std::string::FromUtf8Error,
    },
    /// Indicates that a logging operation failed.
    #[error(transparent)]
    Log(LogError),
    /// Indicates that an inventory operation failed.
    #[error(transparent)]
    Inventory(InventoryError),
    /// Indicates that flake metadata was malformed.
    #[error("failed to parse metadata for flake `{}`", path.display())]
    ParseFlakeMetadata {
        /// Identifies the flake.
        path: PathBuf,
        /// Contains the JSON decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// Indicates that verifier metadata was malformed.
    #[error("failed to parse verifier metadata for host `{host}`")]
    ParseVerifier {
        /// Identifies the inventory host.
        host: String,
        /// Contains the JSON decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// Indicates that a command did not return a Nix store path.
    #[error("invalid Nix store path `{path}`")]
    InvalidStorePath {
        /// Contains the invalid value.
        path: String,
        /// Contains the parse error.
        #[source]
        source: StorePathError,
    },
    /// Indicates that live rollback failed and console recovery is required.
    #[error(
        "live rollback of host `{host}` failed after {trigger}: {rollback}; reboot it through the machine console"
    )]
    RollbackIncomplete {
        /// Identifies the inventory host.
        host: String,
        /// Contains the deployment failure that initiated rollback.
        trigger: Box<Self>,
        /// Contains the immediate rollback failure.
        rollback: Box<Self>,
    },
    /// Indicates that unattended deployment was not explicitly approved.
    #[error("deployment requires `--yes` without a terminal")]
    ConfirmationRequired,
    /// Indicates that deployment confirmation failed.
    #[error("failed to confirm deployment")]
    Confirmation {
        /// Contains the terminal interaction error.
        #[source]
        source: dialoguer::Error,
    },
    /// Indicates that the progress display template was invalid.
    #[error("invalid progress display template")]
    ProgressStyle {
        /// Contains the template error.
        #[source]
        source: indicatif::style::TemplateError,
    },
}
