# Prepared native results

Use only when the native controller supplies `native-selected-results-v1` or
`native-finalize-results-v1`. The complete request arrives through supervised stdin.
It is neither semantic completion nor permission to process unrelated work.

## Selected tasks

Read every supplied resource and applicable contract. `tasks` contains exact queue
metadata, captured subject/context revisions, admitted resources and target owner.
The retained source appears once as evidence; `task.input.text` is replaced by its
explicit resource reference. `contract_indices` select shared `contracts`.
These values replace ordinary configuration, queue enumeration, repeated `work context`
and reference-file intake. A candidate is not automatically the same concept or
independent corroboration.

## Completed Work

`completed` supplies the exact complete root, evidence mapping, source questions and
processing concerns. Its `input_revision` is distinct from the publication
`context_revision`. Generate from those observations without restarting preceding
intake, discovery or full-source reading. Preserve source questions as attributed
uncertainty; processing blockers do not become source questions or approval.

## Semantic response

Return one `results` entry for each selected task in supplied order, or one entry for
a completed Work. Each entry has exactly `status`, `value` and `reason`:

- Complete: `status: complete`, a semantic `value` object and `reason: null`.
- Required action: `status: needs-action`, `value: null` and a concrete nonblank reason.

An extraction value is `{concepts: [...]}` using the supplied concept schema: names,
aliases, categories, explicit new/reuse admission, grounding and RSS article selections
remain semantic judgments. A synthesis value is `{synthesis: "...", related: "..." | null}`.
Use null or empty arrays for absent values required by the response schema. An empty
concept list is valid. Do not add task IDs, hashes, dates, target, context, discovery,
selection or source-question fields. Rust assembles the existing full result envelope
from the original prepared scope, seals/stages it and publishes through guarded apply.
Do not write result files or invoke `work finish`/`queue apply` in this native role.
Standalone processing and `work finish` retain their existing full-result contracts.

Targeted name/alias resolution, missing decision-relevant reads and review/repair tools
remain available. Do not repeat setup or drain other tasks. New discovery, changed
scope or review publication requires fresh preparation and regeneration, never new
guards on old prose. Return `needs-action` when a required step cannot be completed.
An unchanged exact request/action defers before another call; independent work continues.
Never edit native receipts or retag earlier replies.

The complete request remains subject to `max_input_bytes`. `max_response_bytes`
reserves native result metadata within `max_result_bytes`; complete assembled results
are checked again. An oversized request defers or enters the existing exact Work
protocol without clipping evidence/rules or rewriting paid plans. After the host,
Rust reconstructs the current full request/root before recording, then apply validates
publication scope again. Codex's explicit final-response file and Claude's terminal
`structured_output` are the response authorities; tool logs and prose are not results.
