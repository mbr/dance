# Dance

A tool for deploying servers with `nixos-rebuild`. Everything you wished your pet k3s cluster was, and more. It's a companion to the still-unreleased provisioning tool named `prom`, hence the name.

At its core, Dance takes servers defined in local Nix flakes and deploys them over SSH. A flake can define one server or several related servers. A few conveniences come with that:

- An inventory keeps short server IDs and SSH addresses in one place.
- Status checks and deployments run in parallel across multiple servers.
- `--update` updates the inputs of all selected flakes before comparison.
- `nixos-rebuild test` builds, uploads, and temporarily activates each approved system before it becomes a boot generation.
- Flake verifiers are built after the test activation and can check it externally or over SSH.
- Verified systems are made persistent with `nixos-rebuild switch`; failed tests or verifiers trigger a best-effort live rollback.
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
dance status
```

compares the active system on `www.example.com` with the system defined locally and runs its configured verifier. The `VALID` column reports `yes`, `-` when no verifier is configured, or `failed`. Pass one or more inventory IDs to limit the status check:

```console
dance status www
dance status --update www # Update the production flake before comparison
```

To deploy outdated systems, run:

```console
dance deploy
dance deploy www                   # Deploy only the www inventory entry
dance deploy --update www          # Update its flake before deploying
dance deploy --update --commit www # Commit the lock after successful deployment
dance deploy --git www             # Shorthand for --update --commit
```

Dance asks for confirmation, temporarily activates each system with `nixos-rebuild test`, builds and runs its verifier, then makes a successful test persistent with `nixos-rebuild switch`. A failed test or verifier causes Dance to reactivate the system observed during review. Final host statuses and the deployment summary identify rolled-back hosts explicitly, and a deployment that required rollback exits unsuccessfully. If that live rollback fails, reboot the machine through its console to return to the persistent generation.

A deployment centers on these two commands:

```console
nixos-rebuild test --use-substitutes --flake ../production#web-server --target-host root@www.example.com
# Run the optional verifier.
nixos-rebuild switch --use-substitutes --flake ../production#web-server --target-host root@www.example.com
```

After a verified deployment, Dance keeps the current and previous system generations and collects unreachable paths on that server. Use `--keep-generations` to change the limit or `--no-gc` to skip cleanup.

With `--update --commit`, or its `--git` shorthand, Dance requires `flake.nix` and the tracked `flake.lock` to be clean before updating. After every selected host deploys successfully, changed locks are committed as `Update flake inputs`. The commit contains only those lock files and preserves unrelated staged changes.

## Safety

Editing `flake.nix`, `flake.lock`, or other flake source files while Dance is running is undefined behavior.

Before activation, Dance acquires `/run/lock/dance-deployment.lock` through SSH and verifies that the reviewed system is still active. A deployment fails rather than waiting when another Dance process holds the host lock. Dance assumes an acquired lock's SSH process remains alive until release. The lock is advisory and does not prevent manual `nixos-rebuild` commands.

Targets must be reachable through noninteractive SSH authentication. Dance disables password prompts and uses connection timeouts and keepalives for its SSH and `nixos-rebuild` operations. Deployments should have an out-of-band reboot path such as a hosting console.

## Verification

A flake can optionally provide a runnable verifier with the same key as its NixOS configuration:

```nix
danceVerifiers.${system}.web-server = pkgs.writeShellApplication {
  name = "verify-web-server";
  runtimeInputs = [ pkgs.hurl ];
  text = ''
    hurl --test <<'EOF'
    GET https://www.example.com/
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

Dance keeps command output under `$XDG_STATE_HOME/dance/logs` when a run fails, falling back to `~/.local/state/dance/logs` when `XDG_STATE_HOME` is unset. Each retained run contains a `run.log` tracing log and numbered raw output streams for its commands. Use `--logs always` to keep successful runs, `--logs never` to disable logs, or `--logs-dir` to change their location.
