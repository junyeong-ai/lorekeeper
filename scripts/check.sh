#!/usr/bin/env bash
# Every gate CI runs, in one command, failing on the first that does.
#
# The gates are the same invocations `.github/workflows/ci.yml` uses, so a green run here is
# the same answer CI gives. Running them by hand and reading the output is not: a
# `cargo doc | grep -c error` prints a number and exits 0, and a non-zero count read as
# informational shipped a broken doc build twice.
#
# `gate` names the ci.yml job each step stands for, and `unrunnable` names the one that cannot
# run from here. `every_ci_gate_is_run_or_declared_unrunnable` compares both against the
# workflow, so a job added there is a failing test until it is answered — the claim in the line
# above is a promise, and it was false for five of ten jobs before anything compared them.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n\033[1m▸ %s\033[0m\n' "$*" >&2; }
gate() { step "$1"; }
unrunnable() { printf '\033[2m· %s — %s\033[0m\n' "$1" "$2" >&2; }

gate fmt
cargo fmt --all --check
taplo fmt --check

gate clippy
cargo clippy --workspace --all-targets --all-features -- -D warnings

gate test
cargo nextest run --workspace --all-targets
cargo test --workspace --doc

gate shell
bash -n scripts/*.sh
shellcheck --severity=warning scripts/*.sh
actionlint
zizmor .github/workflows/

gate doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

gate msrv
msrv=$(awk -F'"' '
  /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { in_section = 1; next }
  /^[[:space:]]*\[/                                   { in_section = 0 }
  in_section && /^[[:space:]]*rust-version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)
[ -n "$msrv" ] || { echo "Cargo.toml declares no rust-version" >&2; exit 1; }
# `+<toolchain>` rather than a bare `cargo`: `rust-toolchain.toml` pins the build toolchain for
# this checkout and would otherwise override the declared floor, turning this into a duplicate
# of the gate above it.
cargo "+$msrv" check --workspace --all-targets

gate audit
cargo audit --deny warnings

gate deny
cargo deny check

gate machete
cargo machete

gate build
cargo build --workspace --release --locked

unrunnable windows "needs a Windows C toolchain for blake3 and aws-lc-sys; CI's job is the only one that answers"

printf '\n\033[32m✓ every gate passed\033[0m\n' >&2
