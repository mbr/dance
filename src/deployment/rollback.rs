//! Best-effort restoration of the system reviewed before deployment.

use indicatif::ProgressBar;

use super::operations::{self, SSH_TIMEOUT};
use crate::{error::Error, inventory::Host, process::ProcessContext, store_path::StorePath};

/// Selects whether restoration also repairs the persistent system profile.
pub(super) enum Persistence {
    /// Leaves the already-correct profile and bootloader untouched.
    Runtime,
    /// Restores the profile after a failed persistent switch.
    System,
}

/// Restores the reviewed system after a failed deployment step.
pub(super) async fn restore(
    host: &Host,
    previous: &StorePath,
    persistence: Persistence,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let action = match persistence {
        Persistence::Runtime => "test",
        Persistence::System => {
            operations::ssh(
                host,
                format!("deploy-{}-rollback-profile", host.id()),
                processes,
                progress,
            )
            .argument(previous.as_ref().join("sw/bin/nix-env"))
            .arguments(["--profile", "/nix/var/nix/profiles/system", "--set"])
            .argument(previous.as_ref())
            .timeout(SSH_TIMEOUT)
            .status()
            .await?;
            "switch"
        }
    };

    operations::ssh(
        host,
        format!("deploy-{}-rollback-activate", host.id()),
        processes,
        progress,
    )
    .argument(previous.as_ref().join("bin/switch-to-configuration"))
    .argument(action)
    .timeout(SSH_TIMEOUT)
    .status()
    .await
}
