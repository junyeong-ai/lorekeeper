//! Standard network configuration for child processes that are allowed to use the network.

/// Standard proxy variables in both casings. Callers forward each set value unchanged,
/// including an empty one: every network client owns its own precedence between them.
pub const PROXY_ENV_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];
