//! Inventory parsing and host operations.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{error::Error, process::run, store_path::StorePath};

/// Contains the hosts managed by Dance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
    /// Defines the managed hosts.
    hosts: Vec<Host>,
}

impl IntoIterator for Inventory {
    type IntoIter = std::vec::IntoIter<Host>;
    type Item = Host;

    fn into_iter(self) -> Self::IntoIter {
        self.hosts.into_iter()
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

        for host in &mut inventory.hosts {
            if host.flake_path.is_relative() {
                host.flake_path = directory.join(&host.flake_path);
            }
        }

        inventory.validate()?;
        Ok(inventory)
    }

    /// Returns each selected flake path once in inventory order.
    pub(crate) fn flake_paths(&self) -> Vec<&Path> {
        let mut seen = HashSet::new();
        self.hosts
            .iter()
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
        for name in requested {
            if !selected.insert(name.as_str()) {
                return Err(Error::DuplicateSelectedHost { host: name.clone() });
            }
            if !self.hosts.iter().any(|host| host.name == *name) {
                return Err(Error::UnknownHost { host: name.clone() });
            }
        }

        self.hosts
            .retain(|host| selected.contains(host.name.as_str()));
        Ok(self)
    }

    /// Ensures inventory host names are unique.
    fn validate(&self) -> Result<(), Error> {
        let mut names = HashSet::new();
        for host in &self.hosts {
            if !names.insert(host.name.as_str()) {
                return Err(Error::DuplicateInventoryHost {
                    host: host.name.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Defines deployment metadata and connection details for a machine.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Host {
    /// Identifies the host in user-facing output.
    name: String,
    /// Documents the host's purpose.
    #[serde(rename = "description")]
    _description: String,
    /// Selects the SSH destination.
    ssh_host: String,
    /// Locates the flake relative to the inventory.
    flake_path: PathBuf,
    /// Selects the NixOS configuration in the flake.
    flake_name: String,
}

impl Host {
    /// Returns the inventory name.
    pub(crate) fn name(&self) -> &str {
        &self.name
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

    use super::{Host, Inventory};
    use crate::error::Error;

    /// Creates a minimal inventory with hosts in the given order.
    fn inventory(names: &[&str]) -> Inventory {
        Inventory {
            hosts: names
                .iter()
                .map(|name| Host {
                    name: (*name).to_owned(),
                    _description: String::new(),
                    ssh_host: (*name).to_owned(),
                    flake_path: PathBuf::from(name),
                    flake_name: (*name).to_owned(),
                })
                .collect(),
        }
    }

    /// Checks selection validation and inventory-order preservation.
    #[test]
    fn selects_hosts_by_unique_inventory_name() {
        let requested = ["party".to_owned(), "www".to_owned()];
        let selected = inventory(&["www", "other", "party"])
            .select(&requested)
            .expect("known hosts should be selected");
        let names = selected
            .into_iter()
            .map(|host| host.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["www", "party"]);

        let mut shared = inventory(&["www", "party", "other"]);
        shared.hosts[1].flake_path = PathBuf::from("www");
        let flakes = shared.flake_paths();
        assert_eq!(flakes, [Path::new("www"), Path::new("other")]);

        let unknown = inventory(&["www"]).select(&["party".to_owned()]);
        assert!(matches!(
            unknown,
            Err(Error::UnknownHost { host }) if host == "party"
        ));

        let duplicate = inventory(&["www"]).select(&["www".to_owned(), "www".to_owned()]);
        assert!(matches!(
            duplicate,
            Err(Error::DuplicateSelectedHost { host }) if host == "www"
        ));

        let duplicate = inventory(&["www", "www"]).validate();
        assert!(matches!(
            duplicate,
            Err(Error::DuplicateInventoryHost { host }) if host == "www"
        ));
    }
}
