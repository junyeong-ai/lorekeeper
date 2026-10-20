# Collection and operations commands

| Command | Purpose |
|---------|---------|
| `lore validate` | Parse + validate config, print summary |
| `lore run daily [--profile NAME] [--wait] [--json]` | Complete collection, semantic processing, graph reconciliation and retained periods using the configured agent |
| `lore run process [--profile NAME]` | Process queued semantic work and verify native publication |
| `lore run knowledge [--profile NAME]` | Review one configured semantic audit batch and verify its completion |
| `lore ingest` | Collect ALL enabled sources, write pages; render the cross-source work-log when a `personal:` module is configured |
| `lore ingest <source>` | Refresh one source's pages only — never rewrites the work-log (it sees a subset) |
| `lore ingest --dry-run` | Preview without vault writes |
| `lore ingest --recover [--dry-run]` | Requery bounded missing closed dates from the explicit recovery start; see ingestion scope and replay limits |
| `lore ingest --date YYYY-MM-DD` | Re-materialize a specific day — RE-FETCHES it; see Safety notes before using it on a past date |
| `lore fetch <URL> [--source ID]` | Save an HTML article, Markdown or text document into a configured manual inbox. Then ingest that source and run lore-process; fetching alone does not complete knowledge integration |
| `lore input install DIRECTORY [--python PATH]` | Install optional pinned binary-document dependencies in a new isolated environment; prints the config block |
| `lore input check` | Check configured decoder packages and OCR model digests without processing source documents |
| `lore input inspect PATH` | Return a bounded DOCX/XLSX/PDF/image projection as JSON without publishing or moving input |
| `lore synthesis weekly [--previous]` | Weekly synthesis + personal review |
| `lore synthesis monthly [--previous]` | Monthly performance review |
| `lore synthesis quarterly [--previous]` | Quarterly review with category stats |
| `lore synthesis annual [--previous]` | Annual review from quarterly reviews |
| `lore agenda [--date YYYY-MM-DD]` | The day read off the task board: what is committed to it, what woke, what is due. A view — it writes nothing, and names `lore task sync` when an editor has changed the board since |
| `lore task add <text> [--state] [--due] [--link URL] [--label]` | Write down something to do (default section: `next`). `--link` records where it came from — the Slack thread, the Jira issue, the mail — as an absolute URL in the task's own text, so it survives onto the archived page and stays out of the link graph. A vault path is refused |
| `lore task list [--state] [--json]` | The board, by section |
| `lore task done <id> [--note]` | Close a task. The note becomes the archived page's body, and through it the concept extraction — which is how work performed compounds the way work read does |
| `lore task drop <id>` · `move <id> <state>` · `wait <id> --until <date>` | Take it off the board, move it between sections, park it until a day |
| `lore task remind notify` | Submit due reminders to the OS notifier and retire only accepted submissions |
| `lore task propose` | Put what the sources say is still OPEN into the board's proposed section. An observation proposes and never creates: accept by dragging the line into another section, decline with `lore task drop`. Nothing is proposed twice — the board says what is open and the history says what was answered |
| `lore task candidate --source <id> --summary <text> --url <URL>` | Name work read out of a page by JUDGMENT, for the next `propose` to offer. Refused for a source `personal.tasks.propose_from` does not name, because unlike a status field a reading of prose can be wrong |
| `lore task sync` | Record what an editor did: adopt lines typed by hand, close lines ticked, wake what is due |
| `lore task rollover` | Close the day — carry every still-committed task, counting each carry so a task carried too long reads as the diagnosis it is |
| `lore resolve <name>` | Which concept page a name addresses, by the rule ingest routes an extraction by. Exit 0 owned, 1 absent, 2 more than one page answers to it |
| `lore wiki snapshot` | Read the prepared capture and its creation/verification times without refreshing source files; `lore wiki prepare` performs full current capture |
| `lore wiki context <path> --snapshot ID [--limit N] [--offset N]` | Browse captured links, evidence, declared source relations and scoped reviews together; each group retains its authority, total and pagination |
| `lore status` | One line per subsystem — the installation, source currency, the LLM queue, page contracts, the link graph — each naming the command that owns it. Reports without gating; the per-source timestamps are `lore health` |
| `lore health [--strict]` | Warn if any source is overdue vs `ingest.schedule` (2 missed fires; 48h fallback). `--strict` also fails when a source has NEVER ingested, which a first install has and a broken one looks identical to |
| `lore performance` | Performance category distribution |
| `lore doctor` | Audit materialized pages against the contracts they must satisfy — text cleanliness, a section whose input was recorded and never answered, and credentials in issuer-published forms. Non-zero on a defect or an unreadable page; a credential is REPORTED and does not gate, because its repair is at the issuer and editing the page would not undo the leak |
| `lore schedule --format launchd --bin <abs>` | Print native scheduled-task definitions; review and register them explicitly |
| `lore maintenance [--dry-run]` | Prune ingest history, drained queue files and proven published work caches past `maintenance.retention_days` (default 90d). Work pruning requires exclusive workflow access and a settled queue; unpublished/referenced work and unowned artifacts survive. Originals and streaming event logs are permanent; each source's latest collection log survives |
| `lore queue prune [--dry-run]` | Leave the queue holding only work that still needs an LLM session: drop dead tasks (stale / missing-target), retire a run whose every task is already answered |
| `lore self status` | What this installation is, and whether every deployed copy — interactive/runtime skills, templates, config example, `AGENTS.md` — still matches the running binary. Non-zero when any differs |
| `lore workflow -- claude` or `lore workflow -- codex` | Start a manual Unix agent session under the semantic workflow lease before reading inputs; add `--wait` to wait for another consumer. Scheduled headless calls already acquire it |
| `lore self deploy` | Rewrite those copies from what the binary carries. This is the repair `lore self status` reports, and what an install and an update both run |
| `lore self update [--version V]` | Replace the binary with a published release, then redeploy. Refuses while the queue still holds work, and refuses a release older than the running one unless `--version` names it deliberately |
| `lore self uninstall` | Remove the binary and everything it deployed. The vault is never touched |
