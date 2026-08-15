//! Inventory parsing and host operations.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{error::Error, process::run, store_path::StorePath};

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

    /// Returns each selected flake path once in inventory order.
    pub(crate) fn flake_paths(&self) -> Vec<&Path> {
        let mut seen = HashSet::new();
        self.hosts
            .values()
            .filter_map(|host| {
                let path = host.flake_path.as_path();
                seen.insert(path).then_some(path)
            })
            .collect()
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
        progress: &ProgressBar,
    ) -> Result<StorePath, Error> {
        let output = run(
            "ssh",
            &[
                self.ssh_url().into(),
                "readlink -f /run/current-system".into(),
            ],
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
        progress: &ProgressBar,
    ) -> Result<StorePath, Error> {
        let output = run(
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

    /// Builds the host configuration in the local Nix store.
    pub(crate) async fn build(&self, progress: &ProgressBar) -> Result<(), Error> {
        run(
            "nixos-rebuild",
            &[
                "build".into(),
                "--no-link".into(),
                "--flake".into(),
                self.flake_selector().into(),
            ],
            progress,
        )
        .await?;
        Ok(())
    }

    /// Copies a built system to the target and performs a dry activation.
    pub(crate) async fn prepare_activation(
        &self,
        store_path: &StorePath,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        run(
            "nixos-rebuild",
            &[
                "dry-activate".into(),
                "--store-path".into(),
                store_path.as_ref().as_os_str().to_owned(),
                "--target-host".into(),
                self.ssh_url().into(),
            ],
            progress,
        )
        .await?;
        Ok(())
    }

    /// Activates a locally built system on the target.
    pub(crate) async fn activate(
        &self,
        store_path: &StorePath,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        run(
            "nixos-rebuild",
            &[
                "switch".into(),
                "--store-path".into(),
                store_path.as_ref().as_os_str().to_owned(),
                "--target-host".into(),
                self.ssh_url().into(),
            ],
            progress,
        )
        .await?;
        Ok(())
    }

    /// Returns the complete flake output used to evaluate the system path.
    fn flake_output(&self) -> String {
        format!(
            "{}#nixosConfigurations.{}.config.system.build.toplevel.outPath",
            self.flake_path.display(),
            self.flake_name
        )
    }

    /// Returns the complete flake selector used by `nixos-rebuild`.
    fn flake_selector(&self) -> String {
        format!("{}#{}", self.flake_path.display(), self.flake_name)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use indexmap::IndexMap;

    use super::{Host, Inventory};
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
        let flakes = shared.flake_paths();
        assert_eq!(flakes, [Path::new("www"), Path::new("other")]);

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
}
