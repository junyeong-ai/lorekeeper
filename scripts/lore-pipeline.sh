# shellcheck shell=bash
# Shared plumbing for the scheduled Lorekeeper pipelines. Sourced, never executed — so it
# carries no shebang, and the shell it is written for is declared above instead.
#
# Every question about the vault is asked THROUGH `lore`, never answered by the shell — for
# two independent reasons. macOS TCC protects ~/Documents per-binary and `/bin/bash` under
# launchd holds no such grant, so a direct `ls` there fails in a way `2>/dev/null` would turn
# into a plausible-looking empty result. And `lore`'s human-readable output is not a
# contract: `config vault-root` and `queue count` exist precisely so a script never has to
# grep prose that could be reworded.
#
# Deliberately NOT `set -e`: a failing stage must not skip the ones after it. Ingest failing
# for one source is exactly when draining the queue and refreshing the graph still matter.
# Every stage's status is recorded and `pipeline_finish` exits non-zero if any of them
# failed, so a scheduler (and `lore health`) still sees the failure.

set -uo pipefail

LORE="${LORE_BIN:-lore}"
CLAUDE="${CLAUDE_BIN:-claude}"
CONFIG="${LORE_CONFIG:-$HOME/.config/lorekeeper/config.yaml}"

log() { printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*"; }

failed=()

# Run one stage, recording its outcome without stopping the pipeline.
run() {
    local name="$1"; shift
    log "▸ $name"
    if "$@"; then
        log "✓ $name"
    else
        local code=$?
        log "✗ $name (exit $code)"
        failed+=("$name")
    fi
}

lore_cmd() { "$LORE" --config "$CONFIG" "$@"; }

pipeline_start() {
    VAULT="$(lore_cmd config vault-root)" || {
        log "✗ could not resolve vault root from $CONFIG"
        exit 1
    }
    log "vault: $VAULT"
}

pipeline_finish() {
    if [ ${#failed[@]} -gt 0 ]; then
        log "done with failures: ${failed[*]}"
        exit 1
    fi
    log "done — all stages ok"
}

# What the skill itself cannot know: that nobody is watching.
#
# A skill is written to serve both an interactive session and a scheduled one, so the fact
# that this run is unattended belongs to the invocation, not to the skill. Without it the
# model can reasonably end a turn on a question or a stated intention — which, with no one
# there to answer, silently becomes work that never happened.
#
# Note what is absent: no instruction to verify, re-check, or double-check. Current models
# do that on their own, and asking compounds with it into wasted work rather than better
# output.
AUTONOMOUS_PREAMBLE='You are operating autonomously as a scheduled job. Nobody is watching
and nobody can answer a question mid-run, so asking one blocks the work rather than
clarifying it. Proceed on reversible actions that follow from the task; stop only if you
need something no one but the user can supply.

Before ending your turn, read your last paragraph. If it is a plan, a question, a list of
next steps, or a promise about work you have not done, do that work now instead of ending.

Report only what you can point to a tool result for. If a step failed or was skipped, say
so plainly with the evidence; state finished work plainly without hedging.'

# Run a Claude Code skill headlessly against the vault.
#
# Run from the vault so the sandbox covers the pages being edited; the skill's own reference
# files live outside it and are added explicitly. Tool access is whitelisted rather than
# bypassed: this job edits a knowledge vault unattended, so it gets file edits, the `lore`
# binary, and the two commands the drain protocol's own steps require — `mkdir -p` and `mv`
# archive the finished queue file, and a drain that cannot archive reports failure every night
# for work it has already committed. The list is a superset of what the protocol asks for, so
# whether a skill's `allowed-tools:` frontmatter widens this set is not something the schedule
# has to depend on.
claude_skill() {
    local prompt="$1" skill_dir="$2"
    env -C "$VAULT" "$CLAUDE" -p "$prompt" \
        --append-system-prompt "$AUTONOMOUS_PREAMBLE" \
        --permission-mode acceptEdits \
        --allowedTools "Bash(lore:*)" "Bash(mkdir:*)" "Bash(mv:*)" \
        "Read" "Edit" "Write" "Glob" "Grep" \
        --add-dir "$skill_dir" \
        --output-format text
}

# Drain the LLM queue, skipping the session entirely when there is nothing to fill.
#
# `llm.provider: queue` only buffers tasks to JSONL; filling them needs an LLM. `claude -p`
# runs Claude Code headlessly on the same subscription session the desktop app uses — no API
# key, no separate billing — so this needs no GUI app open.
#
# The exit code is checked separately from the value: a failure to ASK must never read as an
# answer of "nothing to do", which would skip the drain and still report success.
drain_queue() {
    local pending rc
    pending="$(lore_cmd queue count)"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        log "✗ queue count (exit $rc)"
        failed+=("queue count")
        return
    fi
    if [ "$pending" -gt 0 ]; then
        log "queue: $pending current"
        run "queue drain" claude_skill "/lore-process" "$HOME/.claude/skills/lore-process"
    else
        log "− queue drain skipped (no current tasks)"
    fi
}

# Re-derive everything the vault computes from its own contents.
#
# `queue apply` materializes the concepts the drain reported — creating and merging concept
# pages, and rewriting each origin page's related-concepts links — and the rest derive from
# the result. All idempotent: with nothing pending, every step is a no-op.
#
# `graph lint` exits non-zero when it FINDS things, which is a report rather than a failure,
# so it runs outside `run` and its exit code is deliberately not collected.
sync_graph() {
    run "schema"         lore_cmd schema
    run "queue apply"    lore_cmd queue apply
    run "backlinks-sync" lore_cmd graph backlinks-sync
    run "wiki refresh"   lore_cmd wiki refresh

    log "▸ graph lint"
    lore_cmd graph lint || true
}
