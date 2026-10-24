//! Exercises deployment review with isolated command fixtures and real Git commits.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

use tempfile::{TempDir, tempdir};

/// Provides a configuration repository and isolated deployment commands.
struct Fixture {
    /// Owns the repository and command fixtures.
    directory: TempDir,
}

impl Fixture {
    /// Creates a tracked flake and commands that never contact a target.
    fn new() -> Self {
        let directory = tempdir().expect("temporary repository should be created");
        let root = directory.path();
        fs::create_dir(root.join("bin")).expect("command directory should be created");
        fs::write(root.join("flake.nix"), "{}\n").expect("flake should be written");
        fs::write(root.join("flake.lock"), "old\n").expect("lock should be written");
        fs::write(
            root.join("inventory.toml"),
            "[hosts.test]\nssh_host = 'test'\nflake_path = '.'\nflake_name = 'test'\n",
        )
        .expect("inventory should be written");
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.name", "Dance Test"]);
        git(root, &["config", "user.email", "dance@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["add", "flake.nix", "flake.lock", "inventory.toml"]);
        git(root, &["commit", "--quiet", "-m", "Initial"]);

        executable(
            &root.join("bin/nix"),
            r#"printf '%s\n' "$1" >> commands
case "$1" in
    flake)
        printf 'updated\n' > flake.lock
        test "$SCENARIO" != update-failure
        ;;
    eval)
        case "$SCENARIO" in
            revision-* | no-git) ;;
            *)
                test "$(git show HEAD:flake.lock)" = updated
                test -z "$(git status --porcelain -- flake.nix flake.lock)"
                ;;
        esac
        test "$SCENARIO" != evaluation-failure
        path=/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system
        revision=null
        case "$SCENARIO" in
            confirmation | revision-change | revision-same)
                path=/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-system ;;
        esac
        case "$SCENARIO" in
            revision-change | revision-same-path) revision='"candidate"' ;;
            revision-same) revision='"deployed"' ;;
        esac
        printf '{"path":"%s","configurationRevision":%s}\n' "$path" "$revision"
        ;;
    *) exit 1 ;;
esac
"#,
        );
        executable(
            &root.join("bin/ssh"),
            r#"printf '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system\n'
case "$SCENARIO" in
    revision-*) printf '{"configurationRevision":"deployed"}\n' ;;
    *) printf '{"nixpkgsRevision":"not-the-configuration-revision"}\n' ;;
esac
"#,
        );
        executable(&root.join("bin/nixos-rebuild"), "exit 99\n");
        Self { directory }
    }

    /// Runs the application with fixture commands scoped to its subprocess.
    fn deploy(&self, scenario: &str, flags: &[&str]) -> Output {
        let path = env::join_paths([self.directory.path().join("bin")].into_iter().chain(
            env::split_paths(&env::var_os("PATH").expect("PATH should be set")),
        ))
        .expect("fixture PATH should be valid");
        Command::new(env!("CARGO_BIN_EXE_dance"))
            .current_dir(self.directory.path())
            .env("PATH", path)
            .env("SCENARIO", scenario)
            .args(["--logs", "never", "-i", "inventory.toml", "deploy"])
            .args(flags)
            .output()
            .expect("dance should run")
    }
}

/// Runs a successful Git command inside a fixture repository.
fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .expect("git should run");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("Git output should be UTF-8")
}

/// Writes an executable script using the shell available in the test environment.
fn executable(path: &Path, script: &str) {
    let shell = Command::new("sh")
        .args(["-c", "command -v sh"])
        .output()
        .expect("shell should be available");
    assert!(shell.status.success());
    let shell = String::from_utf8(shell.stdout).expect("shell path should be UTF-8");
    fs::write(path, format!("#!{}\nset -eu\n{script}", shell.trim()))
        .expect("fixture script should be written");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("fixture script should be executable");
}

/// Checks both Git flag forms commit before evaluating even an unchanged system.
#[test]
fn commits_before_evaluation() {
    for flags in [
        vec!["--git", "--yes"],
        vec!["--update", "--commit", "--yes"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.deploy("success", &flags);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            git(fixture.directory.path(), &["log", "-1", "--format=%s"]).trim(),
            "Update flake inputs"
        );
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("commands"))
                .expect("command trace should exist"),
            "flake\neval\n"
        );
    }
}

/// Checks revisions are reported without controlling deployment eligibility.
#[test]
fn reports_revisions_without_changing_path_comparisons() {
    for scenario in ["revision-change", "revision-same", "revision-same-path"] {
        let fixture = Fixture::new();
        let output = fixture.deploy(scenario, &[]);
        let unchanged = scenario == "revision-same-path";
        assert_eq!(output.status.success(), unchanged, "{scenario}: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("output should be UTF-8");
        assert!(stdout.contains(if unchanged {
            "1 up to date, 0 out of date"
        } else {
            "0 up to date, 1 out of date"
        }));
        assert!(stdout.contains(if scenario == "revision-same" {
            "test: revision deployed\n"
        } else {
            "test: revision deployed -> candidate\n"
        }));
    }
}

/// Checks configurations without revision metadata do not require Git.
#[test]
fn deploys_without_git_or_revision_metadata() {
    let fixture = Fixture::new();
    let root = fixture.directory.path();
    fs::remove_dir_all(root.join(".git")).expect("Git repository should be removed");
    executable(
        &root.join("bin/git"),
        "echo unexpected-git-call >&2; exit 99\n",
    );
    let output = fixture.deploy("no-git", &["--yes"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("output should be UTF-8");
    assert!(!stdout.contains("revision"));
    assert_eq!(
        fs::read_to_string(root.join("commands")).expect("command trace should exist"),
        "eval\n"
    );
}

/// Checks update failures stop review and later failures preserve committed locks.
#[test]
fn failures_respect_the_commit_boundary() {
    for scenario in [
        "update-failure",
        "commit-failure",
        "evaluation-failure",
        "confirmation",
    ] {
        let fixture = Fixture::new();
        let root = fixture.directory.path();
        if scenario == "commit-failure" {
            executable(&root.join(".git/hooks/pre-commit"), "exit 1\n");
        }
        let output = fixture.deploy(scenario, &["--git"]);
        assert!(!output.status.success(), "{scenario}: {output:?}");
        let before_commit = matches!(scenario, "update-failure" | "commit-failure");
        assert_eq!(
            git(root, &["show", "HEAD:flake.lock"]),
            if before_commit { "old\n" } else { "updated\n" }
        );
        assert_eq!(
            fs::read_to_string(root.join("commands")).expect("command trace should exist"),
            if before_commit {
                "flake\n"
            } else {
                "flake\neval\n"
            }
        );
    }
}
