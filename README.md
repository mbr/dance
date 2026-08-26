# Dance

`nixos-rebuild` can deploy a flake-based NixOS server with a single command:

```console
nixos-rebuild switch --use-substitutes --flake .#web-server --target-host root@www.example.com
```

`dance` wraps this in a nice utility to make it more comfortable:

(TKTK asciinema record that i already have will go here)

## What `dance` does for you

Dance keeps the complete `nixos-rebuild` operations and adds:

- An inventory list of hosts to deploy, mapping a flake output to a remote host.
- Parallel execution of all selected deployments.
- Status queries: evaluate the desired system locally, then check if the remote needs to be updated.
- Rollback including watchdog timers -- if you lock yourself out of SSH, wait 6 minutes for it to revert automatically.
- Deployment locks to prevent accidental parallel Dance deployments.
- Verification of deployments: write a script to smoke-test a server, with automatic rollbacks on failure.

The top-level flow is `update -> review -> approve -> deploy hosts independently`. Each approved, outdated host follows one state machine:

```text
lock -> reject inherited watchdog -> revalidate -> build verifier -> dry-activate
     -> arm rollback -> test -> verify -> switch -> confirm -> disarm -> collect
```

Verifier, rollback-protection, and collection steps are omitted when disabled. A failure while rollback is armed starts the target-side restoration service immediately. If Dance crashes or loses SSH, the timer starts the same service after six minutes. The service restores the system observed during review, waiting for any disconnected activation process to release NixOS's activation lock. Failures remain local to the affected host.

Dance keeps no separate deployment state: it compares the desired store path with `/run/current-system`. Validation of the old active system is a status check, not a deployment gate; only verification of the tested candidate gates `switch`.

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

Dance deploys each reviewed server independently. Final host statuses and the deployment summary identify rolled-back hosts explicitly, and a deployment that required rollback exits unsuccessfully. If rollback cannot be confirmed, inspect the machine and use its console if necessary.

A deployment centers on these complete rebuild commands:

```console
nixos-rebuild dry-activate --use-substitutes --flake ../production#web-server --target-host root@www.example.com
# Arm target-side rollback, then:
nixos-rebuild test --use-substitutes --flake ../production#web-server --target-host root@www.example.com
# Run the optional verifier, then:
nixos-rebuild switch --use-substitutes --flake ../production#web-server --target-host root@www.example.com
```

After a verified deployment, Dance keeps the current and previous system generations and collects unreachable paths on that server. Use `--keep-generations` to change the limit or `--no-gc` to skip cleanup.

With `--update --commit`, or its `--git` shorthand, Dance requires `flake.nix` and the tracked `flake.lock` to be clean before updating. After every selected host deploys successfully, changed locks are committed as `Update flake inputs`. The commit contains only those lock files and preserves unrelated staged changes.

## Safety

Editing `flake.nix`, `flake.lock`, or other flake source files while Dance is running is undefined behavior.

Before preparing a server, Dance acquires `/run/lock/dance-deployment.lock` through SSH, rejects any watchdog left by an earlier deployment, and verifies that the reviewed system is still active. That server's deployment fails rather than waiting when another Dance process holds its lock. An inherited watchdog must finish or be resolved manually before retrying. Dance assumes an acquired lock's SSH process remains alive until release. The lock is advisory and does not prevent manual `nixos-rebuild` commands.

Targets must be reachable through noninteractive SSH authentication. Dance disables password prompts and uses connection timeouts and keepalives for its SSH and `nixos-rebuild` operations. The watchdog is transient and best-effort: it does not survive a target reboot or failures that stop systemd. Deployments must retain an out-of-band recovery path such as a hosting console.

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
