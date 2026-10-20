//! A project's own document graph, read through `nodex`.
//!
//! A repository's ADRs, learnings and guides are knowledge, and what the vault adds is the
//! layer above — the concept a document names, which accumulates evidence from every project
//! and every feed that names it too.
//!
//! Each document becomes its own page, because a decision record is a whole document its
//! author wrote and maintains rather than one item among the many a day holds. That is what
//! makes it reachable by name: the vault search reads the wiki, never the daily pages under
//! it, so a document aggregated onto a dated page is findable only through the concepts it
//! happens to have named. The repository stays its store — the page carries its address —
//! and the document's own `kind` rides along as a tag.
//!
//! `nodex` supplies document identity, kind and lifecycle state. Archived and superseded
//! records remain historical evidence; their declared state travels with the copied body.
//! Admission follows the requested date window and kinds, not an inference from prose.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

pub struct NodexSource;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodexParams {
    /// Absolute path to the repository root holding `nodex.toml`. Absolute because a
    /// scheduled run starts in no particular directory, and a relative path would read a
    /// different repository — or none — depending on who launched it.
    repo: PathBuf,
    /// Explicit compatible binary; omitted uses `nodex` on PATH. Never a shell command.
    #[serde(default)]
    executable: Option<PathBuf>,
    /// Reconcile current inventory independently of author-declared dates.
    #[serde(default)]
    reconcile: bool,
    /// Portable repository namespace; a checkout path is only a location.
    #[serde(default)]
    repository_id: Option<String>,
    /// Document kinds to ingest, from the repository's own `kinds.allowed`. Empty admits
    /// every kind.
    #[serde(default)]
    kinds: Vec<String>,
    /// Exact document identities for a bounded backfill. Every requested identity must
    /// occur in the selected date/kind window; an empty list selects the whole window.
    #[serde(default)]
    document_ids: Vec<String>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    /// How many of the window's documents this source will ingest in one run. A guard against
    /// a window wide enough to pull a repository's whole history at once, never a bound on the
    /// query: a query that stops short of the window reports a quiet day, which is the one
    /// answer a cap must not produce.
    #[serde(default = "default_max_documents")]
    max_documents: usize,
    /// Prefix a document's repository-relative path is appended to, to form the URL a
    /// citation links back through — e.g. `https://github.com/org/repo/blob/main`.
    ///
    /// Omitted for a repository with no remote. A local path would be provenance that
    /// resolves nowhere but this machine, which is worse than stating none.
    #[serde(default)]
    base_url: Option<String>,
}

fn default_lookback() -> u32 {
    24
}

fn default_max_documents() -> usize {
    200
}

/// How much of the index one query asks for before the next asks for more.
///
/// `nodex` bounds a query from BELOW (`--since`) and answers newest-first, so `--limit` cuts
/// at the RECENT end — ahead of the window whenever the window is not the newest days. A
/// backfill of an older day therefore came back holding only documents newer than the day it
/// asked about, filtered every one of them out, and reported a quiet day: `lore ingest --date`
/// is the repair path for a lost or wrong page, and it repaired nothing, in silence. So the
/// reach GROWS until the answer spans the window, which is the same "read to the end of the
/// window" rule every other windowed adapter here follows.
const INITIAL_REACH: usize = 500;

/// Where growing stops and the source fails rather than reading further. Eight queries at
/// most (500 doubling to 64_000), and a window holding this many documents is a
/// `lookback_hours` the operator meant differently — not something the adapter can settle
/// by asking again.
const MAX_REACH: usize = 64_000;
const NODEX_COMPATIBILITY: &str = ">=0.60, <0.61";
const INVENTORY_FIELDS: &str = "id,title,kind,status,path,created,updated,reviewed,supersedes,superseded_by,implements,related,covers,tags";

/// One document, as `nodex query recent` reports it.
#[derive(Debug, Clone, Deserialize)]
struct RecentItem {
    id: String,
    title: String,
    kind: String,
    status: String,
    path: String,
    date: jiff::civil::Date,
}

#[derive(Debug, Deserialize)]
struct RecentData {
    items: Vec<RecentItem>,
}

/// `nodex`'s reply envelope. Every operational command answers in it, so one shape reads
/// both the answer and the refusal.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    error: Option<EnvelopeError>,
    snapshot: Option<Snapshot>,
    #[serde(default)]
    warnings: Vec<NodexWarning>,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    verification: String,
    unbuildable: usize,
    unfollowed: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NodexWarning {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct NodeDetail {
    node: serde_json::Value,
    body: String,
    outgoing: Vec<OutgoingEdge>,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct OutgoingEdge {
    relation: String,
    target: String,
    // Preserve public reference locations and any future additive edge metadata.
    #[serde(flatten)]
    metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeError {
    code: String,
    message: String,
}

/// Validate this source's params at config-load time, before any I/O.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<NodexParams>(params).map(|_| ())
}

impl crate::ValidatedParams for NodexParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.reconcile
            && self
                .repository_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
        {
            return Err(SourceError::InvalidParams(
                "nodex reconciliation requires a nonempty `repository_id`".into(),
            ));
        }
        if self
            .repository_id
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(SourceError::InvalidParams(
                "nodex `repository_id` must not be blank".into(),
            ));
        }
        if self
            .executable
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(SourceError::InvalidParams(
                "nodex `executable` must be an absolute path to one binary".into(),
            ));
        }
        if !self.repo.is_absolute() {
            return Err(SourceError::InvalidParams(format!(
                "nodex `repo` must be an absolute path, got '{}'",
                self.repo.display()
            )));
        }
        if self.max_documents == 0 {
            return Err(SourceError::InvalidParams(
                "nodex `max_documents` must be > 0".into(),
            ));
        }
        if self.kinds.iter().any(|k| k.trim().is_empty()) {
            return Err(SourceError::InvalidParams(
                "nodex `kinds` entries must not be empty".into(),
            ));
        }
        let unique: std::collections::BTreeSet<_> = self.document_ids.iter().collect();
        if self.document_ids.iter().any(|id| id.trim().is_empty())
            || unique.len() != self.document_ids.len()
        {
            return Err(SourceError::InvalidParams(
                "nodex `document_ids` must contain unique, nonempty identities".into(),
            ));
        }
        Ok(())
    }
}

/// The CLI may disable its target-day filter only for explicitly selected reconciliation.
/// This parses the same typed params as extraction; callers never infer a mode from metadata.
#[cfg(test)]
fn is_reconciliation(params: &serde_json::Value) -> Result<bool, SourceError> {
    Ok(crate::parse_validated::<NodexParams>(params)?.reconcile)
}

#[async_trait]
impl Source for NodexSource {
    fn selection(&self, params: &serde_json::Value) -> Result<crate::Selection, SourceError> {
        let p: NodexParams = crate::parse_validated(params)?;
        Ok(if p.reconcile {
            crate::Selection::DocumentSnapshot
        } else {
            crate::Selection::DateWindow
        })
    }

    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: NodexParams = crate::parse_validated(params)?;
        // The lower bound goes through the shared window rule, which rounds padding outward
        // to whole days; the upper one is the target day itself, since nothing dated after it
        // is knowledge yet and the pipeline would drop it anyway.
        let (start, _) = ctx.day_window(p.lookback_hours, 0)?;
        let first_day = start.to_zoned(ctx.timezone.clone()).date();
        let last_day = ctx.target_date;

        let executable = p
            .executable
            .as_deref()
            .unwrap_or_else(|| Path::new("nodex"));

        // Asked per FIELD rather than through nodex's `any`, which also matches a review
        // stamp. A review is not an edit: the document is unchanged, so re-admitting it on the
        // day it was re-read would put a second copy of knowledge the vault already holds onto
        // a later page and run the extraction over it again. Two queries rather than one
        // answer plus a guess at which field nodex reports when several match.
        // `check` builds in memory and exposes scope-declaration warnings that current
        // query envelopes do not carry. Its unrelated rule findings do not govern ingestion.
        run_nodex::<serde_json::Value>(
            executable,
            &p.repo,
            last_day,
            &["check", "--severity", "error"],
            false,
        )
        .await?;
        let lens: serde_json::Value =
            run_nodex(executable, &p.repo, last_day, &["export", "config"], false).await?;
        let mut inventory = None;
        let mut items: Vec<RecentItem> = Vec::new();
        if p.reconcile {
            let listing: serde_json::Value = run_nodex(
                executable,
                &p.repo,
                last_day,
                &["query", "nodes", "--fields", INVENTORY_FIELDS],
                true,
            )
            .await?;
            let rows = listing
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| SourceError::Parse("nodex inventory omitted items".into()))?;
            if listing.get("total").and_then(|v| v.as_u64()) != Some(rows.len() as u64) {
                return Err(SourceError::Parse("nodex inventory was truncated".into()));
            }
            inventory = Some(listing.clone());
            for row in rows {
                let mut row = row.clone();
                row["date"] = serde_json::json!(last_day.to_string());
                items.push(
                    serde_json::from_value(row)
                        .map_err(|e| SourceError::Parse(format!("nodex inventory item: {e}")))?,
                );
            }
        } else {
            for field in ["created", "updated"] {
                items.extend(query_window(executable, &p.repo, field, first_day, last_day).await?);
            }
        }
        let admitted: Vec<RecentItem> = latest_per_document(items)
            .into_iter()
            .filter(|item| {
                (p.reconcile || (first_day..=last_day).contains(&item.date))
                    && (p.kinds.is_empty() || p.kinds.iter().any(|k| k == &item.kind))
            })
            .collect();
        let admitted = select_documents(admitted, &p.document_ids)?;
        // Refused whole rather than ingested in part. Which documents a partial run kept would
        // be decided by the order the index happened to list them, so the vault would differ
        // between two runs asking the same question — and the operator would have no way to
        // see that from the pages.
        if admitted.len() > p.max_documents {
            return Err(SourceError::InvalidParams(format!(
                "{} holds {} selected documents, past `max_documents` ({}) — narrow the selection or raise the cap deliberately",
                if p.reconcile {
                    "current inventory".to_owned()
                } else {
                    format!("{first_day}..={last_day}")
                },
                admitted.len(),
                p.max_documents
            )));
        }

        let attempted = admitted.len();
        let mut kept = Vec::new();
        let mut observations = Vec::new();
        for item in admitted {
            // A path that leaves the repository is refused before it is read, and refusing it
            // ends the SOURCE rather than skipping the document. A read that failed is one
            // document missing; a path the query had no business naming is the answer itself
            // being wrong, and the next one cannot be trusted either.
            contained(&item.path).map_err(|why| {
                SourceError::Parse(format!(
                    "{} names '{}', which {why} — a document graph addresses files inside the \
                     repository it describes",
                    item.id, item.path
                ))
            })?;
            let detail: NodeDetail = run_nodex(
                executable,
                &p.repo,
                last_day,
                &["query", "node", &item.id, "--with-body"],
                true,
            )
            .await?;
            verify_selection(&item, &detail.node, p.reconcile)?;
            let raw = build_document_observation(
                &p.repo,
                &item,
                p.base_url.as_deref(),
                ctx,
                &detail,
                p.reconcile,
                p.repository_id.as_deref(),
            )?;
            observations.push((item, observation_revision(&detail)?));
            kept.push(raw);
        }
        if let Some(before) = inventory {
            let after: serde_json::Value = run_nodex(
                executable,
                &p.repo,
                last_day,
                &["query", "nodes", "--fields", INVENTORY_FIELDS],
                true,
            )
            .await?;
            if before != after {
                return Err(SourceError::Parse(
                    "nodex inventory changed during reconciliation".into(),
                ));
            }
        }
        // Normal date runs also require the final graph's content currentness. Reconcile
        // already performs the full final inventory query above.
        if !p.reconcile {
            let _: serde_json::Value = run_nodex(
                executable,
                &p.repo,
                last_day,
                &["query", "nodes", "--fields", INVENTORY_FIELDS],
                true,
            )
            .await?;
        }
        // Requery public observations, including resolved outgoing edges whose targets can
        // change without changing this file. Link projection also depends on live paths.
        // These are bounded live checks, not a filesystem-wide writer lock.
        for ((item, before), raw) in observations.into_iter().zip(&kept) {
            let after: NodeDetail = run_nodex(
                executable,
                &p.repo,
                last_day,
                &["query", "node", &item.id, "--with-body"],
                true,
            )
            .await?;
            verify_selection(&item, &after.node, p.reconcile)?;
            verify_observation(
                &item,
                &before,
                &after,
                &raw.body,
                &p.repo,
                p.base_url.as_deref(),
            )?;
        }
        // Configuration is another input to every public observation, including link
        // resolution. Check it again after all selected observations have been revalidated.
        let final_lens: serde_json::Value =
            run_nodex(executable, &p.repo, last_day, &["export", "config"], false).await?;
        if lens != final_lens {
            return Err(SourceError::Parse(
                "nodex configuration changed during observation".into(),
            ));
        }
        crate::require_any_observation("document", kept.len(), attempted)?;
        Ok(kept)
    }
}

fn select_documents(
    items: Vec<RecentItem>,
    requested: &[String],
) -> Result<Vec<RecentItem>, SourceError> {
    if requested.is_empty() {
        return Ok(items);
    }
    let mut missing: std::collections::BTreeSet<_> = requested.iter().map(String::as_str).collect();
    let selected = items
        .into_iter()
        .filter(|item| missing.remove(item.id.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(SourceError::InvalidParams(format!(
            "requested document IDs absent from the selected date/kind scope or current inventory: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(selected)
}

/// One observation per document, dated by the newest date it declares inside the window.
///
/// The two queries answer about the same document twice when it was written and edited in one
/// window, and a document source writes ONE page per document — so the rows are not two
/// observations, they are two facts about one. The newest is what the page is dated by: the
/// day the vault last saw this document change.
///
/// Keyed on the date rather than on which query answered, so the window's WIDTH cannot decide
/// it. A document written Sunday and edited Monday is dated Monday whether both rows fit in
/// one window or only the later one does, and a backfill of Monday reproduces what the live
/// run wrote.
fn latest_per_document(mut items: Vec<RecentItem>) -> Vec<RecentItem> {
    items.sort_by(|a, b| a.id.cmp(&b.id).then(b.date.cmp(&a.date)));
    items.dedup_by(|a, b| a.id == b.id);
    items
}

/// Read the window whole, growing the query's reach until it spans the window.
///
/// The upper bound is the caller's: `nodex` answers everything on or after `--since`, newest
/// first, so a limit that stops before the window leaves the answer entirely outside it — and
/// the filter downstream then drops every row and reports a day on which nothing happened.
async fn query_window(
    executable: &Path,
    repo: &Path,
    field: &str,
    first_day: jiff::civil::Date,
    last_day: jiff::civil::Date,
) -> Result<Vec<RecentItem>, SourceError> {
    let mut reach = INITIAL_REACH;
    loop {
        let page = query_recent(executable, repo, field, first_day, last_day, reach).await?;
        if spans_window(&page, reach, first_day) {
            return Ok(page);
        }
        reach = reach.saturating_mul(2);
        if reach > MAX_REACH {
            return Err(SourceError::Parse(format!(
                "{}: more than {MAX_REACH} documents declare a {field} date on or after \
                 {first_day}, so the query never reached the window ending {last_day}",
                repo.display()
            )));
        }
    }
}

/// Has this answer read PAST the window's first day?
///
/// Either the repository ran out of documents — it answered with fewer than it was asked for —
/// or it answered with a row dated BEFORE the window opens, which happens only once that day's
/// own documents are all behind it. `at` the first day would not do: the query is bounded one
/// day below the window precisely so a page cut in the middle of the first day's documents
/// cannot pass for one that read them all. The oldest is taken by MINIMUM rather than from the
/// last row, so nothing here rests on the order `nodex` listed them in.
fn spans_window(page: &[RecentItem], reach: usize, first_day: jiff::civil::Date) -> bool {
    page.len() < reach
        || page
            .iter()
            .map(|item| item.date)
            .min()
            .is_some_and(|oldest| oldest < first_day)
}

/// Ask the repository which documents declare a date in the window.
///
/// `--today` pins the clock so a backfill (`lore ingest --date <past>`) asks the same
/// question the day itself would have, and `--since` is a lower bound only — the upper one is
/// the caller's, against the same declared date this reports. `limit` is therefore how far
/// back this one call reaches and not how many documents the window holds; [`query_window`]
/// owns the difference.
///
/// The lower bound is asked one day BEFORE the window opens. An answer reaching only as far as
/// the window's first day proves nothing — the query cannot return anything older, so a page
/// cut in the middle of that day's documents looks exactly like one that read them all. A row
/// dated before the window is the proof, and the caller's own date filter drops it.
async fn query_recent(
    executable: &Path,
    repo: &Path,
    field: &str,
    first_day: jiff::civil::Date,
    last_day: jiff::civil::Date,
    limit: usize,
) -> Result<Vec<RecentItem>, SourceError> {
    let first = first_day.yesterday().unwrap_or(first_day).to_string();
    let limit = limit.to_string();
    let data: RecentData = run_nodex(
        executable,
        repo,
        last_day,
        &[
            "query", "recent", "--field", field, "--since", &first, "--limit", &limit,
        ],
        true,
    )
    .await?;
    Ok(data.items)
}

fn decode_envelope<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    success: bool,
    query: bool,
) -> Result<T, SourceError> {
    let envelope: Envelope<T> = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("nodex envelope: {e}")))?;
    if !envelope.ok {
        let e = envelope.error.unwrap_or(EnvelopeError {
            code: "UNKNOWN".into(),
            message: "refused without reason".into(),
        });
        let remedy = if e.code == "VERSION_MISMATCH" {
            format!(
                "; select a supported nodex {NODEX_COMPATIBILITY} executable with params.executable; the repository must permit that version and already have a current compatible index"
            )
        } else {
            String::new()
        };
        return Err(SourceError::Parse(format!(
            "nodex refused ({}): {}{remedy}",
            e.code, e.message
        )));
    }
    for w in envelope.warnings {
        if matches!(
            w.code.as_str(),
            "scope_coverage" | "snapshot_divergence" | "file_skipped" | "binary_compat"
        ) {
            return Err(SourceError::Parse(format!(
                "nodex incomplete/unsupported observation ({}): {}",
                w.code, w.message
            )));
        }
    }
    if query {
        let snap = envelope.snapshot.ok_or_else(|| {
            SourceError::Parse("nodex query omitted snapshot verification".into())
        })?;
        if snap.verification != "content" || snap.unbuildable != 0 || snap.unfollowed != Some(0) {
            return Err(SourceError::Parse(format!(
                "nodex incomplete snapshot: verification={}, unbuildable={}, unfollowed={:?}",
                snap.verification, snap.unbuildable, snap.unfollowed
            )));
        }
    }
    if !success {
        return Err(SourceError::Parse("nodex exited unsuccessfully".into()));
    }
    envelope
        .data
        .ok_or_else(|| SourceError::Parse("nodex success omitted data".into()))
}

async fn run_nodex<T: serde::de::DeserializeOwned>(
    executable: &Path,
    repo: &Path,
    today: jiff::civil::Date,
    args: &[&str],
    query: bool,
) -> Result<T, SourceError> {
    let output = tokio::process::Command::new(executable)
        .arg("-C")
        .arg(repo)
        .arg("--check-version")
        .arg(NODEX_COMPATIBILITY)
        .arg("--today")
        .arg(today.to_string())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| SourceError::Parse(format!(
            "running nodex executable {} (requires {NODEX_COMPATIBILITY}): {e}; set params.executable to an absolute compatible binary path; no other executable was tried",
            executable.display()
        )))?;
    // check's exit 1 is a rule verdict, not a failure to observe the graph.
    decode_envelope(
        &output.stdout,
        output.status.success()
            || (args.first() == Some(&"check") && output.status.code() == Some(1)),
        query,
    )
    .map_err(|error| SourceError::Parse(format!(
        "nodex executable {} (requires {NODEX_COMPATIBILITY}): {error}; no other executable was tried",
        executable.display()
    )))
}

fn verify_selection(
    item: &RecentItem,
    node: &serde_json::Value,
    reconcile: bool,
) -> Result<(), SourceError> {
    for (field, expected) in [
        ("id", &item.id),
        ("path", &item.path),
        ("title", &item.title),
        ("kind", &item.kind),
        ("status", &item.status),
    ] {
        if node.get(field).and_then(|v| v.as_str()) != Some(expected.as_str()) {
            return Err(SourceError::Parse(format!(
                "{} changed {field} during observation",
                item.id
            )));
        }
    }
    if !reconcile
        && !["created", "updated"].iter().any(|key| {
            node.get(key)
                .and_then(|v| v.as_str())
                .is_some_and(|date| date == item.date.to_string())
        })
    {
        return Err(SourceError::Parse(format!(
            "{} changed its selected date",
            item.id
        )));
    }
    if node
        .get("parse_issues")
        .and_then(|v| v.as_array())
        .is_some_and(|v| !v.is_empty())
    {
        return Err(SourceError::Parse(format!(
            "{} has malformed authored metadata",
            item.id
        )));
    }
    Ok(())
}

/// Point a document's own links back at the repository they resolve in.
///
/// A repository document cites its siblings by a relative path, which means something only
/// inside that repository: carried into the vault verbatim it addresses a page that is not
/// there, and `lore graph lint` reports a broken link on a destination the vault never had any
/// business resolving. Where the repository states a `base_url`, the reference is preserved as
/// the absolute address it has there — `lk_core::link::is_external` keeps a scheme-bearing
/// destination out of the graph, so it reads as a citation without becoming an edge.
///
/// A destination is rewritten only when the repository actually HOLDS that file, which is read
/// from disk rather than assumed. Markdown resolves a relative path against the containing
/// document, and a repository whose links are written from its ROOT instead is common enough to
/// have produced the first one seen here — so the document-relative reading is tried first
/// because it is what the format means, and the root-relative one second because a link that
/// names an existing file was written to mean it. A path neither reading finds is broken where
/// it was written, and becomes its own text: a URL that 404s asserts a record that is not there.
///
/// An ANCHOR-only link (`#section`) addresses the document itself and is left alone, as is a
/// link that already names its own host.
fn repoint_links(body: &str, repo: &Path, doc_path: &str, base_url: Option<&str>) -> String {
    repoint_document_links(body, doc_path, base_url, |rel| holds(repo, rel))
}

/// Canonical nodex link projection with explicit repository membership. Historical
/// readers supply immutable tree membership instead of consulting the live checkout.
pub fn repoint_document_links(
    body: &str,
    doc_path: &str,
    base_url: Option<&str>,
    held: impl Fn(&str) -> bool,
) -> String {
    let dir = Path::new(doc_path).parent().unwrap_or(Path::new(""));
    lk_core::link::rewrite_links_outside_code(body, |text, raw| {
        let (dest, anchor) = lk_core::link::split_raw_dest(raw);
        if dest.is_empty() || lk_core::link::is_external(dest) {
            return None;
        }
        let decoded = lk_core::link::decode_dest(dest);
        let held = [dir.join(&decoded), PathBuf::from(&decoded)]
            .into_iter()
            .filter_map(|candidate| normalize(&candidate))
            .find(|rel| held(rel));
        match (held, base_url) {
            (Some(rel), Some(base)) => Some(format!(
                "[{text}]({}/{}{anchor})",
                base.trim_end_matches('/'),
                lk_core::link::encode_dest(&rel)
            )),
            _ => Some(text.to_owned()),
        }
    })
}

/// Does the repository hold this exact file, under this exact name?
///
/// `Path::is_file` answers yes to a name differing only in case on macOS and Windows, which are
/// the filesystems a repository is commonly checked out on — so a link written `file.md` against
/// a stored `File.md` would be rewritten to a URL a case-sensitive host answers 404 to, which is
/// the one outcome [`repoint_links`] exists to avoid. `canonicalize` answers with the name the
/// filesystem actually stores, so comparing its answer to the path asked for settles the case
/// without a rule about which filesystems fold.
///
/// It also refuses a symlinked destination — canonicalize resolves the link, so the answer is
/// not the path asked for — and one spelled in a different Unicode normal form than the stored
/// name. Both become plain text, which is the safe direction: the alternative is a URL that may
/// or may not resolve at a host this cannot ask.
fn holds(repo: &Path, rel: &str) -> bool {
    let (Ok(real), Ok(root)) = (repo.join(rel).canonicalize(), repo.canonicalize()) else {
        return false;
    };
    real.is_file() && real == root.join(rel)
}

/// A repository-relative path with `.` and `..` resolved, or `None` where it leaves the
/// repository — a destination above the root has no address under `base_url` to be rewritten to.
fn normalize(path: &Path) -> Option<String> {
    use std::path::Component;

    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(
        parts
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Refuse a path that does not stay inside the repository it is relative to.
///
/// `Path::join` DISCARDS its base for a rooted operand and resolves `..` lexically against
/// whatever is above the repository, so those are the two shapes that escape. The check is on
/// path COMPONENTS rather than on the string — the same discipline `manual`'s inbox validation
/// uses — which closes the whole class rather than the spellings someone thought of.
///
/// `has_root`, not `is_absolute`: on Windows a path is absolute only WITH a prefix, so `\foo`
/// is relative by that test while `join` still replaces everything but the drive — an escape
/// the stricter-sounding predicate would admit. `Prefix` is checked beside it for `C:foo`,
/// which carries a prefix and no root and replaces the drive the same way.
///
/// Symlinks are deliberately NOT chased here. A repository's own files are its author's, and
/// `nodex` has already parsed this one to report it — a rule refusing what the document graph
/// already accepted would put the two at odds over which files the repository contains.
fn contained(path: &str) -> Result<(), &'static str> {
    use std::path::Component;

    let path = Path::new(path);
    if path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
        return Err("is a rooted path");
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err("climbs out of the repository with `..`");
    }
    Ok(())
}

/// Consume one verified public observation; never reread or parse the source file.
fn build_document_observation(
    repo: &Path,
    item: &RecentItem,
    base_url: Option<&str>,
    ctx: &ExtractContext,
    detail: &NodeDetail,
    reconcile: bool,
    repository_id: Option<&str>,
) -> Result<RawItem, SourceError> {
    let path = repo.join(&item.path);
    let body = repoint_links(&detail.body, repo, &item.path, base_url);
    let authored_date = ["created", "updated"]
        .iter()
        .filter_map(|key| {
            detail
                .node
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<jiff::civil::Date>().ok())
        })
        .max();
    let date = if reconcile {
        authored_date.unwrap_or(ctx.target_date)
    } else {
        item.date
    };
    let mut record = serde_json::json!({
        "id": item.id, "kind": item.kind, "status": item.status,
        "observation_revision": observation_revision(detail)?,
        "created": detail.node.get("created"),
        "updated": detail.node.get("updated"),
        "timestamp_basis": if reconcile && authored_date.is_none() { "observed" } else { "authored" },
    });
    for key in [
        "superseded_by",
        "supersedes",
        "implements",
        "related",
        "covers",
        "reviewed",
    ] {
        record[key] = detail
            .node
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }
    record["relations"] = serde_json::json!(detail.outgoing);
    if let Some(attributes) = detail
        .node
        .get("attrs")
        .filter(|v| !v.as_object().is_some_and(serde_json::Map::is_empty))
    {
        record["attributes"] = attributes.clone();
    }

    Ok(RawItem {
        // The repository's own vocabulary, carried so a reader can ask for decision records
        // across every project at once.
        labels: vec![item.kind.clone()],
        external_id: Some(item.id.clone()),
        title: item.title.clone(),
        body,
        url: base_url.map(|base| {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                lk_core::link::encode_dest(&item.path)
            )
        }),
        timestamp: date
            .to_zoned(ctx.timezone.clone())
            .map_err(|e| {
                SourceError::Parse(format!("{} declares an impossible date: {e}", item.id))
            })?
            .timestamp(),
        // A repository records no author per document, so the provenance line carries what
        // it does record: which kind of document this is. Never a git author — the commit
        // that touched a file is not a claim about who wrote what is in it.
        author: Some(item.kind.clone()),
        is_self: false,
        work: None,
        metadata: serde_json::json!({
            "kind": item.kind,
            "source_record": record,
            "source_identity": repository_id.map(|repository| serde_json::json!({"repository": repository, "document": item.id})),
            // A location, not a portable identity. Older ingests without repository_id
            // retain their original path-based identity; migration is the pipeline's seam.
            "source_file": path.to_string_lossy(),
            "path": item.path,
        }),
    })
}

const OBSERVATION_SCHEME: &str = "nodex-query-node-v1:sha256";

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct ObservationRevision {
    scheme: &'static str,
    value: String,
}

fn observation_revision(detail: &NodeDetail) -> Result<ObservationRevision, SourceError> {
    use sha2::Digest;
    // serde_json's default Map is sorted; arrays retain the public response's order.
    // Incoming edges are not source declarations and are deliberately not collected.
    let payload =
        serde_json::json!({"node": detail.node, "body": detail.body, "outgoing": detail.outgoing});
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| SourceError::Parse(format!("nodex observation: {e}")))?;
    let mut digest = sha2::Sha256::new();
    digest.update(OBSERVATION_SCHEME.as_bytes());
    digest.update(b"\0");
    digest.update(bytes);
    let value: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(ObservationRevision {
        scheme: OBSERVATION_SCHEME,
        value,
    })
}

fn verify_observation(
    item: &RecentItem,
    before: &ObservationRevision,
    after: &NodeDetail,
    projected_body: &str,
    repo: &Path,
    base_url: Option<&str>,
) -> Result<(), SourceError> {
    if *before != observation_revision(after)?
        || repoint_links(&after.body, repo, &item.path, base_url) != projected_body
    {
        return Err(SourceError::Parse(format!(
            "{} changed during nodex observation",
            item.id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_selection_is_explicit_and_validated_without_io() {
        assert!(
            crate::parse_validated::<NodexParams>(&params(serde_json::json!({})))
                .unwrap()
                .executable
                .is_none()
        );
        let root = tempfile::tempdir().unwrap();
        let absolute = root.path().join("not installed/노덱스");
        assert!(validate_params(&params(serde_json::json!({"executable": absolute}))).is_ok());
        for path in ["", "nodex", "./nodex", "~/bin/nodex"] {
            assert!(validate_params(&params(serde_json::json!({"executable": path}))).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_executable_receives_guards_and_failure_never_falls_back() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("노덱스 binary");
        std::fs::write(&executable, r#"#!/bin/sh
printf '%s\n' "$@" > "$0.args"
printf '%s' '{"ok":true,"data":{"items":[]},"snapshot":{"verification":"content","unbuildable":0,"unfollowed":0}}'
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let day = "2026-10-17".parse().unwrap();
        assert!(
            query_window(&executable, root.path(), "created", day, day)
                .await
                .unwrap()
                .is_empty()
        );
        let args =
            std::fs::read_to_string(executable.with_file_name("노덱스 binary.args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "-C",
                root.path().to_str().unwrap(),
                "--check-version",
                NODEX_COMPATIBILITY,
                "--today",
                "2026-10-17",
                "query",
                "recent",
                "--field",
                "created",
                "--since",
                "2026-10-16",
                "--limit",
                "500"
            ]
        );
        // An absolute executable's launch failure must be returned, never retried on PATH.
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error =
            run_nodex::<serde_json::Value>(&executable, root.path(), day, &["check"], false)
                .await
                .unwrap_err()
                .to_string();
        assert!(error.contains(executable.to_str().unwrap()));
        assert!(error.contains("no other executable was tried"));
        std::fs::remove_file(&executable).unwrap();
        assert!(
            run_nodex::<serde_json::Value>(&executable, root.path(), day, &["check"], false)
                .await
                .is_err()
        );
    }

    /// Opt-in contract exercise against separately installed real binaries, never PATH or a
    /// source checkout. The fixture and its graph are disposable; no vault is written.
    #[tokio::test]
    #[ignore = "requires LORE_TEST_NODEX_UNSUPPORTED and LORE_TEST_NODEX_SUPPORTED absolute binary paths"]
    async fn selected_real_binaries_preserve_compatibility_and_content_refusal() {
        let supported =
            PathBuf::from(std::env::var_os("LORE_TEST_NODEX_SUPPORTED").expect("supported binary"));
        let unsupported = PathBuf::from(
            std::env::var_os("LORE_TEST_NODEX_UNSUPPORTED").expect("unsupported binary"),
        );
        assert!(supported.is_absolute() && unsupported.is_absolute());
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("docs")).unwrap();
        std::fs::write(
            root.path().join("nodex.toml"),
            r#"[scope]
include = ["docs/**/*.md"]
[kinds]
allowed = ["generic"]
[statuses]
allowed = ["draft", "active", "superseded", "archived"]
terminal = ["superseded", "archived"]
initial = "draft"
[[identity.kind_rules]]
glob = "docs/**"
kind = "generic"
[[identity.id_rules]]
kind = "*"
template = "{kind}-{path_slug}"
"#,
        )
        .unwrap();
        let path = root.path().join("docs/a.md");
        let original = "---\r\nid: a\r\ntitle: Alpha\r\nkind: generic\r\nstatus: active\r\ncreated: 2026-10-17\r\nrelated: [b]\r\n---\r\n# Alpha\r\nBody.\r\n[Beta](b.md)\r\n[Beta again](b.md)\r\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(
            root.path().join("docs/b.md"),
            "---\nid: b\ntitle: Beta\nkind: generic\nstatus: active\n---\n# Beta\n",
        )
        .unwrap();
        let built = std::process::Command::new(&supported)
            .arg("-C")
            .arg(root.path())
            .arg("build")
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stdout)
        );
        let ctx = ExtractContext {
            target_date: "2026-10-17".parse().unwrap(),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::En,
            identity: Default::default(),
            vault_root: root.path().join("unused-vault"),
            vault_dirs: Default::default(),
            documents: None,
        };
        let selected = serde_json::json!({"repo":root.path(), "executable":supported, "reconcile":true, "repository_id":"fixture/repo", "document_ids":["a"]});
        let raw = NodexSource.extract(&selected, &ctx).await.unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(
            raw[0].metadata["source_record"]["observation_revision"]["scheme"],
            OBSERVATION_SCHEME
        );
        let relations = raw[0].metadata["source_record"]["relations"]
            .as_array()
            .unwrap();
        let body = relations
            .iter()
            .find(|edge| edge["relation"] == "references" && edge["target"] == "b")
            .unwrap();
        let references = body["references"].as_array().unwrap();
        assert_eq!(references.len(), 2);
        for reference in references {
            assert_eq!(reference["source"], "a");
            assert_eq!(reference["syntax"], "markdown");
            assert_eq!(reference["location"]["type"], "body");
        }
        let related = relations
            .iter()
            .find(|edge| edge["relation"] == "related" && edge["target"] == "b")
            .unwrap();
        assert_eq!(related["references"].as_array().unwrap().len(), 1);
        assert_eq!(
            related["references"][0]["location"],
            serde_json::json!({"type":"frontmatter","field":"related"})
        );
        let mut rejected = selected.clone();
        rejected["executable"] = serde_json::json!(unsupported);
        let error = NodexSource
            .extract(&rejected, &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("VERSION_MISMATCH"), "{error}");
        assert!(error.contains("params.executable") && error.contains(NODEX_COMPATIBILITY));
        // Same canonical body, different exact raw revision must still be refused.
        std::fs::write(&path, original.replace("\r\n", "\n")).unwrap();
        let error = NodexSource
            .extract(&selected, &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("GRAPH_OUTDATED"), "{error}");
        let rebuilt = std::process::Command::new(&supported)
            .arg("-C")
            .arg(root.path())
            .arg("build")
            .output()
            .unwrap();
        assert!(rebuilt.status.success());
        let canonical = NodexSource.extract(&selected, &ctx).await.unwrap();
        assert_eq!(
            raw[0].metadata["source_record"]["observation_revision"],
            canonical[0].metadata["source_record"]["observation_revision"],
            "rebuilt formatting-only changes are the same public observation, never a claim of equal raw bytes"
        );
        let config: serde_json::Value = run_nodex(
            &supported,
            root.path(),
            ctx.target_date,
            &["export", "config"],
            false,
        )
        .await
        .unwrap();
        let graph_path = root
            .path()
            .join(config["output"]["dir"].as_str().unwrap())
            .join("graph.json");
        let current_graph = std::fs::read(&graph_path).unwrap();
        let mut foreign_graph: serde_json::Value = serde_json::from_slice(&current_graph).unwrap();
        foreign_graph["schema_version"] =
            serde_json::json!(foreign_graph["schema_version"].as_u64().unwrap() - 1);
        foreign_graph["edges"] = serde_json::json!([{"source":"a","reference":null}]);
        let foreign_bytes = serde_json::to_vec(&foreign_graph).unwrap();
        std::fs::write(&graph_path, &foreign_bytes).unwrap();
        let error = NodexSource
            .extract(&selected, &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("GRAPH_OUTDATED"), "{error}");
        assert!(error.contains("nodex build --full"), "{error}");
        assert_eq!(std::fs::read(&graph_path).unwrap(), foreign_bytes);
        std::fs::write(&graph_path, &current_graph).unwrap();
        // A rebuilding writer can change body without changing inventory fields.
        // Exercise the entire first-read/final-requery loop, not only its comparison helper.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let wrapper = root.path().join("changing-nodex");
            let script = format!(
                r#"#!/bin/sh
{binary:?} "$@" > "$0.output"
rc=$?
case "$*" in
  *"query node a --with-body"*)
    if [ ! -f "$0.changed" ]; then
      touch "$0.changed"
      printf '\nChanged between selected reads.\n' >> {path:?}
      {binary:?} -C {repo:?} build >/dev/null || exit 2
    fi
    ;;
esac
cat "$0.output"
exit "$rc"
"#,
                binary = supported,
                path = path,
                repo = root.path()
            );
            std::fs::write(&wrapper, script).unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut changing = selected.clone();
            changing["executable"] = serde_json::json!(wrapper);
            let error = NodexSource
                .extract(&changing, &ctx)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("changed during nodex observation"),
                "{error}"
            );
        }
        assert!(!ctx.vault_root.exists());
    }

    fn complete_envelope() -> serde_json::Value {
        serde_json::json!({"ok":true,"data":{"items":[]},"snapshot":{"verification":"content","unbuildable":0,"unfollowed":0}})
    }

    #[test]
    fn a_current_snapshot_is_not_necessarily_a_complete_observation() {
        let clean = complete_envelope();
        assert!(
            decode_envelope::<serde_json::Value>(&serde_json::to_vec(&clean).unwrap(), true, true)
                .is_ok()
        );
        for (field, value) in [
            ("verification", serde_json::json!("unavailable")),
            ("verification", serde_json::json!("membership")),
            ("unbuildable", serde_json::json!(1)),
            ("unfollowed", serde_json::json!(1)),
            ("unfollowed", serde_json::Value::Null),
        ] {
            let mut e = clean.clone();
            e["snapshot"][field] = value;
            assert!(
                decode_envelope::<serde_json::Value>(&serde_json::to_vec(&e).unwrap(), true, true)
                    .is_err()
            );
        }
        // An unrelated check rule finding carries ok:true and exit 1, not an I/O error.
        let verdict = serde_json::json!({"ok":true,"data":{"has_errors":true,"violations":[]}});
        assert!(
            decode_envelope::<serde_json::Value>(
                &serde_json::to_vec(&verdict).unwrap(),
                true,
                false
            )
            .is_ok()
        );
        let mut missing = clean.clone();
        missing.as_object_mut().unwrap().remove("snapshot");
        assert!(
            decode_envelope::<serde_json::Value>(
                &serde_json::to_vec(&missing).unwrap(),
                true,
                true
            )
            .is_err()
        );
        for code in [
            "scope_coverage",
            "snapshot_divergence",
            "file_skipped",
            "binary_compat",
        ] {
            let mut e = clean.clone();
            e["warnings"] = serde_json::json!([{"code":code,"message":"gap"}]);
            assert!(
                decode_envelope::<serde_json::Value>(&serde_json::to_vec(&e).unwrap(), true, true)
                    .is_err()
            );
        }
    }

    #[test]
    fn reconciliation_is_explicit_and_requires_portable_namespace() {
        assert!(!is_reconciliation(&params(serde_json::json!({}))).unwrap());
        assert!(is_reconciliation(&params(serde_json::json!({"reconcile":true}))).is_err());
        assert!(is_reconciliation(&params(serde_json::json!({"reconcile":true,"repository_id":"org/repository","document_ids":["hub"]}))).unwrap());
    }

    fn observation() -> NodeDetail {
        NodeDetail {
            node: serde_json::json!({"id":"hub","title":"hub","kind":"learning","status":"archived","path":"docs/hub.md","inferred_fields":["title"]}),
            body: String::new(),
            outgoing: vec![],
        }
    }

    #[test]
    fn public_observation_binds_metadata_body_and_outgoing_without_raw_claims() {
        let root = tempfile::tempdir().unwrap();
        let mut detail = observation();
        detail.node["attrs"] = serde_json::json!({
            "prompt": "Compare the same SHA.\nKeep nonadditive missing values blank.",
            "cadence": "monthly",
            "thresholds": {"days": 14, "enabled": false},
            "llm_inputs": {"summary_done": "source declaration only"}
        });
        let relations = serde_json::json!([{
            "relation": "references",
            "target": "other",
            "references": [
                {"source":"hub","syntax":"markdown","target":"other.md",
                 "location":{"type":"body","start_line":2,"end_line":2}},
                {"source":"hub","syntax":"markdown","target":"other.md",
                 "location":{"type":"body","start_line":4,"end_line":4}}
            ]
        }]);
        detail.outgoing = serde_json::from_value(relations.clone()).unwrap();
        let mut selected = item("hub", "2026-10-17");
        selected.status = "archived".into();
        let ctx = ExtractContext {
            target_date: "2026-10-17".parse().unwrap(),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::En,
            identity: Default::default(),
            vault_root: root.path().to_owned(),
            vault_dirs: Default::default(),
            documents: None,
        };
        verify_selection(&selected, &detail.node, true).unwrap();
        let raw = build_document_observation(
            root.path(),
            &selected,
            None,
            &ctx,
            &detail,
            true,
            Some("org/repo"),
        )
        .unwrap();
        assert!(raw.body.is_empty());
        assert_eq!(raw.metadata["source_record"]["status"], "archived");
        assert_eq!(
            raw.metadata["source_record"]["attributes"],
            detail.node["attrs"]
        );
        assert_eq!(raw.metadata["source_record"]["relations"], relations);
        assert!(raw.metadata.get("llm_inputs").is_none());
        assert_eq!(raw.metadata["source_record"]["timestamp_basis"], "observed");
        assert_eq!(
            raw.metadata["source_record"]["observation_revision"]["scheme"],
            OBSERVATION_SCHEME
        );
        assert!(raw.metadata["source_record"].get("revision").is_none());
        assert!(
            raw.metadata["source_record"]
                .get("content_revision")
                .is_none()
        );
        assert_eq!(
            raw.metadata["source_identity"],
            serde_json::json!({"repository":"org/repo","document":"hub"})
        );
        assert!(
            !root.path().join("docs/hub.md").exists(),
            "collection never needs a second source-file read"
        );
        for changed in [
            NodeDetail {
                node: serde_json::json!({"id":"hub","attrs":{"a":1}}),
                ..detail.clone()
            },
            NodeDetail {
                body: "changed".into(),
                ..detail.clone()
            },
            NodeDetail {
                outgoing: vec![
                    serde_json::from_value(
                        serde_json::json!({"relation":"references","target":"other"}),
                    )
                    .unwrap(),
                ],
                ..detail.clone()
            },
        ] {
            assert!(
                verify_observation(
                    &selected,
                    &observation_revision(&detail).unwrap(),
                    &changed,
                    "",
                    root.path(),
                    None
                )
                .is_err()
            );
        }
        let mut moved_reference = detail.clone();
        moved_reference.outgoing[0]
            .metadata
            .get_mut("references")
            .unwrap()[1]["location"]["start_line"] = serde_json::json!(5);
        assert!(
            verify_observation(
                &selected,
                &observation_revision(&detail).unwrap(),
                &moved_reference,
                "",
                root.path(),
                None,
            )
            .is_err()
        );
        let value = serde_json::json!({"node":detail.node,"body":"","outgoing":[]});
        assert!(serde_json::from_value::<NodeDetail>(value.clone()).is_ok());
        for field in ["body", "outgoing"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<NodeDetail>(missing).is_err());
        }
        for field in ["id", "path", "title", "kind", "status"] {
            let mut node = detail.node.clone();
            node[field] = serde_json::json!("changed");
            assert!(verify_selection(&selected, &node, true).is_err());
        }
        assert!(verify_selection(&selected, &detail.node, false).is_err());
        let mut malformed = detail.node.clone();
        malformed["parse_issues"] = serde_json::json!([{"field":"created"}]);
        assert!(verify_selection(&selected, &malformed, true).is_err());
        for malformed in [
            serde_json::json!({"node":detail.node,"body":"","outgoing":[{"relation":"references"}]}),
            serde_json::json!({"node":detail.node,"body":null,"outgoing":[]}),
        ] {
            assert!(serde_json::from_value::<NodeDetail>(malformed).is_err());
        }
        let mut dated = detail.clone();
        dated.node["created"] = serde_json::json!("2026-09-01");
        let historical =
            build_document_observation(root.path(), &selected, None, &ctx, &dated, true, None)
                .unwrap();
        assert_eq!(
            historical.metadata["source_record"]["timestamp_basis"],
            "authored"
        );
        assert_eq!(
            historical
                .timestamp
                .to_zoned(ctx.timezone)
                .date()
                .to_string(),
            "2026-09-01"
        );
    }

    #[test]
    fn final_observation_rechecks_filesystem_dependent_link_projection() {
        let root = repo_with(&[]);
        let mut detail = observation();
        detail.body = "[target](target.md)".into();
        let selected = item("hub", "2026-10-17");
        let before = repoint_links(
            &detail.body,
            root.path(),
            &selected.path,
            Some("https://host/repo"),
        );
        verify_observation(
            &selected,
            &observation_revision(&detail).unwrap(),
            &detail,
            &before,
            root.path(),
            Some("https://host/repo"),
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("docs")).unwrap();
        std::fs::write(root.path().join("docs/target.md"), "target").unwrap();
        assert!(
            verify_observation(
                &selected,
                &observation_revision(&detail).unwrap(),
                &detail,
                &before,
                root.path(),
                Some("https://host/repo")
            )
            .is_err()
        );
    }

    fn params(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({ "repo": "/tmp/repo" });
        let (serde_json::Value::Object(base_map), serde_json::Value::Object(extra_map)) =
            (&mut base, extra)
        else {
            unreachable!("both are objects")
        };
        base_map.extend(extra_map);
        base
    }

    #[test]
    fn explicit_selection_is_complete_and_identity_exact() {
        let selected = select_documents(
            vec![
                item("a", "2026-10-14"),
                item("A", "2026-10-14"),
                item("b", "2026-10-14"),
            ],
            &["A".into()],
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "A");
        assert!(
            select_documents(
                vec![item("a", "2026-10-14")],
                &["a".into(), "missing".into()]
            )
            .is_err()
        );
        assert!(validate_params(&params(serde_json::json!({"document_ids": ["a", "a"]}))).is_err());
        assert!(validate_params(&params(serde_json::json!({"document_ids": [" "]}))).is_err());
    }

    /// A relative repo path reads whichever directory the process happened to start in — for a
    /// scheduled run, not the one the config names.
    #[test]
    fn a_relative_repo_path_is_refused() {
        assert!(validate_params(&serde_json::json!({ "repo": "../aix-platform" })).is_err());
        assert!(validate_params(&params(serde_json::json!({}))).is_ok());
    }

    /// Every cap in this workspace is validated `> 0`: a zero cap drops every document from
    /// the first one on, which is an entire day lost rather than a guard.
    #[test]
    fn a_zero_document_cap_is_refused() {
        assert!(validate_params(&params(serde_json::json!({ "max_documents": 0 }))).is_err());
    }

    /// `Path::join` discards its base when handed an absolute path and resolves `..` against
    /// whatever sits above the repository, so an answer naming either reads a file the
    /// repository does not contain — straight into a vault page and from there into concept
    /// extraction. The subprocess is a boundary like any other: what it says is checked, not
    /// trusted.
    #[test]
    fn a_path_that_leaves_the_repository_is_refused() {
        assert!(contained("docs/learnings/a.md").is_ok());
        assert!(contained("/etc/passwd").is_err());
        // Windows spells the same escape two more ways, and the one CI compiles is the one a
        // Unix test cannot make: `\one\two` is rooted with no prefix, which `is_absolute`
        // calls relative there while `join` still replaces everything but the drive.
        #[cfg(windows)]
        {
            assert!(contained(r"\one\two").is_err());
            assert!(contained(r"C:docs\a.md").is_err());
        }
        assert!(contained("../../.ssh/id_rsa").is_err());
        assert!(contained("docs/../../../etc/passwd").is_err());
        assert!(
            contained("docs/./a.md").is_ok(),
            "a no-op component is not an escape"
        );
    }

    fn item(id: &str, date: &str) -> RecentItem {
        RecentItem {
            id: id.into(),
            title: id.into(),
            kind: "learning".into(),
            status: "active".into(),
            path: format!("docs/{id}.md"),
            date: date.parse().unwrap(),
        }
    }

    /// A document source writes ONE page per document, so the two queries answering about the
    /// same document are two facts about one thing rather than two observations. Keeping both
    /// put one document on two pages — the second disambiguated by a content hash as though it
    /// were a different document that happened to share a title, with its prose, its concept
    /// extraction and its citations permanently split.
    #[test]
    fn a_document_edited_inside_its_own_window_is_one_observation() {
        let kept = latest_per_document(vec![
            item("learning-a", "2026-06-14"),
            item("learning-b", "2026-06-14"),
            item("learning-a", "2026-06-15"),
        ]);
        assert_eq!(
            kept.iter()
                .map(|i| (i.id.as_str(), i.date.to_string()))
                .collect::<Vec<_>>(),
            [
                ("learning-a", "2026-06-15".to_string()),
                ("learning-b", "2026-06-14".to_string()),
            ],
            "the newest declared date is the day the vault last saw the document change"
        );
    }

    /// The date must not be decided by how wide the window happened to be: a document written
    /// Sunday and edited Monday falls in one window on Monday and in two separate ones by
    /// Tuesday, and a backfill has to reproduce what the live run wrote.
    #[test]
    fn the_windows_width_does_not_decide_a_documents_date() {
        let both = latest_per_document(vec![
            item("learning-a", "2026-06-14"),
            item("learning-a", "2026-06-15"),
        ]);
        let later_only = latest_per_document(vec![item("learning-a", "2026-06-15")]);
        assert_eq!(both[0].date, later_only[0].date);
    }

    /// The query is bounded one day BELOW the window, so an answer whose oldest row is dated
    /// the window's first day proves nothing — nodex cannot return anything older than the
    /// bound, and a page cut in the middle of that day's documents looks identical to one that
    /// read them all. Only a row dated before the window is proof.
    #[test]
    fn stopping_inside_the_first_days_documents_is_not_spanning_the_window() {
        let first_day: jiff::civil::Date = "2026-06-13".parse().unwrap();
        let all_on_the_first_day: Vec<RecentItem> = (0..3)
            .map(|i| item(&format!("learning-{i}"), "2026-06-13"))
            .collect();
        assert!(
            !spans_window(&all_on_the_first_day, 3, first_day),
            "a full page holding only the first day's rows may have been cut inside it"
        );
        assert!(
            spans_window(&all_on_the_first_day, 4, first_day),
            "an answer short of what was asked for is the whole of what the repository holds"
        );
        assert!(spans_window(
            &[item("a", "2026-06-13"), item("b", "2026-06-12")],
            2,
            first_day
        ));
    }

    /// A rewritten destination is a link's destination and has to be written as one. A path
    /// holding a space is not a link at all, and one holding `)` ends the link early.
    #[test]
    fn a_rewritten_destination_is_encoded() {
        let repo = repo_with(&["docs/my file.md", "docs/a (1).md"]);
        let base = Some("https://host/r");
        assert_eq!(
            repoint_links("[s](my%20file.md)", repo.path(), "docs/a.md", base),
            "[s](https://host/r/docs/my%20file.md)"
        );
        assert_eq!(
            repoint_links("[p](a%20%281%29.md)", repo.path(), "docs/a.md", base),
            "[p](https://host/r/docs/a%20%281%29.md)"
        );
    }

    /// The silent one. `nodex` answers everything on or after `--since`, newest first, so a
    /// query that stops at its limit stops at the RECENT end — and a backfill of an older day
    /// came home holding only documents newer than the day it asked about, filtered every one
    /// of them out, and reported a day on which the project wrote nothing.
    #[test]
    fn an_answer_that_stops_before_the_window_is_asked_again() {
        let first_day = "2026-06-13".parse().unwrap();
        let newer_than_the_window = [item("a", "2026-09-12"), item("b", "2026-09-11")];
        assert!(
            !spans_window(&newer_than_the_window, 2, first_day),
            "a full answer whose oldest row is still newer than the window has not reached it"
        );
        assert!(
            spans_window(&newer_than_the_window, 3, first_day),
            "an answer short of what was asked for is the whole of what the repository holds"
        );
        assert!(
            spans_window(
                &[item("a", "2026-09-12"), item("b", "2026-06-12")],
                2,
                first_day
            ),
            "a row dated before the window is proof the window's own documents are behind it"
        );
        assert!(
            spans_window(&[], 500, first_day),
            "a repository declaring nothing since the cut-off is a quiet window, not a short read"
        );
    }

    fn repo_with(files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for f in files {
            let path = tmp.path().join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        tmp
    }

    /// A repository document cites its siblings by a relative path, which addresses nothing
    /// once the body is in the vault — `lore graph lint` reported exactly that as a broken link
    /// on a destination the vault never had any business resolving.
    #[test]
    fn a_link_the_repository_holds_is_repointed_at_the_repository() {
        let repo = repo_with(&["docs/learnings/b.md", "docs/decisions/x.md"]);
        let out = repoint_links(
            "see [b](b.md) and [x](../decisions/x.md#why)",
            repo.path(),
            "docs/learnings/a.md",
            Some("https://host/org/repo/blob/main/"),
        );
        assert_eq!(
            out,
            "see [b](https://host/org/repo/blob/main/docs/learnings/b.md) and \
             [x](https://host/org/repo/blob/main/docs/decisions/x.md#why)"
        );
    }

    /// Markdown resolves a relative path against the containing document, and this repository
    /// writes several from its ROOT instead — a form already broken in the repository's own
    /// renderer. Neither reading is guessed at: the file the repository HOLDS is what decides,
    /// and the format's own reading is tried first.
    #[test]
    fn a_root_relative_link_is_found_where_the_repository_actually_holds_it() {
        let repo = repo_with(&["docs/learnings/b.md"]);
        let base = Some("https://host/r");
        assert_eq!(
            repoint_links(
                "[b](docs/learnings/b.md)",
                repo.path(),
                "docs/learnings/a.md",
                base
            ),
            "[b](https://host/r/docs/learnings/b.md)"
        );
        assert_eq!(
            repoint_links(
                "[gone](nowhere.md)",
                repo.path(),
                "docs/learnings/a.md",
                base
            ),
            "gone",
            "a URL that 404s asserts a record that is not there"
        );
        assert_eq!(
            repoint_links("[b](b.md)", repo.path(), "docs/learnings/a.md", None),
            "b",
            "with no address to point at, a destination that resolves nowhere is worse than none"
        );
    }

    /// `Path::is_file` folds case on macOS and Windows, so a link whose casing differs from the
    /// stored name read as held and was rewritten to a URL a case-sensitive host answers 404 to
    /// — the one outcome this rewriting exists to avoid.
    #[test]
    fn a_link_whose_casing_differs_from_the_stored_name_is_not_held() {
        let repo = repo_with(&["docs/File.md"]);
        let base = Some("https://host/r");
        assert_eq!(
            repoint_links("[x](file.md)", repo.path(), "docs/a.md", base),
            "x"
        );
        assert_eq!(
            repoint_links("[x](File.md)", repo.path(), "docs/a.md", base),
            "[x](https://host/r/docs/File.md)"
        );
    }

    /// What must NOT be rewritten: a link that already names its own host, and one addressing
    /// the document itself.
    #[test]
    fn an_external_or_anchor_only_link_is_left_alone() {
        let repo = repo_with(&[]);
        let body = "[a](https://example.com/x) [b](#section)";
        assert_eq!(
            repoint_links(body, repo.path(), "docs/a.md", Some("https://host/r")),
            body
        );
        assert_eq!(repoint_links(body, repo.path(), "docs/a.md", None), body);
    }

    /// A document the query named and this could not read is a document that yielded nothing,
    /// whatever the reason — and a run that read NONE of them is an outage wearing the shape
    /// of a quiet day. The ingest log records one bit, so an empty success would let the
    /// source read fresh every morning while collecting nothing.
    #[test]
    fn reading_none_of_the_documents_the_query_named_is_an_error() {
        assert!(crate::require_any_observation("document", 0, 4).is_err());
        assert!(
            crate::require_any_observation("document", 0, 0).is_ok(),
            "a window the repository declared nothing for is a quiet day, not an outage"
        );
    }

    /// The refusal has to reach the caller as an error rather than an empty day: `lore health`
    /// reads a source's last collection as its only evidence it is alive, so a repository
    /// whose graph stopped building would otherwise report fresh forever.
    #[test]
    fn a_refusal_is_an_error_not_an_empty_day() {
        let envelope: Envelope<RecentData> = serde_json::from_str(
            r#"{"ok":false,"error":{"code":"VERSION_MISMATCH","message":"binary out of range"}}"#,
        )
        .unwrap();
        assert!(!envelope.ok);
        assert_eq!(envelope.error.unwrap().code, "VERSION_MISMATCH");
    }
}
