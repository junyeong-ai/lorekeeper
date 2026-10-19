# Coordinated parallel processing

For selected synthesis inputs, use the [read-only evidence packet workflow](queue-format.md#read-only-evidence-packets).
Assign each task and target page to one worker. Workers stage typed result files;
one coordinator runs apply, retires queue files through `queue prune`, and performs Finalize. During a
synthesis wave, pause ingest, merges, capture/extract/wiki edits, and other source or
target writers: external editors do not share the applier lock. Workers return
durable-review proposals to the coordinator instead of editing shared exploration
pages. The coordinator reconciles proposals against existing reviews and publishes
them serially before the final synthesis snapshot. Re-run backlinks-sync and status
after review publication; reissue only affected tasks and draft against their new
source revisions and target snapshots. A review proposal is
a coordination artifact, not a field accepted by `lore queue apply`.

