//! Explicit ownership migration. A location selects evidence; exact projected bytes prove it.
use lk_core::{config::SourceType, frontmatter::parse_page};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(clap::Subcommand)]
pub enum IdentityCommand {
    /// Write a reviewable manifest; blocked entries remain explicit
    Plan {
        /// Configured nodex source with a stable repository_id
        #[arg(long)]
        source: String,
        /// New manifest path; existing files are refused
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Reobserve all inputs and refuse a changed manifest
    Check {
        /// Previously generated manifest to revalidate
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Archive evidence and bind verified entries without changing their bodies
    Apply {
        /// Previously generated manifest to revalidate and publish
        #[arg(long)]
        manifest: PathBuf,
        /// Validate the plan without archiving or changing vault pages
        #[arg(long)]
        dry_run: bool,
        /// Explicitly apply verified entries while leaving reported blocked entries untouched
        #[arg(long)]
        allow_blocked: bool,
    },
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    source: String,
    config_revision: String,
    snapshot: String,
    entries: Vec<Entry>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    file_revision: String,
    applied_revision: Option<String>,
    identity: Option<serde_json::Value>,
    source_record: Option<serde_json::Value>,
    projection_revision: Option<String>,
    blocked: Option<String>,
}
fn failure(error: impl std::fmt::Display) -> miette::Report {
    miette::miette!("{error}")
}
fn read(path: &Path) -> miette::Result<String> {
    std::fs::read_to_string(path).map_err(failure)
}
fn revision(raw: &str) -> String {
    lk_core::concept::file_revision(raw)
}

pub async fn run(opts: &super::GlobalOptions, command: IdentityCommand) -> miette::Result<()> {
    let config_path = super::find_config(opts)?;
    let config_raw = read(&config_path)?;
    let config = super::load_config(&config_path)?;
    verify_config(&config_path, &config_raw)?;
    let (path, applying, dry_run) = match &command {
        IdentityCommand::Plan { manifest, .. } | IdentityCommand::Check { manifest } => {
            (manifest, false, true)
        }
        IdentityCommand::Apply {
            manifest, dry_run, ..
        } => (manifest, true, *dry_run),
    };
    // Acquire before observing on apply; read-only commands never recover or lock.
    let session = if applying && !dry_run {
        Some(lk_vault::MutationSession::acquire(&config.vault.root_path()).map_err(failure)?)
    } else {
        None
    };
    let expected: Option<Manifest> = if matches!(command, IdentityCommand::Plan { .. }) {
        None
    } else {
        Some(serde_json::from_str(&read(path)?).map_err(failure)?)
    };
    let source = match &command {
        IdentityCommand::Plan { source, .. } => source.clone(),
        _ => expected.as_ref().unwrap().source.clone(),
    };
    let observed = observe(&config, &source, revision(&config_raw)).await?;
    verify_config(&config_path, &config_raw)?;
    if let Some(mut expected) = expected {
        verify_manifest(&mut expected, &observed, &config.vault.root_path())?;
    } else {
        use std::io::Write;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(failure)?;
        output
            .write_all(
                serde_json::to_string_pretty(&observed)
                    .map_err(failure)?
                    .as_bytes(),
            )
            .map_err(failure)?;
    }
    let verified = observed
        .entries
        .iter()
        .filter(|e| e.blocked.is_none())
        .count();
    if applying {
        authorize_subset(
            &observed,
            matches!(
                command,
                IdentityCommand::Apply {
                    allow_blocked: true,
                    ..
                }
            ),
        )?;
    }
    if let Some(session) = session {
        let plan = prepare_mutation(&session, &observed)?;
        // Archive publication may take time. Reobserve source, config and targets before commit.
        verify_config(&config_path, &config_raw)?;
        let refreshed = observe(&config, &source, revision(&config_raw)).await?;
        verify_config(&config_path, &config_raw)?;
        if refreshed != observed {
            return Err(failure("migration inputs changed while archiving"));
        }
        session.apply(plan).map_err(failure)?;
    }
    println!(
        "{}",
        serde_json::json!({"verified":verified,"blocked": observed.entries.len()-verified,"applied":applying && !dry_run,"manifest":path})
    );
    Ok(())
}

fn verify_config(path: &Path, expected: &str) -> miette::Result<()> {
    if read(path)? != expected {
        return Err(failure("configuration changed during migration"));
    }
    Ok(())
}
fn verify_manifest(
    expected: &mut Manifest,
    observed: &Manifest,
    root: &Path,
) -> miette::Result<()> {
    // A replay accepts only the exact bytes this plan would have written.
    expected.entries.retain(|entry| {
        entry.blocked.is_some()
            || entry.applied_revision.as_ref().is_none_or(|after| {
                read(&root.join(&entry.path)).map_or(true, |raw| revision(&raw) != *after)
            })
    });
    if expected != observed {
        return Err(failure(
            "identity manifest inputs changed; create and review a fresh plan",
        ));
    }
    Ok(())
}
fn authorize_subset(manifest: &Manifest, allow_blocked: bool) -> miette::Result<()> {
    if !allow_blocked && manifest.entries.iter().any(|entry| entry.blocked.is_some()) {
        return Err(failure(
            "manifest contains blocked entries; review them and pass --allow-blocked to apply only verified entries",
        ));
    }
    Ok(())
}
fn prepare_mutation(
    session: &lk_vault::MutationSession,
    observed: &Manifest,
) -> miette::Result<lk_vault::MutationPlan> {
    let root = session.root();
    let mut plan = lk_vault::MutationPlan::default();
    let mut addresses = Vec::new();
    for entry in observed.entries.iter().filter(|e| e.blocked.is_none()) {
        let raw = read(&root.join(&entry.path))?;
        if revision(&raw) != entry.file_revision {
            return Err(failure("target changed during migration"));
        }
        let after = lk_vault::set_frontmatter_field(
            &raw,
            "source_identity",
            &serde_json::to_string(entry.identity.as_ref().unwrap()).map_err(failure)?,
        )
        .ok_or_else(|| failure("cannot write source_identity"))?;
        plan.publish(root, &entry.path, after).map_err(failure)?;
        addresses.push(entry.path.clone());
    }
    let archive = lk_vault::archive_evidence(root, &addresses).map_err(failure)?;
    // The archive retains the first snapshot for an authored revision, so an existing
    // snapshot may have older bookkeeping. The report guards the current target bytes
    // captured for this migration; it does not claim a second, byte-identical backup.
    for entry in observed.entries.iter().filter(|e| e.blocked.is_none()) {
        if archive.file_revisions.get(&entry.path) != Some(&entry.file_revision) {
            return Err(failure("target changed during evidence capture"));
        }
    }
    Ok(plan)
}

async fn observe(
    config: &lk_core::config::Config,
    source_id: &str,
    config_revision: String,
) -> miette::Result<Manifest> {
    let source_config = config
        .sources
        .get(source_id)
        .ok_or_else(|| failure("unknown source"))?;
    if source_config.source_type != SourceType::Nodex {
        return Err(failure("identity migration requires a nodex source"));
    }
    let mut params = source_config.params.clone();
    if params
        .get("repository_id")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err(failure("configure repository_id before migration"));
    }
    params["reconcile"] = true.into();
    // Migration observes the complete declared collection rather than a time-window selection.
    params["document_ids"] = serde_json::json!([]);
    let repo = PathBuf::from(
        params["repo"]
            .as_str()
            .ok_or_else(|| failure("missing repo"))?,
    )
    .canonicalize()
    .map_err(failure)?;
    let root = config.vault.root_path().canonicalize().map_err(failure)?;
    let timezone = config.vault.timezone();
    let ctx = lk_source::ExtractContext {
        target_date: jiff::Timestamp::now().to_zoned(timezone.clone()).date(),
        timezone: timezone.clone(),
        locale: config.vault.locale(),
        identity: config.identity.clone(),
        vault_root: root.clone(),
    };
    let http = lk_source::build_http_client().map_err(failure)?;
    let adapter = lk_source::build_source(
        SourceType::Nodex,
        http,
        &Default::default(),
        &Default::default(),
        None,
    )
    .map_err(failure)?;
    let raw = adapter.extract(&params, &ctx).await.map_err(failure)?;
    let mut events = lk_pipeline::normalize_events(source_id, SourceType::Nodex, raw, &timezone);
    events.sort_by(|a, b| {
        a.metadata["source_file"]
            .as_str()
            .cmp(&b.metadata["source_file"].as_str())
    });
    let snapshot = revision(&serde_json::to_string(&events).map_err(failure)?);
    build_manifest(
        config,
        source_id,
        config_revision,
        snapshot,
        &root,
        &repo,
        events,
    )
}
fn build_manifest(
    config: &lk_core::config::Config,
    source_id: &str,
    config_revision: String,
    snapshot: String,
    root: &Path,
    repo: &Path,
    events: Vec<lk_core::event::Event>,
) -> miette::Result<Manifest> {
    let mut origins = BTreeMap::new();
    let mut source_identities = std::collections::BTreeSet::new();
    for event in events {
        if !source_identities.insert(event.metadata["source_identity"].to_string()) {
            return Err(failure("duplicate stable source identity"));
        }
        let location = event.metadata["source_file"]
            .as_str()
            .ok_or_else(|| failure("source omitted source_file"))?
            .to_owned();
        let canonical = Path::new(&location).canonicalize().map_err(failure)?;
        if !canonical.starts_with(repo) || canonical != Path::new(&location) {
            return Err(failure("source path is not canonical and contained"));
        }
        if origins.insert(location, event).is_some() {
            return Err(failure("duplicate source location"));
        }
    }
    let mut pages = Vec::new();
    let mut owners = BTreeMap::<String, Vec<String>>::new();
    let mut locations = BTreeMap::<String, usize>::new();
    let documents = root.join(lk_core::vault_path::documents_dir(&config.vault.dirs));
    for item in walkdir::WalkDir::new(&documents).follow_links(false) {
        let item = item.map_err(failure)?;
        if item.file_type().is_symlink() {
            return Err(failure("document collection contains a symlink"));
        }
        if !item.file_type().is_file() || item.path().extension().is_none_or(|e| e != "md") {
            continue;
        }
        let raw = read(item.path())?;
        let page = parse_page(&raw).map_err(failure)?;
        let path = item
            .path()
            .strip_prefix(root)
            .map_err(failure)?
            .to_string_lossy()
            .into_owned();
        if let Some(identity) = page
            .frontmatter
            .get("source_identity")
            .filter(|v| !v.is_null())
        {
            owners
                .entry(identity.to_string())
                .or_default()
                .push(path.clone());
        }
        if let Some(location) = page.frontmatter.get("source_file").and_then(|v| v.as_str()) {
            *locations.entry(location.to_owned()).or_default() += 1;
        }
        pages.push((path, raw, page));
    }
    let mut entries = Vec::new();
    for (path, raw, page) in pages {
        if page
            .frontmatter
            .get("source_identity")
            .is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let Some(location) = page.frontmatter.get("source_file").and_then(|v| v.as_str()) else {
            continue;
        };
        if !Path::new(location).starts_with(repo) {
            continue;
        }
        let mut entry = Entry {
            path,
            file_revision: revision(&raw),
            applied_revision: None,
            identity: None,
            source_record: None,
            projection_revision: None,
            blocked: None,
        };
        entry.blocked = if page.frontmatter.get("type").and_then(|v| v.as_str()) != Some("document")
        {
            Some("target does not declare document format".into())
        } else {
            match origins.get(location) {
                None => {
                    Some("no current source at exact location; historical proof required".into())
                }
                Some(event) => {
                    let identity = event.metadata["source_identity"].clone();
                    entry.identity = Some(identity.clone());
                    entry.source_record = Some(event.metadata["source_record"].clone());
                    entry.projection_revision = Some(revision(event.body.trim()));
                    if locations[location] != 1 || owners.contains_key(&identity.to_string()) {
                        Some("multiple legacy or stable owners".into())
                    } else if page
                        .frontmatter
                        .get("source_record")
                        .and_then(|r| r.get("id"))
                        .is_some_and(|id| id != &event.metadata["source_record"]["id"])
                    {
                        Some("source_record ID disagrees".into())
                    } else if !matches_projection(&raw, &event.body) {
                        Some(
                        "copied body differs from production projection; historical proof required"
                            .into(),
                    )
                    } else {
                        None
                    }
                }
            }
        };
        if entry.blocked.is_none() {
            let after = lk_vault::set_frontmatter_field(
                &raw,
                "source_identity",
                &serde_json::to_string(entry.identity.as_ref().unwrap()).map_err(failure)?,
            )
            .ok_or_else(|| failure("cannot construct identity binding"))?;
            entry.applied_revision = Some(revision(&after));
        }
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest {
        version: 1,
        source: source_id.into(),
        config_revision,
        snapshot,
        entries,
    })
}

fn matches_projection(raw: &str, projected: &str) -> bool {
    lk_vault::resolve_section(raw, |s| s.document_content)
        .is_some_and(|section| section.body.trim() == projected.trim())
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        temp: tempfile::TempDir,
        config: lk_core::config::Config,
        events: Vec<lk_core::event::Event>,
        target: PathBuf,
        original: String,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let repo = root.join("repo");
            std::fs::create_dir(&repo).unwrap();
            let source = repo.join("doc.md");
            std::fs::write(&source, "source bytes").unwrap();
            let config: lk_core::config::Config = serde_json::from_value(serde_json::json!({"vault":{"root":root},"identity":{"name":"test","email":"test@example.test"},"sources":{}})).unwrap();
            let projected = "#### Heading\n\n```md\n## literal heading\n```\n\n[link](https://example.test/doc.md)";
            let original = format!(
                "---\ntype: document\nsource_file: {}\nllm_inputs:\n  summary_done: historical\n---\n## Content\n\n{projected}\n\n## Summary\nRetained summary\n",
                serde_json::to_string(&source).unwrap()
            );
            let documents = root.join(lk_core::vault_path::documents_dir(&config.vault.dirs));
            std::fs::create_dir_all(&documents).unwrap();
            let target = documents.join("copy.md");
            std::fs::write(&target, &original).unwrap();
            let raw = lk_core::event::RawItem {
                external_id: Some("doc".into()),
                title: "Doc".into(),
                body: projected.into(),
                url: None,
                author: None,
                timestamp: "2026-10-17T00:00:00Z".parse().unwrap(),
                is_self: false,
                work: None,
                labels: vec![],
                metadata: serde_json::json!({"source_file":source,"source_identity":{"repository":"org/repo","document":"doc"},"source_record":{"id":"doc","status":"archived","content_revision":"source-hash"}}),
            };
            let events = lk_pipeline::normalize_events(
                "repo",
                SourceType::Nodex,
                vec![raw],
                &jiff::tz::TimeZone::UTC,
            );
            Self {
                temp,
                config,
                events,
                target,
                original,
            }
        }
        fn manifest(&self) -> Manifest {
            build_manifest(
                &self.config,
                "repo",
                "config".into(),
                revision(&serde_json::to_string(&self.events).unwrap()),
                &self.temp.path().canonicalize().unwrap(),
                &self.temp.path().canonicalize().unwrap().join("repo"),
                self.events.clone(),
            )
            .unwrap()
        }
        fn root(&self) -> PathBuf {
            self.temp.path().canonicalize().unwrap()
        }
    }
    fn clone_manifest(manifest: &Manifest) -> Manifest {
        serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap()
    }
    #[test]
    fn guarded_publication_archives_exact_bytes_keeps_blocked_and_replays() {
        let fixture = Fixture::new();
        let blocked = fixture.target.with_file_name("old.md");
        let blocked_raw = fixture
            .original
            .replace("doc.md", "removed.md")
            .replace("Retained summary", "Historical summary");
        std::fs::write(&blocked, &blocked_raw).unwrap();
        let manifest = fixture.manifest();
        assert_eq!(
            manifest
                .entries
                .iter()
                .filter(|e| e.blocked.is_some())
                .count(),
            1
        );
        assert!(authorize_subset(&manifest, false).is_err());
        authorize_subset(&manifest, true).unwrap();
        let session = lk_vault::MutationSession::acquire(&fixture.root()).unwrap();
        let plan = prepare_mutation(&session, &manifest).unwrap();
        assert_eq!(read(&fixture.target).unwrap(), fixture.original);
        let old_revision =
            lk_core::concept::source_revision(&parse_page(&fixture.original).unwrap());
        assert_eq!(
            read(
                &fixture
                    .root()
                    .join(format!(".lorekeeper/evidence/{old_revision}.md"))
            )
            .unwrap(),
            fixture.original
        );
        session.apply(plan).unwrap();
        assert_eq!(read(&blocked).unwrap(), blocked_raw);
        let after = read(&fixture.target).unwrap();
        assert_eq!(
            parse_page(&after).unwrap().body,
            parse_page(&fixture.original).unwrap().body
        );
        assert!(
            parse_page(&after)
                .unwrap()
                .frontmatter
                .get("source_record")
                .is_none()
        );
        let remaining = fixture.manifest();
        verify_manifest(&mut clone_manifest(&manifest), &remaining, &fixture.root()).unwrap();
        session
            .apply(prepare_mutation(&session, &remaining).unwrap())
            .unwrap();
        assert_eq!(read(&fixture.target).unwrap(), after);
    }
    #[test]
    fn source_target_and_config_drift_refuse_before_publication() {
        let mut fixture = Fixture::new();
        let manifest = fixture.manifest();
        fixture.events[0].metadata["source_record"]["status"] = "active".into();
        assert!(
            verify_manifest(
                &mut clone_manifest(&manifest),
                &fixture.manifest(),
                &fixture.root()
            )
            .is_err()
        );
        let mut wrong_config = clone_manifest(&manifest);
        wrong_config.config_revision = "changed".into();
        assert!(
            verify_manifest(
                &mut clone_manifest(&manifest),
                &wrong_config,
                &fixture.root()
            )
            .is_err()
        );
        std::fs::write(
            &fixture.target,
            fixture
                .original
                .replace("Retained summary", "Edited summary"),
        )
        .unwrap();
        assert!(
            verify_manifest(
                &mut clone_manifest(&manifest),
                &fixture.manifest(),
                &fixture.root()
            )
            .is_err()
        );
        let session = lk_vault::MutationSession::acquire(&fixture.root()).unwrap();
        assert!(prepare_mutation(&session, &manifest).is_err());
        assert!(!fixture.root().join(".lorekeeper/evidence").exists());
        let config_path = fixture.root().join("config.yaml");
        std::fs::write(&config_path, "new config").unwrap();
        assert!(verify_config(&config_path, "old config").is_err());
    }
    #[test]
    fn existing_authored_archive_keeps_older_bookkeeping_and_current_bytes_guard_publication() {
        let fixture = Fixture::new();
        let address = fixture
            .target
            .strip_prefix(fixture.root())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let archived = lk_vault::archive_evidence(&fixture.root(), &[address]).unwrap();
        let authored_revision = &archived.citations[0].revision;
        let archive_path = fixture
            .root()
            .join(format!(".lorekeeper/evidence/{authored_revision}.md"));
        let current = fixture
            .original
            .replace("summary_done: historical", "summary_done: current");
        assert_eq!(
            lk_core::concept::source_revision(&parse_page(&current).unwrap()),
            *authored_revision
        );
        std::fs::write(&fixture.target, &current).unwrap();
        let manifest = fixture.manifest();
        assert!(manifest.entries[0].blocked.is_none());
        assert_eq!(manifest.entries[0].file_revision, revision(&current));
        let session = lk_vault::MutationSession::acquire(&fixture.root()).unwrap();
        let plan = prepare_mutation(&session, &manifest).unwrap();
        assert_eq!(read(&archive_path).unwrap(), fixture.original);

        let changed = current.replace("summary_done: current", "summary_done: changed");
        std::fs::write(&fixture.target, &changed).unwrap();
        assert!(session.apply(plan).is_err());
        assert_eq!(read(&fixture.target).unwrap(), changed);

        std::fs::write(&fixture.target, &current).unwrap();
        session
            .apply(prepare_mutation(&session, &manifest).unwrap())
            .unwrap();
        let after = read(&fixture.target).unwrap();
        assert_eq!(
            parse_page(&after).unwrap().frontmatter.get("llm_inputs"),
            parse_page(&current).unwrap().frontmatter.get("llm_inputs")
        );
        assert_eq!(read(&archive_path).unwrap(), fixture.original);
        assert!(
            lk_vault::load_archived_evidence(&fixture.root(), authored_revision)
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn stable_and_legacy_collisions_and_conflicting_records_block() {
        let fixture = Fixture::new();
        let other = fixture.target.with_file_name("second.md");
        std::fs::write(&other, &fixture.original).unwrap();
        assert!(
            fixture
                .manifest()
                .entries
                .iter()
                .all(|e| e.blocked.is_some())
        );
        let stable = lk_vault::set_frontmatter_field(
            &fixture.original,
            "source_identity",
            r#"{"repository":"org/repo","document":"doc"}"#,
        )
        .unwrap();
        std::fs::write(&other, &stable).unwrap();
        assert!(
            fixture
                .manifest()
                .entries
                .iter()
                .all(|e| e.blocked.is_some())
        );
        std::fs::remove_file(other).unwrap();
        let conflicting = lk_vault::set_frontmatter_field(
            &fixture.original,
            "source_record",
            r#"{"id":"different"}"#,
        )
        .unwrap();
        std::fs::write(&fixture.target, conflicting).unwrap();
        assert!(
            fixture.manifest().entries[0]
                .blocked
                .as_ref()
                .unwrap()
                .contains("ID disagrees")
        );
    }
    #[test]
    fn proof_is_exact_and_fence_aware_across_locales() {
        let copied = "#### Heading\n\n```md\n## Example\n```\n\n[link](https://example.test/a.md)";
        for heading in ["Content", "내용"] {
            let raw = format!(
                "---\ntype: document\n---\n## {heading}\n\n{copied}\n\n## Summary\nretained"
            );
            assert!(matches_projection(&raw, copied));
            assert!(!matches_projection(&raw, &copied.replace("a.md", "b.md")));
            assert!(!matches_projection(
                &raw,
                &copied.replace("Heading", "Old Heading")
            ));
        }
        assert!(!matches_projection("authored aggregate", copied));
    }
    #[test]
    fn binding_preserves_body_and_semantic_markers() {
        let raw = "---\ntype: document\nllm_inputs:\n  summary_done: old\n---\n## Content\nold historical body\n";
        let after = lk_vault::set_frontmatter_field(
            raw,
            "source_identity",
            r#"{"repository":"repo","document":"doc"}"#,
        )
        .unwrap();
        assert_eq!(
            parse_page(raw).unwrap().body,
            parse_page(&after).unwrap().body
        );
        assert_eq!(
            parse_page(raw).unwrap().frontmatter.get("llm_inputs"),
            parse_page(&after).unwrap().frontmatter.get("llm_inputs")
        );
        assert!(
            parse_page(&after)
                .unwrap()
                .frontmatter
                .get("source_record")
                .is_none()
        );
    }
}
