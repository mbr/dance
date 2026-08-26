//! Shared noninteractive SSH policy.

/// Configures unattended connections and detects unreachable peers promptly.
pub(crate) const ARGUMENTS: [&str; 8] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=15",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=2",
];

/// Returns the SSH options accepted through `NIX_SSHOPTS`.
pub(crate) fn nix_options() -> String {
    ARGUMENTS.join(" ")
}
