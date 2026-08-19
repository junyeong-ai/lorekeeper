//! What the sources say is still open, and how it reaches the board.
//!
//! The intent plane's first joint, which the second one has always implied: an observation may
//! PROPOSE a task and never creates one. An adapter answers "is this unfinished" from the field
//! its provider defines — never from prose — the ingest writes that answer down, and
//! `lore task propose` puts what has not been answered yet into the board's proposed section.
//! Only a person moves it out.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lk_core::event::OpenWork;
use serde::{Deserialize, Serialize};

use crate::TaskError;

/// One source's open work, as of the last ingest that reached it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Candidate {
    /// Which source declared it, so a proposal can say where it came from.
    pub source_id: String,
    pub summary: String,
    pub url: String,
}

impl Candidate {
    /// What this proposal answers to. See [`lk_core::origin`].
    pub fn origin(&self) -> String {
        lk_core::origin::identity(&self.url)
    }
}

/// Candidates a JUDGMENT produced, which are OBSERVED once rather than re-declared.
///
/// A source that re-fetches its window can say what is still open every morning, so its answer
/// is a snapshot and a closed issue simply stops appearing. An LLM session reading one day's
/// page cannot do that — it saw one day, and tomorrow it sees a different one. So these are
/// events, and `lore task propose` CONSUMES them: proposed once, and from then on the board
/// says it is open and the history says it was answered. Left to accumulate they would be
/// re-read forever, and a proposal deleted in an editor in March would return every day after.
pub struct Judged {
    root: PathBuf,
}

impl Judged {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            root: vault_root
                .join(".lorekeeper")
                .join("proposals")
                .join("judged"),
        }
    }

    /// Add one candidate to `date`'s file.
    pub fn add(&self, date: jiff::civil::Date, candidate: &Candidate) -> Result<(), TaskError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| TaskError::io(format!("create {}", self.root.display()), e))?;
        let path = self.root.join(format!("{date}.jsonl"));
        let mut held = self.read(&path)?;
        // One origin, one candidate: a session re-run over the same day must not stack a
        // second copy of what it already named.
        if held.iter().any(|existing| existing.url == candidate.url) {
            return Ok(());
        }
        held.push(candidate.clone());
        let mut buf = String::new();
        for candidate in &held {
            let line = serde_json::to_string(candidate)
                .map_err(|e| TaskError::Malformed(format!("serialize candidate: {e}")))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        lk_core::fs::write_atomic(&path, buf.as_bytes(), None)
            .map_err(|e| TaskError::io(format!("write {}", path.display()), e))
    }

    fn read(&self, path: &Path) -> Result<Vec<Candidate>, TaskError> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::io(format!("read {}", path.display()), e)),
        };
        content
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|e| {
                    TaskError::Malformed(format!(
                        "{} is corrupt at line {}: {e} (left intact — recover or delete it)",
                        path.display(),
                        index + 1
                    ))
                })
            })
            .collect()
    }

    /// Everything judged so far, with the files it came from so a caller can retire them.
    pub fn take(&self) -> Result<(Vec<Candidate>, Vec<PathBuf>), TaskError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(e) => return Err(TaskError::io(format!("read {}", self.root.display()), e)),
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| TaskError::io(format!("read {}", self.root.display()), e))?
                .path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                paths.push(path);
            }
        }
        paths.sort();
        let mut all = Vec::new();
        for path in &paths {
            all.extend(self.read(path)?);
        }
        Ok((all, paths))
    }

    /// Drop the files a proposal run consumed.
    ///
    /// Called only after the board write has LANDED. A failure here re-reads them next time,
    /// where the board already holds what they name and nothing is proposed twice.
    pub fn retire(paths: &[PathBuf]) -> Result<(), TaskError> {
        for path in paths {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(TaskError::io(format!("remove {}", path.display()), e)),
            }
        }
        Ok(())
    }
}

/// The per-source record of what is still open.
///
/// A SNAPSHOT, replaced whole by each ingest, rather than a log appended to. Every source that
/// can declare open work re-fetches its whole window on demand, so what it did not declare this
/// time is no longer open — an issue closed in Jira simply stops being in the file, and stops
/// being proposed, with nothing to reconcile and no tombstone to write. A source not reached by
/// a run keeps the answer it last gave, which is the only honest one available.
pub struct Candidates {
    root: PathBuf,
}

impl Candidates {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            root: vault_root.join(".lorekeeper").join("proposals"),
        }
    }

    fn path(&self, source_id: &str) -> PathBuf {
        self.root.join(format!("{source_id}.jsonl"))
    }

    /// Replace `source_id`'s snapshot with what this run declared.
    ///
    /// Writes even when the list is EMPTY — that is the answer that retires yesterday's
    /// proposals for a source whose work is all finished, and skipping it would leave them
    /// standing forever.
    pub fn record(&self, source_id: &str, open: &[OpenWork]) -> Result<(), TaskError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| TaskError::io(format!("create {}", self.root.display()), e))?;
        let mut buf = String::new();
        for work in open {
            let line = serde_json::to_string(&Candidate {
                source_id: source_id.to_string(),
                summary: work.summary.clone(),
                url: work.url.clone(),
            })
            .map_err(|e| TaskError::Malformed(format!("serialize candidate: {e}")))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        let path = self.path(source_id);
        lk_core::fs::write_atomic(&path, buf.as_bytes(), None)
            .map_err(|e| TaskError::io(format!("write {}", path.display()), e))
    }

    /// Every source's open work, in a stable order.
    ///
    /// Ordered by source id and then by the order the source declared them, so a run that
    /// proposes several produces the same board twice. An unreadable line is a hard error, as
    /// in the transition log and for the same reason: the writer only ever produces this file
    /// through an atomic replace, so damage is external.
    pub fn read_all(&self) -> Result<Vec<Candidate>, TaskError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::io(format!("read {}", self.root.display()), e)),
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| TaskError::io(format!("read {}", self.root.display()), e))?
                .path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut all = Vec::new();
        let mut seen = BTreeSet::new();
        for path in paths {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| TaskError::io(format!("read {}", path.display()), e))?;
            for (index, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let candidate: Candidate = serde_json::from_str(line).map_err(|e| {
                    TaskError::Malformed(format!(
                        "{} is corrupt at line {}: {e} (left intact — recover or delete it)",
                        path.display(),
                        index + 1
                    ))
                })?;
                // One origin, one proposal, however many sources observed it: a Jira issue
                // linked from a mail is one piece of work, and two lines for it would be two
                // things to answer about one.
                if seen.insert(candidate.origin()) {
                    all.push(candidate);
                }
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(summary: &str, url: &str) -> OpenWork {
        OpenWork {
            summary: summary.into(),
            url: url.into(),
        }
    }

    #[test]
    fn a_snapshot_is_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let candidates = Candidates::new(tmp.path());

        candidates
            .record(
                "jira",
                &[
                    work("[PLAT-1] one", "https://j/browse/PLAT-1"),
                    work("[PLAT-2] two", "https://j/browse/PLAT-2"),
                ],
            )
            .unwrap();
        assert_eq!(candidates.read_all().unwrap().len(), 2);

        // PLAT-1 was closed, so the next ingest simply does not declare it.
        candidates
            .record("jira", &[work("[PLAT-2] two", "https://j/browse/PLAT-2")])
            .unwrap();
        let held = candidates.read_all().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].summary, "[PLAT-2] two");
    }

    /// The answer that retires every proposal a source had standing. Skipping the write would
    /// leave them there forever.
    #[test]
    fn a_source_with_nothing_open_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let candidates = Candidates::new(tmp.path());
        candidates
            .record("jira", &[work("[PLAT-1] one", "https://j/browse/PLAT-1")])
            .unwrap();
        candidates.record("jira", &[]).unwrap();
        assert!(candidates.read_all().unwrap().is_empty());
    }

    #[test]
    fn one_origin_seen_by_two_sources_is_one_proposal() {
        let tmp = tempfile::tempdir().unwrap();
        let candidates = Candidates::new(tmp.path());
        candidates
            .record("jira", &[work("[PLAT-1] one", "https://j/browse/PLAT-1")])
            .unwrap();
        candidates
            .record("mail", &[work("Re: PLAT-1", "https://j/browse/PLAT-1")])
            .unwrap();

        let held = candidates.read_all().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(
            held[0].source_id, "jira",
            "the first source in a stable order"
        );
    }

    #[test]
    fn a_vault_that_has_never_ingested_has_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Candidates::new(tmp.path()).read_all().unwrap().is_empty());
    }
}
