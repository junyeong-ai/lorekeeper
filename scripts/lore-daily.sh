#!/usr/bin/env bash
# Daily Lorekeeper pipeline, runnable with no interactive session.
#
# One script rather than three scheduled jobs because each stage consumes what the previous
# one produced: ingest writes queue tasks, the drain answers them, the graph sync derives
# citations and catalogs from those answers. Scheduling them separately would mean guessing
# at the gaps, and a slow ingest would leave the later stages working on stale input.
source "$(dirname "${BASH_SOURCE[0]}")/lore-pipeline.sh"

pipeline_start

# Close out the day that just ended, before anything reads its record.
#
# It carries every task still committed to yesterday into today and counts the carry, and it
# harvests whatever was ticked in an editor since — which is what puts those completions into
# the transition log the ingest below reads. Running it after the ingest would archive them a
# day late, every day.
#
# Conditional on the board existing, asked of `lore` rather than guessed at from a path: an
# install that never turned the intent plane on has no day to close, and a stage that failed
# nightly for that reason would be noise the scheduler learns to ignore.
if lore_cmd config board-path >/dev/null 2>&1; then
    # The day this closes is the one that ENDED, named rather than inferred: a close run by
    # hand last night and this one are two closes of the same day, and only a declared date
    # lets the second recognise the first. `lore` resolves the word against `vault.timezone`,
    # because a `date` here answers in the machine's zone — on a host an hour the other side of
    # the vault's, that is a different day, and the declared day is the key.
    run "task rollover" lore_cmd task rollover --closing yesterday
fi

# The one stage whose failure LOSES data: source windows roll (24h lookbacks) and RSS items
# scroll out, so a day never ingested is gone unless backfilled by hand.
run "ingest" lore_cmd ingest

drain_queue
sync_graph
pipeline_finish
