//! Inventory parsing and deployment metadata.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use serde::Deserialize;
use thiserror::Error;

use crate::error::PathedError;

/// Describes a failure encountered while using an inventory.
#[derive(Debug, Error)]
pub(crate) enum InventoryError {
    /// Indicates that the inventory could not be loaded.
    #[error(transparent)]
    Load(LoadError),
    /// Indicates that a requested ID is not in the inventory.
    #[error("ID `{0}` is not in the inventory")]
    UnknownId(
        /// Identifies the requested inventory entry.
        String,
    ),
    /// Indicates that an ID was requested more than once.
    #[error("ID `{0}` was selected more than once")]
    DuplicateSelectedId(
        /// Identifies the repeatedly selected inventory entry.
        String,
    ),
}

/// Describes a failure encountered while loading an inventory.
#[derive(Debug, Error)]
pub(crate) enum LoadError {
    /// Indicates that the inventory could not be read.
    #[error("failed to read inventory")]
    Read(#[source] PathedError),
    /// Indicates that the inventory was malformed.
    #[error("failed to parse inventory")]
    Parse(#[source] PathedError<toml::de::Error>),
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
    pub(crate) fn load(path: &Path) -> Result<Self, InventoryError> {
        let contents = fs::read_to_string(path).map_err(|source| {
            InventoryError::Load(LoadError::Read(PathedError::new(
                path.to_path_buf(),
                source,
            )))
        })?;
        let mut inventory: Self = toml::from_str(&contents).map_err(|source| {
            InventoryError::Load(LoadError::Parse(PathedError::new(
                path.to_path_buf(),
                source,
            )))
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
    pub(crate) fn select<S: AsRef<str>>(mut self, requested: &[S]) -> Result<Self, InventoryError> {
        if requested.is_empty() {
            return Ok(self);
        }

        let mut hosts = IndexMap::new();
        for id in requested {
            let id = id.as_ref();
            if hosts.contains_key(id) {
                return Err(InventoryError::DuplicateSelectedId(id.to_owned()));
            }
            let Some((id, host)) = self.hosts.shift_remove_entry(id) else {
                return Err(InventoryError::UnknownId(id.to_owned()));
            };
            hosts.insert(id, host);
        }
        Ok(Self { hosts })
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

    /// Returns the NixOS configuration name within the flake.
    pub(crate) fn flake_name(&self) -> &str {
        &self.flake_name
    }

    /// Returns a concise flake selector for user-facing output.
    pub(crate) fn display_flake(&self) -> String {
        let name = self
            .flake_path
            .file_name()
            .map_or_else(|| self.flake_path.as_os_str(), |name| name)
            .to_string_lossy();
        format!("{name}#{}", self.flake_name)
    }

    /// Returns the flake selector accepted by `nixos-rebuild`.
    pub(crate) fn flake(&self) -> OsString {
        self.flake_output(&self.flake_name)
    }

    /// Returns the flake output used to evaluate the desired system.
    pub(crate) fn nixos_output(&self) -> OsString {
        self.flake_output(&format!(
            "nixosConfigurations.{}.config.system.build.toplevel.outPath",
            self.flake_name
        ))
    }

    /// Returns the verifier output for a deployment-machine system.
    pub(crate) fn verifier_output(&self, system: &str) -> OsString {
        self.flake_output(&format!("danceVerifiers.{system}.{}", self.flake_name))
    }

    /// Appends an output attribute to the host's flake path.
    fn flake_output(&self, output: &str) -> OsString {
        let mut selector = self.flake_path.as_os_str().to_owned();
        selector.push("#");
        selector.push(output);
        selector
    }

    /// Returns the root SSH destination.
    pub(crate) fn ssh_url(&self) -> String {
        format!("root@{}", self.ssh_host)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indexmap::IndexMap;

    use super::{Host, Inventory, InventoryError, LoadError};

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

    /// Checks that loading preserves structured read and parse failures.
    #[test]
    fn reports_inventory_load_errors() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let missing = directory.path().join("missing.toml");
        assert!(matches!(
            Inventory::load(&missing),
            Err(InventoryError::Load(LoadError::Read(error))) if error.path == missing
        ));

        let malformed = directory.path().join("malformed.toml");
        std::fs::write(&malformed, "not TOML =").expect("malformed inventory should be written");
        assert!(matches!(
            Inventory::load(&malformed),
            Err(InventoryError::Load(LoadError::Parse(error))) if error.path == malformed
        ));
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
        assert_eq!(ids, ["party", "www"]);

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

        let unknown = inventory(&["www"]).select(&["party"]);
        assert!(matches!(
            unknown,
            Err(InventoryError::UnknownId(id)) if id == "party"
        ));

        let duplicate = inventory(&["www"]).select(&["www", "www"]);
        assert!(matches!(
            duplicate,
            Err(InventoryError::DuplicateSelectedId(id)) if id == "www"
        ));
    }
}
