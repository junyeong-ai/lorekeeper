//! The files the intent plane keeps beside the vault.
//!
//! Four stores grew here — the transition log, the proposal snapshots, the day's schedule, the
//! standing reminders — and each arrived with its own copy of the same four decisions: read a
//! JSONL file, treat an absent one as empty, refuse a line that will not parse, and replace the
//! whole file atomically. Four copies is four places for those answers to drift, and they had
//! already begun to: one store named the corrupt line, another named only the file.
//!
//! So the decisions live here once. What each store still owns is its SHAPE and its lifetime —
//! whether it is one file or a directory of them, whether a write replaces or accumulates, and
//! whether anything ever retires it. Those genuinely differ; the plumbing does not.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::TaskError;

/// One JSONL file: every line a record, the whole file replaced at once.
///
/// Replaced rather than appended even where a caller only adds, because these files are read by
/// a command that then acts on what it read — a torn append is a record half-present, and
/// `write_atomic` is the workspace's answer to that everywhere else.
pub(crate) struct Jsonl {
    path: PathBuf,
}

impl Jsonl {
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Every record, in the order the file holds them.
    ///
    /// An absent file is EMPTY rather than an error: a vault that has never run one of these
    /// paths has nothing to say, which is an answer. A line that will not parse is a hard error
    /// naming the file and the line, because the only writer replaces the file whole — so
    /// damage is external, and dropping the line would let the next write erase it for good.
    pub(crate) fn read<T: DeserializeOwned>(&self) -> Result<Vec<T>, TaskError> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::io(format!("read {}", self.path.display()), e)),
        };
        content
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|e| {
                    TaskError::Malformed(format!(
                        "{} is corrupt at line {}: {e} (left intact — recover or delete it)",
                        self.path.display(),
                        index + 1
                    ))
                })
            })
            .collect()
    }

    /// Replace the file with `rows`, creating the directory it belongs in.
    ///
    /// An empty `rows` writes an empty FILE rather than removing it: for a snapshot that is the
    /// answer which retires what the last run declared, and a caller that means "this is gone"
    /// says so with [`Self::retire`].
    pub(crate) fn replace<T: Serialize>(&self, rows: &[T]) -> Result<(), TaskError> {
        let dir = self.path.parent().ok_or_else(|| {
            TaskError::Malformed(format!("{} has no parent", self.path.display()))
        })?;
        std::fs::create_dir_all(dir)
            .map_err(|e| TaskError::io(format!("create {}", dir.display()), e))?;
        let mut buf = String::new();
        for row in rows {
            let line = serde_json::to_string(row)
                .map_err(|e| TaskError::Malformed(format!("serialize a record: {e}")))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        lk_core::fs::write_atomic(&self.path, buf.as_bytes(), None)
            .map_err(|e| TaskError::io(format!("write {}", self.path.display()), e))
    }

    /// Remove the file. An absent one is already retired.
    pub(crate) fn retire(&self) -> Result<(), TaskError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TaskError::io(format!("remove {}", self.path.display()), e)),
        }
    }
}

/// A directory of JSONL files, each named by a key.
///
/// The key is a date for the stores that record a day and a source id for the one that records
/// what a source declared. Both are enumerated the same way; what a caller does with the names
/// is where they differ.
pub(crate) struct Shelf {
    root: PathBuf,
}

impl Shelf {
    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn file(&self, key: &str) -> Jsonl {
        Jsonl::at(self.root.join(format!("{key}.jsonl")))
    }

    /// Every key the shelf holds, sorted, so a run over them is reproducible.
    ///
    /// A shelf that does not exist holds nothing. Anything that is not a `.jsonl` is skipped
    /// rather than refused: this directory sits inside the user's vault, where a sync client
    /// leaves conflict copies and an editor leaves swap files, and neither is this tool's to
    /// judge.
    pub(crate) fn keys(&self) -> Result<Vec<String>, TaskError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::io(format!("read {}", self.root.display()), e)),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| TaskError::io(format!("read {}", self.root.display()), e))?
                .path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                keys.push(stem.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Row {
        n: u32,
    }

    #[test]
    fn an_absent_file_is_empty_and_a_corrupt_one_names_its_line() {
        let tmp = tempfile::tempdir().unwrap();
        let file = Jsonl::at(tmp.path().join("x.jsonl"));
        assert!(file.read::<Row>().unwrap().is_empty());

        file.replace(&[Row { n: 1 }, Row { n: 2 }]).unwrap();
        assert_eq!(file.read::<Row>().unwrap(), [Row { n: 1 }, Row { n: 2 }]);

        std::fs::write(file.path(), "{\"n\":1}\nnot json\n").unwrap();
        let err = file.read::<Row>().unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("left intact"), "{err}");
    }

    /// The answer that retires what the last run declared. A store meaning "gone" removes the
    /// file instead, which is a different statement.
    #[test]
    fn an_empty_write_is_an_answer_and_retiring_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let file = Jsonl::at(tmp.path().join("x.jsonl"));
        file.replace(&[Row { n: 1 }]).unwrap();
        file.replace::<Row>(&[]).unwrap();
        assert!(file.path().exists());
        assert!(file.read::<Row>().unwrap().is_empty());

        file.retire().unwrap();
        assert!(!file.path().exists());
        file.retire().unwrap();
    }

    #[test]
    fn a_shelf_lists_only_its_own_files_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let shelf = Shelf::at(tmp.path().join("shelf"));
        assert!(shelf.keys().unwrap().is_empty());

        for key in ["2026-08-19", "2026-08-17", "jira"] {
            shelf.file(key).replace(&[Row { n: 1 }]).unwrap();
        }
        // What a sync client and an editor leave behind is not this tool's to judge.
        std::fs::write(shelf.file("x").path().with_extension("jsonl.tmp"), "").unwrap();
        std::fs::write(shelf.file("notes").path().with_extension("md"), "").unwrap();

        assert_eq!(shelf.keys().unwrap(), ["2026-08-17", "2026-08-19", "jira"]);
    }
}
