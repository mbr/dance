//! Concurrent deployment coordination.

mod host;

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::host::{HostRow, ReviewOutcome, ReviewedHost};
use crate::{
    error::Error,
    inventory::Inventory,
    status::{self, ProgressDisplay},
};

/// Runs the concurrent deployment workflow.
pub(crate) async fn deploy(inventory: Inventory) -> Result<(), Error> {
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let mut rows = hosts.iter().map(HostRow::collecting).collect::<Vec<_>>();
    let review_progress = status::review(hosts.len())?;
    let outcomes = join_all(
        hosts
            .into_iter()
            .map(|host| host::review(host, &review_progress)),
    )
    .await;

    let mut reviewed = Vec::new();
    let mut first_error = None;
    for (row, outcome) in rows.iter_mut().zip(outcomes) {
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

    let activation = activate(reviewed).await;
    first_error.map_or(activation, Err)
}

/// Builds, prepares, and activates all reviewed hosts concurrently.
async fn activate(reviewed: Vec<ReviewedHost>) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::name));
    let results = join_all(reviewed.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let result = host::activate(host, progress).await;
            if result.is_err() {
                status::fail(progress);
            }
            result
        },
    ))
    .await;

    results.into_iter().find(Result::is_err).unwrap_or(Ok(()))
}
