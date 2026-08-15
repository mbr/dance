//! Application error types.

use std::{io, path::PathBuf, process::ExitStatus};

use thiserror::Error;

use crate::store_path::{StorePath, StorePathError};

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
    /// Indicates that command output was not UTF-8.
    #[error("`{command}` returned non-UTF-8 output")]
    CommandOutput {
        /// Shows the attempted command.
        command: String,
        /// Contains the decoding error.
        #[source]
        source: std::string::FromUtf8Error,
    },
    /// Indicates that an inventory could not be read.
    #[error("failed to read inventory `{}`", path.display())]
    ReadInventory {
        /// Identifies the inventory.
        path: PathBuf,
        /// Contains the filesystem error.
        #[source]
        source: io::Error,
    },
    /// Indicates that an inventory was malformed.
    #[error("failed to parse inventory `{}`", path.display())]
    ParseInventory {
        /// Identifies the inventory.
        path: PathBuf,
        /// Contains the TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// Indicates that a requested ID is not in the inventory.
    #[error("ID `{id}` is not in the inventory")]
    UnknownId {
        /// Identifies the requested inventory entry.
        id: String,
    },
    /// Indicates that an ID was requested more than once.
    #[error("ID `{id}` was selected more than once")]
    DuplicateSelectedId {
        /// Identifies the repeatedly selected inventory entry.
        id: String,
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
    /// Indicates that the reviewed target changed before activation.
    #[error("host `{host}` changed after review: reviewed {reviewed}, currently {current}")]
    TargetChanged {
        /// Identifies the inventory host.
        host: String,
        /// Identifies the system observed during review.
        reviewed: StorePath,
        /// Identifies the system observed immediately before activation.
        current: StorePath,
    },
    /// Indicates that activation did not select the expected system.
    #[error("host `{host}` activated {current}, expected {expected}")]
    ActivationMismatch {
        /// Identifies the inventory host.
        host: String,
        /// Identifies the prepared system.
        expected: StorePath,
        /// Identifies the system observed after activation.
        current: StorePath,
    },
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
