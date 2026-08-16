//! Discovery, building, and execution of flake-provided verifiers.

use std::{ffi::OsString, path::PathBuf, time::Duration};

use futures::future::join_all;
use indicatif::ProgressBar;
use serde::Deserialize;

use crate::{
    error::Error,
    inventory::{Host, Inventory},
    logs::RunLog,
    process::{run, run_status, run_status_bounded},
    status::{self, DeploymentPhase, ProgressDisplay},
    store_path::StorePath,
};

/// Limits how long a configured verifier may run.
const TIMEOUT: Duration = Duration::from_secs(60);
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
pub(crate) struct Verifier {
    /// Identifies the verifier's unrealized output path.
    output: StorePath,
    /// Identifies the executable within the output path.
    main_program: String,
    /// Identifies the deployment machine system used by the output.
    system: String,
}

impl Verifier {
    /// Discovers the verifier for one host when configured.
    pub(crate) async fn discover(
        host: &Host,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<Option<Self>, Error> {
        let metadata_label = format!("verify-{}-flake-metadata", host.id());
        let metadata = run(
            logs,
            &metadata_label,
            "nix",
            &[
                "flake".into(),
                "metadata".into(),
                "--json".into(),
                "--no-write-lock-file".into(),
                host.flake_path().as_os_str().to_owned(),
            ],
            progress,
        )
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
        let label = format!("verify-{}-evaluate", host.id());
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
        parse(host.id(), &output)
    }

    /// Returns the verifier flake installable.
    pub(crate) fn installable(&self, host: &Host) -> String {
        format!(
            "{}#danceVerifiers.{}.{}",
            host.flake_path().display(),
            self.system,
            host.flake_name()
        )
    }

    /// Builds the verifier in the local Nix store.
    pub(crate) async fn build(
        &self,
        host: &Host,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("verify-{}-build", host.id());
        run_status(
            logs,
            &label,
            "nix",
            &[
                "build".into(),
                "--no-link".into(),
                self.installable(host).into(),
            ],
            progress,
        )
        .await
    }

    /// Runs the built verifier on the deployment machine.
    pub(crate) async fn run(
        &self,
        host: &Host,
        logs: &RunLog,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let label = format!("verify-{}-run", host.id());
        let environment = [
            ("DANCE_HOST_ID", OsString::from(host.id())),
            ("DANCE_SSH_TARGET", OsString::from(host.ssh_url())),
        ];
        run_status_bounded(
            logs,
            &label,
            &self.executable(),
            &[],
            &environment,
            TIMEOUT,
            progress,
        )
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
    status::set_phase(progress, host.id(), DeploymentPhase::BuildingVerifier);
    let verifier = match Verifier::discover(&host, logs, progress).await {
        Ok(Some(verifier)) => verifier,
        Ok(None) => {
            status::verifier_missing(progress, host.id());
            return Outcome::Skipped;
        }
        Err(error) => return Outcome::Failed(error),
    };
    if let Err(error) = verifier.build(&host, logs, progress).await {
        return Outcome::Failed(error);
    }

    status::set_phase(progress, host.id(), DeploymentPhase::RunningVerifier);
    match verifier.run(&host, logs, progress).await {
        Ok(()) => {
            status::verified(progress, host.id());
            Outcome::Verified
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
