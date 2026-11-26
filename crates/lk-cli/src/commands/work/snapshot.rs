//! A synthesis request's context snapshot: the exact bytes of each context-only page (a
//! direct link or ranked discovery match) its Work read. A later preparation of the same
//! request reads the sections Lore owns on such a page from those bytes and everything else
//! as it is now, so a neighbor's resynthesis does not replan the request while an authored
//! correction is read at once. Its subject, citations and reviews stay current reads, and
//! live page bytes still guard publication. A page that joins the request's context is read
//! as it is now; one that leaves it is dropped at the next record.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const STORE: &str = ".lorekeeper/queue/context";
const SCHEMA: &str = "context-snapshot-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    task_id: String,
    cache_hash: String,
    #[serde(deserialize_with = "super::receipt::unique_entries")]
    pages: BTreeMap<String, String>,
}

impl Record {
    fn decode(raw: &str) -> miette::Result<Self> {
        let record: Self = super::receipt::decode(raw)
            .map_err(|error| miette::miette!("invalid context snapshot: {error}"))?;
        if record.schema != SCHEMA {
            return Err(miette::miette!(
                "unsupported context snapshot schema {}",
                record.schema
            ));
        }
        Ok(record)
    }
}

/// The context pages one synthesis request reads, by address: the stored bytes of a page the
/// snapshot holds, otherwise the bytes captured now.
#[derive(Clone)]
pub(super) struct ContextSnapshot {
    task_id: String,
    cache_hash: String,
    stored: Option<(String, BTreeMap<String, String>)>,
    pages: BTreeMap<String, String>,
}

impl ContextSnapshot {
    pub(super) fn load(
        root: &Path,
        read: &lk_vault::ReadControl,
        task_id: &str,
        cache_hash: &str,
    ) -> miette::Result<Self> {
        read.check().into_diagnostic()?;
        let stored = match read_optional(root, &path(task_id, cache_hash))? {
            Some(raw) => {
                let record = Record::decode(&raw)?;
                if record.task_id != task_id || record.cache_hash != cache_hash {
                    return Err(miette::miette!(
                        "context snapshot belongs to a different synthesis request"
                    ));
                }
                Some((raw, record.pages))
            }
            None => None,
        };
        Ok(Self {
            task_id: task_id.into(),
            cache_hash: cache_hash.into(),
            stored,
            pages: BTreeMap::new(),
        })
    }

    /// Binds the context pages captured now; on an address the snapshot holds, the sections
    /// Lore owns keep their recorded bodies.
    pub(super) fn bind(&mut self, captured: BTreeMap<String, String>) {
        let stored = self.stored.as_ref().map(|(_, pages)| pages);
        self.pages = captured
            .into_iter()
            .map(|(address, raw)| {
                let raw = match stored.and_then(|pages| pages.get(&address)) {
                    Some(stored) => compose(stored, &raw),
                    None => raw,
                };
                (address, raw)
            })
            .collect();
    }

    pub(super) fn page(&self, address: &str) -> Option<&str> {
        self.pages.get(address).map(String::as_str)
    }

    /// Guards the snapshot this request was read from, or its absence.
    pub(super) fn guard(&self, plan: &mut lk_vault::MutationPlan) {
        let path = path(&self.task_id, &self.cache_hash);
        match &self.stored {
            Some((raw, _)) => plan.require_unchanged(path, raw.clone()),
            None => plan.require_absent(path),
        }
    }

    /// Records the bound pages unless the stored snapshot already holds exactly them; the
    /// recorded snapshot is then the one later guards expect.
    pub(super) fn plan_record(
        &mut self,
        root: &Path,
        plan: &mut lk_vault::MutationPlan,
    ) -> miette::Result<bool> {
        if self
            .stored
            .as_ref()
            .is_some_and(|(_, pages)| pages == &self.pages)
        {
            return Ok(false);
        }
        let record = Record {
            schema: SCHEMA.into(),
            task_id: self.task_id.clone(),
            cache_hash: self.cache_hash.clone(),
            pages: self.pages.clone(),
        };
        let raw = serde_json::to_string(&record).into_diagnostic()?;
        plan.publish_expected(
            root,
            path(&self.task_id, &self.cache_hash),
            self.stored.as_ref().map(|(raw, _)| raw.clone()),
            raw.clone(),
        )
        .into_diagnostic()?;
        self.stored = Some((raw, record.pages));
        Ok(true)
    }
}

/// Removes the snapshots of requests no longer pending. Only regular JSON files are
/// snapshots; one that does not decode as a snapshot at its own address is kept and reported.
pub(super) fn plan_prune(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    let directory = lk_vault::page_path(root, Path::new(STORE)).into_diagnostic()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(miette::miette!("read context snapshots: {error}")),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        if !entry.file_type().into_diagnostic()?.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let relative = Path::new(STORE).join(entry.file_name());
        let raw = std::fs::read_to_string(entry.path());
        let record = match raw.as_deref().map(Record::decode) {
            Ok(Ok(record)) if relative == path(&record.task_id, &record.cache_hash) => record,
            _ => {
                eprintln!(
                    "preserving unrecognized context snapshot: {}",
                    entry.path().display()
                );
                continue;
            }
        };
        if pending.contains(&(record.task_id, record.cache_hash)) {
            continue;
        }
        plan.remove(relative, raw.into_diagnostic()?);
        removed += 1;
    }
    Ok(removed)
}

type SectionKey = fn(&'static lk_core::i18n::Strings) -> &'static str;

/// The sections Lore writes on a context page of `format`: a concept's synthesis, related
/// concepts and sources list, and a document's summary and related concepts. Their bodies are
/// derived and a later run rewrites them, so an edit there is not an authored correction.
/// Every section of any other page is the author's.
fn owned_sections(format: &str) -> &'static [SectionKey] {
    match format {
        "concept" => &[
            |s| s.concept_synthesis,
            |s| s.related,
            |s| s.concept_sources,
        ],
        "document" => &[|s| s.summary, |s| s.related_concepts],
        _ => &[],
    }
}

/// `current` with each section Lore owns on its page format taken byte for byte from
/// `stored`, where both pages declare that format and carry that section once, so a page
/// whose only changes are Lore's rewrites composes to `stored`. Frontmatter and every other
/// section stay current: an authored correction is read at once while a neighbor's rewrite of
/// its derived sections is not. A kept body takes the current page's line ending, and gains a
/// line break only where it would otherwise run into the current text around it.
fn compose(stored: &str, current: &str) -> String {
    let format = |raw: &str| {
        lk_core::frontmatter::parse_page(raw).ok().and_then(|page| {
            page.frontmatter
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
    };
    let Some(format) = format(current).filter(|current| format(stored).as_ref() == Some(current))
    else {
        return current.to_owned();
    };
    fn body<'a>(content: &'a str, heading: &str) -> Option<(std::ops::Range<usize>, &'a str)> {
        lk_vault::section_body(content, heading).map(|body| {
            let start = body.as_ptr() as usize - content.as_ptr() as usize;
            (start..start + body.len(), body)
        })
    }
    let mut spans = Vec::new();
    for section in owned_sections(&format) {
        for heading in lk_vault::section_headings(section) {
            if let (Some((_, old)), Some((range, new))) =
                (body(stored, heading), body(current, heading))
            {
                if old == new {
                    continue;
                }
                let crlf = current.contains("\r\n");
                let newline = if crlf { "\r\n" } else { "\n" };
                let mut kept = if crlf && !old.contains('\r') {
                    old.replace('\n', newline)
                } else {
                    old.to_owned()
                };
                if !kept.is_empty()
                    && !kept.starts_with(['\r', '\n'])
                    && !current[..range.start].ends_with('\n')
                {
                    kept.insert_str(0, newline);
                }
                if range.end < current.len() {
                    // An ATX heading starts at any line; another heading form after text needs a
                    // blank line or the text would join it.
                    let needed: usize =
                        if kept.trim().is_empty() || current[range.end..].starts_with('#') {
                            1
                        } else {
                            2
                        };
                    let present = kept[kept.trim_end().len()..].matches('\n').count();
                    kept.push_str(&newline.repeat(needed.saturating_sub(present)));
                }
                spans.push((range, kept));
            }
        }
    }
    spans.sort_by_key(|(range, _)| range.start);
    let mut composed = String::with_capacity(current.len());
    let mut cursor = 0;
    for (range, body) in spans {
        composed.push_str(&current[cursor..range.start]);
        composed.push_str(&body);
        cursor = range.end;
    }
    composed.push_str(&current[cursor..]);
    composed
}

fn path(task_id: &str, cache_hash: &str) -> PathBuf {
    let digest = lk_core::rss_original::digest(
        &serde_json::to_vec(&(task_id, cache_hash)).expect("identity serializes"),
    );
    Path::new(STORE).join(format!("{digest}.json"))
}

fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
    let full = lk_vault::page_path(root, path).into_diagnostic()?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(miette::miette!(
                "context snapshots must be direct regular files: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_to_string(full).map(Some).into_diagnostic()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(synthesis: &str, note: &str) -> String {
        format!(
            "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\n\n{synthesis}\n\n## Notes\n\n{note}\n\n## Related\n\n## Sources\n"
        )
    }

    #[test]
    fn only_sections_lore_owns_keep_their_recorded_bodies() {
        let stored = page("Limit is 5.", "Owner: platform.");
        assert_eq!(compose(&stored, &stored), stored);
        assert_eq!(
            compose(&stored, &page("Limit is 7.", "Owner: platform.")),
            stored
        );
        assert_eq!(
            compose(&stored, &page("Limit is 7.", "Owner: data.")),
            page("Limit is 5.", "Owner: data."),
            "an authored correction beside a rewrite is read as it is now"
        );
        let renamed = stored.replace("title: Neighbor", "title: Renamed");
        assert_eq!(compose(&stored, &renamed), renamed);
        assert_eq!(compose(&stored, "not a page"), "not a page");

        let exploration = |summary: &str| {
            format!("---\ntype: exploration\ntitle: Notes\n---\n## Summary\n\n{summary}\n")
        };
        assert_eq!(
            compose(&exploration("Limit is 5."), &exploration("Limit is 9.")),
            exploration("Limit is 9."),
            "another format's sections are its author's"
        );

        let at_end = "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\nLimit is 5.";
        let adjacent =
            "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\n## Notes\nOwner: data.\n";
        assert_eq!(
            compose(&page("Limit is 5.", "Owner: platform."), adjacent),
            "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\n\nLimit is 5.\n\n## Notes\nOwner: data.\n",
            "an emptied current section still separates its content from the next heading"
        );
        let crlf = adjacent.replace('\n', "\r\n");
        assert!(
            compose(&page("Limit is 5.", "Owner: platform."), &crlf)
                .contains("Limit is 5.\r\n\r\n## Notes"),
            "the page's own line ending separates them"
        );
        let extended = "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\nLimit is 7.\n\nNotes\n-----\nOwner: data.\n";
        assert_eq!(
            compose(at_end, extended),
            "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\nLimit is 5.\n\nNotes\n-----\nOwner: data.\n",
            "a following section is separated from the kept body"
        );
        let filled = page("Limit is 5.", "Owner: platform.").replace(
            "## Related\n\n## Sources",
            "## Related\n\nA neighbor's new related list.\n\n## Sources",
        );
        let stored = page("Limit is 5.", "Owner: platform.");
        assert_eq!(
            compose(&stored, &filled),
            stored,
            "a section a neighbor filled composes to its recorded empty body"
        );
        let tight = "---\ntype: concept\ntitle: Neighbor\n---\n## Synthesis\nLimit is 5.\n## Notes\nOwner: data.\n";
        assert_eq!(
            compose(tight, &tight.replace("Limit is 5.", "Limit is 7.")),
            tight,
            "a body that already ends its line before an ATX heading stays as recorded"
        );
        let indented = page("    let limit = 5;", "Owner: platform.");
        assert_eq!(
            compose(&indented, &page("let limit = 5;", "Owner: platform.")),
            indented,
            "significant indentation is part of the recorded body"
        );
    }

    #[test]
    fn a_recorded_snapshot_guards_its_bytes_and_retention_keeps_unrelated_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        let read = lk_vault::ReadControl::default();
        let neighbor = "wiki/concepts/neighbor.md";
        let first = page("Prior answer.", "Operational note.");
        let second = page("Resynthesized answer.", "Operational note.");
        let mut snapshot = ContextSnapshot::load(&root, &read, "task", "hash").unwrap();
        snapshot.bind(BTreeMap::from([(neighbor.into(), first.clone())]));
        let mut plan = lk_vault::MutationPlan::default();
        assert!(snapshot.plan_record(&root, &mut plan).unwrap());
        session.apply(plan).unwrap();
        let mut unchanged = lk_vault::MutationPlan::default();
        assert!(!snapshot.plan_record(&root, &mut unchanged).unwrap());

        let mut reloaded = ContextSnapshot::load(&root, &read, "task", "hash").unwrap();
        reloaded.bind(BTreeMap::from([(neighbor.into(), second)]));
        assert_eq!(reloaded.page(neighbor), Some(first.as_str()));

        let file = root.join(path("task", "hash"));
        let raw = std::fs::read_to_string(&file).unwrap();
        let forged = raw.replace(
            "\"pages\":{",
            &format!("\"pages\":{{\"{neighbor}\":\"forged\","),
        );
        std::fs::write(&file, &forged).unwrap();
        assert!(ContextSnapshot::load(&root, &read, "task", "hash").is_err());
        let mut guard = lk_vault::MutationPlan::default();
        snapshot.guard(&mut guard);
        assert!(session.apply(guard).is_err(), "a replaced snapshot refuses");
        std::fs::write(&file, &raw).unwrap();

        let store = root.join(STORE);
        std::fs::create_dir(store.join("directory.json")).unwrap();
        std::fs::write(store.join("interrupted.json.0.tmp"), [0xff]).unwrap();
        std::fs::write(store.join("unreadable.json"), [0xff]).unwrap();
        let pending = BTreeSet::from([("task".to_owned(), "hash".to_owned())]);
        let mut plan = lk_vault::MutationPlan::default();
        assert_eq!(plan_prune(&root, &pending, &mut plan).unwrap(), 0);
        assert_eq!(plan_prune(&root, &BTreeSet::new(), &mut plan).unwrap(), 1);
        session.apply(plan).unwrap();
        assert!(!file.exists());
        assert!(store.join("unreadable.json").exists());
    }
}
