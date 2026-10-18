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
        /// Audited immutable Git commit (full SHA-1); repeat to prove historical copies
        #[arg(long)]
        history_ref: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history_refs: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    historical_proof: Option<HistoricalProof>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoricalProof {
    commit: String,
    path: String,
    blob: String,
    document: String,
    projection_revision: String,
}
struct HistoricalTree {
    commit: String,
    blobs: BTreeMap<String, String>,
}
fn git(repo: &Path, args: &[&str]) -> miette::Result<String> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(failure)?;
    if !output.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(failure)
}
fn github_repository(remote: &str) -> Option<String> {
    let path = remote
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| remote.trim().strip_prefix("git@github.com:"))
        .or_else(|| remote.trim().strip_prefix("ssh://git@github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.chars().any(char::is_whitespace))
    {
        return None;
    }
    Some(format!("github.com/{path}"))
}
fn historical_trees(
    repo: &Path,
    repository: &str,
    refs: &[String],
) -> miette::Result<Vec<HistoricalTree>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    if github_repository(&git(repo, &["remote", "get-url", "origin"])?).as_deref()
        != Some(repository)
    {
        return Err(failure(
            "historical proof requires origin to match configured repository_id",
        ));
    }
    let mut trees = Vec::new();
    for commit in refs {
        if commit.len() != 40
            || !commit
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(failure(
                "history-ref must be a full lowercase 40-character commit ID",
            ));
        }
        if git(repo, &["cat-file", "-t", commit])?.trim() != "commit" {
            return Err(failure("history-ref is not a commit object"));
        }
        let raw = git(repo, &["ls-tree", "-r", "-z", commit])?;
        let mut blobs = BTreeMap::new();
        for entry in raw.split('\0').filter(|s| !s.is_empty()) {
            let (meta, path) = entry
                .split_once('\t')
                .ok_or_else(|| failure("invalid git tree entry"))?;
            let fields: Vec<_> = meta.split_whitespace().collect();
            if fields.len() == 3 && matches!(fields[0], "100644" | "100755") && fields[1] == "blob"
            {
                blobs.insert(path.to_owned(), fields[2].to_owned());
            }
        }
        trees.push(HistoricalTree {
            commit: commit.clone(),
            blobs,
        });
    }
    Ok(trees)
}
fn historical_match(
    repo: &Path,
    trees: &[HistoricalTree],
    path: &str,
    target: &str,
    base_url: Option<&str>,
) -> miette::Result<Result<Option<HistoricalProof>, String>> {
    use std::path::Component;
    if Path::new(path)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
        || path.is_empty()
    {
        return Err(failure(
            "historical source path must be a contained normal relative path",
        ));
    }
    let mut matches = Vec::new();
    let mut bodies = BTreeMap::new();
    for tree in trees {
        let Some(blob) = tree.blobs.get(path) else {
            continue;
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = bodies.entry(blob.clone()) {
            entry.insert(git(repo, &["cat-file", "blob", blob])?);
        }
        let page = parse_page(&bodies[blob]).map_err(failure)?;
        // Historical identity is authored, never inferred from a title or filename.
        let Some(document) = page
            .frontmatter
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let linked = lk_source::repoint_document_links(&page.body, path, base_url, |p| {
            tree.blobs.contains_key(p)
        });
        let projected =
            lk_core::markdown::demote_headings(&lk_core::text::collapse_blank_lines(&linked), 4);
        if matches_projection(target, &projected) {
            matches.push(HistoricalProof {
                commit: tree.commit.clone(),
                path: path.into(),
                blob: blob.clone(),
                document: document.into(),
                projection_revision: revision(projected.trim()),
            });
        }
    }
    if matches
        .iter()
        .map(|p| &p.document)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        > 1
    {
        return Ok(Err(
            "historical body matches conflicting explicit document IDs".into(),
        ));
    }
    Ok(Ok(matches.into_iter().next()))
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
    let mut history_refs = match &command {
        IdentityCommand::Plan { history_ref, .. } => history_ref.clone(),
        _ => expected.as_ref().unwrap().history_refs.clone(),
    };
    history_refs.sort();
    history_refs.dedup();
    let observed = observe(&config, &source, revision(&config_raw), &history_refs).await?;
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
        let refreshed = observe(&config, &source, revision(&config_raw), &history_refs).await?;
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
    history_refs: &[String],
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
        vault_dirs: config.vault.dirs.clone(),
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
        &repo,
        events,
        history_refs,
    )
}
fn build_manifest(
    config: &lk_core::config::Config,
    source_id: &str,
    config_revision: String,
    snapshot: String,
    repo: &Path,
    events: Vec<lk_core::event::Event>,
    history_refs: &[String],
) -> miette::Result<Manifest> {
    let root_path = config.vault.root_path().canonicalize().map_err(failure)?;
    let root = root_path.as_path();
    let params = config.sources.get(source_id).map(|s| &s.params);
    let repository = params
        .and_then(|p| p.get("repository_id"))
        .and_then(|v| v.as_str());
    let base_url = params
        .and_then(|p| p.get("base_url"))
        .and_then(|v| v.as_str());
    let trees = historical_trees(repo, repository.unwrap_or(""), history_refs)?;
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
            historical_proof: None,
        };
        let mut history_eligible = false;
        entry.blocked = if page.frontmatter.get("type").and_then(|v| v.as_str()) != Some("document")
        {
            Some("target does not declare document format".into())
        } else {
            match origins.get(location) {
                None => {
                    history_eligible = true;
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
                        history_eligible = true;
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
        if !trees.is_empty() && history_eligible {
            let relative = Path::new(location).strip_prefix(repo).map_err(failure)?;
            // Reject even lexically equivalent escape paths; a historical source need not exist today.
            let relative = relative
                .to_str()
                .ok_or_else(|| failure("non-UTF8 historical source path"))?;
            match historical_match(repo, &trees, relative, &raw, base_url)? {
                Err(reason) => entry.blocked = Some(reason),
                Ok(None) => {}
                Ok(Some(proof)) => {
                    let identity = serde_json::json!({"repository":repository.unwrap(), "document":proof.document});
                    if locations[location] != 1 || owners.contains_key(&identity.to_string()) {
                        entry.blocked = Some("multiple legacy or stable owners".into());
                    } else if page
                        .frontmatter
                        .get("source_record")
                        .and_then(|r| r.get("id"))
                        .is_some_and(|id| id.as_str() != Some(proof.document.as_str()))
                    {
                        entry.blocked =
                            Some("source_record ID disagrees with historical proof".into());
                    } else if entry
                        .identity
                        .as_ref()
                        .is_some_and(|current| current != &identity)
                    {
                        entry.blocked =
                            Some("current and historical explicit document IDs disagree".into());
                    } else {
                        entry.identity = Some(identity);
                        entry.projection_revision = Some(proof.projection_revision.clone());
                        entry.historical_proof = Some(proof);
                        entry.blocked = None;
                    }
                }
            }
        }
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
    let mut bindings = BTreeMap::<String, usize>::new();
    for entry in &entries {
        if entry.blocked.is_none() {
            *bindings
                .entry(entry.identity.as_ref().unwrap().to_string())
                .or_default() += 1;
        }
    }
    for entry in &mut entries {
        if entry.blocked.is_none() && bindings[&entry.identity.as_ref().unwrap().to_string()] > 1 {
            entry.blocked = Some("multiple legacy or stable owners".into());
            entry.applied_revision = None;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest {
        version: if history_refs.is_empty() { 1 } else { 2 },
        source: source_id.into(),
        config_revision,
        snapshot,
        entries,
        history_refs: history_refs.to_vec(),
    })
}

fn matches_projection(raw: &str, projected: &str) -> bool {
    lk_vault::resolve_section(raw, |s| s.document_content)
        .is_some_and(|section| section.body.trim() == projected.trim())
}
#[cfg(test)]
mod tests {
    use super::*;
    struct GitFixture {
        temp: tempfile::TempDir,
    }
    impl GitFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            git(temp.path(), &["init", "-q"]).unwrap();
            git(temp.path(), &["config", "user.email", "test@example.test"]).unwrap();
            git(temp.path(), &["config", "user.name", "test"]).unwrap();
            git(
                temp.path(),
                &["remote", "add", "origin", "git@github.com:example/repo.git"],
            )
            .unwrap();
            Self { temp }
        }
        fn commit(&self, id: &str, body: &str) -> String {
            std::fs::write(
                self.temp.path().join("doc.md"),
                format!("---\nid: {id}\nkind: learning\nstatus: active\n---\n{body}"),
            )
            .unwrap();
            git(self.temp.path(), &["add", "doc.md"]).unwrap();
            git(self.temp.path(), &["commit", "-qm", "document"]).unwrap();
            git(self.temp.path(), &["rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .into()
        }
        fn trees(&self, refs: &[String]) -> Vec<HistoricalTree> {
            historical_trees(self.temp.path(), "github.com/example/repo", refs).unwrap()
        }
    }
    #[test]
    fn historical_copy_uses_sealed_blob_even_after_source_deleted() {
        let f = GitFixture::new();
        let old = f.commit("authored-id", "# Heading\n\nOriginal.\n");
        f.commit("authored-id", "# Heading\n\nChanged.\n");
        std::fs::remove_file(f.temp.path().join("doc.md")).unwrap();
        let trees = f.trees(std::slice::from_ref(&old));
        let target = "## Content\n\n#### Heading\n\nOriginal.\n\n## Summary\nKeep.\n";
        let proof = historical_match(f.temp.path(), &trees, "doc.md", target, None)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(proof.document, "authored-id");
        assert_eq!(proof.commit, old);
        assert_eq!(
            git(f.temp.path(), &["rev-parse", &format!("{old}:doc.md")])
                .unwrap()
                .trim(),
            proof.blob
        );
        assert!(
            historical_match(
                f.temp.path(),
                &trees,
                "doc.md",
                &target.replace("Original.", "Edited."),
                None
            )
            .unwrap()
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn historical_identical_body_with_conflicting_ids_is_blocked() {
        let f = GitFixture::new();
        let a = f.commit("first", "Identical.\n");
        let b = f.commit("second", "Identical.\n");
        let trees = f.trees(&[a, b]);
        assert!(
            historical_match(
                f.temp.path(),
                &trees,
                "doc.md",
                "## Content\nIdentical.\n",
                None
            )
            .unwrap()
            .is_err()
        );
    }
    #[test]
    fn historical_projection_resolves_links_against_immutable_tree() {
        let f = GitFixture::new();
        std::fs::write(f.temp.path().join("other.md"), "Historical destination.").unwrap();
        git(f.temp.path(), &["add", "other.md"]).unwrap();
        let commit = f.commit(
            "first",
            "# Header\n\n[other](other.md) [absent](absent.md)\n",
        );
        std::fs::remove_file(f.temp.path().join("other.md")).unwrap();
        std::fs::write(f.temp.path().join("absent.md"), "Only present now.").unwrap();
        let trees = f.trees(&[commit]);
        let target = "## Content\n\n#### Header\n\n[other](https://github.com/example/repo/blob/main/other.md) absent\n";
        assert!(
            historical_match(
                f.temp.path(),
                &trees,
                "doc.md",
                target,
                Some("https://github.com/example/repo/blob/main")
            )
            .unwrap()
            .unwrap()
            .is_some()
        );
    }
    #[test]
    fn historical_proof_ignores_git_commit_and_blob_replacements() {
        let f = GitFixture::new();
        let original = f.commit("original-id", "Original body.\n");
        let original_blob = git(f.temp.path(), &["rev-parse", &format!("{original}:doc.md")])
            .unwrap()
            .trim()
            .to_owned();
        let replacement = f.commit("forged-id", "Forged body.\n");
        let replacement_blob = git(
            f.temp.path(),
            &["rev-parse", &format!("{replacement}:doc.md")],
        )
        .unwrap()
        .trim()
        .to_owned();
        git(
            f.temp.path(),
            &["replace", &original_blob, &replacement_blob],
        )
        .unwrap();
        git(f.temp.path(), &["replace", &original, &replacement]).unwrap();
        let trees = f.trees(std::slice::from_ref(&original));
        let proof = historical_match(
            f.temp.path(),
            &trees,
            "doc.md",
            "## Content\nOriginal body.\n",
            None,
        )
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(proof.document, "original-id");
        assert_eq!(proof.blob, original_blob);
        assert_eq!(proof.commit, original);
        assert!(
            historical_match(
                f.temp.path(),
                &trees,
                "doc.md",
                "## Content\nForged body.\n",
                None
            )
            .unwrap()
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn historical_refs_and_paths_are_strict() {
        let f = GitFixture::new();
        let commit = f.commit("first", "Body.\n");
        let blob = git(f.temp.path(), &["rev-parse", &format!("{commit}:doc.md")])
            .unwrap()
            .trim()
            .to_owned();
        assert!(historical_trees(f.temp.path(), "github.com/example/repo", &[blob]).is_err());
        assert!(
            historical_trees(f.temp.path(), "github.com/example/repo", &["HEAD".into()]).is_err()
        );
        assert!(
            historical_trees(
                f.temp.path(),
                "github.com/wrong/repo",
                std::slice::from_ref(&commit)
            )
            .is_err()
        );
        let trees = f.trees(&[commit]);
        for path in ["../doc.md", "/doc.md", "docs/../doc.md"] {
            assert!(
                historical_match(f.temp.path(), &trees, path, "## Content\nBody.\n", None).is_err()
            );
        }
    }
    #[test]
    fn historical_manifest_wiring_revalidates_and_refuses_ownership_conflicts() {
        let f = GitFixture::new();
        let old = f.commit("authored-id", "Historical body.\n");
        f.commit("authored-id", "Current body.\n");
        let repo = f.temp.path().canonicalize().unwrap();
        let root = repo.join("vault");
        let config: lk_core::config::Config = serde_json::from_value(serde_json::json!({
            "vault":{"root":root}, "identity":{"name":"test","email":"test@example.test"},
            "sources":{"repo":{"type":"nodex","params":{"repo":repo,"repository_id":"github.com/example/repo"}}}
        })).unwrap();
        let docs = root.join(lk_core::vault_path::documents_dir(&config.vault.dirs));
        std::fs::create_dir_all(&docs).unwrap();
        let target = docs.join("copy.md");
        let raw = format!(
            "---\ntype: document\nsource_file: {}\n---\n## Content\nHistorical body.\n",
            serde_json::to_string(&repo.join("doc.md")).unwrap()
        );
        std::fs::write(&target, &raw).unwrap();
        let make = |events| {
            build_manifest(
                &config,
                "repo",
                "config".into(),
                "snapshot".into(),
                &repo,
                events,
                std::slice::from_ref(&old),
            )
            .unwrap()
        };
        // Deleted current source is unnecessary to prove an immutable historical copy.
        std::fs::remove_file(repo.join("doc.md")).unwrap();
        let manifest = make(Vec::new());
        assert_eq!(manifest.version, 2);
        assert!(manifest.entries[0].blocked.is_none());
        assert!(manifest.entries[0].historical_proof.is_some());
        let mut replay = clone_manifest(&manifest);
        verify_manifest(&mut replay, &make(Vec::new()), &root).unwrap();
        std::fs::write(&target, raw.replace("Historical body.", "Edited body.")).unwrap();
        assert!(verify_manifest(&mut replay, &make(Vec::new()), &root).is_err());
        std::fs::write(&target, &raw).unwrap();
        std::fs::write(docs.join("duplicate.md"), &raw).unwrap();
        assert!(make(Vec::new()).entries.iter().all(|e| e.blocked.is_some()));
        std::fs::remove_file(docs.join("duplicate.md")).unwrap();
        // A current, explicitly different identity is never overwritten by history.
        std::fs::write(repo.join("doc.md"), "current bytes").unwrap();
        let mut event = Fixture::new().events.remove(0);
        event.body = "Current body.".into();
        event.metadata = serde_json::json!({"source_file":repo.join("doc.md"),"source_identity":{"repository":"github.com/example/repo","document":"different-id"},"source_record":{"id":"different-id"}});
        let disagree = make(vec![event]);
        assert_eq!(
            disagree.entries[0].blocked.as_deref(),
            Some("current and historical explicit document IDs disagree")
        );
    }
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
                &self.temp.path().canonicalize().unwrap().join("repo"),
                self.events.clone(),
                &[],
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
