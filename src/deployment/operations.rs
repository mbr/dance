//! Local Nix and remote NixOS command execution.

use std::{num::NonZeroU32, time::Duration};

use indicatif::ProgressBar;

use crate::{
    error::Error,
    inventory::Host,
    logs::RunLog,
    process::{run, run_bounded, run_status, run_status_bounded},
    store_path::StorePath,
};

/// Limits how long target activation may run.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(240);
/// Limits routine SSH operations.
pub(super) const SSH_TIMEOUT: Duration = Duration::from_secs(60);

/// Reads the system currently activated on a target.
pub(super) async fn deployed_store_path(
    host: &Host,
    phase: &str,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    let label = format!("{phase}-{}-remote", host.id());
    let output = run_bounded(
        logs,
        &label,
        "ssh",
        &[
            host.ssh_url().into(),
            "readlink -f /run/current-system".into(),
        ],
        SSH_TIMEOUT,
        progress,
    )
    .await?;
    let value = output.trim();
    value.parse().map_err(|source| Error::InvalidStorePath {
        path: value.to_owned(),
        source,
    })
}

/// Evaluates a host's desired system without building it.
pub(super) async fn expected_store_path(
    host: &Host,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    let label = format!("review-{}-local", host.id());
    let output = run(
        logs,
        &label,
        "nix",
        &["eval".into(), "--raw".into(), flake_output(host).into()],
        progress,
    )
    .await?;
    let value = output.trim();
    value.parse().map_err(|source| Error::InvalidStorePath {
        path: value.to_owned(),
        source,
    })
}

/// Builds a host configuration and an optional additional installable.
pub(super) async fn build(
    host: &Host,
    additional_installable: Option<&str>,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let label = format!("deploy-{}-build", host.id());
    let mut arguments = vec![
        "build".into(),
        "--no-link".into(),
        flake_output(host).into(),
    ];
    if let Some(installable) = additional_installable {
        arguments.push(installable.into());
    }
    run_status(logs, &label, "nix", &arguments, progress).await
}

/// Copies a built system to a target and performs a dry activation.
pub(super) async fn prepare_activation(
    host: &Host,
    store_path: &StorePath,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let label = format!("deploy-{}-prepare", host.id());
    run_status(
        logs,
        &label,
        "nixos-rebuild",
        &[
            "dry-activate".into(),
            "--use-substitutes".into(),
            "--store-path".into(),
            store_path.as_ref().as_os_str().to_owned(),
            "--target-host".into(),
            host.ssh_url().into(),
        ],
        progress,
    )
    .await
}

/// Activates a built system on a target within the activation deadline.
pub(super) async fn activate(
    host: &Host,
    store_path: &StorePath,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let label = format!("deploy-{}-activate", host.id());
    run_status_bounded(
        logs,
        &label,
        "nixos-rebuild",
        &[
            "switch".into(),
            "--no-reexec".into(),
            "--use-substitutes".into(),
            "--store-path".into(),
            store_path.as_ref().as_os_str().to_owned(),
            "--target-host".into(),
            host.ssh_url().into(),
        ],
        &[],
        ACTIVATION_TIMEOUT,
        progress,
    )
    .await
}

/// Prunes old generations and collects unreachable target store paths.
pub(super) async fn collect_garbage(
    host: &Host,
    keep_generations: NonZeroU32,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let label = format!("deploy-{}-prune-generations", host.id());
    run_status(
        logs,
        &label,
        "ssh",
        &[
            host.ssh_url().into(),
            "nix-env".into(),
            "--profile".into(),
            "/nix/var/nix/profiles/system".into(),
            "--delete-generations".into(),
            format!("+{keep_generations}").into(),
        ],
        progress,
    )
    .await?;

    let label = format!("deploy-{}-refresh-bootloader", host.id());
    run_status(
        logs,
        &label,
        "ssh",
        &[
            host.ssh_url().into(),
            "/run/current-system/bin/switch-to-configuration".into(),
            "boot".into(),
        ],
        progress,
    )
    .await?;

    let label = format!("deploy-{}-collect-garbage", host.id());
    run_status(
        logs,
        &label,
        "ssh",
        &[host.ssh_url().into(), "nix-store".into(), "--gc".into()],
        progress,
    )
    .await
}

/// Returns the complete flake output used to evaluate and build a system.
fn flake_output(host: &Host) -> String {
    format!(
        "{}#nixosConfigurations.{}.config.system.build.toplevel.outPath",
        host.flake_path().display(),
        host.flake_name()
    )
}
