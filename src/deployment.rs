//! Concurrent deployment coordination.

mod host;

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::host::{HostRow, ReviewOutcome, ReviewedHost};
use crate::{
    error::Error,
    inventory::Inventory,
    logs::RunLog,
    status::{self, ProgressDisplay},
    update::{self, UpdateOutcome},
};

/// Runs the concurrent deployment workflow.
pub(crate) async fn deploy(
    inventory: Inventory,
    update_flakes: bool,
    logs: &RunLog,
) -> Result<(), Error> {
    let flakes = inventory.flake_groups();
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let mut rows = hosts.iter().map(HostRow::collecting).collect::<Vec<_>>();
    let review_progress = status::review(hosts.len(), update_flakes)?;

    let deployed = join_all(
        hosts
            .iter()
            .map(|host| host::deployed(host, logs, &review_progress)),
    );
    let local = async {
        let updates = if update_flakes {
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
                    continue;
                }
            },
        };

        match outcome {
            Ok(ReviewOutcome::UpToDate(host_row)) => *row = host_row,
            Ok(ReviewOutcome::OutOfDate(host_row, host)) => {
                *row = host_row;
                reviewed.push(*host);
            }
            Err(error) => {
                row.fail();
                first_error.get_or_insert(error);
            }
        }
    }

    review_progress.finish_and_clear();

    rows.sort_by_key(HostRow::needs_activation);
    println!("{}", Table::new(rows));
    if reviewed.is_empty() {
        return first_error.map_or(Ok(()), Err);
    }

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

    let activation = activate(reviewed, logs).await;
    first_error.map_or(activation, Err)
}

/// Builds, prepares, and activates all reviewed hosts concurrently.
async fn activate(reviewed: Vec<ReviewedHost>, logs: &RunLog) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let results = join_all(reviewed.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let result = host::activate(host, logs, progress).await;
            if result.is_err() {
                status::fail(progress);
            }
            result
        },
    ))
    .await;

    results.into_iter().find(Result::is_err).unwrap_or(Ok(()))
}
