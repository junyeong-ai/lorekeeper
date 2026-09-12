//! A project's own document graph, read through `nodex`.
//!
//! A repository's ADRs, learnings and guides are knowledge, and copying them into the vault
//! would only give them a second home: the repository already stores, validates and searches
//! them. What the vault is for is the layer above — the concept a document names, which
//! accumulates evidence from every project and every feed that names it too. So this source
//! puts each day's documents on a daily page like any feed's articles, and the concept
//! extraction does the rest.
//!
//! `nodex` is what makes admission structural rather than a guess at a repository's layout.
//! It answers with a document's `kind`, its `status`, and the date it declares — the same
//! class of fact a Jira issue's `statusCategory` is — so a superseded decision is excluded
//! because it says it is superseded, not because a heuristic read its prose. A repository
//! that has not declared a document graph has no such answers, and it is not this source's
//! job to invent them: `/lore-extract` is the path for one, under a person's judgment.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

/// The only non-terminal status `nodex` defines: every other value it carries means the
/// document has been superseded or archived, and its knowledge lives in whatever replaced it.
/// Admitting both would put a decision and its reversal in the graph with nothing to tell a
/// reader which one still holds.
const ACTIVE: &str = "active";

pub struct NodexSource;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodexParams {
    /// Absolute path to the repository root holding `nodex.toml`. Absolute because a
    /// scheduled run starts in no particular directory, and a relative path would read a
    /// different repository — or none — depending on who launched it.
    repo: PathBuf,
    /// Document kinds to ingest, from the repository's own `kinds.allowed`. Empty admits
    /// every kind.
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    /// Per-run cap. A repository that adds more documents in one day than this warns rather
    /// than dropping them in silence.
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

/// One document, as `nodex query recent` reports it.
#[derive(Debug, Deserialize)]
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
struct Envelope {
    ok: bool,
    data: Option<RecentData>,
    error: Option<EnvelopeError>,
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
        Ok(())
    }
}

#[async_trait]
impl Source for NodexSource {
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

        // Asked per FIELD rather than through nodex's `any`, which also matches a review
        // stamp. A review is not an edit: the document is unchanged, so re-admitting it on the
        // day it was re-read would put a second copy of knowledge the vault already holds onto
        // a later page and run the extraction over it again. Two queries rather than one
        // answer plus a guess at which field nodex reports when several match.
        let mut items: Vec<RecentItem> = Vec::new();
        let mut capped = false;
        for field in ["created", "updated"] {
            let page = query_recent(&p.repo, field, first_day, last_day, p.max_documents).await?;
            // `--limit` is applied by the query, BEFORE this filters by status, kind and date,
            // so whether the cap bit is a question about what the query returned. Asking it of
            // the kept count instead can never be true while any document is filtered out,
            // which is the normal case — the warning would be silent exactly when documents
            // were dropped.
            capped |= page.len() == p.max_documents;
            items.extend(page);
        }
        // Stable, so a document matching both fields keeps its `created` row and is dated by
        // the day it was written. A document created before the window and edited inside it
        // keeps only its `updated` row and is dated by the edit — so an edited document
        // appears on the day it was written AND on the day it was changed, which is the same
        // "an edit re-enters the pipeline" rule a living Confluence page follows.
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.dedup_by(|a, b| a.id == b.id);
        let mut kept = Vec::new();
        // What the query said belongs to this window, after the declared fields decided it. A
        // document excluded by `status`, `kind` or its date was never attempted, so it cannot
        // stand for a read that failed — the count has to be taken here rather than from the
        // query's own length.
        let mut attempted = 0;
        for item in items {
            if item.status != ACTIVE
                || item.date < first_day
                || item.date > last_day
                || (!p.kinds.is_empty() && !p.kinds.iter().any(|k| k == &item.kind))
            {
                continue;
            }
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
            attempted += 1;
            match read_document(&p.repo, &item, p.base_url.as_deref(), ctx) {
                Ok(raw) => kept.push(raw),
                // One unreadable file must not cost the others their day: the repository is
                // the store, and a document this cannot read is still there to be read
                // tomorrow.
                Err(e) => tracing::warn!(
                    document = %item.id,
                    error = %e,
                    "nodex: skipping document (read failed)"
                ),
            }
        }
        // A repository whose working tree moved out from under the query answers with
        // documents and yields none of them. `lore health` reads the ingest log as its only
        // evidence a source is alive, and that log records one bit, so an empty success here
        // would let the source report fresh every morning while collecting nothing.
        crate::require_any_observation("document", kept.len(), attempted)?;

        if capped {
            tracing::warn!(
                cap = p.max_documents,
                "nodex: `max_documents` reached — raise it, or documents this day wrote are being dropped"
            );
        }
        Ok(kept)
    }
}

/// Ask the repository which documents declare a date in the window.
///
/// `--today` pins the clock so a backfill (`lore ingest --date <past>`) asks the same
/// question the day itself would have, and `--since` is a lower bound only — the upper one is
/// the caller's, against the same declared date this reports.
async fn query_recent(
    repo: &Path,
    field: &str,
    first_day: jiff::civil::Date,
    last_day: jiff::civil::Date,
    limit: usize,
) -> Result<Vec<RecentItem>, SourceError> {
    let output = tokio::process::Command::new("nodex")
        .arg("-C")
        .arg(repo)
        .arg("--today")
        .arg(last_day.to_string())
        .args(["query", "recent", "--field"])
        .arg(field)
        .arg("--since")
        .arg(first_day.to_string())
        .arg("--limit")
        .arg(limit.to_string())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            SourceError::Parse(format!(
                "running `nodex` for {}: {e} — the repository declares a document graph, so \
                 the binary that reads it has to be on PATH",
                repo.display()
            ))
        })?;

    let envelope: Envelope = serde_json::from_slice(&output.stdout).map_err(|e| {
        SourceError::Parse(format!(
            "nodex answered something other than its JSON envelope ({e}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })?;
    if !envelope.ok {
        let error = envelope.error.unwrap_or(EnvelopeError {
            code: "UNKNOWN".into(),
            message: "refused without naming a reason".into(),
        });
        return Err(SourceError::Parse(format!(
            "nodex refused ({}): {}",
            error.code,
            error.message.trim()
        )));
    }
    Ok(envelope.data.map(|d| d.items).unwrap_or_default())
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

/// Read one document's prose. The frontmatter is dropped: it is `nodex`'s own record of the
/// document, already read above, and carrying it through would put a second copy of every
/// field into the page an extraction reads.
fn read_document(
    repo: &Path,
    item: &RecentItem,
    base_url: Option<&str>,
    ctx: &ExtractContext,
) -> Result<RawItem, SourceError> {
    let path = repo.join(&item.path);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| SourceError::Parse(format!("read {}: {e}", path.display())))?;
    let body = lk_core::frontmatter::split_page(&content).body.to_string();

    Ok(RawItem {
        external_id: Some(item.id.clone()),
        title: item.title.clone(),
        body,
        url: base_url.map(|base| format!("{}/{}", base.trim_end_matches('/'), item.path)),
        timestamp: item
            .date
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
        open_work: None,
        metadata: serde_json::json!({
            "kind": item.kind,
            "path": item.path,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let envelope: Envelope = serde_json::from_str(
            r#"{"ok":false,"error":{"code":"VERSION_MISMATCH","message":"binary out of range"}}"#,
        )
        .unwrap();
        assert!(!envelope.ok);
        assert_eq!(envelope.error.unwrap().code, "VERSION_MISMATCH");
    }
}
