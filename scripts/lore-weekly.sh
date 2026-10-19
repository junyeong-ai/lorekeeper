#!/bin/bash
# Weekly Lorekeeper deepening, runnable with no interactive session.
#
# The trigger retains an exact closed-period obligation. Daily resumes it after
# collection and semantic processing; clock gaps do not establish readiness.
source "$(dirname "${BASH_SOURCE[0]}")/lore-pipeline.sh"

pipeline_start

run "synthesis weekly intent" lore_cmd synthesis weekly --previous --defer --wait-for-vault

pipeline_finish
