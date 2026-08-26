//! Nix store-path parsing and presentation.

use std::{
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    str::FromStr,
};

use thiserror::Error;

/// Locates the Nix store used by managed NixOS systems.
const NIX_STORE: &str = "/nix/store";
/// Defines the alphabet used by Nix store hashes.
const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";
/// Limits hashes to a recognizable table-friendly prefix.
const SHORT_HASH_LENGTH: usize = 8;

/// Identifies a path in a Nix store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StorePath {
    /// Retains the complete store path for identity comparisons.
    path: PathBuf,
    /// Contains the store-path hash used for concise presentation.
    hash: String,
}

impl StorePath {
    /// Returns a compact store-path hash for display.
    pub(crate) fn short_hash(&self) -> &str {
        &self.hash[..SHORT_HASH_LENGTH]
    }
}

impl AsRef<Path> for StorePath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl FromStr for StorePath {
    type Err = StorePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let path = PathBuf::from(value);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| StorePathError(value.to_owned()))?;
        let (hash, store_name) = name
            .split_once('-')
            .ok_or_else(|| StorePathError(value.to_owned()))?;

        if path.parent() != Some(Path::new(NIX_STORE))
            || store_name.is_empty()
            || hash.len() != 32
            || !hash.chars().all(|character| NIX_BASE32.contains(character))
        {
            return Err(StorePathError(value.to_owned()));
        }

        let hash = hash.to_owned();
        Ok(Self { path, hash })
    }
}

impl Display for StorePath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.path.display().fmt(formatter)
    }
}

/// Indicates that a value does not follow the Nix store-path format.
#[derive(Debug, Error)]
#[error("invalid Nix store path `{0}`")]
pub(crate) struct StorePathError(
    /// Contains the invalid value.
    String,
);

/// Tests store-path identity and parsing.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::StorePath;

    /// Checks the distinct identity and presentation forms of a store path.
    #[test]
    fn store_paths_validate_identity_and_have_distinct_presentations() {
        let path = "/nix/store/00000000000000000000000000000000-nixos-system-www";
        let store_path = path.parse::<StorePath>().expect("store path should parse");
        let same = path.parse::<StorePath>().expect("store path should parse");
        let different_name = "/nix/store/00000000000000000000000000000000-different"
            .parse::<StorePath>()
            .expect("store path should parse");

        assert_eq!(store_path, same);
        assert_ne!(store_path, different_name);
        assert_eq!(store_path.to_string(), path);
        assert_eq!(store_path.short_hash(), "00000000");
        assert_eq!(store_path.as_ref(), PathBuf::from(path));
        assert!("relative-store-path".parse::<StorePath>().is_err());
        assert!(
            "/tmp/00000000000000000000000000000000-system"
                .parse::<StorePath>()
                .is_err()
        );
        assert!(
            "/nix/store/not-a-nix-hash-name"
                .parse::<StorePath>()
                .is_err()
        );
    }
}
