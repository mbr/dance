//! Best-effort restoration of the system reviewed before deployment.

use indicatif::ProgressBar;

use super::operations::SSH_TIMEOUT;
use crate::{
    error::Error, inventory::Host, logs::RunLog, process::run_status_bounded, store_path::StorePath,
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
    let previous = previous.to_string();
    let command = match persistence {
        Persistence::Runtime => format!(
            "{}/bin/switch-to-configuration test",
            shell_quote(&previous)
        ),
        Persistence::System => format!(
            "{}/sw/bin/nix-env --profile /nix/var/nix/profiles/system --set {} && {}/bin/switch-to-configuration switch",
            shell_quote(&previous),
            shell_quote(&previous),
            shell_quote(&previous),
        ),
    };
    let label = format!("deploy-{}-rollback", host.id());
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
