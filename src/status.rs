//! Deployment progress bar styling.

use std::{
    fmt::{self, Display, Formatter},
    time::Duration,
};

use dialoguer::console::{Style, style};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::error::Error;

/// Controls how often active spinners advance.
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
/// Defines the layout of the information collection bar.
const REVIEW_TEMPLATE: &str =
    "{spinner} collecting info [{bar:20.cyan/blue}] {pos}/{len} {msg:.dim}";
/// Defines the layout of a host status line.
const HOST_TEMPLATE: &str = "{spinner} {prefix} {wide_msg}";
/// Defines the smooth spinner animation and its blank final frame.
const SPINNER_TICKS: &str =
    "\u{280b}\u{2819}\u{2839}\u{2838}\u{283c}\u{2834}\u{2826}\u{2827}\u{2807}\u{280f} ";

/// Identifies a phase of host work.
#[derive(Clone, Copy)]
pub(crate) enum DeploymentPhase {
    /// Acquires exclusive access and validates reviewed state.
    Locking,
    /// Builds and temporarily activates the desired system.
    Testing,
    /// Discovers and builds a configured verifier.
    BuildingVerifier,
    /// Runs the configured verifier.
    RunningVerifier,
    /// Makes the tested system persistent.
    Switching,
    /// Restores the reviewed system.
    RollingBack,
    /// Prunes old generations and unreachable store paths.
    CollectingGarbage,
}

impl DeploymentPhase {
    /// Returns the phase color.
    fn style(self) -> Style {
        let style = Style::new().for_stderr();
        match self {
            Self::Locking | Self::Testing | Self::BuildingVerifier => style.blue(),
            Self::RunningVerifier | Self::CollectingGarbage => style.yellow(),
            Self::Switching => style.magenta(),
            Self::RollingBack => style.red(),
        }
    }
}

impl Display for DeploymentPhase {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Locking => "locking",
            Self::Testing => "testing",
            Self::BuildingVerifier => "building verifier",
            Self::RunningVerifier => "running verifier",
            Self::Switching => "switching",
            Self::RollingBack => "rolling back",
            Self::CollectingGarbage => "collecting garbage",
        })
    }
}

/// Creates the common animated style for a progress display.
fn spinner_style(template: &str) -> Result<ProgressStyle, Error> {
    Ok(ProgressStyle::with_template(template)
        .map_err(|source| Error::ProgressStyle { source })?
        .tick_chars(SPINNER_TICKS))
}

/// Creates a progress bar for host review steps.
pub(crate) fn review(host_count: usize, update: bool) -> Result<ProgressBar, Error> {
    let host_count = u64::try_from(host_count).expect("host count should fit in u64");
    let steps_per_host = if update { 3 } else { 2 };
    let length = host_count
        .checked_mul(steps_per_host)
        .expect("review step count should fit in u64");
    let style = spinner_style(REVIEW_TEMPLATE)?.progress_chars("=>-");
    let progress = ProgressBar::new(length).with_style(style);
    progress.enable_steady_tick(SPINNER_INTERVAL);
    Ok(progress)
}

/// Displays a host identity and status using distinct styles.
fn set_status(progress: &ProgressBar, host: &str, status: impl Display, status_style: &Style) {
    let host = style(format!("{host:<12}")).for_stderr().bold();
    let status = status_style.apply_to(format!("{status:<16}"));
    progress.set_prefix(format!("{host} {status}"));
    progress.set_message("");
}

/// Displays a host identity and its current deployment phase.
pub(crate) fn set_phase(progress: &ProgressBar, host: &str, phase: DeploymentPhase) {
    set_status(progress, host, phase, &phase.style());
}

/// Finishes a host status line with an explicit outcome.
fn finish_with_status(progress: &ProgressBar, host: &str, status: &str, status_style: &Style) {
    let message = progress.message();
    set_status(progress, host, status, status_style);
    progress.finish_with_message(message);
}

/// Marks a host as successfully activated.
pub(crate) fn finish(progress: &ProgressBar, host: &str) {
    finish_with_status(
        progress,
        host,
        "activated",
        &Style::new().for_stderr().green(),
    );
}

/// Marks a host as restored after a failed deployment.
pub(crate) fn rolled_back(progress: &ProgressBar, host: &str) {
    finish_with_status(
        progress,
        host,
        "rolled back",
        &Style::new().for_stderr().red(),
    );
}

/// Marks a host deployment as failed without restoration.
pub(crate) fn deployment_failed(progress: &ProgressBar, host: &str) {
    finish_with_status(progress, host, "failed", &Style::new().for_stderr().red());
}

/// Marks a successful activation whose cleanup failed.
pub(crate) fn cleanup_failed(progress: &ProgressBar, host: &str) {
    finish_with_status(
        progress,
        host,
        "cleanup failed",
        &Style::new().for_stderr().red(),
    );
}

/// Marks a host verifier as successful.
pub(crate) fn verified(progress: &ProgressBar, host: &str) {
    finish_with_status(
        progress,
        host,
        "verified",
        &Style::new().for_stderr().green(),
    );
}

/// Marks a host without a configured verifier as skipped.
pub(crate) fn verifier_missing(progress: &ProgressBar, host: &str) {
    set_status(
        progress,
        host,
        "skipped",
        &Style::new().for_stderr().yellow(),
    );
    progress.finish_with_message("no verifier configured");
}

/// Marks host work as failed while retaining its phase.
pub(crate) fn fail(progress: &ProgressBar) {
    let failed = style("failed").for_stderr().red();
    progress.finish_with_message(format!("{failed}"));
}

/// Reports that live recovery failed and the host needs a reboot.
pub(crate) fn recovery_required(progress: &ProgressBar, host: &str) {
    set_status(
        progress,
        host,
        "recovery required",
        &Style::new().for_stderr().red(),
    );
    let message = style("live rollback failed; reboot through the machine console")
        .for_stderr()
        .red();
    progress.finish_with_message(format!("{message}"));
}

/// Creates consistently styled progress bars for a group of hosts.
pub(crate) struct ProgressDisplay {
    /// Coordinates host status lines on the terminal.
    progress: MultiProgress,
    /// Defines the common host status presentation.
    style: ProgressStyle,
}

impl ProgressDisplay {
    /// Creates a per-host terminal progress display.
    pub(crate) fn new() -> Result<Self, Error> {
        let style = spinner_style(HOST_TEMPLATE)?;
        Ok(Self {
            progress: MultiProgress::new(),
            style,
        })
    }

    /// Creates retained progress bars for a set of hosts.
    pub(crate) fn hosts<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Vec<ProgressBar> {
        names
            .into_iter()
            .map(|name| {
                let progress = self.progress.add(ProgressBar::new_spinner());
                progress.set_style(self.style.clone());
                set_status(&progress, name, "", &Style::new());
                progress.enable_steady_tick(SPINNER_INTERVAL);
                progress
            })
            .collect()
    }
}
