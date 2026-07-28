#!/usr/bin/env bash
# Daily Lorekeeper pipeline, runnable with no interactive session.
#
# One script rather than three scheduled jobs because each stage consumes what the previous
# one produced: ingest writes queue tasks, the drain answers them, the graph sync derives
# citations and catalogs from those answers. Scheduling them separately would mean guessing
# at the gaps, and a slow ingest would leave the later stages working on stale input.
source "$(dirname "${BASH_SOURCE[0]}")/lore-pipeline.sh"

pipeline_start

# The one stage whose failure LOSES data: source windows roll (24h lookbacks) and RSS items
# scroll out, so a day never ingested is gone unless backfilled by hand.
run "ingest" lore_cmd ingest

drain_queue
sync_graph
pipeline_finish
