# Prepared native results

Use only when the native controller supplies `native-selected-results-v3`,
`native-finalize-results-v2` or `native-finalize-frontier-v2`. The complete request arrives through supervised stdin.
It is neither semantic completion nor permission to process unrelated work.

## Selected tasks

Read every supplied resource and applicable contract. `tasks` contains exact queue
metadata, captured subject/context revisions, admitted resources and target owner.
The retained source appears once as evidence; `task.input.text` is replaced by its
explicit resource reference. `contract_indices` select shared `contracts`.
These values replace ordinary configuration, queue enumeration, repeated `work context`
and reference-file intake. A candidate is not automatically the same concept or
independent corroboration.

## Revision ownership

Native revisions are opaque identifiers with distinct domains. Compare only the same
named field from native output, such as `wiki fingerprint`'s `file_revision`.
A raw SHA-256 or another revision field is not interchangeable and does not establish
that an input changed. Rust validates captured revisions; do not reconstruct guards.

## Completed Work

`completed` supplies the exact complete root, evidence mapping, source questions and
processing concerns. Its `input_revision` is distinct from the publication
`context_revision`. Generate from those observations without restarting preceding
intake, discovery or full-source reading. Preserve source questions as attributed
uncertainty; processing blockers do not become source questions or approval.

## Complete observation coverage

`native-finalize-frontier-v2` supplies `frontier` instead of `completed`. Its current
observations jointly cover every planned inspection exactly once, but no combined
root observation has been recorded. Read every supplied observation, its source
questions, resolutions and evidence mapping. Compare claims under their stated scope
and time; an inherited resolution does not resolve another open question. Generate
one semantic result from the whole set, or return `needs-action` for a required step.
Do not create a reduction output or restart paid reads. Rust checks complete disjoint
coverage, request bounds and fresh observation revisions, then seals this distinct
completion authority. The evidence mapping lists resources cited by these observations,
not all inspected resources; an omitted resource does not negate inspection coverage.
Coverage establishes which planned inputs were inspected, not the truth of their claims.
This does not mark the missing root node complete.

## Semantic response

Return one `results` entry for each selected task in supplied order, or one entry for
a completed Work. Each entry has exactly `status`, `value` and `reason`:

- Complete: `status: complete`, a semantic `value` object and `reason: null`.
- Required action: `status: needs-action`, `value: null` and a concrete nonblank reason.
- Extraction discovery: `status: needs-discovery`, `value: {queries: [...]}` and a
  concrete nonblank reason. Each explicit lexical query has `query`, `phrase` and
  `spacing`; supply 1–16 nonblank queries. Other task kinds cannot use this state.

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
remain available. Do not repeat setup or drain other tasks. When extraction needs a
new candidate search, return `needs-discovery` with your explicit queries. Rust
atomically records the recipe and a receipt bound to the actual request, then
replans through normal admission. It reuses Source Work only if its exact source
scope remains current; changed knowledge scope needs a fresh plan. Existing paid
observations remain retained. Recovery grants no observation, seal, publication or
semantic approval. Identical requests and exact request cycles defer without another
call. Never infer queries from an earlier action reason or edit recovery receipts.
Changed scope or review publication requires fresh preparation and regeneration,
never new guards on old prose. Return `needs-action` for other required steps.
An unchanged exact request/action defers before another call; independent work continues.
Actions bind the actual supplied request, including its selected tasks and shared response
allowance. A changed action request is reconsidered individually; a new unchanged action
then defers without another call.
Never edit native receipts or retag earlier replies.

The complete request remains subject to `max_input_bytes`. `max_response_bytes`
reserves native result metadata within `max_result_bytes`; complete assembled results
are checked again. An oversized request defers or enters the existing exact Work
protocol without clipping evidence/rules or rewriting paid plans. After the host,
Rust reconstructs the current full request and completion authority before recording, then apply validates
publication scope again. Codex's explicit final-response file and Claude's terminal
`structured_output` are the response authorities; tool logs and prose are not results.
