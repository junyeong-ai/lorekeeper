#!/usr/bin/env bash
# Verify the scheduled pipeline never absorbs a user's Git state into its receipt.
set -euo pipefail
PIPELINE_SOURCE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lore-pipeline.sh"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT
export PIPELINE_SOURCE
for scenario in clean staged concurrent failed nonrepo; do
    VAULT="$TEST_ROOT/$scenario"
    mkdir -p "$VAULT"
    if [ "$scenario" != nonrepo ]; then
        git -C "$VAULT" init -q
        git -C "$VAULT" config user.name test
        git -C "$VAULT" config user.email test@localhost
        printf 'initial\n' > "$VAULT/note.md"
        git -C "$VAULT" add note.md
        git -C "$VAULT" commit -qm initial
        if [ "$scenario" = staged ]; then
            printf 'staged user change\n' > "$VAULT/note.md"
            git -C "$VAULT" add note.md
            printf 'unstaged user change\n' >> "$VAULT/note.md"
        fi
        if [ "$scenario" != clean ]; then
            printf 'run output\n' > "$VAULT/generated.md"
        fi
        if [ "$scenario" = concurrent ]; then
            printf 'concurrent user note\n' > "$VAULT/personal.md"
        fi
        git -C "$VAULT" rev-parse HEAD > "$TEST_ROOT/head-before"
        git -C "$VAULT" status --porcelain=v1 -uall > "$TEST_ROOT/status-before"
        cp "$VAULT/.git/index" "$TEST_ROOT/index-before"
    fi
    export VAULT
    expected=0
    [ "$scenario" != failed ] || expected=1
    code=0
    bash -c 'source "$PIPELINE_SOURCE"; failed=(); [ "$1" != failed ] || failed+=("test stage"); pipeline_finish' -- "$scenario" > "$TEST_ROOT/receipt" || code=$?
    [ "$code" -eq "$expected" ]
    grep -q 'vault commit skipped: run-owned changes cannot be proven' "$TEST_ROOT/receipt"
    if [ "$scenario" != nonrepo ]; then
        git -C "$VAULT" rev-parse HEAD > "$TEST_ROOT/head-after"
        git -C "$VAULT" status --porcelain=v1 -uall > "$TEST_ROOT/status-after"
        cmp "$TEST_ROOT/head-before" "$TEST_ROOT/head-after"
        cmp "$TEST_ROOT/status-before" "$TEST_ROOT/status-after"
        cmp "$TEST_ROOT/index-before" "$VAULT/.git/index"
    fi
done
printf 'pipeline Git preservation: 5 scenarios passed\n'
