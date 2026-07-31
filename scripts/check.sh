#!/usr/bin/env bash
# Every gate CI runs, in one command, failing on the first that does.
#
# The gates are the same invocations `.github/workflows/ci.yml` uses, so a green run here is
# the same answer CI gives. Running them by hand and reading the output is not: a
# `cargo doc | grep -c error` prints a number and exits 0, and a non-zero count read as
# informational shipped a broken doc build twice.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n\033[1m▸ %s\033[0m\n' "$*" >&2; }

step "fmt"
cargo fmt --check

step "clippy"
cargo clippy --all-targets --all-features -- -D warnings

step "test"
cargo test --workspace

step "doc"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --doc

step "deny"
cargo deny check

printf '\n\033[32m✓ every gate passed\033[0m\n' >&2
