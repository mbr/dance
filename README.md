# Dance

`nixos-rebuild` can deploy a flake-based NixOS server with a single command:

```console
nixos-rebuild switch --use-substitutes --flake .#web-server --target-host root@example.com
```

`dance` adds a few conveniences around doing this for a group of servers:

[![Dance deploying ten NixOS servers](docs/dance-deployment.gif)](https://asciinema.org/a/sSUw7XPxkSdJvDgL)

## What `dance` does for you

Dance keeps the complete `nixos-rebuild` operations and adds:

- An **inventory** list of hosts to deploy, mapping a flake output to a remote host.
- **Parallel execution** of all selected deployments.
- Status queries: evaluate the desired system locally, then check if the remote needs to be updated.
- **Rollback** including watchdog timers: if you lock yourself out of SSH, wait 6 minutes for it to revert automatically.
- **Deployment locks** prevent concurrent deployments to the same host.
- **Verification** of deployments: write a script to smoke-test a server, with automatic rollbacks on failure.
- **No deployment state**: the active state comes from `/run/current-system`; the desired state comes from the flake. With a version-controlled `flake.lock` and inventory, Dance can run from any machine.

## Installation

Install the `dance` binary from crates.io:

```console
$ cargo install --locked dance-ops
```

Or try the latest source with Nix:

```console
$ nix shell github:mbr/dance
$ dance --help
```

To include Dance in a flake, add it as an input and use its default package:

```nix
inputs.dance.url = "github:mbr/dance";

# With `dance` and `pkgs` in scope:
environment.systemPackages = [ dance.packages.${pkgs.system}.default ];
```

## Writing an inventory file

An inventory contains any number of hosts with three fields:

```toml
# `www` is the ID used on the command line and in output.
[hosts.www]
# Hostname or SSH configuration alias used to reach the machine.
ssh_host = "example.com"
# Flake directory, relative to this inventory.
flake_path = "../production"
# Server configuration within the flake.
flake_name = "web-server"
```

Pass the file using `-i/--inventory` or set `DANCE_INVENTORY` instead.

## Querying the status quo

The `status` command can be used to query the status quo:

```console
$ dance -i inventory.toml status
```

This queries every host and performs the following steps:

1. Read each host's active system from `/run/current-system` via SSH.
2. Evaluate the desired system store path from `flake_path#flake_name` in the inventory.
3. Run a verifier against the system (see "verifiers" below).
4. Report whether the system needs to be updated.

These steps run for all servers in parallel.

### Revisions

NixOS configuration revisions let you record which source a running system was built from, such as a specific Git commit.

The `REVISION` column shows optional configuration provenance: the shared revision when unchanged, `old -> new` when it differs, and `-` for missing values. Noninteractive output reports available revisions on separate per-host lines.

A flake can opt in by adding a NixOS module with `self` in scope:

```nix
{ system.configurationRevision = self.rev or self.dirtyRev or null; }
```

Dance reads this value from the desired configuration and the active generation's `nixos-version --json`. It neither sets nor overrides it, and ordinary deployments do not require Git. A missing revision or unavailable remote version command leaves provenance unspecified.

Dance decides whether to deploy by comparing system store paths. The revision is part of that system: setting it to `self.rev` means every new commit changes the store path, even if the commit only changes documentation or another host's configuration. Dance will therefore consider the system outdated.

A dirty revision indicates uncommitted changes. The Git commit alone cannot reproduce that configuration.

## Deploying servers

Using `deploy` instead of `status` performs the same active-versus-desired comparison without verifying the old system, then offers a confirmation prompt. If you answer `y` there (or pass `--yes`), `dance` will, for each outdated server:

1. Acquire a per-host deployment lock to prevent another Dance run from deploying to that host.
2. Build and upload the desired system with `nixos-rebuild dry-activate`.
3. Arm a rollback watchdog and temporarily activate the system with `nixos-rebuild test`. Note that `test` is a bit of a misnomer; this activates the system except for updating the boot loader.
4. Run the configured verifier against the system.
5. Persist the system with `nixos-rebuild switch`.
6. Disarm the watchdog.
7. Garbage collect down to a configurable number of kept generations.

A failed activation or verifier immediately restores the system observed during review. If Dance exits or loses contact first, the watchdog performs the same restoration when its deadline expires.

## Updating flake inputs

Pass `--update` to run `nix flake update` once for each selected flake before Dance evaluates its desired systems:

```console
$ dance status --update
$ dance deploy --update
```

The updated `flake.lock` remains in the worktree for review. For deployments, add `--commit` to commit changed lock files before evaluating and deploying the configurations; `--git` is shorthand for `--update --commit`:

```console
$ dance deploy --update --commit
$ dance deploy --git
```

## Verification

A flake can optionally provide a runnable verifier with the same key as its NixOS configuration:

```nix
danceVerifiers.${system}.web-server = pkgs.writeShellApplication {
  name = "verify-web-server";
  runtimeInputs = [ pkgs.hurl ];
  text = ''
    hurl --test <<'EOF'
    GET https://example.com/
    HTTP 200
    EOF
  '';
};
```

Dance runs the verifier on the deployment machine with `DANCE_HOST_ID` and `DANCE_SSH_TARGET` in its environment. The verifier can use the latter for internal checks such as `ssh "$DANCE_SSH_TARGET" systemctl is-active caddy.service`.

Run verifiers without deploying:

```console
dance verify www
```

## Logs

Dance keeps command output under `$XDG_STATE_HOME/dance/logs` when a run fails, falling back to `~/.local/state/dance/logs` when `XDG_STATE_HOME` is unset. Each retained run contains a `run.log` tracing log and numbered raw output streams for its commands.

## Automation-friendly output

Dance uses live per-host progress in a terminal and concise summaries otherwise. Failures point to retained command logs, making runs easy to inspect from scripts and coding agents.
