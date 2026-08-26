//! Application error types.

use std::{error::Error as StdError, io, path::PathBuf};

use thiserror::Error;

use crate::{
    inventory::InventoryError,
    lock::LockError,
    logs::LogError,
    process::ProcessError,
    store_path::{StorePath, StorePathError},
};

/// Associates a failure with the affected path.
#[derive(Debug, Error)]
#[error("error at `{}`", path.display())]
pub(crate) struct PathedError<E = io::Error>
where
    E: StdError + 'static,
{
    /// Identifies the affected filesystem entry.
    pub(crate) path: PathBuf,
    /// Contains the underlying error.
    #[source]
    source: E,
}

impl<E> PathedError<E>
where
    E: StdError + 'static,
{
    /// Associates a failure with its path.
    pub(crate) fn new(path: PathBuf, source: E) -> Self {
        Self { path, source }
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
    /// Indicates that the global tracing subscriber could not be installed.
    #[error("failed to install tracing subscriber")]
    InstallTracing {
        /// Contains the subscriber installation error.
        #[source]
        source: tracing::dispatcher::SetGlobalDefaultError,
    },
    /// Indicates that an inventory operation failed.
    #[error(transparent)]
    Inventory(InventoryError),
    /// Indicates that a deployment lock operation failed.
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
        /// Identifies the system observed under the deployment lock.
        active: StorePath,
    },
    /// Indicates that automatic commits require a tracked lock file.
    #[error("flake lock `{}` is not tracked by Git", path.display())]
    UntrackedFlakeLock {
        /// Identifies the untracked lock file.
        path: PathBuf,
    },
    /// Indicates that automatic commits require clean flake metadata.
    #[error("flake `{}` has changes before update: {status}", path.display())]
    DirtyFlake {
        /// Identifies the flake directory.
        path: PathBuf,
        /// Describes changes to the flake definition or lock.
        status: String,
    },
    /// Indicates that a repository advanced during deployment.
    #[error("Git repository `{}` changed during deployment", path.display())]
    RepositoryChanged {
        /// Identifies the repository root.
        path: PathBuf,
    },
    /// Indicates that an updated lock changed during deployment.
    #[error("updated flake lock `{}` changed during deployment", path.display())]
    UpdatedFlakeLockChanged {
        /// Identifies the changed lock file.
        path: PathBuf,
    },
    /// Indicates that flake metadata was malformed.
    #[error("failed to parse flake metadata")]
    ParseFlakeMetadata(#[source] PathedError<serde_json::Error>),
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
