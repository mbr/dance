//! Local Nix and remote NixOS command execution.

use std::{num::NonZeroU32, time::Duration};

use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{
    error::Error,
    inventory::Host,
    process::{Invocation, ProcessContext},
    ssh,
    store_path::StorePath,
};

/// Contains a system's identity and optional source provenance.
#[derive(Deserialize)]
pub(super) struct SystemInfo {
    /// Identifies the system output used for deployment comparisons.
    pub(super) path: StorePath,
    /// Identifies the source revision supplied by the configuration author.
    #[serde(rename = "configurationRevision")]
    pub(super) revision: Option<String>,
}

/// Contains optional provenance reported by a generation's version command.
#[derive(Deserialize)]
struct SystemVersion {
    /// Identifies the configuration source, independently of the Nixpkgs revision.
    #[serde(rename = "configurationRevision")]
    revision: Option<String>,
}

/// Selects identity and provenance from the unmodified NixOS configuration.
const EXPECTED_SYSTEM: &str = r#"system: {
  path = system.config.system.build.toplevel.outPath;
  configurationRevision = system.config.system.configurationRevision;
}"#;

/// Limits routine SSH operations.
pub(super) const SSH_TIMEOUT: Duration = Duration::from_mins(1);
/// Bounds rebuild actions performed while the rollback watchdog is armed.
const REBUILD_TIMEOUT: Duration = Duration::from_mins(2);

/// Creates an SSH invocation targeting one inventory host.
pub(super) fn ssh<'a>(
    host: &Host,
    label: impl Into<String>,
    processes: &'a ProcessContext<'a>,
    progress: &'a ProgressBar,
) -> Invocation<'a> {
    Invocation::new(processes, label, "ssh", progress)
        .arguments(ssh::ARGUMENTS)
        .argument(host.ssh_url())
}

/// Reads the system currently activated on a target.
pub(super) async fn deployed_store_path(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<StorePath, Error> {
    let output = ssh(
        host,
        format!("review-{}-remote", host.id()),
        processes,
        progress,
    )
    .argument("readlink -f /run/current-system")
    .timeout(SSH_TIMEOUT)
    .output()
    .await?;
    let value = output.trim();
    value.parse().map_err(Error::StorePath)
}

/// Reads the active system and any provenance supported by its version command.
///
/// Resolves `/run/current-system` once to keep the path and revision tied to the
/// same generation if another deployment switches the system during the query.
pub(super) async fn deployed_system(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<SystemInfo, Error> {
    let output = ssh(
        host,
        format!("review-{}-remote", host.id()),
        processes,
        progress,
    )
    .argument(
        r#"system=$(readlink -f /run/current-system) || exit
version=$("$system/sw/bin/nixos-version" --json 2>/dev/null) || version='{}'
printf '%s\n%s\n' "$system" "$version""#,
    )
    .timeout(SSH_TIMEOUT)
    .output()
    .await?;
    let (path, version) = output.split_once('\n').unwrap_or((&output, ""));
    let path = path.trim().parse().map_err(Error::StorePath)?;
    let version: SystemVersion =
        serde_json::from_str(version).map_err(|source| Error::ParseSystemInfo {
            host: host.id().to_owned(),
            source,
        })?;
    Ok(SystemInfo {
        path,
        revision: version.revision,
    })
}

/// Evaluates a host's desired system and provenance without building it.
pub(super) async fn expected_system(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<SystemInfo, Error> {
    let output = Invocation::new(
        processes,
        format!("review-{}-local", host.id()),
        "nix",
        progress,
    )
    .arguments(["eval", "--json"])
    .argument(host.nixos_output())
    .argument("--apply")
    .argument(EXPECTED_SYSTEM)
    .output()
    .await?;
    serde_json::from_str(&output).map_err(|source| Error::ParseSystemInfo {
        host: host.id().to_owned(),
        source,
    })
}

/// Builds and transfers a host configuration without activating it.
pub(super) async fn dry_activate(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    rebuild(host, "dry-activate", processes, progress)
        .status()
        .await
}

/// Builds and temporarily activates a host configuration.
pub(super) async fn test(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    rebuild(host, "test", processes, progress)
        .timeout(REBUILD_TIMEOUT)
        .status()
        .await
}

/// Builds and persistently activates a host configuration.
pub(super) async fn switch(
    host: &Host,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    rebuild(host, "switch", processes, progress)
        .timeout(REBUILD_TIMEOUT)
        .status()
        .await
}

/// Creates a complete `nixos-rebuild` deployment action.
fn rebuild<'a>(
    host: &Host,
    action: &str,
    processes: &'a ProcessContext<'a>,
    progress: &'a ProgressBar,
) -> Invocation<'a> {
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
    .environment("NIX_SSHOPTS", ssh::nix_options())
}

/// Prunes old generations and collects unreachable target store paths.
pub(super) async fn collect_garbage(
    host: &Host,
    keep_generations: NonZeroU32,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Result<(), Error> {
    ssh(
        host,
        format!("deploy-{}-prune-generations", host.id()),
        processes,
        progress,
    )
    .arguments([
        "nix-env",
        "--profile",
        "/nix/var/nix/profiles/system",
        "--delete-generations",
    ])
    .argument(format!("+{keep_generations}"))
    .status()
    .await?;

    ssh(
        host,
        format!("deploy-{}-refresh-bootloader", host.id()),
        processes,
        progress,
    )
    .arguments(["/run/current-system/bin/switch-to-configuration", "boot"])
    .status()
    .await?;

    ssh(
        host,
        format!("deploy-{}-collect-garbage", host.id()),
        processes,
        progress,
    )
    .arguments(["nix-store", "--gc"])
    .status()
    .await
}

#[cfg(test)]
mod tests {
    use super::{SystemInfo, SystemVersion};

    /// Checks optional provenance does not weaken system-path validation.
    #[test]
    fn parses_system_identity_and_optional_provenance() {
        let info: SystemInfo = serde_json::from_str(
            r#"{"path":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system",
                "configurationRevision":null}"#,
        )
        .expect("system information should parse");
        assert!(info.revision.is_none());
        let version: SystemVersion = serde_json::from_str(
            r#"{"configurationRevision":"release-1","nixpkgsRevision":"other"}"#,
        )
        .expect("version information should parse");
        assert_eq!(version.revision.as_deref(), Some("release-1"));
        let missing: SystemVersion = serde_json::from_str(r#"{"nixpkgsRevision":"other"}"#)
            .expect("configuration revision should be optional");
        assert!(missing.revision.is_none());
        assert!(serde_json::from_str::<SystemInfo>(r#"{"path":"/tmp/not-a-system"}"#).is_err());
        assert!(serde_json::from_str::<SystemVersion>(r#"{"configurationRevision":42}"#).is_err());
    }
}
