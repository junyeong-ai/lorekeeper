use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    pub schema: String,
    pub task_id: String,
    pub cache_hash: String,
    pub context_revision: String,
    pub request_revision: String,
    pub reason: String,
}

pub(super) fn digest(value: &Value) -> miette::Result<String> {
    Ok(lk_core::concept::file_revision(
        &serde_json::to_string(value).into_diagnostic()?,
    ))
}

pub(super) struct Store {
    pub schema: &'static str,
    directory: &'static str,
}
pub(super) const SECTION: Store = Store {
    schema: "whole-section-action-v1",
    directory: ".lorekeeper/queue/section-actions",
};
pub(super) const GENERATION: Store = Store {
    schema: "native-result-action-v1",
    directory: ".lorekeeper/queue/result-actions",
};
impl Store {
    pub fn path(&self, task_id: &str, cache_hash: &str) -> PathBuf {
        let identity = serde_json::to_vec(&(task_id, cache_hash)).expect("identity serializes");
        PathBuf::from(self.directory).join(format!("{}.json", blake3::hash(&identity).to_hex()))
    }

    pub fn read(
        &self,
        root: &Path,
        task_id: &str,
        cache_hash: &str,
    ) -> miette::Result<Option<(String, Action)>> {
        let path = self.path(task_id, cache_hash);
        let absolute = lk_vault::page_path(root, &path).into_diagnostic()?;
        match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(miette::miette!(
                    "generation action must be a direct regular file"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).into_diagnostic(),
        }
        let raw = std::fs::read_to_string(absolute).into_diagnostic()?;
        let action = self.decode(&path, &raw)?;
        if action.task_id != task_id || action.cache_hash != cache_hash {
            return Err(miette::miette!("generation action belongs to another task"));
        }
        Ok(Some((raw, action)))
    }

    fn decode(&self, path: &Path, raw: &str) -> miette::Result<Action> {
        let action: Action = super::receipt::decode(raw).into_diagnostic()?;
        let revision = |value: &str, length: usize| {
            value.len() == length
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        };
        if action.schema != self.schema
            || action.task_id.trim().is_empty()
            || !revision(&action.cache_hash, 32)
            || path != self.path(&action.task_id, &action.cache_hash)
            || !revision(&action.context_revision, 64)
            || !revision(&action.request_revision, 64)
            || action.reason.trim().is_empty()
        {
            return Err(miette::miette!(
                "invalid generation action identity or scope"
            ));
        }
        Ok(action)
    }

    pub fn prune(
        &self,
        session: &lk_vault::MutationSession,
        pending: &std::collections::BTreeSet<(String, String)>,
    ) -> miette::Result<usize> {
        let directory = Path::new(self.directory);
        let absolute = lk_vault::page_path(session.root(), directory).into_diagnostic()?;
        match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(miette::miette!(
                    "generation action store must be a direct directory"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).into_diagnostic(),
        }
        let mut plan = lk_vault::MutationPlan::default();
        let mut removed = 0;
        for entry in std::fs::read_dir(absolute).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            if !entry.file_type().into_diagnostic()?.is_file() {
                return Err(miette::miette!("unknown generation action artifact"));
            }
            let path = directory.join(entry.file_name());
            let raw = std::fs::read_to_string(entry.path()).into_diagnostic()?;
            let action = self.decode(&path, &raw)?;
            if !pending.contains(&(action.task_id, action.cache_hash)) {
                plan.remove(path, raw);
                removed += 1;
            }
        }
        if removed != 0 {
            session.apply(plan).into_diagnostic()?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_generation_actions_require_unambiguous_objects() {
        for store in [SECTION, GENERATION] {
            let action = Action {
                schema: store.schema.into(),
                task_id: "task".into(),
                cache_hash: "a".repeat(32),
                context_revision: "b".repeat(64),
                request_revision: "c".repeat(64),
                reason: "An explicit review is required.".into(),
            };
            let path = store.path(&action.task_id, &action.cache_hash);
            let raw = serde_json::to_string(&action).unwrap();
            assert!(store.decode(&path, &raw).is_ok());
            for (field, value) in [
                ("schema", action.schema.as_str()),
                ("request_revision", action.request_revision.as_str()),
                ("reason", action.reason.as_str()),
            ] {
                let needle = format!("\"{field}\":{}", serde_json::to_string(value).unwrap());
                let duplicate = raw.replacen(&needle, &format!("\"{field}\":null,{needle}"), 1);
                assert!(store.decode(&path, &duplicate).is_err());
            }
            let array = serde_json::json!([
                action.schema,
                action.task_id,
                action.cache_hash,
                action.context_revision,
                action.request_revision,
                action.reason
            ]);
            assert!(store.decode(&path, &array.to_string()).is_err());
        }
    }
}
