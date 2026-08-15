//! Discovery and execution metadata for flake-provided verifiers.

use std::path::{Path, PathBuf};

use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{error::Error, logs::RunLog, process::run, store_path::StorePath};

/// Evaluates an optional Dance verifier for one NixOS configuration.
const VERIFIER_EXPRESSION: &str = r#"
let
  flake = builtins.getFlake flakeReference;
  localSystem = builtins.currentSystem;
  verifiers =
    if flake ? danceVerifiers && builtins.hasAttr localSystem flake.danceVerifiers
    then builtins.getAttr localSystem flake.danceVerifiers
    else {};
  verifier =
    if builtins.hasAttr flakeName verifiers
    then builtins.getAttr flakeName verifiers
    else null;
in
if verifier == null
then null
else {
  output = verifier.outPath;
  mainProgram =
    if verifier ? meta && verifier.meta ? mainProgram
    then verifier.meta.mainProgram
    else throw "Dance verifier must define meta.mainProgram";
  system = localSystem;
}
"#;

/// Describes a runnable verifier evaluated from a flake.
pub(crate) struct Verifier {
    /// Identifies the verifier's unrealized output path.
    output: StorePath,
    /// Identifies the executable within the output path.
    main_program: String,
    /// Identifies the deployment machine system used by the output.
    system: String,
}

impl Verifier {
    /// Returns the verifier flake installable.
    pub(crate) fn installable(&self, flake: &Path, flake_name: &str) -> String {
        format!(
            "{}#danceVerifiers.{}.{flake_name}",
            flake.display(),
            self.system
        )
    }

    /// Returns the realized verifier executable.
    pub(crate) fn executable(&self) -> String {
        self.output
            .as_ref()
            .join("bin")
            .join(&self.main_program)
            .to_string_lossy()
            .into_owned()
    }
}

/// Deserializes the resolved reference reported for a flake.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlakeMetadata {
    /// Contains a reference that preserves the flake's source root.
    resolved_url: String,
}

/// Deserializes verifier metadata from a flake output.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifierRecord {
    /// Contains the verifier output path.
    output: String,
    /// Contains the executable name inside the verifier output.
    main_program: String,
    /// Contains the deployment machine's Nix system.
    system: String,
}

/// Discovers an optional verifier for one NixOS configuration.
pub(crate) async fn discover(
    host: &str,
    flake: &Path,
    flake_name: &str,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<Option<Verifier>, Error> {
    let metadata_label = format!("verify-{host}-flake-metadata");
    let metadata = run(
        logs,
        &metadata_label,
        "nix",
        &[
            "flake".into(),
            "metadata".into(),
            "--json".into(),
            "--no-write-lock-file".into(),
            flake.as_os_str().to_owned(),
        ],
        progress,
    )
    .await?;
    let metadata: FlakeMetadata =
        serde_json::from_str(&metadata).map_err(|source| Error::ParseFlakeMetadata {
            path: PathBuf::from(flake),
            source,
        })?;
    let flake_reference = serde_json::to_string(&metadata.resolved_url)
        .expect("flake reference should serialize as a Nix string");
    let flake_name =
        serde_json::to_string(flake_name).expect("flake name should serialize as a Nix string");
    let expression = format!(
        "let flakeReference = {flake_reference}; flakeName = {flake_name}; in ({VERIFIER_EXPRESSION})"
    );
    let label = format!("verify-{host}-evaluate");
    let output = run(
        logs,
        &label,
        "nix",
        &[
            "eval".into(),
            "--json".into(),
            "--impure".into(),
            "--expr".into(),
            expression.into(),
        ],
        progress,
    )
    .await?;
    parse(host, &output)
}

/// Parses optional verifier metadata returned by Nix.
fn parse(host: &str, output: &str) -> Result<Option<Verifier>, Error> {
    let record: Option<VerifierRecord> =
        serde_json::from_str(output).map_err(|source| Error::ParseVerifier {
            host: host.to_owned(),
            source,
        })?;
    record
        .map(|verifier| {
            let output = verifier
                .output
                .parse()
                .map_err(|source| Error::InvalidStorePath {
                    path: verifier.output,
                    source,
                })?;
            Ok(Verifier {
                output,
                main_program: verifier.main_program,
                system: verifier.system,
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::parse;

    /// Checks the verifier metadata contract used with Nix evaluation.
    #[test]
    fn parses_verifier_metadata() {
        let verifier = parse(
            "www",
            r#"{
                "output": "/nix/store/11111111111111111111111111111111-verifier",
                "mainProgram": "verify-www",
                "system": "x86_64-linux"
            }"#,
        )
        .expect("valid verifier metadata should parse")
        .expect("verifier should be present");
        assert_eq!(
            verifier.executable(),
            "/nix/store/11111111111111111111111111111111-verifier/bin/verify-www"
        );
        assert!(
            parse("www", "null")
                .expect("missing verifier should parse")
                .is_none()
        );
    }
}
