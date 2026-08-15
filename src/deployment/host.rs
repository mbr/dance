//! Per-host review and activation workflows.

use std::future::Future;

use indicatif::ProgressBar;

use crate::{
    error::Error,
    inventory::Host,
    status::{self, DeploymentPhase},
    store_path::StorePath,
};

/// Provides a row in the deployment status table.
#[derive(tabled::Tabled)]
pub(super) struct HostRow {
    /// Identifies the inventory entry.
    #[tabled(rename = "HOST")]
    host: String,
    /// Identifies the flake output.
    #[tabled(rename = "FLAKE")]
    flake: String,
    /// Summarizes the review result.
    #[tabled(rename = "STATUS")]
    status: &'static str,
    /// Displays the store-path transition when deployment is needed.
    #[tabled(rename = "CHANGE")]
    change: String,
}

impl HostRow {
    /// Creates a row before system information has been collected.
    pub(super) fn collecting(host: &Host) -> Self {
        Self {
            host: host.name().to_owned(),
            flake: host.flake(),
            status: "collecting",
            change: "-".to_owned(),
        }
    }

    /// Creates a row from the deployed and desired systems.
    fn collected(host: &Host, deployed: &StorePath, expected: &StorePath) -> Self {
        Self {
            host: host.name().to_owned(),
            flake: host.flake(),
            status: if deployed == expected {
                "up to date"
            } else {
                "out of date"
            },
            change: if deployed == expected {
                "-".to_owned()
            } else {
                format!("{} -> {}", deployed.short_hash(), expected.short_hash())
            },
        }
    }

    /// Returns whether the host needs activation work.
    pub(super) fn needs_activation(&self) -> bool {
        self.status == "out of date"
    }

    /// Marks an information collection failure.
    pub(super) fn fail(&mut self) {
        self.status = "failed";
    }
}

/// Describes the result of reviewing one host.
pub(super) enum ReviewOutcome {
    /// The host already runs the desired system.
    UpToDate(HostRow),
    /// The host needs to be built, prepared, and activated.
    OutOfDate(HostRow, Box<ReviewedHost>),
}

/// Contains the state collected for one outdated host.
pub(super) struct ReviewedHost {
    /// Provides host operations and connection details.
    host: Host,
    /// Identifies the system observed during review.
    deployed: StorePath,
    /// Identifies the desired system.
    expected: StorePath,
}

impl ReviewedHost {
    /// Returns the inventory name.
    pub(super) fn name(&self) -> &str {
        self.host.name()
    }
}

/// Advances information collection after a local or remote step finishes.
async fn collect_step<T, E>(
    progress: &ProgressBar,
    host: &str,
    source: &str,
    step: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let result = step.await;
    let status = if result.is_ok() {
        "collected"
    } else {
        "failed"
    };
    progress.set_message(format!("{host}: {source} info {status}"));
    progress.inc(1);
    result
}

/// Collects the deployed and desired state of one host.
pub(super) async fn review(host: Host, progress: &ProgressBar) -> Result<ReviewOutcome, Error> {
    let (deployed, expected) = tokio::join!(
        collect_step(
            progress,
            host.name(),
            "remote",
            host.deployed_store_path(progress),
        ),
        collect_step(
            progress,
            host.name(),
            "local",
            host.expected_store_path(progress),
        ),
    );
    let deployed = deployed?;
    let expected = expected?;
    let row = HostRow::collected(&host, &deployed, &expected);

    if deployed == expected {
        return Ok(ReviewOutcome::UpToDate(row));
    }

    Ok(ReviewOutcome::OutOfDate(
        row,
        Box::new(ReviewedHost {
            host,
            deployed,
            expected,
        }),
    ))
}

/// Builds, prepares, and activates one outdated host.
pub(super) async fn activate(reviewed: ReviewedHost, progress: &ProgressBar) -> Result<(), Error> {
    status::set_phase(progress, reviewed.host.name(), DeploymentPhase::Building);
    reviewed.host.build(progress).await?;
    status::set_phase(progress, reviewed.host.name(), DeploymentPhase::Preparing);
    reviewed
        .host
        .prepare_activation(&reviewed.expected, progress)
        .await?;

    status::set_phase(progress, reviewed.host.name(), DeploymentPhase::Verifying);
    let current = reviewed.host.deployed_store_path(progress).await?;
    if current != reviewed.deployed {
        return Err(Error::TargetChanged {
            host: reviewed.host.name().to_owned(),
            reviewed: reviewed.deployed,
            current,
        });
    }

    status::set_phase(progress, reviewed.host.name(), DeploymentPhase::Activating);
    reviewed.host.activate(&reviewed.expected, progress).await?;

    status::set_phase(progress, reviewed.host.name(), DeploymentPhase::Verifying);
    let current = reviewed.host.deployed_store_path(progress).await?;
    if current != reviewed.expected {
        return Err(Error::ActivationMismatch {
            host: reviewed.host.name().to_owned(),
            expected: reviewed.expected,
            current,
        });
    }

    status::finish(progress, reviewed.host.name());
    Ok(())
}

#[cfg(test)]
mod tests {
    use indicatif::ProgressBar;

    use super::collect_step;

    /// Checks that failed collection work still completes its progress step.
    #[tokio::test]
    async fn failed_collection_advances_progress() {
        let progress = ProgressBar::hidden();
        let result = collect_step(&progress, "host", "remote", async { Err::<(), ()>(()) }).await;

        assert!(result.is_err());
        assert_eq!(progress.position(), 1);
        assert_eq!(progress.message(), "host: remote info failed");
    }
}
