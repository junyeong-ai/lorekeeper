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

/// Where growing stops and the source fails rather than reading further. Seven queries at
/// most, and a window holding this many documents is a `lookback_hours` the operator meant
/// differently — not something the adapter can settle by asking again.
const MAX_REACH: usize = 64_000;

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
        for field in ["created", "updated"] {
            items.extend(query_window(&p.repo, field, first_day, last_day).await?);
        }
        // What the declared fields say belongs to this window. A document excluded by
        // `status`, `kind` or its date was never attempted, so it cannot stand for a read that
        // failed — the count of attempts is taken from here rather than from what the query
        // returned, which reaches past the window on both sides.
        let admitted: Vec<RecentItem> = union_by_declared_date(items)
            .into_iter()
            .filter(|item| {
                item.status == ACTIVE
                    && (first_day..=last_day).contains(&item.date)
                    && (p.kinds.is_empty() || p.kinds.iter().any(|k| k == &item.kind))
            })
            .collect();
        // Refused whole rather than ingested in part. Which documents a partial run kept would
        // be decided by the order the index happened to list them, so the vault would differ
        // between two runs asking the same question — and the operator would have no way to
        // see that from the pages.
        if admitted.len() > p.max_documents {
            return Err(SourceError::InvalidParams(format!(
                "{first_day}..={last_day} holds {} documents, past `max_documents` ({}) — \
                 narrow `lookback_hours`, or raise the cap for a deliberate backfill",
                admitted.len(),
                p.max_documents
            )));
        }

        let attempted = admitted.len();
        let mut kept = Vec::new();
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
        Ok(kept)
    }
}

/// One observation per date a document DECLARES, which is what the two queries answer
/// separately: a document written inside the window and edited inside it later declares both
/// days and belongs to both, the same rule a living Confluence page follows when an edit
/// re-enters the pipeline. Only the same date reported by both queries is one observation.
///
/// Folding the answers onto the id instead would let the WINDOW'S WIDTH decide which day a
/// document lands on. A document written Sunday and edited Monday falls in one window on
/// Monday and in two separate ones by Tuesday, so the run that happened to see both rows
/// placed it on Sunday while the run a day later placed it on Monday — and a backfill of
/// Monday reproduced neither. `EventId` is `source:date:hash`, so two declared dates are
/// already two events downstream; nothing but this fold stood between them.
fn union_by_declared_date(mut items: Vec<RecentItem>) -> Vec<RecentItem> {
    items.sort_by(|a, b| a.id.cmp(&b.id).then(a.date.cmp(&b.date)));
    items.dedup_by(|a, b| a.id == b.id && a.date == b.date);
    items
}

/// Read the window whole, growing the query's reach until it spans the window.
///
/// The upper bound is the caller's: `nodex` answers everything on or after `--since`, newest
/// first, so a limit that stops before the window leaves the answer entirely outside it — and
/// the filter downstream then drops every row and reports a day on which nothing happened.
async fn query_window(
    repo: &Path,
    field: &str,
    first_day: jiff::civil::Date,
    last_day: jiff::civil::Date,
) -> Result<Vec<RecentItem>, SourceError> {
    let mut reach = INITIAL_REACH;
    loop {
        let page = query_recent(repo, field, first_day, last_day, reach).await?;
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

/// Does this answer reach back to the window's first day?
///
/// Either the repository ran out of documents — it answered with fewer than it was asked for —
/// or the oldest row it did answer with is at or before the day the window opens. An empty
/// answer is the first case. The oldest is taken by MINIMUM rather than from the last row,
/// so nothing here rests on the order `nodex` chose to list them in.
fn spans_window(page: &[RecentItem], reach: usize, first_day: jiff::civil::Date) -> bool {
    page.len() < reach
        || page
            .iter()
            .map(|item| item.date)
            .min()
            .is_none_or(|oldest| oldest <= first_day)
}

/// Ask the repository which documents declare a date in the window.
///
/// `--today` pins the clock so a backfill (`lore ingest --date <past>`) asks the same
/// question the day itself would have, and `--since` is a lower bound only — the upper one is
/// the caller's, against the same declared date this reports. `limit` is therefore how far
/// back this one call reaches and not how many documents the window holds; [`query_window`]
/// owns the difference.
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
            .find(|rel| repo.join(rel).is_file());
        match (held, base_url) {
            (Some(rel), Some(base)) => Some(format!(
                "[{text}]({}/{rel}{anchor})",
                base.trim_end_matches('/')
            )),
            _ => Some(text.to_owned()),
        }
    })
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
    let body = repoint_links(
        &lk_core::frontmatter::split_page(&content).body,
        repo,
        &item.path,
        base_url,
    );

    Ok(RawItem {
        // The repository's own vocabulary, carried so a reader can ask for decision records
        // across every project at once.
        labels: vec![item.kind.clone()],
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

    fn item(id: &str, date: &str) -> RecentItem {
        RecentItem {
            id: id.into(),
            title: id.into(),
            kind: "learning".into(),
            status: ACTIVE.into(),
            path: format!("docs/{id}.md"),
            date: date.parse().unwrap(),
        }
    }

    /// A document belongs to every day it declares. Folding the two queries onto the id made
    /// the answer depend on how wide the window happened to be: with both rows inside it the
    /// document landed on the day it was written, and with only the later row inside it — the
    /// same document, one day on — it landed on the day it was changed. A backfill of the
    /// edit's day therefore did not reproduce what the live run had written.
    #[test]
    fn a_document_belongs_to_every_day_it_declares() {
        let union = union_by_declared_date(vec![
            item("learning-a", "2026-06-14"),
            item("learning-b", "2026-06-14"),
            item("learning-a", "2026-06-15"),
        ]);
        assert_eq!(
            union
                .iter()
                .map(|i| (i.id.as_str(), i.date.to_string()))
                .collect::<Vec<_>>(),
            [
                ("learning-a", "2026-06-14".to_string()),
                ("learning-a", "2026-06-15".to_string()),
                ("learning-b", "2026-06-14".to_string()),
            ]
        );
    }

    /// The same date answered by both queries is one observation — a document written and
    /// edited on one day is one thing that happened, not two.
    #[test]
    fn one_date_answered_twice_is_one_observation() {
        let union = union_by_declared_date(vec![
            item("learning-a", "2026-06-14"),
            item("learning-a", "2026-06-14"),
        ]);
        assert_eq!(union.len(), 1);
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
        assert!(spans_window(
            &[item("a", "2026-09-12"), item("b", "2026-06-13")],
            2,
            first_day
        ));
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
        let envelope: Envelope = serde_json::from_str(
            r#"{"ok":false,"error":{"code":"VERSION_MISMATCH","message":"binary out of range"}}"#,
        )
        .unwrap();
        assert!(!envelope.ok);
        assert_eq!(envelope.error.unwrap().code, "VERSION_MISMATCH");
    }
}
