//! Per-host testing, verification, activation, rollback, and cleanup.

use std::fmt::{self, Display, Formatter};

use indicatif::ProgressBar;

use super::{ActivationOptions, operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    process::ProcessContext,
    status::{self, DeploymentPhase},
    verification,
};

/// Describes the result of one host deployment.
pub(super) enum Outcome {
    /// The host was activated and cleaned successfully.
    Activated,
    /// The deployment failed without requiring operator recovery.
    Failed(Error),
    /// The host restored its reviewed system after a deployment failure.
    RolledBack(Error),
    /// Live restoration failed and the host requires operator recovery.
    RecoveryRequired(Error),
    /// The host was activated but could not be cleaned.
    CleanupFailed(Error),
}

impl Outcome {
    /// Finalizes progress output for a completed host operation.
    pub(super) fn finish_progress(&self, host: &str, progress: &ProgressBar) {
        match self {
            Self::Activated => status::finish(progress, host),
            Self::RecoveryRequired(_) => status::recovery_required(progress),
            Self::Failed(_) | Self::RolledBack(_) | Self::CleanupFailed(_) => {
                status::fail(progress);
            }
        }
    }
}

/// Aggregates deployment outcomes for terminal output and exit status.
#[derive(Default)]
pub(super) struct Summary {
    /// Counts successful activations.
    activated: usize,
    /// Counts deployment failures left in place.
    failed: usize,
    /// Counts failures followed by successful restoration.
    rolled_back: usize,
    /// Counts hosts that need operator recovery.
    recovery_required: usize,
    /// Counts successful activations whose cleanup failed.
    cleanup_failed: usize,
    /// Retains the first deployment error.
    first_error: Option<Error>,
}

impl Summary {
    /// Adds one host outcome.
    pub(super) fn record(&mut self, outcome: Outcome) {
        let error = match outcome {
            Outcome::Activated => {
                self.activated += 1;
                return;
            }
            Outcome::Failed(error) => {
                self.failed += 1;
                error
            }
            Outcome::RolledBack(error) => {
                self.rolled_back += 1;
                error
            }
            Outcome::RecoveryRequired(error) => {
                self.recovery_required += 1;
                error
            }
            Outcome::CleanupFailed(error) => {
                self.activated += 1;
                self.cleanup_failed += 1;
                error
            }
        };
        self.first_error.get_or_insert(error);
    }

    /// Returns the first deployment failure when present.
    pub(super) fn result(self) -> Result<(), Error> {
        self.first_error.map_or(Ok(()), Err)
    }
}

impl Display for Summary {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "deployment: {} activated, {} failed",
            self.activated, self.failed
        )?;
        for (count, label) in [
            (self.rolled_back, "rolled back"),
            (self.recovery_required, "recovery required"),
            (self.cleanup_failed, "cleanup failed"),
        ] {
            if count != 0 {
                write!(formatter, ", {count} {label}")?;
            }
        }
        Ok(())
    }
}

/// Handles one failed deployment step according to the rollback policy.
async fn fail(
    reviewed: &ReviewedHost,
    options: ActivationOptions,
    persistence: rollback::Persistence,
    trigger: Error,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Outcome {
    if !options.rollback {
        return Outcome::Failed(trigger);
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::RollingBack);
    match rollback::restore(
        &reviewed.host,
        &reviewed.deployed,
        persistence,
        processes,
        progress,
    )
    .await
    {
        Ok(()) => Outcome::RolledBack(trigger),
        Err(rollback) => Outcome::RecoveryRequired(Error::RollbackIncomplete {
            host: reviewed.host.id().to_owned(),
            trigger: Box::new(trigger),
            rollback: Box::new(rollback),
        }),
    }
}

/// Temporarily activates, verifies, and persists one host configuration.
pub(super) async fn activate(
    reviewed: &ReviewedHost,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Testing);
    if let Err(error) = operations::test(&reviewed.host, processes, progress).await {
        return fail(
            reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            processes,
            progress,
        )
        .await;
    }

    if options.include_verifier
        && let Err(error) = verification::run_configured(&reviewed.host, processes, progress).await
    {
        return fail(
            reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            processes,
            progress,
        )
        .await;
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Switching);
    if let Err(error) = operations::switch(&reviewed.host, processes, progress).await {
        return fail(
            reviewed,
            options,
            rollback::Persistence::System,
            error,
            processes,
            progress,
        )
        .await;
    }

    if let Some(keep_generations) = options.keep_generations {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::CollectingGarbage,
        );
        if let Err(error) =
            operations::collect_garbage(&reviewed.host, keep_generations, processes, progress).await
        {
            return Outcome::CleanupFailed(error);
        }
    }

    Outcome::Activated
}
