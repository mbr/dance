//! Flake input updates before deployment review.

use crate::{error::Error, inventory::Inventory, process::run, status};

/// Updates every distinct flake referenced by the selected hosts.
pub(crate) async fn flakes(inventory: &Inventory) -> Result<(), Error> {
    let flakes = inventory.flake_paths();
    let progress = status::update(flakes.len())?;

    for flake in flakes {
        progress.set_prefix(flake.display().to_string());
        progress.set_message("");
        if let Err(error) = run(
            "nix",
            &[
                "flake".into(),
                "update".into(),
                "--flake".into(),
                flake.as_os_str().to_owned(),
            ],
            &progress,
        )
        .await
        {
            progress.abandon();
            return Err(error);
        }
        progress.inc(1);
    }

    progress.finish_and_clear();
    Ok(())
}
