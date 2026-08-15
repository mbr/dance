# Dance

A tool for deploying servers with `nixos-rebuild`. Everything you wished your pet k3s cluster was, and more. It's a companion to the still-unreleased provisioning tool named `prom`, hence the name.

At its core, Dance takes servers defined in local Nix flakes and deploys them over SSH. A flake can define one server or several related servers. A few conveniences come with that:

- An inventory keeps short server IDs and SSH addresses in one place.
- Review and deployment run in parallel across multiple servers.
- `--update` updates the inputs of all selected flakes during review.
- Systems are built locally and transferred with Nix.
- No separate deployment state is kept: Dance checks `/run/current-system` to determine whether an update is needed.

## Usage

Run Dance from a directory containing `inventory.toml` (override through either `DANCE_INVENTORY` or `--inventory`):

```toml
# `www` is the ID used on the command line and in output.
[hosts.www]
# Hostname or SSH configuration alias used to reach the machine.
ssh_host = "www.example.com"
# Flake directory, relative to this inventory.
flake_path = "../production"
# Server configuration within the flake.
flake_name = "web-server"
```

Running

```console
dance
```

compares the active system on `www.example.com` with the system defined locally. If they differ, Dance asks for confirmation, builds locally, dry-activates, and verifies the target before and after switching. This is roughly equivalent to:

```console
nixos-rebuild switch --flake ../production#web-server --target-host root@www.example.com
```

After a deployment, Dance keeps the current and previous system generations and collects unreachable paths on that server. Use `--keep-generations` to change the limit or `--no-gc` to skip cleanup.

Other ways to run Dance:

```console
dance                     # Review every server
dance www                 # Review only the www inventory entry
dance --update www        # Update the production flake during review
dance --update --review-only www # Update and review without deploying
dance --yes www           # Deploy without confirmation
```

## Logs

Dance keeps command output under `$XDG_STATE_HOME/dance/logs` when a run fails. Use `--logs always` to keep successful runs, `--logs never` to disable logs, or `--logs-dir` to change their location.
