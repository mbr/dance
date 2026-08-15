//! Target-side rollback watchdog management.

use indicatif::ProgressBar;
use uuid::Uuid;

use super::operations::{self, SSH_TIMEOUT};
use crate::{
    error::Error, inventory::Host, logs::RunLog, process::run_status_bounded, store_path::StorePath,
};

/// Gives activation and its local timeout enough time to settle.
const ACTIVATION_TIMEOUT_SECONDS: u64 = 360;
/// Gives verification and watchdog disarming a bounded completion window.
const VERIFICATION_TIMEOUT_SECONDS: u64 = 180;

/// Selects the deadline protected by a rollback watchdog.
pub(super) enum Phase {
    /// Protects target activation.
    Activation,
    /// Protects post-activation verification.
    Verification,
}

impl Phase {
    /// Returns the phase label used in command diagnostics.
    fn label(&self) -> &'static str {
        match self {
            Self::Activation => "activation",
            Self::Verification => "verification",
        }
    }

    /// Returns the watchdog deadline in seconds.
    fn timeout_seconds(&self) -> u64 {
        match self {
            Self::Activation => ACTIVATION_TIMEOUT_SECONDS,
            Self::Verification => VERIFICATION_TIMEOUT_SECONDS,
        }
    }
}

/// Identifies one armed target-side rollback watchdog.
pub(super) struct Watchdog {
    /// Contains the transient systemd unit stem.
    unit: String,
}

/// Arms a target-side watchdog for the reviewed system.
pub(super) async fn arm(
    host: &Host,
    previous: &StorePath,
    phase: Phase,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<Watchdog, Error> {
    let unit = format!("dance-rollback-{}", Uuid::now_v7());
    let rollback = rollback_command(previous);
    let command = format!(
        "systemd-run --quiet --collect --unit={} --on-active={}s --property=Type=oneshot -- /bin/sh -c {}",
        shell_quote(&unit),
        phase.timeout_seconds(),
        shell_quote(&rollback)
    );
    let label = format!("deploy-{}-arm-{}-rollback", host.id(), phase.label());
    run_status_bounded(
        logs,
        &label,
        "ssh",
        &[host.ssh_url().into(), command.into()],
        &[],
        SSH_TIMEOUT,
        progress,
    )
    .await?;
    Ok(Watchdog { unit })
}

/// Disarms a target-side rollback watchdog.
pub(super) async fn disarm(
    host: &Host,
    watchdog: &Watchdog,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let label = format!("deploy-{}-disarm-rollback", host.id());
    let command = format!("systemctl stop {}.timer", shell_quote(&watchdog.unit));
    run_status_bounded(
        logs,
        &label,
        "ssh",
        &[host.ssh_url().into(), command.into()],
        &[],
        SSH_TIMEOUT,
        progress,
    )
    .await
}

/// Restores the reviewed system and disarms its watchdogs.
///
/// # Panics
///
/// Panics when called without an armed watchdog.
pub(super) async fn restore(
    host: &Host,
    previous: &StorePath,
    watchdogs: &[Watchdog],
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let watchdog = watchdogs
        .last()
        .expect("rollback should have an armed watchdog");
    let label = format!("deploy-{}-rollback", host.id());
    let command = format!("systemctl start {}.service", shell_quote(&watchdog.unit));
    run_status_bounded(
        logs,
        &label,
        "ssh",
        &[host.ssh_url().into(), command.into()],
        &[],
        SSH_TIMEOUT,
        progress,
    )
    .await?;

    let current = operations::deployed_store_path(host, "verify-rollback", logs, progress).await?;
    if &current != previous {
        return Err(Error::RollbackMismatch {
            host: host.id().to_owned(),
            expected: previous.clone(),
            current,
        });
    }
    for watchdog in watchdogs {
        disarm(host, watchdog, logs, progress).await?;
    }
    Ok(())
}

/// Returns the remote command that restores a reviewed system.
fn rollback_command(previous: &StorePath) -> String {
    let previous = previous.to_string();
    format!(
        "/run/current-system/sw/bin/nix-env --profile /nix/var/nix/profiles/system --set {} && {}/bin/switch-to-configuration switch",
        shell_quote(&previous),
        shell_quote(&previous)
    )
}

/// Quotes one value for a POSIX-compatible remote shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    /// Checks that values remain one argument in remote shell commands.
    #[test]
    fn quotes_remote_shell_values() {
        assert_eq!(shell_quote("one'two"), "'one'\"'\"'two'");
    }
}
