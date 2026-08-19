//! Application error types.

use std::{
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    inventory::InventoryError,
    lock::LockError,
    logs::LogError,
    process::ProcessError,
    store_path::{StorePath, StorePathError},
};

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
    /// Indicates that a subprocess operation failed.
    #[error(transparent)]
    Process(ProcessError),
    /// Indicates that a logging operation failed.
    #[error(transparent)]
    Log(LogError),
    /// Indicates that an inventory operation failed.
    #[error(transparent)]
    Inventory(InventoryError),
    /// Indicates that a deployment lease operation failed.
    #[error(transparent)]
    Lock(LockError),
    /// Indicates that a host changed after deployment review.
    #[error(
        "host `{host}` changed from reviewed system `{reviewed}` to `{active}` before deployment"
    )]
    DeploymentStateChanged {
        /// Identifies the inventory host.
        host: String,
        /// Identifies the system observed during review.
        reviewed: StorePath,
        /// Identifies the system observed under the deployment lease.
        active: StorePath,
    },
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
    /// Indicates that a Nix store path was malformed.
    #[error(transparent)]
    StorePath(StorePathError),
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
