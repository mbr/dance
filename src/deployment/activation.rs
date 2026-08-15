//! Per-host activation, verification, rollback, and cleanup.

use std::num::NonZeroU32;

use indicatif::ProgressBar;

use super::{operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    logs::RunLog,
    status::{self, DeploymentPhase},
    verification::Verifier,
};

/// Configures verifier execution, rollback, and cleanup for one host.
#[derive(Clone, Copy)]
pub(super) struct Options {
    /// Selects whether a configured verifier is included.
    pub(super) include_verifier: bool,
    /// Selects whether post-activation failures trigger rollback.
    pub(super) rollback: bool,
    /// Selects cleanup and generation retention after activation.
    pub(super) keep_generations: Option<NonZeroU32>,
}

/// Describes the result of activation and post-deployment cleanup.
pub(super) enum Outcome {
    /// The host was activated and cleaned successfully.
    Activated,
    /// The host could not be activated.
    ActivationFailed(Error),
    /// The host activated but its configured verifier failed.
    VerificationFailed(Error),
    /// The host restored its reviewed system after a deployment failure.
    RolledBack(Error),
    /// Rollback remained incomplete while the target watchdog stayed armed.
    RollbackPending(Error),
    /// The host was activated but could not be cleaned.
    CleanupFailed(Error),
}

/// Restores the reviewed system after a post-watchdog failure.
async fn recover(
    reviewed: &ReviewedHost,
    watchdogs: &[rollback::Watchdog],
    trigger: Error,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::RollingBack);
    match rollback::restore(
        &reviewed.host,
        &reviewed.deployed,
        watchdogs,
        logs,
        progress,
    )
    .await
    {
        Ok(()) => Outcome::RolledBack(trigger),
        Err(rollback) => Outcome::RollbackPending(Error::RollbackIncomplete {
            host: reviewed.host.id().to_owned(),
            trigger: Box::new(trigger),
            rollback: Box::new(rollback),
        }),
    }
}

/// Activates one host and performs configured verification and cleanup.
pub(super) async fn activate(
    reviewed: ReviewedHost,
    options: Options,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Building);
    let verifier = if options.include_verifier {
        match Verifier::discover(&reviewed.host, logs, progress).await {
            Ok(verifier) => verifier,
            Err(error) => return Outcome::ActivationFailed(error),
        }
    } else {
        None
    };
    let verifier_installable = verifier
        .as_ref()
        .map(|verifier| verifier.installable(&reviewed.host));

    if let Err(error) = operations::build(
        &reviewed.host,
        verifier_installable.as_deref(),
        logs,
        progress,
    )
    .await
    {
        return Outcome::ActivationFailed(error);
    }
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Preparing);
    if let Err(error) =
        operations::prepare_activation(&reviewed.host, &reviewed.expected, logs, progress).await
    {
        return Outcome::ActivationFailed(error);
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Verifying);
    let current = match operations::deployed_store_path(
        &reviewed.host,
        "verify-before",
        logs,
        progress,
    )
    .await
    {
        Ok(current) => current,
        Err(error) => return Outcome::ActivationFailed(error),
    };
    if current != reviewed.deployed {
        return Outcome::ActivationFailed(Error::TargetChanged {
            host: reviewed.host.id().to_owned(),
            reviewed: reviewed.deployed,
            current,
        });
    }

    let mut watchdogs = Vec::new();
    if options.rollback {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::ArmingRollback,
        );
        match rollback::arm(
            &reviewed.host,
            &reviewed.deployed,
            rollback::Phase::Activation,
            logs,
            progress,
        )
        .await
        {
            Ok(watchdog) => watchdogs.push(watchdog),
            Err(error) => return Outcome::ActivationFailed(error),
        }
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Activating);
    if let Err(error) =
        operations::activate(&reviewed.host, &reviewed.expected, logs, progress).await
    {
        return if options.rollback {
            recover(&reviewed, &watchdogs, error, logs, progress).await
        } else {
            Outcome::ActivationFailed(error)
        };
    }

    if options.rollback {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::ArmingRollback,
        );
        let verification = match rollback::arm(
            &reviewed.host,
            &reviewed.deployed,
            rollback::Phase::Verification,
            logs,
            progress,
        )
        .await
        {
            Ok(watchdog) => watchdog,
            Err(error) => return recover(&reviewed, &watchdogs, error, logs, progress).await,
        };
        watchdogs.push(verification);
        if let Err(error) = rollback::disarm(&reviewed.host, &watchdogs[0], logs, progress).await {
            return recover(&reviewed, &watchdogs, error, logs, progress).await;
        }
        watchdogs.remove(0);
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Verifying);
    let current =
        match operations::deployed_store_path(&reviewed.host, "verify-after", logs, progress).await
        {
            Ok(current) => current,
            Err(error) => {
                return if options.rollback {
                    recover(&reviewed, &watchdogs, error, logs, progress).await
                } else {
                    Outcome::ActivationFailed(error)
                };
            }
        };
    if current != reviewed.expected {
        let error = Error::ActivationMismatch {
            host: reviewed.host.id().to_owned(),
            expected: reviewed.expected.clone(),
            current,
        };
        return if options.rollback {
            recover(&reviewed, &watchdogs, error, logs, progress).await
        } else {
            Outcome::ActivationFailed(error)
        };
    }

    if let Some(verifier) = &verifier {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::RunningVerifier,
        );
        if let Err(error) = verifier.run(&reviewed.host, logs, progress).await {
            return if options.rollback {
                recover(&reviewed, &watchdogs, error, logs, progress).await
            } else {
                Outcome::VerificationFailed(error)
            };
        }
    }

    if let Some(watchdog) = watchdogs.first()
        && let Err(error) = rollback::disarm(&reviewed.host, watchdog, logs, progress).await
    {
        return recover(&reviewed, &watchdogs, error, logs, progress).await;
    }

    if let Some(keep_generations) = options.keep_generations {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::CollectingGarbage,
        );
        if let Err(error) =
            operations::collect_garbage(&reviewed.host, keep_generations, logs, progress).await
        {
            return Outcome::CleanupFailed(error);
        }
    }

    status::finish(progress, reviewed.host.id());
    Outcome::Activated
}
