#!/usr/bin/env bash
# Exercise the shipped dependency chain, including retry after a failed drain.
set -euo pipefail
PIPELINE_SOURCE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lore-pipeline.sh"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT
export PIPELINE_SOURCE TEST_ROOT
for scenario in success drain-failure apply-failure; do
    export scenario
    bash <<'CASE' > "$TEST_ROOT/output"
source "$PIPELINE_SOURCE"
log() { :; }
lore_cmd() {
    printf '%s\n' "$*" >> "$TEST_ROOT/calls"
    if [ "$*" = 'queue count' ]; then printf '1\n'; fi
    if [ "$scenario" = apply-failure ] && [ "$*" = 'queue apply' ]; then return 1; fi
}
claude_skill() {
    printf 'drain\n' >> "$TEST_ROOT/calls"
    if [ "$scenario" = drain-failure ]; then return 1; fi
}
resume_periods
if [ "$scenario" = success ]; then
    [ ${#failed[@]} -eq 0 ] || exit 1
else
    [ ${#failed[@]} -eq 1 ] || exit 1
fi
CASE
    if [ "$scenario" = success ]; then
        cat > "$TEST_ROOT/expected" <<'EXPECTED'
synthesis pending --stage base --wait-for-vault
queue count
drain
queue apply
synthesis pending --stage base --wait-for-vault
synthesis pending --stage quarter --wait-for-vault
queue count
drain
queue apply
synthesis pending --stage quarter --wait-for-vault
synthesis pending --stage annual --wait-for-vault
queue count
drain
queue apply
synthesis pending --stage annual --wait-for-vault
queue check
wiki refresh
EXPECTED
        cmp "$TEST_ROOT/expected" "$TEST_ROOT/calls"
    else
        if grep -q 'stage quarter' "$TEST_ROOT/calls"; then exit 1; fi
        if grep -q 'stage annual' "$TEST_ROOT/calls"; then exit 1; fi
    fi
    rm "$TEST_ROOT/calls"
done
printf 'period dependency chain: 3 scenarios passed\n'
