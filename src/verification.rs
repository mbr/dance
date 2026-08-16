//! Discovery, building, and execution of flake-provided verifiers.

use std::{path::PathBuf, time::Duration};

use futures::future::join_all;
use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{
    error::Error,
    inventory::{Host, Inventory},
    logs::RunLog,
    process::Invocation,
    status::{self, DeploymentPhase, ProgressDisplay},
    store_path::StorePath,
};

/// Limits how long a configured verifier may run.
const TIMEOUT: Duration = Duration::from_mins(1);
/// Evaluates an optional Dance verifier for one NixOS configuration.
const EXPRESSION: &str = r#"
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
struct Verifier {
    /// Identifies the verifier's unrealized output path.
    output: StorePath,
    /// Identifies the executable within the output path.
    main_program: String,
    /// Identifies the deployment machine system used by the output.
    system: String,
}

impl Verifier {
    /// Discovers the verifier for one host when configured.
    async fn discover(
        host: &Host,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<Option<Self>, Error> {
        let metadata = Invocation::new(
            logs,
            format!("verify-{}-flake-metadata", host.id()),
            "nix",
            progress,
        )
        .arguments(["flake", "metadata", "--json", "--no-write-lock-file"])
        .argument(host.flake_path())
        .output()
        .await?;
        let metadata: FlakeMetadata =
            serde_json::from_str(&metadata).map_err(|source| Error::ParseFlakeMetadata {
                path: PathBuf::from(host.flake_path()),
                source,
            })?;
        let flake_reference = serde_json::to_string(&metadata.resolved_url)
            .expect("flake reference should serialize as a Nix string");
        let flake_name = serde_json::to_string(host.flake_name())
            .expect("flake name should serialize as a Nix string");
        let expression = format!(
            "let flakeReference = {flake_reference}; flakeName = {flake_name}; in ({EXPRESSION})"
        );
        let output = Invocation::new(
            logs,
            format!("verify-{}-evaluate", host.id()),
            "nix",
            progress,
        )
        .arguments(["eval", "--json", "--impure", "--expr"])
        .argument(expression)
        .output()
        .await?;
        parse(host.id(), &output)
    }

    /// Builds the verifier in the local Nix store.
    async fn build(&self, host: &Host, logs: &RunLog, progress: &ProgressBar) -> Result<(), Error> {
        Invocation::new(logs, format!("verify-{}-build", host.id()), "nix", progress)
            .arguments(["build", "--no-link"])
            .argument(host.verifier_output(&self.system))
            .status()
            .await
    }

    /// Runs the built verifier on the deployment machine.
    async fn run(&self, host: &Host, logs: &RunLog, progress: &ProgressBar) -> Result<(), Error> {
        Invocation::new(
            logs,
            format!("verify-{}-run", host.id()),
            self.executable(),
            progress,
        )
        .environment("DANCE_HOST_ID", host.id())
        .environment("DANCE_SSH_TARGET", host.ssh_url())
        .timeout(TIMEOUT)
        .status()
        .await
    }

    /// Returns the realized verifier executable.
    fn executable(&self) -> String {
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

/// Describes whether a configured verifier ran.
pub(crate) enum ConfiguredOutcome {
    /// The configured verifier succeeded.
    Verified,
    /// The host has no configured verifier.
    Missing,
}

/// Discovers, builds, and runs the verifier configured for one host.
pub(crate) async fn run_configured(
    host: &Host,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Result<ConfiguredOutcome, Error> {
    status::set_phase(progress, host.id(), DeploymentPhase::BuildingVerifier);
    let Some(verifier) = Verifier::discover(host, logs, progress).await? else {
        return Ok(ConfiguredOutcome::Missing);
    };
    verifier.build(host, logs, progress).await?;

    status::set_phase(progress, host.id(), DeploymentPhase::RunningVerifier);
    verifier.run(host, logs, progress).await?;
    Ok(ConfiguredOutcome::Verified)
}

/// Describes one host's standalone verification result.
enum Outcome {
    /// The configured verifier succeeded.
    Verified,
    /// The host was skipped because no verifier is configured.
    Skipped,
    /// Verifier discovery, building, or execution failed.
    Failed(Error),
}

/// Runs the verifier configured for one host.
async fn verify_host(host: Host, logs: &RunLog, progress: &ProgressBar) -> Outcome {
    match run_configured(&host, logs, progress).await {
        Ok(ConfiguredOutcome::Verified) => {
            status::verified(progress, host.id());
            Outcome::Verified
        }
        Ok(ConfiguredOutcome::Missing) => {
            status::verifier_missing(progress, host.id());
            Outcome::Skipped
        }
        Err(error) => Outcome::Failed(error),
    }
}

/// Runs configured verifiers for all selected hosts.
pub(crate) async fn verify(inventory: Inventory, logs: &RunLog) -> Result<(), Error> {
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(hosts.iter().map(Host::id));
    let outcomes = join_all(hosts.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let outcome = verify_host(host, logs, progress).await;
            if matches!(&outcome, Outcome::Failed(_)) {
                status::fail(progress);
            }
            outcome
        },
    ))
    .await;

    let mut verified = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut first_error = None;
    for outcome in outcomes {
        match outcome {
            Outcome::Verified => verified += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed(error) => {
                failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }
    println!("verification: {verified} passed, {skipped} skipped, {failed} failed");
    first_error.map_or(Ok(()), Err)
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
