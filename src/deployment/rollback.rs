//! Target-side rollback watchdog management.

use std::time::Duration;

use indicatif::ProgressBar;
use uuid::Uuid;

use super::operations::{self, SSH_TIMEOUT};
use crate::{error::Error, inventory::Host, process::ProcessContext, store_path::StorePath};

/// Bounds the activation and verification window after cache warming.
const WATCHDOG_TIMEOUT: Duration = Duration::from_mins(6);

/// Rejects a host still protected by an earlier deployment watchdog.
pub(super) async fn ensure_absent(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    let units = operations::ssh(
        host,
        format!("deploy-{}-check-rollback", host.id()),
        processes,
        progress,
    )
    .argument("systemctl list-units --all --full --plain --no-legend 'dance-rollback-*'")
    .timeout(SSH_TIMEOUT)
    .output()
    .await?;
    let units = units.trim();
    if units.is_empty() {
        Ok(())
    } else {
        Err(Error::ExistingRollbackWatchdog {
            host: host.id().to_owned(),
            units: units.to_owned(),
        })
    }
}

/// Identifies one armed target-side rollback watchdog.
///
/// Dropping the handle intentionally leaves the target timer armed.
#[must_use = "an armed rollback watchdog must be explicitly disarmed"]
pub(super) struct Watchdog {
    /// Contains the transient systemd unit stem.
    unit: String,
}

impl Watchdog {
    /// Arms a target-side watchdog for the reviewed system.
    pub(super) async fn arm(
        host: &Host,
        previous: &StorePath,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<Self, Error> {
        let unit = format!("dance-rollback-{}", Uuid::now_v7());
        operations::ssh(
            host,
            format!("deploy-{}-arm-rollback", host.id()),
            processes,
            progress,
        )
        .argument(arm_command(&unit, previous))
        .timeout(SSH_TIMEOUT)
        .status()
        .await?;
        Ok(Self { unit })
    }

    /// Restores the reviewed system immediately and disarms the watchdog.
    pub(super) async fn restore(
        &self,
        host: &Host,
        previous: &StorePath,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        operations::ssh(
            host,
            format!("deploy-{}-trigger-rollback", host.id()),
            processes,
            progress,
        )
        .argument(format!(
            "systemctl start {}.service",
            shell_quote(&self.unit)
        ))
        .timeout(SSH_TIMEOUT)
        .status()
        .await?;

        let active = operations::deployed_store_path(host, processes, progress).await?;
        if &active != previous {
            return Err(Error::RollbackMismatch {
                host: host.id().to_owned(),
                expected: previous.clone(),
                active,
            });
        }
        self.disarm(host, processes, progress).await
    }

    /// Disarms a target-side watchdog after a conclusive outcome.
    pub(super) async fn disarm(
        &self,
        host: &Host,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        operations::ssh(
            host,
            format!("deploy-{}-disarm-rollback", host.id()),
            processes,
            progress,
        )
        .argument(format!("systemctl stop {}.timer", shell_quote(&self.unit)))
        .timeout(SSH_TIMEOUT)
        .status()
        .await
    }
}

/// Builds the command that creates a transient rollback timer.
fn arm_command(unit: &str, previous: &StorePath) -> String {
    let timeout = WATCHDOG_TIMEOUT.as_secs();
    let rollback = rollback_command(previous);
    format!(
        "systemd-run --quiet --collect --unit={} --on-active={timeout}s --timer-property=AccuracySec=1s --property=Type=oneshot -- /bin/sh -c {}",
        shell_quote(unit),
        shell_quote(&rollback),
    )
}

/// Builds the command that restores a reviewed system persistently.
fn rollback_command(previous: &StorePath) -> String {
    let profile = previous.as_ref().join("sw/bin/nix-env");
    let activate = previous.as_ref().join("bin/switch-to-configuration");
    format!(
        "{} --profile /nix/var/nix/profiles/system --set {} && {} switch",
        shell_quote(&profile.to_string_lossy()),
        shell_quote(&previous.to_string()),
        shell_quote(&activate.to_string_lossy()),
    )
}

/// Quotes one value for a POSIX-compatible remote shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{arm_command, rollback_command, shell_quote};
    use crate::store_path::StorePath;

    /// Checks remote quoting and the complete target-side watchdog command.
    #[test]
    fn constructs_rollback_watchdogs() {
        assert_eq!(shell_quote("one'two"), "'one'\"'\"'two'");

        let path = "/nix/store/00000000000000000000000000000000-nixos-system-old";
        let previous = path.parse::<StorePath>().expect("store path should parse");
        let rollback = rollback_command(&previous);
        assert_eq!(
            rollback,
            format!(
                "'{path}/sw/bin/nix-env' --profile /nix/var/nix/profiles/system --set '{path}' && '{path}/bin/switch-to-configuration' switch"
            )
        );
        assert_eq!(
            arm_command("dance-rollback-test", &previous),
            format!(
                "systemd-run --quiet --collect --unit='dance-rollback-test' --on-active=360s --timer-property=AccuracySec=1s --property=Type=oneshot -- /bin/sh -c {}",
                shell_quote(&rollback)
            )
        );
    }
}
