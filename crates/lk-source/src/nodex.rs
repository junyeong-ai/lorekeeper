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

        let items = query_recent(&p.repo, first_day, last_day, p.max_documents).await?;
        let mut kept = Vec::new();
        for item in items {
            if item.status != ACTIVE
                || item.date < first_day
                || item.date > last_day
                || (!p.kinds.is_empty() && !p.kinds.iter().any(|k| k == &item.kind))
            {
                continue;
            }
            match read_document(&p.repo, &item, p.base_url.as_deref(), ctx) {
                Ok(raw) => kept.push(raw),
                // One unreadable file must not cost the others their day: the repository is
                // the store, and a document this cannot read is still there to be read
                // tomorrow. A run that reached NOTHING is the case the caller's own
                // `require_any_observation` answers.
                Err(e) => tracing::warn!(
                    document = %item.id,
                    error = %e,
                    "nodex: skipping document (read failed)"
                ),
            }
        }

        if kept.len() == p.max_documents {
            tracing::warn!(
                cap = p.max_documents,
                "nodex: `max_documents` reached — raise it or narrow `kinds`, or documents are being dropped"
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
    first_day: jiff::civil::Date,
    last_day: jiff::civil::Date,
    limit: usize,
) -> Result<Vec<RecentItem>, SourceError> {
    let output = tokio::process::Command::new("nodex")
        .arg("-C")
        .arg(repo)
        .arg("--today")
        .arg(last_day.to_string())
        .args(["query", "recent", "--since"])
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
