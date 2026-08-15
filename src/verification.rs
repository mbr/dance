//! Concurrent execution of flake-provided verifiers.

use futures::future::join_all;
use indicatif::ProgressBar;

use crate::{
    error::Error,
    inventory::{Host, Inventory},
    logs::RunLog,
    status::{self, DeploymentPhase, ProgressDisplay},
};

/// Describes one host's standalone verification result.
enum Outcome {
    /// The configured verifier succeeded.
    Verified,
    /// The host has no configured verifier.
    NotConfigured,
    /// Verifier discovery, building, or execution failed.
    Failed(Error),
}

/// Runs the verifier configured for one host.
async fn verify_host(host: Host, logs: &RunLog, progress: &ProgressBar) -> Outcome {
    status::set_phase(progress, host.id(), DeploymentPhase::Building);
    let verifier = match host.verifier(logs, progress).await {
        Ok(Some(verifier)) => verifier,
        Ok(None) => {
            status::verifier_missing(progress, host.id());
            return Outcome::NotConfigured;
        }
        Err(error) => return Outcome::Failed(error),
    };
    if let Err(error) = host.build_verifier(&verifier, logs, progress).await {
        return Outcome::Failed(error);
    }

    status::set_phase(progress, host.id(), DeploymentPhase::RunningVerifier);
    match host.verify(&verifier, logs, progress).await {
        Ok(()) => {
            status::verified(progress, host.id());
            Outcome::Verified
        }
        Err(error) => Outcome::Failed(error),
    }
}

/// Runs configured verifiers for all selected hosts.
pub(crate) async fn verify(inventory: Inventory, logs: &RunLog) -> Result<(), Error> {
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(hosts.iter().map(Host::id));
    let outcomes = join_all(hosts.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let outcome = verify_host(host, logs, progress).await;
            if matches!(&outcome, Outcome::Failed(_)) {
                status::fail(progress);
            }
            outcome
        },
    ))
    .await;

    let mut verified = 0;
    let mut not_configured = 0;
    let mut failed = 0;
    let mut first_error = None;
    for outcome in outcomes {
        match outcome {
            Outcome::Verified => verified += 1,
            Outcome::NotConfigured => not_configured += 1,
            Outcome::Failed(error) => {
                failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }
    println!("verification: {verified} passed, {not_configured} not configured, {failed} failed");
    first_error.map_or(Ok(()), Err)
}
