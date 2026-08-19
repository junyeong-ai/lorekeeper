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
    shelf: crate::store::Shelf,
}

impl Judged {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            shelf: crate::store::Shelf::at(
                vault_root
                    .join(".lorekeeper")
                    .join("proposals")
                    .join("judged"),
            ),
        }
    }

    /// Add one candidate to `date`'s file.
    pub fn add(&self, date: jiff::civil::Date, candidate: &Candidate) -> Result<(), TaskError> {
        let file = self.shelf.file(&date.to_string());
        let mut held: Vec<Candidate> = file.read()?;
        // One origin, one candidate: a session re-run over the same day must not stack a
        // second copy of what it already named.
        if held.iter().any(|existing| existing.url == candidate.url) {
            return Ok(());
        }
        held.push(candidate.clone());
        file.replace(&held)
    }

    /// Everything judged so far, with the files it came from so a caller can retire them.
    pub fn take(&self) -> Result<(Vec<Candidate>, Vec<PathBuf>), TaskError> {
        let mut all = Vec::new();
        let mut paths = Vec::new();
        for key in self.shelf.keys()? {
            let file = self.shelf.file(&key);
            all.extend(file.read::<Candidate>()?);
            paths.push(file.path().to_path_buf());
        }
        Ok((all, paths))
    }

    /// Drop the files a proposal run consumed.
    ///
    /// Called only after the board write has LANDED. A failure here re-reads them next time,
    /// where the board already holds what they name and nothing is proposed twice.
    pub fn retire(paths: &[PathBuf]) -> Result<(), TaskError> {
        for path in paths {
            crate::store::Jsonl::at(path.clone()).retire()?;
        }
        Ok(())
    }
}

/// The candidates nobody has answered about yet, one per origin.
///
/// The whole judgment of what to offer, as a pure function of three sets — so it is exhaustible
/// by tests rather than reachable only by running a command against a vault. `answered` is what
/// the history holds a completion or a drop for, `standing` is every origin the board already
/// carries however it got there, and what is left is work nobody has answered about.
///
/// One origin, ONE proposal. The two stores each dedup inside themselves, which is not the same
/// rule: a Jira issue linked from a mail arrives as two candidates from two stores and would
/// become two lines about one piece of work. A person answers about the WORK, and two decisions
/// with one right answer is one decision too many.
pub fn select(
    candidates: Vec<Candidate>,
    answered: &BTreeSet<String>,
    standing: &BTreeSet<String>,
) -> Vec<Candidate> {
    let mut seen = standing.clone();
    candidates
        .into_iter()
        .filter(|candidate| {
            let origin = candidate.origin();
            !answered.contains(&origin) && seen.insert(origin)
        })
        .collect()
}

/// The per-source record of what is still open.
///
/// A SNAPSHOT, replaced whole by each ingest, rather than a log appended to. Every source that
/// can declare open work re-fetches its whole window on demand, so what it did not declare this
/// time is no longer open — an issue closed in Jira simply stops being in the file, and stops
/// being proposed, with nothing to reconcile and no tombstone to write. A source not reached by
/// a run keeps the answer it last gave, which is the only honest one available.
pub struct Candidates {
    shelf: crate::store::Shelf,
}

impl Candidates {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            shelf: crate::store::Shelf::at(vault_root.join(".lorekeeper").join("proposals")),
        }
    }

    /// Replace `source_id`'s snapshot with what this run declared.
    ///
    /// Writes even when the list is EMPTY — that is the answer that retires yesterday's
    /// proposals for a source whose work is all finished, and skipping it would leave them
    /// standing forever.
    pub fn record(&self, source_id: &str, open: &[OpenWork]) -> Result<(), TaskError> {
        let rows: Vec<Candidate> = open
            .iter()
            .map(|work| Candidate {
                source_id: source_id.to_string(),
                summary: work.summary.clone(),
                url: work.url.clone(),
            })
            .collect();
        self.shelf.file(source_id).replace(&rows)
    }

    /// The snapshots of the sources `configured` names, in a stable order.
    ///
    /// Filtered by the configuration rather than by what the directory holds, because a source
    /// removed from `config.yaml` leaves its snapshot behind and would go on proposing work
    /// from a system this vault no longer reads. `lore maintenance` is what removes the file;
    /// this is what stops it mattering in the meantime.
    pub fn read_all(&self, configured: &[String]) -> Result<Vec<Candidate>, TaskError> {
        let mut all = Vec::new();
        for key in self.shelf.keys()? {
            if !configured.contains(&key) {
                continue;
            }
            all.extend(self.shelf.file(&key).read::<Candidate>()?);
        }
        Ok(all)
    }

    /// The snapshots no configured source answers for.
    pub fn orphans(&self, configured: &[String]) -> Result<Vec<PathBuf>, TaskError> {
        Ok(self
            .shelf
            .keys()?
            .into_iter()
            .filter(|key| !configured.contains(key))
            .map(|key| self.shelf.file(&key).path().to_path_buf())
            .collect())
    }

    pub fn retire(paths: &[PathBuf]) -> Result<(), TaskError> {
        for path in paths {
            crate::store::Jsonl::at(path.clone()).retire()?;
        }
        Ok(())
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
        assert_eq!(
            candidates
                .read_all(&["jira".into(), "mail".into()])
                .unwrap()
                .len(),
            2
        );

        // PLAT-1 was closed, so the next ingest simply does not declare it.
        candidates
            .record("jira", &[work("[PLAT-2] two", "https://j/browse/PLAT-2")])
            .unwrap();
        let held = candidates
            .read_all(&["jira".into(), "mail".into()])
            .unwrap();
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
        assert!(
            candidates
                .read_all(&["jira".into(), "mail".into()])
                .unwrap()
                .is_empty()
        );
    }

    fn candidate(source: &str, summary: &str, url: &str) -> Candidate {
        Candidate {
            source_id: source.into(),
            summary: summary.into(),
            url: url.into(),
        }
    }

    /// A Jira issue linked from a mail is one piece of work. The two stores each dedup inside
    /// themselves, which cannot see the other's.
    #[test]
    fn one_origin_seen_by_two_sources_is_one_proposal() {
        let offered = select(
            vec![
                candidate("jira", "[PLAT-1] one", "https://j/browse/PLAT-1"),
                candidate("mail", "Re: PLAT-1", "https://j/browse/PLAT-1"),
                candidate("jira", "[PLAT-2] two", "https://j/browse/PLAT-2"),
            ],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(
            offered
                .iter()
                .map(|c| c.summary.as_str())
                .collect::<Vec<_>>(),
            ["[PLAT-1] one", "[PLAT-2] two"],
            "the first to name an origin keeps it"
        );
    }

    /// Three things settle whether an observation has been dealt with, and a proposal needs no
    /// store of its own because of it.
    #[test]
    fn what_is_answered_or_standing_is_not_offered_again() {
        let one = candidate("jira", "[PLAT-1] one", "https://j/browse/PLAT-1");
        let two = candidate("jira", "[PLAT-2] two", "https://j/browse/PLAT-2");
        let answered: BTreeSet<String> = [one.origin()].into_iter().collect();
        let standing: BTreeSet<String> = [two.origin()].into_iter().collect();

        assert!(
            select(vec![one.clone(), two.clone()], &answered, &standing).is_empty(),
            "finished, dropped, or already on the board"
        );
        assert_eq!(
            select(vec![one, two], &BTreeSet::new(), &BTreeSet::new()).len(),
            2
        );
    }

    #[test]
    fn a_vault_that_has_never_ingested_has_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let configured = vec!["jira".to_string()];
        assert!(
            Candidates::new(tmp.path())
                .read_all(&configured)
                .unwrap()
                .is_empty()
        );
    }

    /// A source removed from `config.yaml` leaves its snapshot behind. Reading it would go on
    /// proposing work from a system this vault no longer ingests, so the configuration decides
    /// what is read and `orphans` names what may be removed.
    #[test]
    fn a_snapshot_no_configured_source_answers_for_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let candidates = Candidates::new(tmp.path());
        candidates
            .record("jira", &[work("[PLAT-1] one", "https://j/browse/PLAT-1")])
            .unwrap();
        candidates
            .record("gone", &[work("[OLD-1] two", "https://j/browse/OLD-1")])
            .unwrap();

        let configured = vec!["jira".to_string()];
        let held = candidates.read_all(&configured).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].source_id, "jira");

        let orphans = candidates.orphans(&configured).unwrap();
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].ends_with("gone.jsonl"));

        Candidates::retire(&orphans).unwrap();
        assert!(candidates.orphans(&configured).unwrap().is_empty());
    }
}
