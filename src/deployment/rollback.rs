//! Best-effort restoration of the system reviewed before deployment.

use indicatif::ProgressBar;

use super::operations::SSH_TIMEOUT;
use crate::{
    error::Error, inventory::Host, logs::RunLog, process::Invocation, store_path::StorePath,
};

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
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let action = match persistence {
        Persistence::Runtime => "test",
        Persistence::System => {
            let label = format!("deploy-{}-rollback-profile", host.id());
            Invocation::new(
                logs,
                &label,
                "ssh",
                &[
                    host.ssh_url().into(),
                    previous
                        .as_ref()
                        .join("sw/bin/nix-env")
                        .as_os_str()
                        .to_owned(),
                    "--profile".into(),
                    "/nix/var/nix/profiles/system".into(),
                    "--set".into(),
                    previous.as_ref().as_os_str().to_owned(),
                ],
                progress,
            )
            .timeout(SSH_TIMEOUT)
            .status()
            .await?;
            "switch"
        }
    };

    let label = format!("deploy-{}-rollback-activate", host.id());
    Invocation::new(
        logs,
        &label,
        "ssh",
        &[
            host.ssh_url().into(),
            previous
                .as_ref()
                .join("bin/switch-to-configuration")
                .as_os_str()
                .to_owned(),
            action.into(),
        ],
        progress,
    )
    .timeout(SSH_TIMEOUT)
    .status()
    .await
}
