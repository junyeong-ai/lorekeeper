use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::*;

#[derive(Clone)]
struct Manifest {
    reference: RssOriginalReference,
    value: Arc<RssOriginalManifest>,
    raw: Arc<String>,
    entries: BTreeMap<(String, String), lk_core::rss_original::RssEventEntry>,
}

#[derive(Clone)]
struct Payload {
    source: String,
    date: jiff::civil::Date,
    entry: lk_core::rss_original::RssEventEntry,
    raw: Arc<String>,
}

#[derive(Clone)]
pub(super) struct Originals {
    pub(super) root: PathBuf,
    pub(super) read: crate::ReadControl,
    manifests: BTreeMap<String, Arc<Manifest>>,
    payloads: BTreeMap<String, Arc<Payload>>,
}

impl Originals {
    pub(super) fn new(root: &Path, read: &crate::ReadControl) -> Result<Self, VaultError> {
        read.check()?;
        let root = root.canonicalize()?;
        read.check()?;
        Ok(Self {
            root,
            read: read.clone(),
            manifests: BTreeMap::new(),
            payloads: BTreeMap::new(),
        })
    }

    pub(super) fn manifest(
        &mut self,
        reference: &RssOriginalReference,
    ) -> Result<Arc<RssOriginalManifest>, VaultError> {
        self.read.check()?;
        reference.validate().map_err(err)?;
        if !self.manifests.contains_key(&reference.revision) {
            let value =
                crate::rss_original::read_manifest_controlled(&self.root, reference, &self.read)?;
            let raw = Arc::new(
                String::from_utf8(lk_core::rss_original::canonical_bytes(&value).map_err(err)?)
                    .map_err(err)?,
            );
            let entries = value
                .events
                .iter()
                .map(|entry| {
                    (
                        (entry.event_id.clone(), entry.revision.clone()),
                        entry.clone(),
                    )
                })
                .collect();
            self.read.check()?;
            self.manifests.insert(
                reference.revision.clone(),
                Arc::new(Manifest {
                    reference: reference.clone(),
                    value: Arc::new(value),
                    raw,
                    entries,
                }),
            );
        }
        let manifest = &self.manifests[&reference.revision];
        if manifest.reference != *reference {
            return Err(err("RSS manifest reference changed during capture"));
        }
        self.read.check()?;
        Ok(Arc::clone(&manifest.value))
    }

    pub(super) fn entry(
        &mut self,
        reference: &RssOriginalReference,
        selection: &RssEventSelection,
    ) -> Result<lk_core::rss_original::RssEventEntry, VaultError> {
        self.manifest(reference)?;
        self.manifests[&reference.revision]
            .entries
            .get(&(selection.event_id.clone(), selection.revision.clone()))
            .cloned()
            .ok_or_else(|| err("RSS selection absent from declared original"))
    }

    pub(super) fn payload(
        &mut self,
        reference: &RssOriginalReference,
        selection: &RssEventSelection,
    ) -> Result<Arc<String>, VaultError> {
        self.read.check()?;
        let manifest = self.manifest(reference)?;
        let entry = self.entry(reference, selection)?;
        if !self.payloads.contains_key(&selection.revision) {
            let relative = format!("{RSS_ORIGINAL_STORE}/events/{}.json", selection.revision);
            let path = crate::original_io::store_path(
                &self.root,
                Path::new(&relative),
                RSS_ORIGINAL_STORE,
                false,
            )?;
            let bytes =
                crate::original_io::read_bytes_controlled(&path, MAX_RSS_EVENT_BYTES, &self.read)?;
            verify_payload(&manifest, &entry, &bytes).map_err(err)?;
            let raw = Arc::new(String::from_utf8(bytes).map_err(err)?);
            self.read.check()?;
            self.payloads.insert(
                selection.revision.clone(),
                Arc::new(Payload {
                    source: manifest.source_id.clone(),
                    date: manifest.date,
                    entry: entry.clone(),
                    raw,
                }),
            );
        }
        let payload = &self.payloads[&selection.revision];
        if payload.source != manifest.source_id
            || payload.date != manifest.date
            || payload.entry != entry
        {
            return Err(err("RSS event differs from manifest"));
        }
        self.read.check()?;
        Ok(Arc::clone(&payload.raw))
    }
}

#[derive(Clone)]
struct Origin {
    owner: PathBuf,
    raw: Arc<String>,
    page: crate::VaultPage,
    citations: BTreeMap<String, Vec<Citation>>,
}

/// Exact verified input bytes from one request-local RSS evidence capture.
pub struct RssCitationInput {
    pub origin_owner: PathBuf,
    pub origin: Arc<String>,
    pub payload_path: PathBuf,
    pub payload: Arc<String>,
}

/// Share complete declaration and immutable-original verification within one fresh
/// read phase. Discard this capture after a host call, writer wait or publication.
#[derive(Clone)]
pub struct RssEvidenceCapture {
    originals: Originals,
    origins: BTreeMap<String, Arc<Origin>>,
    dependencies: BTreeMap<PathBuf, Arc<String>>,
}

impl RssEvidenceCapture {
    pub fn new(root: &Path, read: &crate::ReadControl) -> Result<Self, VaultError> {
        Ok(Self {
            originals: Originals::new(root, read)?,
            origins: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })
    }

    fn origin(&mut self, address: &str, raw: Option<&str>) -> Result<(), VaultError> {
        let read = &self.originals.read;
        read.check()?;
        let owner = crate::resolve_page_owner(&self.originals.root, Path::new(address))?;
        read.check()?;
        if let Some(origin) = self.origins.get(address) {
            if origin.owner != owner || raw.is_some_and(|raw| raw != origin.raw.as_str()) {
                return Err(err("RSS origin changed during capture"));
            }
            return Ok(());
        }
        let raw = match raw {
            Some(raw) => raw.to_owned(),
            None => {
                read.read_to_string(&crate::page_path(&self.originals.root, Path::new(address))?)?
            }
        };
        let page = crate::parse_page(&raw).map_err(err)?;
        let mut entries = crate::ExistingEntryResolver::new(&self.originals.root);
        let citations = super::rss_concept_citations_captured(
            &mut self.originals,
            &mut entries,
            Path::new(address),
            &raw,
            false,
        )?;
        self.originals.read.check()?;
        self.origins.insert(
            address.into(),
            Arc::new(Origin {
                owner,
                raw: Arc::new(raw),
                page,
                citations,
            }),
        );
        Ok(())
    }

    /// Reuse an exact authored origin snapshot across selectors in one read phase.
    /// Cached origins still revalidate their physical owner and cancellation state.
    pub fn has_citation_with_reader(
        &mut self,
        concept: &Path,
        citation: &Citation,
        reader: &mut impl FnMut(&Path) -> Result<String, VaultError>,
    ) -> Result<bool, VaultError> {
        self.originals.read.check()?;
        citation
            .rss_event
            .as_ref()
            .ok_or_else(|| err("expected RSS citation"))?
            .validate(&citation.revision)
            .map_err(err)?;
        if self.origins.contains_key(&citation.path) {
            return self.has_citation(concept, citation);
        }
        let path = lk_core::link::resolve_dest(Path::new(""), &citation.path)
            .ok_or_else(|| err("invalid RSS origin address"))?;
        let captured = reader(&path);
        self.originals.read.check()?;
        let raw = captured?;
        self.has_citation_in_page(concept, citation, &raw)
    }

    pub fn has_citation_in_page(
        &mut self,
        concept: &Path,
        citation: &Citation,
        raw: &str,
    ) -> Result<bool, VaultError> {
        self.originals.read.check()?;
        citation
            .rss_event
            .as_ref()
            .ok_or_else(|| err("expected RSS citation"))?
            .validate(&citation.revision)
            .map_err(err)?;
        self.origin(&citation.path, Some(raw))?;
        self.has_citation(concept, citation)
    }

    pub(super) fn has_citation(
        &mut self,
        concept: &Path,
        citation: &Citation,
    ) -> Result<bool, VaultError> {
        self.originals.read.check()?;
        let selector = citation
            .rss_event
            .as_ref()
            .ok_or_else(|| err("expected RSS citation"))?;
        selector.validate(&citation.revision).map_err(err)?;
        self.origin(&citation.path, None)?;
        let origin = &self.origins[&citation.path];
        let mut entries = crate::ExistingEntryResolver::new(&self.originals.root);
        if !has_selected_membership(
            &mut entries,
            &origin.citations,
            concept,
            citation,
            &self.originals.read,
        )? {
            self.originals.read.check()?;
            return Ok(false);
        }
        let selection = RssEventSelection {
            event_id: selector.event_id.clone(),
            revision: citation.revision.clone(),
        };
        let binding = find_binding(
            &mut entries,
            &origin.page,
            concept,
            &selection,
            &self.originals.read,
        )?;
        self.originals.payload(&binding.manifest, &selection)?;
        self.originals.read.check()?;
        Ok(true)
    }

    pub fn validate_citation(
        &mut self,
        concept: &Path,
        citation: &Citation,
    ) -> Result<(), VaultError> {
        if !self.has_citation(concept, citation)? {
            return Err(err("RSS concept evidence membership changed"));
        }
        Ok(())
    }

    pub fn load_citation(
        &mut self,
        concept: &Path,
        citation: &Citation,
    ) -> Result<RssCitationInput, VaultError> {
        self.validate_citation(concept, citation)?;
        let origin = &self.origins[&citation.path];
        let mut entries = crate::ExistingEntryResolver::new(&self.originals.root);
        let selector = citation.rss_event.as_ref().expect("validated selector");
        let selection = RssEventSelection {
            event_id: selector.event_id.clone(),
            revision: citation.revision.clone(),
        };
        let binding = find_binding(
            &mut entries,
            &origin.page,
            concept,
            &selection,
            &self.originals.read,
        )?;
        let current: RssOriginalReference = serde_json::from_value(
            origin
                .page
                .frontmatter
                .get("source_record")
                .and_then(|record| record.get(RSS_ORIGINAL_FIELD))
                .cloned()
                .ok_or_else(|| err("missing current manifest"))?,
        )
        .map_err(err)?;
        for reference in [&current, &binding.manifest] {
            self.originals.manifest(reference)?;
            self.dependencies.insert(
                PathBuf::from(&reference.manifest_path),
                Arc::clone(&self.originals.manifests[&reference.revision].raw),
            );
        }
        let payload_path = PathBuf::from(format!(
            "{RSS_ORIGINAL_STORE}/events/{}.json",
            selection.revision
        ));
        let payload = Arc::clone(&self.originals.payloads[&selection.revision].raw);
        self.dependencies
            .insert(payload_path.clone(), Arc::clone(&payload));
        self.dependencies
            .insert(PathBuf::from(&citation.path), Arc::clone(&origin.raw));
        self.originals.read.check()?;
        Ok(RssCitationInput {
            origin_owner: origin.owner.clone(),
            origin: Arc::clone(&origin.raw),
            payload_path,
            payload,
        })
    }

    pub fn guard_citation(
        &mut self,
        plan: &mut crate::MutationPlan,
        concept: &Path,
        citation: &Citation,
    ) -> Result<(), VaultError> {
        self.load_citation(concept, citation)?;
        self.guard(plan)
    }

    pub fn guard(&self, plan: &mut crate::MutationPlan) -> Result<(), VaultError> {
        self.originals.read.check()?;
        for (path, raw) in &self.dependencies {
            self.originals.read.check()?;
            plan.require_unchanged(path, raw.as_str().to_owned());
        }
        self.originals.read.check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fixture() -> (
        tempfile::TempDir,
        String,
        Vec<Citation>,
        RssOriginalReference,
    ) {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let events = (0..2).map(|index| serde_json::from_value::<lk_core::event::Event>(serde_json::json!({
            "id":format!("rss:2026-10-18:{index:016x}"),"source_id":"rss","source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":format!("Title {index}"),"body":format!("Evidence {index}."),
            "url":null,"author":null,"labels":[],"category":null,"performance_category":null,"is_self":false,"is_personal":false,"metadata":{}
        })).unwrap()).collect::<Vec<_>>();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(
            &events.iter().collect::<Vec<_>>(),
        )
        .unwrap();
        let reference = crate::publish_rss_original(&session, &capture).unwrap();
        let selections = capture
            .manifest
            .events
            .iter()
            .map(|event| RssEventSelection {
                event_id: event.event_id.clone(),
                revision: event.revision.clone(),
            })
            .collect::<Vec<_>>();
        let declarations = vec![ConceptEvidence {
            schema: RSS_EVIDENCE_SCHEMA.into(),
            concept: "wiki/concepts/topic.md".into(),
            manifest: reference.clone(),
            events: selections.clone(),
        }];
        let raw = format!(
            "---\ntype: daily\nsource: rss\ncreated: 2026-10-18\nsource_record: {{rss_original: {}}}\nconcept_evidence: {}\n---\n## Related Concepts\n- [Topic](../wiki/concepts/topic.md)\n",
            serde_json::to_string(&reference).unwrap(),
            serde_json::to_string(&declarations).unwrap()
        );
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        std::fs::write(root.path().join("daily/rss.md"), &raw).unwrap();
        std::fs::write(
            root.path().join("wiki/concepts/topic.md"),
            "---\ntype: concept\n---\nA scoped claim.\n",
        )
        .unwrap();
        let citations = selections
            .into_iter()
            .map(|event| Citation {
                path: "daily/rss.md".into(),
                revision: event.revision,
                rss_event: Some(RssEventSelector {
                    schema: RSS_EVIDENCE_SCHEMA.into(),
                    event_id: event.event_id,
                }),
            })
            .collect();
        drop(session);
        (root, raw, citations, reference)
    }

    #[test]
    fn rss_capture_rejects_invalid_selectors_before_invoking_the_origin_reader() {
        let (root, raw, citations, _) = fixture();
        let concept = Path::new("wiki/concepts/topic.md");
        let mut missing = citations[0].clone();
        missing.rss_event = None;
        let mut invalid_revision = citations[0].clone();
        invalid_revision.revision = "invalid".into();
        let mut invalid_schema = citations[0].clone();
        invalid_schema.rss_event.as_mut().unwrap().schema = "unknown".into();
        let mut capture =
            RssEvidenceCapture::new(root.path(), &crate::ReadControl::default()).unwrap();
        let mut reads = 0;
        for citation in [missing, invalid_revision, invalid_schema] {
            assert!(
                capture
                    .has_citation_with_reader(concept, &citation, &mut |_| {
                        reads += 1;
                        Ok(raw.clone())
                    })
                    .is_err()
            );
        }
        assert_eq!(reads, 0);
        assert!(capture.origins.is_empty());
    }

    #[test]
    fn rss_capture_shares_exact_original_reads_and_preserves_publication_guards() {
        let (root, raw, citations, reference) = fixture();
        let concept = Path::new("wiki/concepts/topic.md");
        let observation = crate::original_io::read_tests::observe();
        let mut capture =
            RssEvidenceCapture::new(root.path(), &crate::ReadControl::default()).unwrap();
        for citation in citations.iter().chain(&citations) {
            assert!(
                capture
                    .has_citation_in_page(concept, citation, &raw)
                    .unwrap()
            );
            let input = capture.load_citation(concept, citation).unwrap();
            assert_eq!(input.origin.as_str(), raw);
            assert_eq!(
                lk_core::rss_original::digest(input.payload.as_bytes()),
                citation.revision
            );
        }
        let paths = observation.paths();
        assert_eq!(paths.len(), 3, "one manifest and two selected payloads");
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.ends_with(Path::new(&reference.manifest_path)))
                .count(),
            1
        );
        for citation in &citations {
            assert!(crate::has_rss_citation_in_page(root.path(), concept, citation, &raw).unwrap());
        }
        let mut plan = crate::MutationPlan::default();
        capture.guard(&mut plan).unwrap();
        let encoded = serde_json::to_value(&plan).unwrap();
        assert_eq!(encoded["steps"].as_array().unwrap().len(), 4);
        let payload = root.path().join(format!(
            "{RSS_ORIGINAL_STORE}/events/{}.json",
            citations[0].revision
        ));
        std::fs::write(&payload, "changed").unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        assert!(session.apply(plan).is_err());
        let mut fresh =
            RssEvidenceCapture::new(root.path(), &crate::ReadControl::default()).unwrap();
        assert!(
            fresh
                .has_citation_in_page(concept, &citations[0], &raw)
                .is_err()
        );
    }

    #[test]
    fn rss_capture_cancellation_and_unrelated_invalid_declarations_never_become_cached_success() {
        let (root, raw, citations, _) = fixture();
        let _observation = crate::original_io::read_tests::observe();
        let concept = Path::new("wiki/concepts/topic.md");
        let cancelled = Arc::new(AtomicBool::new(false));
        let read = crate::ReadControl::new(cancelled.clone());
        let mut capture = RssEvidenceCapture::new(root.path(), &read).unwrap();
        crate::original_io::read_tests::CANCEL_READ
            .with_borrow_mut(|flag| *flag = Some(cancelled.clone()));
        assert!(matches!(
            capture.has_citation_in_page(concept, &citations[0], &raw),
            Err(VaultError::Cancelled)
        ));
        assert!(capture.originals.manifests.is_empty());
        assert!(capture.origins.is_empty());
        cancelled.store(false, Ordering::Relaxed);
        assert!(
            capture
                .has_citation_in_page(concept, &citations[0], &raw)
                .unwrap()
        );
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(
            capture.has_citation_in_page(concept, &citations[0], &raw),
            Err(VaultError::Cancelled)
        ));
        cancelled.store(false, Ordering::Relaxed);
        let page = crate::parse_page(&raw).unwrap();
        let mut declarations = bindings(&page).unwrap();
        let mut unrelated = declarations[0].clone();
        unrelated.concept = "wiki/concepts/unrelated.md".into();
        declarations.push(unrelated);
        let invalid = write_bindings(&raw, &declarations).unwrap();
        let mut fresh = RssEvidenceCapture::new(root.path(), &read).unwrap();
        assert!(
            fresh
                .has_citation_in_page(concept, &citations[0], &invalid)
                .is_err()
        );
        assert!(fresh.origins.is_empty());
        assert!(
            capture
                .has_citation_in_page(concept, &citations[0], &invalid)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rss_capture_preserves_authored_alias_context_and_refuses_owner_drift() {
        let (root, raw, citations, _) = fixture();
        let concept = Path::new("wiki/concepts/topic.md");
        let mut capture =
            RssEvidenceCapture::new(root.path(), &crate::ReadControl::default()).unwrap();
        assert!(
            capture
                .has_citation_in_page(concept, &citations[0], &raw)
                .unwrap()
        );
        std::fs::create_dir(root.path().join("other")).unwrap();
        std::os::unix::fs::symlink("../daily", root.path().join("other/alias")).unwrap();
        let mut aliased = citations[0].clone();
        aliased.path = "other/alias/rss.md".into();
        assert!(
            capture
                .has_citation_in_page(concept, &aliased, &raw)
                .is_err(),
            "same owner and bytes do not establish identical relative-link context"
        );
        std::fs::write(root.path().join("daily/replacement.md"), &raw).unwrap();
        std::fs::remove_file(root.path().join("daily/rss.md")).unwrap();
        std::os::unix::fs::symlink("replacement.md", root.path().join("daily/rss.md")).unwrap();
        assert!(
            capture
                .has_citation_in_page(concept, &citations[0], &raw)
                .is_err()
        );
    }
}
