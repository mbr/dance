//! Per-host state collection and comparison.

use std::{
    fmt::{self, Display, Formatter},
    future::Future,
};

use indicatif::ProgressBar;

use super::operations;
use crate::{
    error::Error,
    inventory::Host,
    process::ProcessContext,
    status::{self, DeploymentPhase},
    store_path::StorePath,
    verification::{self, ConfiguredOutcome},
};

/// Describes a host's state during deployment review.
#[derive(Clone, Copy)]
enum HostStatus {
    /// The deployed and desired systems match.
    UpToDate,
    /// The desired system needs to be activated.
    OutOfDate,
    /// Information collection failed.
    Failed,
}

impl Display for HostStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UpToDate => "up to date",
            Self::OutOfDate => "out of date",
            Self::Failed => "failed",
        })
    }
}

/// Describes whether a host passed its configured verifier.
#[derive(Clone, Copy)]
pub(super) enum Validity {
    /// The verifier passed.
    Valid,
    /// The host has no configured verifier.
    Missing,
    /// Verifier discovery, building, or execution failed.
    Failed,
    /// Verification was not requested for this table.
    Unchecked,
}

impl Display for Validity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Valid => "yes",
            Self::Missing | Self::Unchecked => "-",
            Self::Failed => "failed",
        })
    }
}

/// Provides a row in the deployment status table.
#[derive(tabled::Tabled)]
pub(super) struct HostRow {
    /// Identifies the inventory entry.
    #[tabled(rename = "ID")]
    id: String,
    /// Identifies the flake output.
    #[tabled(rename = "FLAKE")]
    flake: String,
    /// Summarizes the review result.
    #[tabled(rename = "STATUS")]
    status: HostStatus,
    /// Reports the configured verifier result.
    #[tabled(rename = "VALID")]
    valid: Validity,
    /// Displays the store-path transition when deployment is needed.
    #[tabled(rename = "CHANGE")]
    change: String,
}

impl HostRow {
    /// Creates a row for a host whose state could not be collected.
    pub(super) fn failed(host: &Host, valid: Validity) -> Self {
        Self {
            id: host.id().to_owned(),
            flake: host.display_flake(),
            status: HostStatus::Failed,
            valid,
            change: "-".to_owned(),
        }
    }

    /// Creates a row from the deployed and desired systems.
    fn collected(host: &Host, deployed: &StorePath, expected: &StorePath, valid: Validity) -> Self {
        let up_to_date = deployed == expected;
        Self {
            id: host.id().to_owned(),
            flake: host.display_flake(),
            status: if up_to_date {
                HostStatus::UpToDate
            } else {
                HostStatus::OutOfDate
            },
            valid,
            change: if up_to_date {
                "-".to_owned()
            } else {
                format!("{} -> {}", deployed.short_hash(), expected.short_hash())
            },
        }
    }

    /// Returns whether the host needs activation work.
    pub(super) fn needs_activation(&self) -> bool {
        matches!(self.status, HostStatus::OutOfDate)
    }
}

/// Describes the result of reviewing one host.
pub(super) enum ReviewOutcome {
    /// The host already runs the desired system.
    UpToDate(HostRow),
    /// The host needs to be tested, verified, and switched.
    OutOfDate(HostRow, ReviewedHost),
}

/// Contains the state collected for one outdated host.
pub(super) struct ReviewedHost {
    /// Provides host operations and connection details.
    pub(super) host: Host,
    /// Identifies the system observed during review.
    pub(super) deployed: StorePath,
    /// Identifies the desired system observed during review.
    pub(super) expected: StorePath,
}

impl ReviewedHost {
    /// Returns the inventory ID.
    pub(super) fn id(&self) -> &str {
        self.host.id()
    }
}

/// Reports information collection after a local or remote step finishes.
async fn collect_step<T, E>(
    progress: &ProgressBar,
    source: &str,
    step: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let result = step.await;
    let status = if result.is_ok() {
        "collected"
    } else {
        "failed"
    };
    progress.set_message(format!("{source} info {status}"));
    result
}

/// Collects the system currently active on one host.
pub(super) async fn deployed(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    collect_step(
        progress,
        "remote",
        operations::deployed_store_path(host, processes, progress),
    )
    .await
}

/// Collects the system expected from one host's local configuration.
pub(super) async fn expected(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    collect_step(
        progress,
        "local",
        operations::expected_store_path(host, processes, progress),
    )
    .await
}

/// Checks the currently deployed host with its configured verifier.
pub(super) async fn validity(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<ConfiguredOutcome, Error> {
    let result = verification::run_configured(host, processes, progress).await;
    status::set_phase(progress, host.id(), DeploymentPhase::CheckingStatus);
    let status = if result.is_ok() {
        "collected"
    } else {
        "failed"
    };
    progress.set_message(format!("validation info {status}"));
    result
}

/// Completes the local collection step after a flake update failure.
pub(super) fn skip_expected(progress: &ProgressBar) {
    progress.set_message("local info skipped");
}

/// Completes the validation step after a flake update failure.
pub(super) fn skip_validity(progress: &ProgressBar) {
    progress.set_message("validation skipped");
}

/// Compares the deployed and expected systems for one host.
pub(super) fn review(
    host: Host,
    deployed: StorePath,
    expected: &StorePath,
    valid: Validity,
) -> ReviewOutcome {
    let row = HostRow::collected(&host, &deployed, expected, valid);

    if &deployed == expected {
        return ReviewOutcome::UpToDate(row);
    }

    ReviewOutcome::OutOfDate(
        row,
        ReviewedHost {
            host,
            deployed,
            expected: expected.clone(),
        },
    )
}
