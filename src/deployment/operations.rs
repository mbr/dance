//! Local Nix and remote NixOS command execution.

use std::{num::NonZeroU32, time::Duration};

use indicatif::ProgressBar;

use crate::{
    error::Error,
    inventory::Host,
    process::{Invocation, ProcessContext},
    store_path::StorePath,
};

/// Limits routine SSH operations.
pub(super) const SSH_TIMEOUT: Duration = Duration::from_mins(1);

/// Reads the system currently activated on a target.
pub(super) async fn deployed_store_path(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    let output = Invocation::new(
        processes,
        format!("review-{}-remote", host.id()),
        "ssh",
        progress,
    )
    .argument(host.ssh_url())
    .argument("readlink -f /run/current-system")
    .timeout(SSH_TIMEOUT)
    .output()
    .await?;
    let value = output.trim();
    value.parse().map_err(Error::StorePath)
}

/// Evaluates a host's desired system without building it.
pub(super) async fn expected_store_path(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    let output = Invocation::new(
        processes,
        format!("review-{}-local", host.id()),
        "nix",
        progress,
    )
    .arguments(["eval", "--raw"])
    .argument(host.nixos_output())
    .output()
    .await?;
    let value = output.trim();
    value.parse().map_err(Error::StorePath)
}

/// Builds and temporarily activates a host configuration.
pub(super) async fn test(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    rebuild(host, "test", processes, progress).await
}

/// Builds and persistently activates a host configuration.
pub(super) async fn switch(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    rebuild(host, "switch", processes, progress).await
}

/// Delegates a complete deployment action to `nixos-rebuild`.
async fn rebuild(
    host: &Host,
    action: &str,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    Invocation::new(
        processes,
        format!("deploy-{}-{action}", host.id()),
        "nixos-rebuild",
        progress,
    )
    .argument(action)
    .arguments(["--use-substitutes", "--flake"])
    .argument(host.flake())
    .argument("--target-host")
    .argument(host.ssh_url())
    .status()
    .await
}

/// Prunes old generations and collects unreachable target store paths.
pub(super) async fn collect_garbage(
    host: &Host,
    keep_generations: NonZeroU32,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    Invocation::new(
        processes,
        format!("deploy-{}-prune-generations", host.id()),
        "ssh",
        progress,
    )
    .argument(host.ssh_url())
    .arguments([
        "nix-env",
        "--profile",
        "/nix/var/nix/profiles/system",
        "--delete-generations",
    ])
    .argument(format!("+{keep_generations}"))
    .status()
    .await?;

    Invocation::new(
        processes,
        format!("deploy-{}-refresh-bootloader", host.id()),
        "ssh",
        progress,
    )
    .argument(host.ssh_url())
    .arguments(["/run/current-system/bin/switch-to-configuration", "boot"])
    .status()
    .await?;

    Invocation::new(
        processes,
        format!("deploy-{}-collect-garbage", host.id()),
        "ssh",
        progress,
    )
    .argument(host.ssh_url())
    .arguments(["nix-store", "--gc"])
    .status()
    .await
}
