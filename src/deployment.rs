//! Concurrent deployment coordination.

mod host;

use std::{
    io::{self, IsTerminal},
    num::NonZeroU32,
};

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::host::{ActivationOutcome, HostRow, ReviewOutcome, ReviewedHost};
use crate::{
    error::Error,
    inventory::Inventory,
    logs::RunLog,
    status::{self, ProgressDisplay},
    update::{self, UpdateOutcome},
};

/// Selects how Dance proceeds after reviewing outdated hosts.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    /// Requests interactive approval.
    Confirm,
    /// Deploys without requesting approval.
    Approved,
    /// Stops after collecting host information.
    ReviewOnly,
}

/// Configures one deployment workflow.
#[derive(Clone, Copy)]
pub(crate) struct Options {
    /// Selects whether flake inputs are updated during review.
    pub(crate) update_flakes: bool,
    /// Selects how outdated hosts proceed after review.
    pub(crate) mode: Mode,
    /// Selects post-deployment cleanup and generation retention.
    pub(crate) keep_generations: Option<NonZeroU32>,
}

/// Runs the concurrent deployment workflow.
pub(crate) async fn deploy(
    inventory: Inventory,
    options: Options,
    logs: &RunLog,
) -> Result<(), Error> {
    let flakes = inventory.flake_groups();
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let mut rows = hosts.iter().map(HostRow::collecting).collect::<Vec<_>>();
    let review_progress = status::review(hosts.len(), options.update_flakes)?;

    let deployed = join_all(
        hosts
            .iter()
            .map(|host| host::deployed(host, logs, &review_progress)),
    );
    let local = async {
        let updates = if options.update_flakes {
            update::flakes(flakes, logs, &review_progress).await
        } else {
            UpdateOutcome::default()
        };
        let expected = join_all(hosts.iter().map(|host| async {
            if updates.failed(host.flake_path()) {
                host::skip_expected(host, &review_progress);
                None
            } else {
                Some(host::expected(host, logs, &review_progress).await)
            }
        }))
        .await;
        (updates, expected)
    };
    let (deployed, (mut updates, expected)) = tokio::join!(deployed, local);

    let mut reviewed = Vec::new();
    let mut up_to_date = 0;
    let mut failed = 0;
    let mut first_error = updates.take_error();
    for (((row, host), deployed), expected) in
        rows.iter_mut().zip(hosts).zip(deployed).zip(expected)
    {
        let outcome = match deployed {
            Err(error) => Err(error),
            Ok(deployed) => match expected {
                Some(Ok(expected)) => Ok(host::review(host, deployed, expected)),
                Some(Err(error)) => Err(error),
                None => {
                    row.fail();
                    failed += 1;
                    continue;
                }
            },
        };

        match outcome {
            Ok(ReviewOutcome::UpToDate(host_row)) => {
                *row = host_row;
                up_to_date += 1;
            }
            Ok(ReviewOutcome::OutOfDate(host_row, host)) => {
                *row = host_row;
                reviewed.push(*host);
            }
            Err(error) => {
                row.fail();
                failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }

    review_progress.finish_and_clear();

    rows.sort_by_key(HostRow::needs_activation);
    if io::stdout().is_terminal() {
        println!("{}", Table::new(rows));
    } else {
        println!(
            "review: {up_to_date} up to date, {} out of date, {failed} failed",
            reviewed.len()
        );
    }
    if reviewed.is_empty() {
        return first_error.map_or(Ok(()), Err);
    }
    if matches!(options.mode, Mode::ReviewOnly) {
        println!("deployment: skipped");
        return first_error.map_or(Ok(()), Err);
    }
    if matches!(options.mode, Mode::Confirm)
        && !(io::stdin().is_terminal() && io::stderr().is_terminal())
    {
        return Err(Error::ConfirmationRequired);
    }

    if matches!(options.mode, Mode::Confirm) {
        let theme = ColorfulTheme {
            success_prefix: style(">".to_owned()).for_stderr().green(),
            ..ColorfulTheme::default()
        };
        let approved = Confirm::with_theme(&theme)
            .with_prompt("Deploy the outdated systems?")
            .default(false)
            .interact()
            .map_err(|source| Error::Confirmation { source })?;
        if !approved {
            return first_error.map_or(Ok(()), Err);
        }
    }

    let activation = activate(reviewed, options.keep_generations, logs).await;
    first_error.map_or(activation, Err)
}

/// Builds, prepares, and activates all reviewed hosts concurrently.
async fn activate(
    reviewed: Vec<ReviewedHost>,
    keep_generations: Option<NonZeroU32>,
    logs: &RunLog,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let outcomes = join_all(reviewed.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let outcome = host::activate(host, keep_generations, logs, progress).await;
            if !matches!(&outcome, ActivationOutcome::Activated) {
                status::fail(progress);
            }
            outcome
        },
    ))
    .await;

    let mut activated = 0;
    let mut failed = 0;
    let mut cleanup_failed = 0;
    let mut first_error = None;
    for outcome in outcomes {
        match outcome {
            ActivationOutcome::Activated => activated += 1,
            ActivationOutcome::ActivationFailed(error) => {
                failed += 1;
                first_error.get_or_insert(error);
            }
            ActivationOutcome::CleanupFailed(error) => {
                activated += 1;
                cleanup_failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }
    if cleanup_failed == 0 {
        println!("deployment: {activated} activated, {failed} failed");
    } else {
        println!(
            "deployment: {activated} activated, {failed} failed, {cleanup_failed} cleanup failed"
        );
    }

    first_error.map_or(Ok(()), Err)
}
