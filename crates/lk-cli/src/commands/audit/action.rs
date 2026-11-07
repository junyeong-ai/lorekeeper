use std::io::Read;
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const DIRECTORY: &str = ".lorekeeper/audit-actions";
const SCHEMA: &str = "whole-audit-action-v1";
const MAX_ACTION_BYTES: usize = 1_048_576;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    path: String,
    context_revision: String,
    contract_revision: String,
    reason: String,
    recorded_at: i64,
}

#[cfg(unix)]
pub(in crate::commands) struct Binding<'a> {
    pub path: &'a str,
    pub context_revision: &'a str,
    pub contract_revision: &'a str,
}

#[derive(Clone)]
pub(in crate::commands) struct Snapshot {
    pub raw: String,
    receipt: Receipt,
}

#[cfg(unix)]
impl Snapshot {
    pub fn reason(&self, binding: &Binding<'_>) -> Option<&str> {
        (self.receipt.path == binding.path
            && self.receipt.context_revision == binding.context_revision
            && self.receipt.contract_revision == binding.contract_revision)
            .then_some(self.receipt.reason.as_str())
    }
}

fn path(subject: &str) -> PathBuf {
    Path::new(DIRECTORY).join(format!("{}.json", lk_core::concept::file_revision(subject)))
}

pub(in crate::commands) fn present(root: &Path) -> miette::Result<bool> {
    let directory = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(miette::miette!(
            "audit action store must be a direct directory"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn load(absolute: &Path) -> miette::Result<Option<String>> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        match rustix::fs::open(
            absolute,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => std::fs::File::from(descriptor),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error).into_diagnostic(),
        }
    };
    #[cfg(not(unix))]
    let file = {
        match std::fs::symlink_metadata(absolute) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(miette::miette!(
                    "audit action must be a direct regular file"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).into_diagnostic(),
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        options.open(absolute).into_diagnostic()?
    };
    let metadata = file.metadata().into_diagnostic()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(miette::miette!(
            "audit action must be a direct regular file"
        ));
    }
    let mut bytes = Vec::new();
    file.take((MAX_ACTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() > MAX_ACTION_BYTES {
        return Err(miette::miette!("audit action exceeds its storage bound"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| miette::miette!("audit action must be UTF-8"))
}

fn decode(root: &Path, raw: String) -> miette::Result<Snapshot> {
    let value = crate::commands::json::decode_object(raw.as_bytes())
        .map_err(|_| miette::miette!("audit action requires unique JSON fields"))?;
    let receipt: Receipt = serde_json::from_value(value)
        .map_err(|_| miette::miette!("invalid audit action fields"))?;
    let revision = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if receipt.schema != SCHEMA
        || receipt.path.is_empty()
        || !revision(&receipt.context_revision)
        || !revision(&receipt.contract_revision)
        || receipt.reason.trim().is_empty()
    {
        return Err(miette::miette!("invalid audit action identity or scope"));
    }
    lk_vault::page_path(root, Path::new(&receipt.path)).into_diagnostic()?;
    Ok(Snapshot { raw, receipt })
}

pub(in crate::commands) fn read(root: &Path, subject: &str) -> miette::Result<Option<Snapshot>> {
    let absolute = lk_vault::page_path(root, &path(subject)).into_diagnostic()?;
    let Some(raw) = load(&absolute)? else {
        return Ok(None);
    };
    let snapshot = decode(root, raw)?;
    if snapshot.receipt.path != subject {
        return Err(miette::miette!("audit action differs from its subject"));
    }
    Ok(Some(snapshot))
}

#[cfg(unix)]
fn receipt(binding: &Binding<'_>, reason: String, recorded_at: i64) -> Receipt {
    Receipt {
        schema: SCHEMA.into(),
        path: binding.path.into(),
        context_revision: binding.context_revision.into(),
        contract_revision: binding.contract_revision.into(),
        reason,
        recorded_at,
    }
}

#[cfg(unix)]
pub(in crate::commands) fn response_limit(binding: &Binding<'_>) -> miette::Result<usize> {
    let envelope = serde_json::to_vec(&receipt(binding, String::new(), i64::MIN))
        .into_diagnostic()?
        .len();
    MAX_ACTION_BYTES
        .checked_sub(envelope)
        .ok_or_else(|| miette::miette!("audit action identity exceeds its storage bound"))
}

#[cfg(unix)]
pub(in crate::commands) fn plan_hold(
    root: &Path,
    binding: Binding<'_>,
    reason: String,
    before: Option<&Snapshot>,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    if reason.trim().is_empty() {
        return Err(miette::miette!("audit action requires a nonblank reason"));
    }
    if let Some(before) = before
        && before.reason(&binding) == Some(reason.as_str())
    {
        mutation.require_unchanged(path(binding.path), before.raw.clone());
        return Ok(());
    }
    let receipt = receipt(&binding, reason, jiff::Timestamp::now().as_second());
    let raw = serde_json::to_string(&receipt).into_diagnostic()?;
    if raw.len() > MAX_ACTION_BYTES {
        return Err(miette::miette!("audit action exceeds its storage bound"));
    }
    mutation
        .publish_expected(
            root,
            path(binding.path),
            before.map(|snapshot| snapshot.raw.clone()),
            raw,
        )
        .into_diagnostic()
}

pub(in crate::commands) fn guard_completed(
    root: &Path,
    subject: &str,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    if let Some(snapshot) = read(root, subject)? {
        mutation.remove(path(subject), snapshot.raw);
    } else {
        mutation.require_absent(path(subject));
    }
    Ok(())
}

pub(in crate::commands) fn prune(
    root: &Path,
    session: Option<&lk_vault::MutationSession>,
    cutoff_secs: i64,
) -> miette::Result<usize> {
    if !present(root)? {
        return Ok(0);
    }
    let mut mutation = lk_vault::MutationPlan::default();
    let mut removed = 0;
    let directory = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    for entry in std::fs::read_dir(directory).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw =
            load(&entry.path())?.ok_or_else(|| miette::miette!("audit action disappeared"))?;
        let snapshot = decode(root, raw)?;
        let subject = snapshot.receipt.path.as_str();
        if entry.file_name() != path(subject).file_name().expect("hashed receipt name") {
            return Err(miette::miette!(
                "audit action filename differs from its identity"
            ));
        }
        if snapshot.receipt.recorded_at >= cutoff_secs {
            continue;
        }
        let target = lk_vault::page_path(root, Path::new(subject)).into_diagnostic()?;
        match std::fs::symlink_metadata(target) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).into_diagnostic(),
        }
        mutation.require_absent(subject);
        mutation.remove(path(subject), snapshot.raw);
        removed += 1;
    }
    if let Some(session) = session {
        session.apply(mutation).into_diagnostic()?;
    }
    Ok(removed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn holds_remain_readable_and_existing_subjects_survive_retention() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let subject = "wiki/documents/subject.md";
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        std::fs::write(root.join(subject), "Subject").unwrap();
        let revision = "a".repeat(64);
        let binding = Binding {
            path: subject,
            context_revision: &revision,
            contract_revision: &revision,
        };
        let limit = response_limit(&binding).unwrap();
        let reason = "x".repeat(limit);
        let raw = serde_json::to_string(&receipt(&binding, reason.clone(), 1)).unwrap();
        assert!(raw.len() <= MAX_ACTION_BYTES);
        std::fs::create_dir_all(root.join(DIRECTORY)).unwrap();
        std::fs::write(root.join(path(subject)), &raw).unwrap();
        let snapshot = read(&root, subject).unwrap().unwrap();
        assert_eq!(snapshot.reason(&binding), Some(reason.as_str()));
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        assert_eq!(prune(&root, Some(&session), 2).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(root.join(path(subject))).unwrap(),
            raw
        );
        std::fs::remove_file(root.join(subject)).unwrap();
        assert_eq!(prune(&root, None, 2).unwrap(), 1);
        assert!(root.join(path(subject)).exists());
        assert_eq!(prune(&root, Some(&session), 2).unwrap(), 1);
        assert!(!root.join(path(subject)).exists());
    }

    #[test]
    fn invalid_or_nonregular_actions_refuse_without_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let subject = "wiki/documents/subject.md";
        let action = root.join(path(subject));
        std::fs::create_dir_all(action.parent().unwrap()).unwrap();
        for bytes in [
            b"{}".to_vec(),
            vec![b'x'; MAX_ACTION_BYTES + 1],
            br#"{"schema":"a","schema":"b"}"#.to_vec(),
        ] {
            std::fs::write(&action, &bytes).unwrap();
            assert!(read(root, subject).is_err());
            assert_eq!(std::fs::read(&action).unwrap(), bytes);
        }
        std::fs::remove_file(&action).unwrap();
        let original = root.join("target");
        std::fs::write(&original, "{}").unwrap();
        std::os::unix::fs::symlink(&original, &action).unwrap();
        assert!(read(root, subject).is_err());
        std::fs::remove_file(&action).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&action)
                .status()
                .unwrap()
                .success()
        );
        assert!(read(root, subject).is_err());
        assert_eq!(std::fs::read_to_string(original).unwrap(), "{}");
    }
}
