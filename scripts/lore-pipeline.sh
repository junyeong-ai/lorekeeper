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
# failed, so a scheduler still sees the failure (`lore health` cannot: it reads per-source collection freshness, not stage outcomes).

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

# The pipeline has no per-write ownership manifest: even a clean starting tree cannot
# distinguish a user's concurrent edit from this run's output. Never stage or commit
# the vault implicitly. Keep this receipt in the scheduler log, including on failed
# runs, so an operator knows that reviewing/committing the changes is still pending.
# In particular, leave the user's index and existing staging exactly as they were.
commit_vault() {
    log "− vault commit skipped: run-owned changes cannot be proven; working tree and staging left untouched — review and commit separately"
}

pipeline_finish() {
    commit_vault
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
How long the run has taken is not a reason to stop. While work remains and nothing has
failed, keep going; a report that names what is left is still a turn that ended early.

Report only what you can point to a tool result for. If a step failed or was skipped, say
so plainly with the evidence; state finished work plainly without hedging.'

# Run a Claude Code skill headlessly against the vault.
#
# Run from the vault so the sandbox covers the pages being edited; the skill's own reference
# files live outside it and are added explicitly. Tool access is whitelisted rather than
# bypassed: this job edits a knowledge vault unattended, so it gets file edits, the `lore`
# binary, and the commands the drain protocol's own fenced steps run — `ls` lists the queue
# directory, `cat`/`jq` read the tasks, and `mkdir -p`/`mv` archive the finished file, without
# which a drain reports failure every night for work it has already committed. Nothing in `-p`
# mode can prompt, so a command the protocol spells and this list omits is simply denied: the
# list has to cover the protocol exactly rather than approximately.
claude_skill() {
    local prompt="$1" skill_dir="$2"
    if [ ! -f "$skill_dir/SKILL.md" ]; then
        log "✗ skill not installed at $skill_dir — re-run the installer, or point"
        log "  LORE_SKILL_DIR at where you installed the skills (\`--skill project\` puts them"
        log "  under the project's .claude/skills)"
        return 1
    fi
    local plugin_name="${skill_dir##*/}"
    prompt="/${plugin_name}:${prompt#/}"
    # Load the selected SKILL.md as a single-skill plugin. --add-dir grants file
    # access; it does not register a skill passed as an arbitrary directory.
    # `LORE_CONFIG` is passed IN, not left to be rediscovered. `lore` auto-discovers
    # `./config.yaml` before the XDG one, and this session runs with the vault as its CWD — so a
    # `config.yaml` sitting at the vault root silently drained a different vault than the one
    # every other stage of this run operated on.
    # Only the semantic consumer is serialized, from its first read through publication.
    # Collection/period selection and short deterministic writers keep their own locks.
    # workflow passes the selected canonical config/template paths to this exec chain.
    lore_cmd workflow --wait -- env -C "$VAULT" "$CLAUDE" -p "$prompt" \
        --plugin-dir "$skill_dir" \
        --append-system-prompt "$AUTONOMOUS_PREAMBLE" \
        --permission-mode acceptEdits \
        --allowedTools "Bash(lore:*)" "Bash(ls:*)" "Bash(cat:*)" "Bash(jq:*)" \
        "Bash(mkdir:*)" "Bash(mv:*)" "Bash(basename:*)" "Bash(test:*)" "Bash([:*)" \
        "Bash(find:*)" "Bash(wc:*)" "Bash(head:*)" "Bash(date:*)" "Bash(grep:*)" \
        "Read" "Edit" "Write" "Glob" "Grep" \
        --add-dir "$skill_dir" \
        --output-format text
}

# Where the skills were installed. `install.sh --skill project` puts them under the project
# rather than the home directory, and a hardcoded `$HOME/.claude/skills` then named a directory
# that does not exist — which `--add-dir` accepts, leaving the session without the skill it was
# told to run.
SKILL_DIR="${LORE_SKILL_DIR:-$HOME/.claude/skills}"

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
        run "queue drain" claude_skill "/lore-process" "$SKILL_DIR/lore-process"
    else
        log "− queue drain skipped (no current tasks)"
    fi
}

# Re-derive everything the vault computes from its own contents.
#
# `queue apply` materializes the concepts the drain reported — creating and merging concept
# pages, and rewriting each origin page's related-concepts links — and the rest derive from
# the result. Idempotent in the steady state: with nothing pending and no evidence moved,
# every step is a no-op. The FIRST run against a vault whose concept pages predate the
# synthesis input is not: every cited concept is owed one, and the drain REWRITES the section
# it finds. `lore graph backlinks-sync` names how many already hold a written synthesis
# before that happens — run it once by hand on an established vault and read that line.
#
# The second drain is why this is an order rather than a list. `backlinks-sync` is the only
# thing that knows which concepts a run has changed the evidence for, so it is where the
# synthesis rewrites are queued — after `queue apply` has created the pages and their
# citations. Leaving them for tomorrow would put every concept page a day behind its own
# sources AND leave a queue file pending at every ingest, whose startup warning about
# unprocessed work would then never clear. `wiki refresh` follows both drains because the
# SQLite navigation must capture the final semantic bodies (and configured exports follow).
#
# `graph lint` is a stage like any other. It exits non-zero only when the vault contradicts
# itself — a link to a page that is not there, an enabled automatic catalog export that drifts, an unknown
# category, one name on two pages — and a run of this pipeline is what produces those. What a
# healthy vault carries (uncited concepts, hubs, open conflicts) it reports without gating, so its
# exit code belongs in `failed`.
sync_graph() {
    run "schema"         lore_cmd schema
    run "queue apply"    lore_cmd queue apply
    run "backlinks-sync" lore_cmd graph backlinks-sync
    drain_queue
    # Applied again because the second drain can answer a leftover extraction as well as a
    # synthesis, and a concept result written after the only apply of the run would wait a
    # day for its pages. Idempotent: with no results on disk it is a no-op.
    run "queue apply"    lore_cmd queue apply
    # The last apply may publish leftover extractions. Derive their synthesis obligations
    # before checking completion, even when the drain did not finish its own Finalize.
    run "backlinks-sync" lore_cmd graph backlinks-sync
    run "wiki refresh"   lore_cmd wiki refresh
    run "graph lint"     lore_cmd graph lint
    run "queue completion" lore_cmd queue check
    run "knowledge reviews" lore_cmd wiki reviews --check
}

# Resume exact retained periods in dependency order. A failed semantic stage keeps
# the obligations on disk and stops this chain until the next daily run.
resume_periods() {
    local stage before
    for stage in base quarter annual; do
        before=${#failed[@]}
        run "period resume $stage" lore_cmd synthesis pending --stage "$stage" --wait-for-vault
        if [ ${#failed[@]} -ne "$before" ]; then return; fi
        drain_queue
        run "period apply $stage" lore_cmd queue apply
        if [ ${#failed[@]} -ne "$before" ]; then return; fi
        run "period settle $stage" lore_cmd synthesis pending --stage "$stage" --wait-for-vault
        if [ ${#failed[@]} -ne "$before" ]; then return; fi
    done
    run "period queue completion" lore_cmd queue check
    run "period navigation" lore_cmd wiki refresh
}
