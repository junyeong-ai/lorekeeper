#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/lore-pipeline.sh"

pipeline_start
reconcile_graph
if ! pipeline_failed; then
    run "knowledge audit" claude_skill "/lore-wiki maintain" "$SKILL_DIR/lore-wiki"
    reconcile_graph
    run "knowledge reviews" lore_cmd wiki reviews --check
fi
pipeline_finish
