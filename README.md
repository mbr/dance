# Dance

Dance reviews and deploys NixOS configurations across a small fleet. It compares each target's active system with its local flake output, presents the pending changes for confirmation, then builds, prepares, activates, and verifies outdated hosts concurrently.

## Usage

Run Dance from a directory containing `inventory.toml`:

```console
dance                     # Review every host
dance www party           # Review only named hosts
dance --update party      # Update Party's flake inputs before review
dance --inventory hosts.toml
```

`DANCE_INVENTORY` provides an alternative default inventory path. Host arguments refer to unique inventory `name` values and preserve inventory order. `--update` runs `nix flake update` once for each distinct flake selected by the invocation; lock-file changes remain even if deployment is declined or fails.

An inventory contains one entry per target:

```toml
[[hosts]]
name = "www"
description = "Static web server"
ssh_host = "www.example.com"
flake_path = "../www"
flake_name = "www"
```

Dance requires `nix`, `nixos-rebuild`, and SSH access as `root` to each target. Review failures are shown without blocking valid outdated hosts. Before activation, Dance checks that the target has not changed since review; afterward, it verifies that the expected system is active.

## Development

The Nix flake provides the package and development shell. Run `cargo test` and `./check.sh` to validate changes, then `./format.sh`.

## License

Licensed under either the Apache License, Version 2.0 or the MIT license, at your option.
