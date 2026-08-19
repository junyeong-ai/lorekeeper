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

    /// Everything judged so far that `permitted` still names, with the files it came from.
    ///
    /// Filtered by the same opt-in the write is validated against, because a source dropped
    /// from `propose_from` — or from `config.yaml` outright — leaves judgments behind, and a
    /// file dated five months ago went on putting work from a system nobody reads onto the
    /// board. Validating only at the write answers for the moment it was made, not for the
    /// moment it is acted on.
    ///
    /// A file that will NOT read is reported and skipped rather than failing the whole run, and
    /// is left where it is: it holds judgments nobody has seen, so dropping them loses a
    /// proposal, while refusing everything blocks every other one for as long as the file sits
    /// there. Skipped, it is named on every run until a person deals with it — the same
    /// per-item isolation an adapter uses when one feed of many is broken.
    pub fn take(&self, permitted: &[String]) -> Result<Gathered, TaskError> {
        let mut all = Vec::new();
        let mut consumed = Vec::new();
        let mut unreadable = Vec::new();
        for key in self.shelf.keys()? {
            let file = self.shelf.file(&key);
            match file.read::<Candidate>() {
                Ok(held) => {
                    let (taken, left): (Vec<_>, Vec<_>) = held
                        .into_iter()
                        .partition(|candidate| permitted.contains(&candidate.source_id));
                    if !taken.is_empty() {
                        consumed.push(Consumed {
                            path: file.path().to_path_buf(),
                            left,
                        });
                    }
                    all.extend(taken);
                }
                Err(e) => unreadable.push(e),
            }
        }
        Ok(Gathered {
            candidates: all,
            consumed,
            unreadable,
        })
    }

    /// The judgments left by a source no CONFIGURED source answers for.
    ///
    /// The same rule the snapshots follow, applied a level down: a snapshot is keyed by source
    /// so an orphan is a whole file, while these are keyed by DATE and an orphan is a row. A
    /// source deleted from `config.yaml` leaves judgments nothing can ever act on; one merely
    /// dropped from `propose_from` is PAUSED, and its judgment waits on disk for the day it
    /// comes back.
    pub fn orphans(&self, configured: &[String]) -> Result<Vec<Consumed>, TaskError> {
        let mut orphaned = Vec::new();
        for key in self.shelf.keys()? {
            let file = self.shelf.file(&key);
            // A file that will not read is left exactly where it is, as everywhere here: it
            // holds judgments nobody has seen, and a janitor is the last thing that should be
            // deciding they are gone.
            let Ok(held) = file.read::<Candidate>() else {
                continue;
            };
            let left: Vec<Candidate> = held
                .iter()
                .filter(|candidate| configured.contains(&candidate.source_id))
                .cloned()
                .collect();
            if left.len() != held.len() {
                orphaned.push(Consumed {
                    path: file.path().to_path_buf(),
                    left,
                });
            }
        }
        Ok(orphaned)
    }

    /// Write back what each file still holds, removing the ones left holding nothing.
    ///
    /// Called only after the board write has LANDED. A failure here re-reads them next time,
    /// where the board already holds what they name and nothing is proposed twice.
    pub fn retire(consumed: &[Consumed]) -> Result<(), TaskError> {
        for file in consumed {
            let jsonl = crate::store::Jsonl::at(file.path.clone());
            if file.left.is_empty() {
                jsonl.retire()?;
            } else {
                jsonl.replace(&file.left)?;
            }
        }
        Ok(())
    }
}

/// What a read of the candidate stores yielded.
///
/// The unreadable files travel WITH the candidates rather than as an error that replaces them:
/// a store damaged in one place must not cost the rest their day, and the caller says what it
/// could not read while acting on what it could.
pub struct Gathered {
    pub candidates: Vec<Candidate>,
    /// Files a proposal took from, for a caller to settle once the write lands. Empty for a
    /// snapshot, which is re-declared rather than consumed.
    pub consumed: Vec<Consumed>,
    pub unreadable: Vec<TaskError>,
}

/// A judged file a proposal took from, and the judgments it still holds.
///
/// Retired whole ONLY when nothing is left. Keyed by DATE, one day's file holds every source's
/// judgments, so a file taken from partly is the ordinary shape the moment `propose_from` names
/// fewer sources than the session did — and retiring only a WHOLE file left the taken half on
/// disk with nothing recording that it had been offered, so it came back every morning: the
/// exact outcome consuming exists to prevent. Written back without them, a judgment the filter
/// passed over is still on disk for the day its source comes back.
pub struct Consumed {
    path: PathBuf,
    left: Vec<Candidate>,
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
    pub fn read_all(&self, configured: &[String]) -> Result<Gathered, TaskError> {
        let mut all = Vec::new();
        let mut unreadable = Vec::new();
        for key in self.shelf.keys()? {
            if !configured.contains(&key) {
                continue;
            }
            // Reported and skipped rather than fatal: a snapshot is rewritten whole by the next
            // ingest, so a damaged one heals on its own and refusing every other source's
            // proposals in the meantime buys nothing.
            match self.shelf.file(&key).read::<Candidate>() {
                Ok(held) => all.extend(held),
                Err(e) => unreadable.push(e),
            }
        }
        Ok(Gathered {
            candidates: all,
            consumed: Vec::new(),
            unreadable,
        })
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
                .candidates
                .len(),
            2
        );

        // PLAT-1 was closed, so the next ingest simply does not declare it.
        candidates
            .record("jira", &[work("[PLAT-2] two", "https://j/browse/PLAT-2")])
            .unwrap();
        let held = candidates
            .read_all(&["jira".into(), "mail".into()])
            .unwrap()
            .candidates;
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
                .candidates
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

    /// A judgment the filter passed over is left where it is — its source is paused rather than
    /// gone — while the one that WAS offered is written out of the file. Retiring only a WHOLE
    /// file left the offered half on disk with nothing recording that it had been offered, and
    /// one day's file holds every source's judgments, so a mixed file is the ordinary shape the
    /// moment `propose_from` names fewer sources than the session did.
    #[test]
    fn a_judgment_taken_is_written_out_and_one_passed_over_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let judged = Judged::new(tmp.path());
        let day = jiff::civil::date(2026, 8, 19);
        judged
            .add(day, &candidate("alpha", "one", "https://x/alpha-1"))
            .unwrap();
        judged
            .add(day, &candidate("beta", "two", "https://x/beta-2"))
            .unwrap();

        let mixed = judged.take(&["alpha".to_string()]).unwrap();
        assert_eq!(mixed.candidates.len(), 1);
        assert_eq!(mixed.consumed.len(), 1);
        Judged::retire(&mixed.consumed).unwrap();

        // Alpha's judgment was offered and is gone; beta's was never seen and waits on disk.
        let again = judged
            .take(&["alpha".to_string(), "beta".to_string()])
            .unwrap();
        assert_eq!(again.candidates.len(), 1);
        assert_eq!(again.candidates[0].source_id, "beta");
        Judged::retire(&again.consumed).unwrap();

        // And with nothing left, the file goes rather than sitting there empty forever.
        assert!(
            judged
                .take(&["beta".to_string()])
                .unwrap()
                .candidates
                .is_empty()
        );
        assert!(judged.shelf.keys().unwrap().is_empty());
    }

    #[test]
    fn a_vault_that_has_never_ingested_has_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let configured = vec!["jira".to_string()];
        assert!(
            Candidates::new(tmp.path())
                .read_all(&configured)
                .unwrap()
                .candidates
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
        let gathered = candidates.read_all(&configured).unwrap();
        assert!(gathered.unreadable.is_empty());
        let held = gathered.candidates;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].source_id, "jira");

        let orphans = candidates.orphans(&configured).unwrap();
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].ends_with("gone.jsonl"));

        Candidates::retire(&orphans).unwrap();
        assert!(candidates.orphans(&configured).unwrap().is_empty());
    }
}
