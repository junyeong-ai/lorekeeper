use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::file_revision;
use lk_core::config::SourceConfig;
use lk_core::content::InputFormat;
use lk_core::manual_original::{
    MANUAL_ORIGINAL_FIELD, ManualBundleManifest, ManualOriginalReference,
};
use lk_source::{ExtractContext, ManualAdmission, ManualDependencyInventory, SourceError};

fn error(value: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("manual admission: {value}"))
}

pub(super) async fn plan(
    pipeline: &mut lk_pipeline::Pipeline,
    llm: &std::sync::Arc<dyn lk_queue::LlmClient>,
    id: &str,
    source: &SourceConfig,
    mut items: Vec<lk_core::event::RawItem>,
    options: &lk_pipeline::IngestOptions,
    mut manual: Option<&mut Acquisition>,
) -> Result<lk_pipeline::IngestResult, lk_pipeline::PipelineError> {
    let mut admitted = items.len();
    loop {
        match pipeline
            .plan(id, source, items[..admitted].to_vec(), options)
            .await
        {
            Err(error)
                if manual.is_some()
                    && !items.is_empty()
                    && matches!(
                        &error,
                        lk_pipeline::PipelineError::Input(lk_core::input::InputError::Limit { .. })
                            | lk_pipeline::PipelineError::ProjectionLimit { .. }
                    ) =>
            {
                eprintln!(
                    "  ! manual planning: {error}; retaining non-fitting entries for a later run"
                );
                llm.rollback_source().await;
                if admitted > 1 {
                    admitted /= 2;
                } else {
                    let item = items.remove(0);
                    let entry = item
                        .metadata
                        .get("source_file")
                        .and_then(|value| value.as_str())
                        .and_then(|value| Path::new(value).file_name())
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| {
                            lk_pipeline::PipelineError::Render(
                                "manual capacity refusal has no exact entry identity".into(),
                            )
                        })?;
                    admitted = items.len();
                    manual
                        .as_mut()
                        .expect("manual capacity retry")
                        .refused
                        .insert(entry.to_owned());
                }
                llm.begin_source().await;
            }
            result => return result,
        }
    }
}

fn read(root: &Path, path: &Path) -> Result<Option<String>, SourceError> {
    let full = lk_vault::page_path(root, path).map_err(error)?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(error("state must be a direct regular file"));
        }
        Ok(_) => {}
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(error(cause)),
    }
    std::fs::read_to_string(full).map(Some).map_err(error)
}

#[derive(Debug)]
pub(super) struct Acquisition {
    path: PathBuf,
    marker_path: PathBuf,
    marker: String,
    before: String,
    state: ManualAdmission,
    pub inventory: ManualDependencyInventory,
    pub manifests: Vec<ManualBundleManifest>,
    decoders: BTreeMap<String, String>,
    total: usize,
    verified: usize,
    attempted: usize,
    admitted: usize,
    refused: BTreeSet<String>,
    published: usize,
}

impl Acquisition {
    pub fn has_pending(&self) -> bool {
        self.state.pending_count() > 0
    }

    pub fn preview_has_pending(&self, result: &lk_pipeline::IngestResult) -> bool {
        self.state.pending_count() > result.document_recipes.len()
    }

    pub async fn retire(
        &self,
        source: &SourceConfig,
        context: &ExtractContext,
        pipeline: &lk_pipeline::Pipeline,
        captures: &[lk_core::manual_original::ManualBundleCapture],
        id: &str,
    ) -> Result<(), SourceError> {
        if !self.refused.is_empty() {
            return Ok(());
        }
        self.require_inbox(source, context)?;
        let mut decoders = BTreeMap::new();
        for manifest in &self.manifests {
            let receipt = self
                .state
                .published()
                .get(&manifest.entry)
                .ok_or_else(|| error("retirement has no publication receipt"))?;
            let format = Path::new(&manifest.entry)
                .extension()
                .and_then(|value| value.to_str())
                .and_then(InputFormat::from_extension)
                .ok_or_else(|| error("retirement format is unsupported"))?;
            let decoder = decoder(source, context, format, &mut decoders)
                .await
                .map_err(error)?;
            let recipe =
                lk_pipeline::DocumentProcessingRecipe::decode(receipt.processing_recipe.clone())
                    .map_err(error)?;
            if !pipeline
                .document_recipe_current(id, source, &decoder, &recipe)
                .map_err(error)?
                || !target_current(
                    &context.vault_root,
                    self.state.inbox(),
                    &manifest.entry,
                    receipt,
                )?
                || lk_vault::load_manual_original_manifest(&context.vault_root, &receipt.original)
                    .map_err(error)?
                    != *manifest
            {
                return Err(error(
                    "published input or processing recipe changed before retirement",
                ));
            }
        }
        lk_source::archive_verified_manual_originals(
            &source.params,
            &self.manifests,
            captures,
            &self.inventory,
            context,
        )
    }

    pub fn report(&self, dry_run: bool) {
        let pending = self.state.pending_count();
        eprintln!(
            "  {}manual inventory: {} current entries; {} verified_published; {} attempted; {} admitted; {} published; {} refused; {} deferred; {} pending (publication receipts, not semantic review)",
            if dry_run { "[dry-run] " } else { "" },
            self.total,
            self.verified,
            self.attempted,
            self.admitted,
            self.published,
            self.refused.len(),
            self.state
                .pending()
                .iter()
                .filter(|entry| !self.refused.contains(*entry))
                .count(),
            pending
        );
        let referenced = self
            .manifests
            .iter()
            .filter(|manifest| self.inventory.is_referenced(&manifest.entry))
            .count();
        if referenced > 0 {
            eprintln!(
                "  ! retirement blocked: {referenced} verified publication(s) remain dependencies of other inbox entries"
            );
        }
        for diagnostic in self.inventory.diagnostics() {
            eprintln!("  ! retirement coverage incomplete: {diagnostic}");
        }
    }

    pub fn publication_plan(
        &mut self,
        root: &Path,
        result: &lk_pipeline::IngestResult,
        source: &SourceConfig,
        context: &ExtractContext,
    ) -> Result<lk_vault::MutationPlan, SourceError> {
        self.require_inbox(source, context)?;
        if read(root, &self.path)?.as_deref() != Some(&self.before) {
            return Err(error(
                "reservation changed during acquisition; no publication receipt was adopted",
            ));
        }
        let inbox = self.state.inbox().to_owned();
        let mut plan = lk_vault::MutationPlan::default();
        plan.require_unchanged(&self.marker_path, self.marker.clone());
        let entries = lk_source::manual_entry_names(&source.params, context)?;
        let mut verified = BTreeSet::new();
        let mut manifests = Vec::new();
        for manifest in &self.manifests {
            let receipt = self
                .state
                .published()
                .get(&manifest.entry)
                .ok_or_else(|| error("verified entry has no publication receipt"))?;
            if lk_vault::load_manual_original_manifest(root, &receipt.original).map_err(error)?
                != *manifest
            {
                return Err(error(
                    "verified publication or original changed during acquisition",
                ));
            }
            let Some(raw) = current_target(root, &inbox, &manifest.entry, receipt)? else {
                continue;
            };
            if !entries.contains(&manifest.entry)
                || !lk_source::manual_original_current(&inbox, manifest)?
            {
                continue;
            }
            plan.require_unchanged(&receipt.target, raw);
            verified.insert(manifest.entry.clone());
            manifests.push(manifest.clone());
        }
        self.verified = verified.len();
        for output in &result.document_pages {
            let page = lk_vault::parse_page(&output.content).map_err(error)?;
            let Some(source_file) = page
                .frontmatter
                .get("source_file")
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            let Some(entry) = Path::new(source_file)
                .strip_prefix(&inbox)
                .ok()
                .and_then(|value| value.to_str())
            else {
                return Err(error("rendered manual document belongs to another inbox"));
            };
            if Path::new(entry).components().count() != 1 {
                return Err(error("rendered manual document is not a direct entry"));
            }
            let reference: ManualOriginalReference = serde_json::from_value(
                page.frontmatter
                    .get("source_record")
                    .and_then(|value| value.get(MANUAL_ORIGINAL_FIELD))
                    .cloned()
                    .ok_or_else(|| error("rendered document lacks captured original"))?,
            )
            .map_err(error)?;
            let decoder = self
                .decoders
                .get(entry)
                .ok_or_else(|| error("rendered entry was not admitted"))?;
            let recipe = result
                .document_recipes
                .get(&output.path.to_string())
                .ok_or_else(|| error("rendered document lacks actual render recipe"))?
                .clone()
                .with_decoder(decoder.clone());
            let processing_recipe = serde_json::to_value(recipe).map_err(error)?;
            self.state.record(
                entry.to_owned(),
                lk_source::ManualPublishedInput {
                    policy_revision: file_revision(
                        &serde_json::to_string(&processing_recipe).map_err(error)?,
                    ),
                    processing_recipe,
                    original: reference.clone(),
                    target: output.path.to_string(),
                },
            )?;
            let manifest =
                lk_vault::load_manual_original_manifest(root, &reference).map_err(error)?;
            if entries.contains(entry) && lk_source::manual_original_current(&inbox, &manifest)? {
                verified.insert(entry.to_owned());
                manifests.push(manifest);
            }
            self.published += 1;
        }
        self.state.reconcile(&entries, &verified)?;
        self.total = entries.len();
        self.manifests = manifests;
        let after = serde_json::to_string(&self.state).map_err(error)?;
        if after == self.before {
            plan.require_unchanged(&self.path, self.before.clone());
        } else {
            plan.publish_expected(root, &self.path, Some(self.before.clone()), after)
                .map_err(error)?;
        }
        Ok(plan)
    }

    fn require_inbox(
        &self,
        source: &SourceConfig,
        context: &ExtractContext,
    ) -> Result<(), SourceError> {
        if lk_source::manual_inbox_dir(&source.params, &context.vault_root, &context.vault_dirs)?
            != self.state.inbox()
        {
            return Err(error(
                "canonical inbox changed during acquisition; restore the selected source binding before retrying",
            ));
        }
        Ok(())
    }
}

fn current_target(
    root: &Path,
    inbox: &Path,
    entry: &str,
    receipt: &lk_source::ManualPublishedInput,
) -> Result<Option<String>, SourceError> {
    let Some(raw) = read(root, Path::new(&receipt.target))? else {
        return Ok(None);
    };
    let page = lk_vault::parse_page(&raw).map_err(error)?;
    let expected = inbox.join(entry);
    let fields = &page.frontmatter;
    let valid = fields.get("type").and_then(|value| value.as_str()) == Some("document")
        && fields.get("source_file").and_then(|value| value.as_str()) == expected.to_str()
        && fields
            .get("source_record")
            .and_then(|value| value.get(MANUAL_ORIGINAL_FIELD))
            == Some(&serde_json::to_value(&receipt.original).map_err(error)?);
    Ok(valid.then_some(raw))
}

fn target_current(
    root: &Path,
    inbox: &Path,
    entry: &str,
    receipt: &lk_source::ManualPublishedInput,
) -> Result<bool, SourceError> {
    Ok(current_target(root, inbox, entry, receipt)?.is_some())
}

fn state_paths(id: &str, inbox: &Path) -> (PathBuf, PathBuf, String) {
    let namespace = file_revision(&serde_json::to_string(&(id, inbox)).expect("paths serialize"));
    let directory = PathBuf::from(".lorekeeper/manual-admission").join(&namespace);
    let marker = serde_json::json!({"version":1,"source":id,"inbox":inbox}).to_string();
    (
        directory.join("state.json"),
        PathBuf::from(".lorekeeper/manual-admission-initialized")
            .join(format!("{}.json", file_revision(id))),
        marker,
    )
}

async fn decoder(
    source: &SourceConfig,
    context: &ExtractContext,
    format: InputFormat,
    cache: &mut BTreeMap<String, Result<String, String>>,
) -> Result<String, String> {
    let key = format!("{format:?}");
    if let Some(value) = cache.get(&key) {
        return value.clone();
    }
    let value = lk_source::manual_processing_revision(&source.params, context, format)
        .await
        .map_err(|error| error.to_string());
    cache.insert(key, value.clone());
    value
}

pub(super) async fn acquire(
    id: &str,
    source: &SourceConfig,
    context: &ExtractContext,
    pipeline: &lk_pipeline::Pipeline,
    dry_run: bool,
    wait: bool,
    validate_config: impl Fn() -> miette::Result<()>,
) -> Result<super::SourceCapture, SourceError> {
    let root = &context.vault_root;
    if !dry_run {
        std::fs::create_dir_all(root).map_err(error)?;
        if wait {
            eprintln!("Waiting for exclusive vault access... (manual admission recovery)");
        }
        let _session = if wait {
            lk_vault::MutationSession::acquire_waiting(root)
        } else {
            lk_vault::MutationSession::acquire(root)
        }
        .map_err(error)?;
        validate_config().map_err(error)?;
    }
    let inbox = lk_source::manual_inbox_dir(&source.params, root, &context.vault_dirs)?;
    let (path, marker_path, marker) = state_paths(id, &inbox);
    let before = read(root, &path)?;
    let known = read(root, &marker_path)?;
    match inbox.symlink_metadata() {
        Err(cause) => return Err(error(cause)),
        Ok(metadata) if !metadata.is_dir() => return Err(error("inbox must be a directory")),
        Ok(_) => {}
    }
    let entries = lk_source::manual_entry_names(&source.params, context)?;
    let mut state = match (&before, &known) {
        (None, None) => ManualAdmission::new(inbox.clone()),
        (Some(raw), Some(existing)) if existing == &marker => ManualAdmission::decode(raw, &inbox)?,
        _ => {
            return Err(error(
                "known admission state or initialization marker is missing or inconsistent; restore state before retrying",
            ));
        }
    };
    let mut decoders = BTreeMap::new();
    let mut verified = BTreeSet::new();
    let mut manifests = Vec::new();
    for (entry, receipt) in state
        .published()
        .iter()
        .filter(|(entry, _)| entries.contains(*entry))
    {
        let format = Path::new(entry)
            .extension()
            .and_then(|value| value.to_str())
            .and_then(InputFormat::from_extension)
            .ok_or_else(|| error("receipt entry format is unsupported"))?;
        let recipe =
            lk_pipeline::DocumentProcessingRecipe::decode(receipt.processing_recipe.clone())
                .map_err(error)?;
        let Ok(decoder) = decoder(source, context, format, &mut decoders).await else {
            continue;
        };
        if !pipeline
            .document_recipe_current(id, source, &decoder, &recipe)
            .map_err(error)?
            || !target_current(root, &inbox, entry, receipt)?
        {
            continue;
        }
        let manifest =
            lk_vault::load_manual_original_manifest(root, &receipt.original).map_err(error)?;
        if lk_source::manual_original_current(&inbox, &manifest)? {
            verified.insert(entry.clone());
            manifests.push(manifest);
        }
    }
    let limit = lk_source::manual_entry_limit(&source.params)?;
    let turn = state.reserve(&entries, &verified, limit)?;
    let reserved = serde_json::to_string(&state).map_err(error)?;
    if !dry_run {
        let session = if wait {
            lk_vault::MutationSession::acquire_waiting(root)
        } else {
            lk_vault::MutationSession::acquire(root)
        }
        .map_err(error)?;
        validate_config().map_err(error)?;
        if lk_source::manual_inbox_dir(&source.params, root, &context.vault_dirs)? != inbox {
            return Err(error("canonical inbox changed before reservation"));
        }
        if lk_source::manual_entry_names(&source.params, context)? != entries {
            return Err(error("inbox entry inventory changed before reservation"));
        }
        let mut plan = lk_vault::MutationPlan::default();
        plan.publish_expected(root, &path, before.clone(), reserved.clone())
            .map_err(error)?;
        if let Some(known) = known {
            plan.require_unchanged(&marker_path, known);
        } else {
            plan.create(&marker_path, marker.clone());
        }
        // Even an unchanged state must guard against a competing collector.
        if before.as_deref() == Some(&reserved) {
            plan.require_unchanged(&path, reserved.clone());
        }
        session.apply(plan).map_err(error)?;
    } else {
        validate_config().map_err(error)?;
    }
    let inventory =
        ManualDependencyInventory::capture(&inbox, context.originals.max_entry_bytes.get())?;
    let documents =
        PathBuf::from(&context.vault_dirs.wiki).join(lk_core::vault_path::DOCUMENTS_SUBDIR);
    let mut entry_decoders = BTreeMap::new();
    let mut selected = Vec::new();
    let mut failures = Vec::new();
    let mut formats = BTreeMap::new();
    for entry in &turn {
        let format = Path::new(entry)
            .extension()
            .and_then(|value| value.to_str())
            .and_then(InputFormat::from_extension)
            .ok_or_else(|| error("selected format is unsupported"))?;
        match decoder(source, context, format, &mut decoders).await {
            Ok(revision) => {
                entry_decoders.insert(entry.clone(), revision.clone());
                formats.insert(format!("{format:?}"), (format, revision));
                selected.push(entry.clone());
            }
            Err(error) => failures.push(format!("{entry}: {error}")),
        }
    }
    let mut capture =
        lk_source::capture_manual_batch(&source.params, context, &selected, &documents).await?;
    capture.failures.extend(failures);
    for (format, expected) in formats.values() {
        if lk_source::manual_processing_revision(&source.params, context, *format).await?
            != *expected
        {
            return Err(error("decoder recipe changed during acquisition"));
        }
    }
    for name in &capture.deferred {
        eprintln!(
            "  ! deferred {name}: bounded admission budget has no remaining capacity for this complete entry"
        );
    }
    for failure in &capture.failures {
        eprintln!("  ✗ {failure}");
    }
    let admitted_names = capture
        .originals
        .iter()
        .map(|capture| capture.manifest.entry.as_str())
        .collect::<BTreeSet<_>>();
    let refused = turn
        .iter()
        .filter(|entry| {
            !admitted_names.contains(entry.as_str()) && !capture.deferred.contains(entry)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let admitted = capture.items.len();
    let incomplete = verified.len() + admitted < entries.len() || !refused.is_empty();
    let retirement_blocked = !refused.is_empty();
    let manual = Acquisition {
        path,
        marker_path,
        marker,
        before: reserved,
        state,
        inventory,
        manifests,
        decoders: entry_decoders,
        total: entries.len(),
        verified: verified.len(),
        attempted: turn.len(),
        admitted,
        refused,
        published: 0,
    };
    Ok(super::SourceCapture {
        observation: lk_source::SourceObservation {
            items: capture.items,
            collection_identity: None,
            scope: lk_source::ObservationScope::Unverified,
            outcome: if incomplete {
                lk_source::ObservationOutcome::Partial
            } else {
                lk_source::ObservationOutcome::Unverified
            },
        },
        originals: capture.originals,
        retirement_blocked,
        manual: Some(manual),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Fixture {
        _directory: tempfile::TempDir,
        config: lk_core::config::Config,
        context: ExtractContext,
        pipeline: lk_pipeline::Pipeline,
        llm: Arc<dyn lk_queue::LlmClient>,
    }

    impl Fixture {
        fn build(names: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("vault");
            std::fs::create_dir_all(root.join("inbox")).unwrap();
            for name in names {
                std::fs::write(
                    root.join("inbox").join(format!("{name}.md")),
                    format!("# {name}\n\nOriginal {name}.\n"),
                )
                .unwrap();
            }
            let mut config: lk_core::config::Config = lk_core::yaml::from_str("vault: {root: vault, timezone: UTC, locale: en}\nidentity: {name: Test, email: test@example.invalid}\nllm: {provider: noop}\nsources:\n  notes:\n    type: manual\n    concept_policy: disabled\n    params: {inbox_dir: inbox, archive_after_ingest: false, max_entries_per_run: 1}\n").unwrap();
            config.vault.root = root.to_string_lossy().into();
            let llm: Arc<dyn lk_queue::LlmClient> = Arc::new(lk_queue::NoopLlmClient);
            let pipeline = lk_pipeline::Pipeline::new(
                &root,
                Arc::new(lk_pipeline::PipelineContext::build(None, llm.clone(), &config).unwrap()),
            );
            let context = ExtractContext {
                target_date: jiff::Timestamp::now()
                    .to_zoned(jiff::tz::TimeZone::UTC)
                    .date(),
                timezone: jiff::tz::TimeZone::UTC,
                locale: config.vault.locale(),
                identity: config.identity.clone(),
                vault_root: root,
                vault_dirs: config.vault.dirs.clone(),
                documents: None,
                projection_cache: None,
                originals: config.ingest.originals.clone(),
                max_input_bytes: config.ingest.inputs.max_source_bytes.get(),
            };
            Self {
                _directory: directory,
                config,
                context,
                pipeline,
                llm,
            }
        }

        async fn capture(&self, dry_run: bool) -> super::super::SourceCapture {
            acquire(
                "notes",
                &self.config.sources["notes"],
                &self.context,
                &self.pipeline,
                dry_run,
                false,
                || Ok(()),
            )
            .await
            .unwrap()
        }

        fn options(&self) -> lk_pipeline::IngestOptions {
            lk_pipeline::IngestOptions {
                today: self.context.target_date,
                target_date: None,
                dry_run: false,
            }
        }
    }

    #[tokio::test]
    async fn competing_reservation_rejects_the_old_capture_without_losing_pending_entries() {
        let fixture = Fixture::build(&["a", "b"]);
        let first = fixture.capture(false).await;
        let second = fixture.capture(false).await;
        assert_eq!(first.observation.items[0].title, "a");
        assert_eq!(second.observation.items[0].title, "b");
        let mut first = first.manual.unwrap();
        let empty = lk_pipeline::Pipeline::new(
            &fixture.context.vault_root,
            Arc::new(
                lk_pipeline::PipelineContext::build(None, fixture.llm.clone(), &fixture.config)
                    .unwrap(),
            ),
        )
        .plan(
            "notes",
            &fixture.config.sources["notes"],
            Vec::new(),
            &fixture.options(),
        )
        .await
        .unwrap();
        assert!(
            first
                .publication_plan(
                    &fixture.context.vault_root,
                    &empty,
                    &fixture.config.sources["notes"],
                    &fixture.context
                )
                .is_err()
        );
        let second = second.manual.unwrap();
        let current = read(&fixture.context.vault_root, &second.path)
            .unwrap()
            .unwrap();
        assert_eq!(
            ManualAdmission::decode(&current, second.state.inbox())
                .unwrap()
                .pending_count(),
            2
        );
        assert!(second.state.published().is_empty());
    }

    #[tokio::test]
    async fn lost_marker_and_failed_page_publication_cannot_commit_a_receipt() {
        let mut fixture = Fixture::build(&["a"]);
        let capture = fixture.capture(false).await;
        let mut admission = capture.manual.unwrap();
        let options = fixture.options();
        let result = fixture
            .pipeline
            .plan(
                "notes",
                &fixture.config.sources["notes"],
                capture.observation.items,
                &options,
            )
            .await
            .unwrap();
        let session = lk_vault::MutationSession::acquire(&fixture.context.vault_root).unwrap();
        for original in &capture.originals {
            lk_vault::publish_manual_original(&session, original).unwrap();
        }
        let plan = admission
            .publication_plan(
                session.root(),
                &result,
                &fixture.config.sources["notes"],
                &fixture.context,
            )
            .unwrap();
        let marker = std::fs::read(session.root().join(&admission.marker_path)).unwrap();
        std::fs::remove_file(session.root().join(&admission.marker_path)).unwrap();
        let mut publication =
            super::super::super::publication::Publication::new(&session, fixture.llm.clone());
        assert!(
            publication
                .publish_with_plan(&result.document_pages, plan)
                .await
                .is_err()
        );
        assert!(
            !session
                .root()
                .join(result.document_pages[0].path.as_ref())
                .exists()
        );
        let state = read(session.root(), &admission.path).unwrap().unwrap();
        assert!(
            ManualAdmission::decode(&state, admission.state.inbox())
                .unwrap()
                .published()
                .is_empty()
        );
        std::fs::write(session.root().join(&admission.marker_path), marker).unwrap();
        let mut second = fixture.capture(true).await.manual.unwrap();
        let plan = second
            .publication_plan(
                session.root(),
                &result,
                &fixture.config.sources["notes"],
                &fixture.context,
            )
            .unwrap();
        let path = session.root().join(result.document_pages[0].path.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\ntype: document\ntitle: Occupied\n---\nAuthored.\n",
        )
        .unwrap();
        assert!(
            publication
                .publish_with_plan(&result.document_pages, plan)
                .await
                .is_err()
        );
        assert_eq!(
            read(session.root(), &admission.path).unwrap().unwrap(),
            state
        );
        assert!(std::fs::read_to_string(path).unwrap().contains("Authored."));
    }

    #[tokio::test]
    async fn pending_journal_recovers_before_reservation_but_preview_stays_read_only() {
        let fixture = Fixture::build(&["a", "b"]);
        let capture = fixture.capture(false).await;
        let admission = capture.manual.unwrap();
        let before = read(&fixture.context.vault_root, &admission.path)
            .unwrap()
            .unwrap();
        let mut after: serde_json::Value = serde_json::from_str(&before).unwrap();
        after["pending"].as_array_mut().unwrap().reverse();
        let mut plan = lk_vault::MutationPlan::default();
        plan.edit(&admission.path, before.clone(), after.to_string());
        let journal = fixture
            .context
            .vault_root
            .join(".lorekeeper/mutation/pending.json");
        std::fs::write(
            &journal,
            serde_json::to_vec(&serde_json::json!({"version":1,"plan":plan})).unwrap(),
        )
        .unwrap();
        let preview = fixture.capture(true).await;
        assert_eq!(preview.observation.items[0].title, "b");
        assert!(journal.exists());
        assert_eq!(
            read(&fixture.context.vault_root, &admission.path)
                .unwrap()
                .unwrap(),
            before
        );
        let resumed = fixture.capture(false).await;
        assert_eq!(resumed.observation.items[0].title, "a");
        assert!(!journal.exists());
    }

    #[tokio::test]
    async fn arrivals_and_replacements_during_acquisition_remain_pending_after_publication() {
        for replacement in [false, true] {
            let mut fixture = Fixture::build(&["a"]);
            let capture = fixture.capture(false).await;
            let mut admission = capture.manual.unwrap();
            let options = fixture.options();
            let result = fixture
                .pipeline
                .plan(
                    "notes",
                    &fixture.config.sources["notes"],
                    capture.observation.items,
                    &options,
                )
                .await
                .unwrap();
            let pending = if replacement { "a.md" } else { "b.md" };
            let changed = fixture.context.vault_root.join("inbox").join(pending);
            std::fs::write(
                &changed,
                "# Changed\n\nNot part of the captured observation.\n",
            )
            .unwrap();
            let session = lk_vault::MutationSession::acquire(&fixture.context.vault_root).unwrap();
            for original in &capture.originals {
                lk_vault::publish_manual_original(&session, original).unwrap();
            }
            let plan = admission
                .publication_plan(
                    session.root(),
                    &result,
                    &fixture.config.sources["notes"],
                    &fixture.context,
                )
                .unwrap();
            let mut publication =
                super::super::super::publication::Publication::new(&session, fixture.llm.clone());
            publication
                .publish_with_plan(&result.document_pages, plan)
                .await
                .unwrap();
            publication.finish().await.unwrap();
            let raw = read(session.root(), &admission.path).unwrap().unwrap();
            let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(state["pending"], serde_json::json!([pending]));
            assert_eq!(state["published"].as_object().unwrap().len(), 1);
            assert!(admission.has_pending());
            assert!(
                std::fs::read_to_string(&changed)
                    .unwrap()
                    .contains("Not part")
            );
            assert!(
                std::fs::read_to_string(
                    session.root().join(result.document_pages[0].path.as_ref())
                )
                .unwrap()
                .contains("Original a.")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_inbox_alias_drift_refuses_publication_even_with_identical_entry_names() {
        let mut fixture = Fixture::build(&["a"]);
        let root = &fixture.context.vault_root;
        let alias = root.join("input-alias");
        std::os::unix::fs::symlink(root.join("inbox"), &alias).unwrap();
        fixture.config.sources.get_mut("notes").unwrap().params["inbox_dir"] = "input-alias".into();
        let capture = fixture.capture(false).await;
        let mut admission = capture.manual.unwrap();
        let options = fixture.options();
        let result = fixture
            .pipeline
            .plan(
                "notes",
                &fixture.config.sources["notes"],
                capture.observation.items,
                &options,
            )
            .await
            .unwrap();
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("a.md"), "# a\n\nAnother authority.\n").unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        let before = read(root, &admission.path).unwrap();
        assert!(
            admission
                .publication_plan(
                    root,
                    &result,
                    &fixture.config.sources["notes"],
                    &fixture.context
                )
                .is_err()
        );
        assert_eq!(read(root, &admission.path).unwrap(), before);
        assert!(!root.join(result.document_pages[0].path.as_ref()).exists());
        assert!(other.join("a.md").exists());
    }
}
