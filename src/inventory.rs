//! Inventory parsing and host operations.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::Duration,
};

use indexmap::IndexMap;
use indicatif::ProgressBar;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    error::Error,
    logs::RunLog,
    process::{run, run_bounded, run_status, run_status_bounded},
    store_path::StorePath,
    verifier::{self, Verifier},
};

/// Limits how long target activation may run.
pub(crate) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(240);
/// Gives an activation rollback watchdog time to fire after a local timeout.
const ACTIVATION_ROLLBACK_TIMEOUT: u64 = 360;
/// Limits routine SSH operations and configured verifiers.
const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Gives verification and watchdog disarming a bounded completion window.
const VERIFICATION_ROLLBACK_TIMEOUT: u64 = 180;
/// Identifies one armed target-side rollback watchdog.
pub(crate) struct RollbackWatchdog {
    /// Contains the transient systemd unit stem.
    unit: String,
}

/// Contains the hosts managed by Dance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
    /// Defines the managed hosts.
    hosts: IndexMap<String, Host>,
}

impl IntoIterator for Inventory {
    type IntoIter = indexmap::map::IntoValues<String, Host>;
    type Item = Host;

    fn into_iter(self) -> Self::IntoIter {
        self.hosts.into_values()
    }
}

impl Inventory {
    /// Loads an inventory and resolves its flake paths.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let contents = fs::read_to_string(path).map_err(|source| Error::ReadInventory {
            path: path.to_path_buf(),
            source,
        })?;
        let mut inventory: Self =
            toml::from_str(&contents).map_err(|source| Error::ParseInventory {
                path: path.to_path_buf(),
                source,
            })?;
        let directory = path.parent().unwrap_or_else(|| Path::new("."));

        for (id, host) in &mut inventory.hosts {
            host.id.clone_from(id);
            if host.flake_path.is_relative() {
                host.flake_path = directory.join(&host.flake_path);
            }
        }

        Ok(inventory)
    }

    /// Returns selected flake paths and host counts in inventory order.
    pub(crate) fn flake_groups(&self) -> Vec<(PathBuf, usize)> {
        let mut flakes = IndexMap::new();
        for host in self.hosts.values() {
            let count = flakes.entry(host.flake_path.clone()).or_insert(0);
            *count += 1;
        }
        flakes.into_iter().collect()
    }

    /// Limits the inventory to explicitly requested hosts.
    pub(crate) fn select(mut self, requested: &[String]) -> Result<Self, Error> {
        if requested.is_empty() {
            return Ok(self);
        }

        let mut selected = HashSet::new();
        for id in requested {
            if !selected.insert(id.as_str()) {
                return Err(Error::DuplicateSelectedId { id: id.clone() });
            }
            if !self.hosts.contains_key(id) {
                return Err(Error::UnknownId { id: id.clone() });
            }
        }

        self.hosts
            .retain(|id, _host| selected.contains(id.as_str()));
        Ok(self)
    }
}

/// Defines deployment metadata and connection details for a machine.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Host {
    /// Identifies the inventory entry.
    #[serde(skip)]
    id: String,
    /// Selects the SSH destination.
    ssh_host: String,
    /// Locates the flake relative to the inventory.
    flake_path: PathBuf,
    /// Selects the NixOS configuration in the flake.
    flake_name: String,
}

impl Host {
    /// Returns the inventory ID.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Returns the flake directory.
    pub(crate) fn flake_path(&self) -> &Path {
        &self.flake_path
    }

    /// Returns the display form of the flake selector.
    pub(crate) fn flake(&self) -> String {
        let name = self
            .flake_path
            .file_name()
            .map_or_else(|| self.flake_path.as_os_str(), |name| name)
            .to_string_lossy();
        format!("{name}#{}", self.flake_name)
    }

    /// Returns the root SSH destination.
    pub(crate) fn ssh_url(&self) -> String {
        format!("root@{}", self.ssh_host)
    }

    /// Reads the system currently activated on the target.
    pub(crate) async fn deployed_store_path(
        &self,
        phase: &str,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<StorePath, Error> {
        let label = format!("{phase}-{}-remote", self.id);
        let output = run_bounded(
            logs,
            &label,
            "ssh",
            &[
                self.ssh_url().into(),
                "readlink -f /run/current-system".into(),
            ],
            VERIFICATION_TIMEOUT,
            progress,
        )
        .await?;
        let value = output.trim();
        value.parse().map_err(|source| Error::InvalidStorePath {
            path: value.to_owned(),
            source,
        })
    }

    /// Evaluates the desired system without building it.
    pub(crate) async fn expected_store_path(
        &self,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<StorePath, Error> {
        let label = format!("review-{}-local", self.id);
        let output = run(
            logs,
            &label,
            "nix",
            &["eval".into(), "--raw".into(), self.flake_output().into()],
            progress,
        )
        .await?;
        let value = output.trim();
        value.parse().map_err(|source| Error::InvalidStorePath {
            path: value.to_owned(),
            source,
        })
    }

    /// Discovers an optional verifier for this host's NixOS configuration.
    pub(crate) async fn verifier(
        &self,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<Option<Verifier>, Error> {
        verifier::discover(&self.id, &self.flake_path, &self.flake_name, logs, progress).await
    }

    /// Builds the host configuration and verifier in the local Nix store.
    pub(crate) async fn build(
        &self,
        verifier: Option<&Verifier>,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("deploy-{}-build", self.id);
        let mut arguments = vec![
            "build".into(),
            "--no-link".into(),
            self.flake_output().into(),
        ];
        if let Some(verifier) = verifier {
            arguments.push(
                verifier
                    .installable(&self.flake_path, &self.flake_name)
                    .into(),
            );
        }
        run_status(logs, &label, "nix", &arguments, progress).await
    }

    /// Builds only the configured verifier in the local Nix store.
    pub(crate) async fn build_verifier(
        &self,
        verifier: &Verifier,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("verify-{}-build", self.id);
        run_status(
            logs,
            &label,
            "nix",
            &[
                "build".into(),
                "--no-link".into(),
                verifier
                    .installable(&self.flake_path, &self.flake_name)
                    .into(),
            ],
            progress,
        )
        .await
    }

    /// Copies a built system to the target and performs a dry activation.
    pub(crate) async fn prepare_activation(
        &self,
        store_path: &StorePath,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("deploy-{}-prepare", self.id);
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
                self.ssh_url().into(),
            ],
            progress,
        )
        .await
    }

    /// Activates a locally built system on the target within the activation deadline.
    pub(crate) async fn activate(
        &self,
        store_path: &StorePath,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("deploy-{}-activate", self.id);
        run_status_bounded(
            logs,
            &label,
            "nixos-rebuild",
            &[
                "switch".into(),
                "--use-substitutes".into(),
                "--store-path".into(),
                store_path.as_ref().as_os_str().to_owned(),
                "--target-host".into(),
                self.ssh_url().into(),
            ],
            &[],
            ACTIVATION_TIMEOUT,
            progress,
        )
        .await
    }

    /// Runs a built verifier on the deployment machine.
    pub(crate) async fn verify(
        &self,
        verifier: &Verifier,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("verify-{}-run", self.id);
        let environment = [
            ("DANCE_HOST_ID", OsString::from(&self.id)),
            ("DANCE_SSH_TARGET", OsString::from(self.ssh_url())),
        ];
        run_status_bounded(
            logs,
            &label,
            &verifier.executable(),
            &[],
            &environment,
            VERIFICATION_TIMEOUT,
            progress,
        )
        .await
    }

    /// Arms a target-side rollback watchdog for the reviewed system.
    pub(crate) async fn arm_rollback(
        &self,
        previous: &StorePath,
        verification: bool,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<RollbackWatchdog, Error> {
        let phase = if verification {
            "verification"
        } else {
            "activation"
        };
        let timeout = if verification {
            VERIFICATION_ROLLBACK_TIMEOUT
        } else {
            ACTIVATION_ROLLBACK_TIMEOUT
        };
        let unit = format!("dance-rollback-{}", Uuid::now_v7());
        let rollback = self.rollback_command(previous);
        let command = format!(
            "systemd-run --quiet --collect --unit={} --on-active={}s --property=Type=oneshot -- /bin/sh -c {}",
            shell_quote(&unit),
            timeout,
            shell_quote(&rollback)
        );
        let label = format!("deploy-{}-arm-{phase}-rollback", self.id);
        run_status_bounded(
            logs,
            &label,
            "ssh",
            &[self.ssh_url().into(), command.into()],
            &[],
            VERIFICATION_TIMEOUT,
            progress,
        )
        .await?;
        Ok(RollbackWatchdog { unit })
    }

    /// Disarms a target-side rollback watchdog.
    pub(crate) async fn disarm_rollback(
        &self,
        watchdog: &RollbackWatchdog,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("deploy-{}-disarm-rollback", self.id);
        let command = format!("systemctl stop {}.timer", shell_quote(&watchdog.unit));
        run_status_bounded(
            logs,
            &label,
            "ssh",
            &[self.ssh_url().into(), command.into()],
            &[],
            VERIFICATION_TIMEOUT,
            progress,
        )
        .await
    }

    /// Immediately restores a reviewed system and disarms its watchdogs.
    pub(crate) async fn rollback(
        &self,
        previous: &StorePath,
        watchdogs: &[RollbackWatchdog],
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let watchdog = watchdogs
            .last()
            .expect("rollback should have an armed watchdog");
        let label = format!("deploy-{}-rollback", self.id);
        let command = format!("systemctl start {}.service", shell_quote(&watchdog.unit));
        run_status_bounded(
            logs,
            &label,
            "ssh",
            &[self.ssh_url().into(), command.into()],
            &[],
            VERIFICATION_TIMEOUT,
            progress,
        )
        .await?;

        let current = self
            .deployed_store_path("verify-rollback", logs, progress)
            .await?;
        if &current != previous {
            return Err(Error::RollbackMismatch {
                host: self.id.clone(),
                expected: previous.clone(),
                current,
            });
        }
        for watchdog in watchdogs {
            self.disarm_rollback(watchdog, logs, progress).await?;
        }
        Ok(())
    }

    /// Prunes old generations and collects the target's unreachable store paths.
    pub(crate) async fn collect_garbage(
        &self,
        keep_generations: NonZeroU32,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("deploy-{}-prune-generations", self.id);
        run_status(
            logs,
            &label,
            "ssh",
            &[
                self.ssh_url().into(),
                "nix-env".into(),
                "--profile".into(),
                "/nix/var/nix/profiles/system".into(),
                "--delete-generations".into(),
                format!("+{keep_generations}").into(),
            ],
            progress,
        )
        .await?;

        let label = format!("deploy-{}-refresh-bootloader", self.id);
        run_status(
            logs,
            &label,
            "ssh",
            &[
                self.ssh_url().into(),
                "/run/current-system/bin/switch-to-configuration".into(),
                "boot".into(),
            ],
            progress,
        )
        .await?;

        let label = format!("deploy-{}-collect-garbage", self.id);
        run_status(
            logs,
            &label,
            "ssh",
            &[self.ssh_url().into(), "nix-store".into(), "--gc".into()],
            progress,
        )
        .await
    }

    /// Returns the complete flake output used to evaluate the system path.
    fn flake_output(&self) -> String {
        format!(
            "{}#nixosConfigurations.{}.config.system.build.toplevel.outPath",
            self.flake_path.display(),
            self.flake_name
        )
    }

    /// Returns the remote command that restores a reviewed system.
    fn rollback_command(&self, previous: &StorePath) -> String {
        let previous = previous.to_string();
        format!(
            "/run/current-system/sw/bin/nix-env --profile /nix/var/nix/profiles/system --set {} && {}/bin/switch-to-configuration switch",
            shell_quote(&previous),
            shell_quote(&previous)
        )
    }
}

/// Quotes one value for a POSIX-compatible remote shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indexmap::IndexMap;

    use super::{Host, Inventory, shell_quote};
    use crate::error::Error;

    /// Creates a minimal inventory with hosts in the given order.
    fn inventory(ids: &[&str]) -> Inventory {
        Inventory {
            hosts: ids
                .iter()
                .map(|id| {
                    (
                        (*id).to_owned(),
                        Host {
                            id: (*id).to_owned(),
                            ssh_host: (*id).to_owned(),
                            flake_path: PathBuf::from(id),
                            flake_name: (*id).to_owned(),
                        },
                    )
                })
                .collect::<IndexMap<_, _>>(),
        }
    }

    /// Checks table parsing, selection validation, and inventory order.
    #[test]
    fn parses_and_selects_hosts_by_unique_inventory_id() {
        let parsed: Inventory = toml::from_str(
            r#"
[hosts.party]
ssh_host = "party"
flake_path = "sgflt.net"
flake_name = "party"

[hosts.www]
ssh_host = "www"
flake_path = "www"
flake_name = "www"
"#,
        )
        .expect("keyed hosts should parse");
        assert_eq!(
            parsed.hosts.keys().map(String::as_str).collect::<Vec<_>>(),
            ["party", "www"]
        );

        let requested = ["party".to_owned(), "www".to_owned()];
        let selected = inventory(&["www", "other", "party"])
            .select(&requested)
            .expect("known hosts should be selected");
        let ids = selected.into_iter().map(|host| host.id).collect::<Vec<_>>();
        assert_eq!(ids, ["www", "party"]);

        let mut shared = inventory(&["www", "party", "other"]);
        shared
            .hosts
            .get_index_mut(1)
            .expect("second host should exist")
            .1
            .flake_path = PathBuf::from("www");
        let flakes = shared.flake_groups();
        assert_eq!(
            flakes,
            [(PathBuf::from("www"), 2), (PathBuf::from("other"), 1)]
        );

        let unknown = inventory(&["www"]).select(&["party".to_owned()]);
        assert!(matches!(
            unknown,
            Err(Error::UnknownId { id }) if id == "party"
        ));

        let duplicate = inventory(&["www"]).select(&["www".to_owned(), "www".to_owned()]);
        assert!(matches!(
            duplicate,
            Err(Error::DuplicateSelectedId { id }) if id == "www"
        ));
    }

    /// Checks that values remain one argument in remote shell commands.
    #[test]
    fn quotes_remote_shell_values() {
        assert_eq!(shell_quote("one'two"), "'one'\"'\"'two'");
    }
}
