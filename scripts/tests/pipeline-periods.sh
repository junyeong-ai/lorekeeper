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

# The semantic maintenance consumer can publish repairs that queue synthesis after
# the initial reconciliation. Exercise the real finalization functions with that
# queue transition, including a consumer that exits successfully too early.
KNOWLEDGE_SOURCE="$(dirname "$PIPELINE_SOURCE")/lore-knowledge.sh"
export KNOWLEDGE_SOURCE
for scenario in repaired blocked audit-failure review-failure; do
    export scenario
    rm -f "$TEST_ROOT/pending" "$TEST_ROOT/audited"
    code=0
    bash <<'KNOWLEDGE' > "$TEST_ROOT/knowledge-output" || code=$?
source "$PIPELINE_SOURCE"
# Keep the actual plumbing above while sourcing the knowledge entrypoint below.
source() { :; }
pipeline_start() { VAULT="$TEST_ROOT"; }
log() { :; }
lore_cmd() {
    case "$*" in
        'queue count')
            if [ -f "$TEST_ROOT/pending" ]; then printf '1\n'; else printf '0\n'; fi ;;
        'queue check') [ ! -f "$TEST_ROOT/pending" ] ;;
        'wiki reviews --check') [ "$scenario" != review-failure ] ;;
    esac
}
claude_skill() {
    if [ "$1" = '/lore-wiki maintain' ]; then
        [ "$2" = "$SKILL_DIR/lore-wiki" ] && [ "$3" = "$SKILL_DIR/lore-process" ] || return 1
        touch "$TEST_ROOT/audited" "$TEST_ROOT/pending"
        [ "$scenario" != audit-failure ]
    else
        [ "$scenario" != blocked ] || return 1
        rm "$TEST_ROOT/pending"
    fi
}
builtin source "$KNOWLEDGE_SOURCE"
KNOWLEDGE
    if [ "$scenario" = repaired ]; then
        [ "$code" -eq 0 ]
    else
        [ "$code" -ne 0 ] || { echo "knowledge pipeline hid $scenario" >&2; exit 1; }
    fi
    [ -f "$TEST_ROOT/audited" ]
    if [ "$scenario" != blocked ]; then
        [ ! -f "$TEST_ROOT/pending" ] || { echo 'knowledge pipeline left queued repairs' >&2; exit 1; }
    fi
done
printf 'knowledge finalization: 4 scenarios passed\n'

for scenario in single dependent missing; do
    export scenario
    bash <<'PLUGINS' > "$TEST_ROOT/plugin-output"
source "$PIPELINE_SOURCE"
set -e
VAULT="$TEST_ROOT"
skills="$TEST_ROOT/project skills"
mkdir -p "$skills/lore-wiki" "$skills/lore-process"
touch "$skills/lore-wiki/SKILL.md" "$skills/lore-process/SKILL.md"
lore_cmd() {
    [ "$1" = workflow ] && [ "$2" = --wait ] && [ "$3" = -- ] || return 1
    shift 3
    printf '%s\n' "$@" > "$TEST_ROOT/consumer-args"
}
rm -f "$TEST_ROOT/consumer-args"
case "$scenario" in
    single) claude_skill /lore-process "$skills/lore-process" ;;
    dependent) claude_skill '/lore-wiki maintain' "$skills/lore-wiki" "$skills/lore-process" ;;
    missing)
        rm "$skills/lore-process/SKILL.md"
        if claude_skill '/lore-wiki maintain' "$skills/lore-wiki" "$skills/lore-process"; then
            echo 'consumer accepted a missing required skill' >&2
            exit 1
        fi
        [ ! -e "$TEST_ROOT/consumer-args" ]
        exit ;;
esac
awk '/^--plugin-dir$/ { getline; print }' "$TEST_ROOT/consumer-args" > "$TEST_ROOT/plugins"
awk '/^--add-dir$/ { getline; print }' "$TEST_ROOT/consumer-args" > "$TEST_ROOT/access"
if [ "$scenario" = single ]; then
    printf '%s\n' "$skills/lore-process" > "$TEST_ROOT/expected-plugins"
    grep -Fxq '/lore-process:lore-process' "$TEST_ROOT/consumer-args"
    if grep -q '^Skill(' "$TEST_ROOT/consumer-args"; then exit 1; fi
else
    printf '%s\n' "$skills/lore-wiki" "$skills/lore-process" > "$TEST_ROOT/expected-plugins"
    grep -Fxq '/lore-wiki:lore-wiki maintain' "$TEST_ROOT/consumer-args"
    grep -Fxq 'Skill(lore-process:lore-process)' "$TEST_ROOT/consumer-args"
    grep -Fxq 'Skill(lore-process:lore-process *)' "$TEST_ROOT/consumer-args"
    [ "$(grep -c '^Skill(' "$TEST_ROOT/consumer-args")" -eq 2 ]
fi
cmp "$TEST_ROOT/expected-plugins" "$TEST_ROOT/plugins"
cmp "$TEST_ROOT/expected-plugins" "$TEST_ROOT/access"
PLUGINS
done
printf 'consumer plugin dependencies: 3 scenarios passed\n'
