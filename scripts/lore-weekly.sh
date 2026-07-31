#!/usr/bin/env bash
# Weekly Lorekeeper deepening, runnable with no interactive session.
#
# Never ingests — that is the daily job's business. This works on already-persisted pages,
# so it has no hard dependency on today's ingest having succeeded.
#
# The monthly/quarterly/annual syntheses and the retention janitors are NOT here: each has
# its own schedule in config.yaml and its own launchd job, and config is the single source of
# truth for cadence. Their LLM tasks land in the durable queue and are drained by the next
# daily run, so nothing is lost by leaving them independent.
source "$(dirname "${BASH_SOURCE[0]}")/lore-pipeline.sh"

pipeline_start

# Writes the cross-source theme page (and the personal weekly review when the personal
# module is configured), enqueueing the narrative for the LLM to fill.
run "synthesis weekly" lore_cmd synthesis weekly --previous

drain_queue
sync_graph

# The semantic review the deterministic linter cannot do: contradiction worklist,
# two different names for one concept, relationship gaps. Read-mostly — destructive
# consolidation (`lore graph merge`) stays a human decision.
run "knowledge audit" claude_skill "/lore-wiki audit" "$SKILL_DIR/lore-wiki"

pipeline_finish
