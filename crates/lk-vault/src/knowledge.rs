use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::{
    ConceptIdentity, ConceptRegistry, Resolution, file_revision, identity_key, source_revision,
};
use lk_core::config::{ConceptCategory, VaultDirs};
use lk_core::frontmatter::KnowledgeStatus;
use serde::{Deserialize, Serialize};

use crate::{KnowledgeReview, MutationPlan, MutationSession, VaultError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurationPlan {
    pub evidence: Vec<PageRevision>,
    #[serde(default)]
    pub concepts: Vec<ConceptCuration>,
    #[serde(default)]
    pub reviews: Vec<ReviewCuration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRevision {
    pub path: String,
    pub revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConceptCuration {
    pub path: String,
    pub revision: String,
    pub title: Option<String>,
    pub aliases: Option<Vec<String>>,
    #[serde(default)]
    pub category: CategoryUpdate,
    pub tags: Option<Vec<String>>,
    pub knowledge_status: Option<KnowledgeStatus>,
    pub knowledge_review: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
pub enum CategoryUpdate {
    Set(String),
    Clear,
    #[default]
    #[serde(skip)]
    Unchanged,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCuration {
    pub path: String,
    pub revision: Option<String>,
    pub content: String,
}

#[derive(Serialize)]
pub struct CurationResult {
    pub concepts: Vec<String>,
    pub reviews: Vec<String>,
    pub evidence: usize,
}

fn invalid(message: impl Into<String>) -> VaultError {
    VaultError::Frontmatter(message.into())
}

fn address(value: &str, directory: Option<&Path>) -> Result<PathBuf, VaultError> {
    let path = lk_core::link::resolve_dest(Path::new(""), value)
        .filter(|path| {
            path.to_string_lossy() == value
                && path.extension().is_some_and(|extension| extension == "md")
        })
        .ok_or_else(|| {
            invalid(format!(
                "curation requires an exact vault-relative Markdown address: {value}"
            ))
        })?;
    if directory.is_some_and(|directory| !path.starts_with(directory)) {
        return Err(invalid(format!(
            "curation address is outside its configured collection: {value}"
        )));
    }
    Ok(path)
}

fn read(root: &Path, path: &Path, revision: Option<&str>) -> Result<Option<String>, VaultError> {
    let current = match std::fs::read_to_string(crate::page_path(root, path)?) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if current.as_deref().map(file_revision).as_deref() != revision {
        return Err(invalid(format!(
            "curation evidence changed: {}; read and reassess before publishing",
            path.display()
        )));
    }
    Ok(current)
}

fn set<T: Serialize>(text: &str, key: &str, value: &T) -> Result<String, VaultError> {
    let value = serde_json::to_string(value)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    crate::set_frontmatter_field(text, key, &value)
        .ok_or_else(|| invalid("curation requires frontmatter"))
}

fn set_title(text: &str, title: &str) -> Result<String, VaultError> {
    let mut updated = set(text, "title", &title)?;
    let page = crate::parse_page(&updated).map_err(invalid)?;
    if let Some((
        pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading {
            level: pulldown_cmark::HeadingLevel::H1,
            ..
        }),
        range,
    )) = pulldown_cmark::Parser::new(&page.body)
        .into_offset_iter()
        .next()
    {
        let offset = updated.len() - page.body.len();
        updated.replace_range(
            offset + range.start..offset + range.end,
            &format!("# {title}\n"),
        );
    }
    Ok(updated)
}

pub fn curate(
    session: &MutationSession,
    dirs: &VaultDirs,
    categories: &[ConceptCategory],
    request: CurationPlan,
) -> Result<CurationResult, VaultError> {
    let (plan, result) = build_curation(session.root(), dirs, categories, request)?;
    session.apply(plan)?;
    Ok(result)
}

pub fn preview_curation(
    root: &Path,
    dirs: &VaultDirs,
    categories: &[ConceptCategory],
    request: CurationPlan,
) -> Result<CurationResult, VaultError> {
    build_curation(root, dirs, categories, request).map(|(_, result)| result)
}

fn build_curation(
    root: &Path,
    dirs: &VaultDirs,
    categories: &[ConceptCategory],
    request: CurationPlan,
) -> Result<(MutationPlan, CurationResult), VaultError> {
    if request.evidence.is_empty() || request.concepts.is_empty() && request.reviews.is_empty() {
        return Err(invalid(
            "curation requires evidence and at least one concept or review",
        ));
    }
    let mut plan = MutationPlan::default();
    let mut staged: BTreeMap<PathBuf, (Option<String>, String)> = BTreeMap::new();
    let mut guarded = BTreeMap::new();
    for evidence in &request.evidence {
        let path = address(&evidence.path, None)?;
        let text = read(root, &path, Some(&evidence.revision))?
            .ok_or_else(|| invalid("missing curation evidence"))?;
        if guarded.insert(path.clone(), text.clone()).is_some() {
            return Err(invalid("duplicate curation evidence"));
        }
        plan.require_unchanged(path, text);
    }
    let concepts_dir = lk_core::vault_path::concepts_dir(dirs);
    let explorations_dir = lk_core::vault_path::explorations_dir(dirs);
    for change in &request.concepts {
        let path = address(&change.path, Some(&concepts_dir))?;
        let before =
            read(root, &path, Some(&change.revision))?.ok_or_else(|| invalid("missing concept"))?;
        let page = crate::parse_page(&before).map_err(invalid)?;
        if page
            .frontmatter
            .get("type")
            .and_then(serde_json::Value::as_str)
            != Some("concept")
        {
            return Err(invalid("concept curation requires a concept page"));
        }
        let mut after = before.clone();
        if let Some(title) = &change.title {
            if identity_key(title).is_none() || title.contains(['\n', '\r']) {
                return Err(invalid("concept title must be a nonblank single line"));
            }
            after = set_title(&after, title)?;
        }
        if let Some(aliases) = &change.aliases {
            if aliases.iter().any(|alias| identity_key(alias).is_none()) {
                return Err(invalid("concept aliases must not be blank"));
            }
            after = set(&after, "aliases", aliases)?;
        }
        match &change.category {
            CategoryUpdate::Unchanged => {}
            CategoryUpdate::Clear => after = set(&after, "category", &Option::<String>::None)?,
            CategoryUpdate::Set(category) => {
                if !categories
                    .iter()
                    .any(|configured| &configured.id == category)
                {
                    return Err(invalid(format!("unknown concept category: {category}")));
                }
                after = set(&after, "category", category)?;
            }
        }
        if let Some(tags) = &change.tags {
            let mut identities = BTreeSet::new();
            for tag in tags {
                let key = identity_key(tag)
                    .filter(|_| !tag.contains(['\n', '\r']))
                    .ok_or_else(|| invalid("concept tags must be nonblank single lines"))?;
                if !identities.insert(key.to_lowercase()) {
                    return Err(invalid("concept tags must be unique"));
                }
            }
            after = set(&after, "tags", tags)?;
        }
        if let Some(status) = change.knowledge_status {
            let review = change
                .knowledge_review
                .as_deref()
                .ok_or_else(|| invalid("publication status requires a durable review"))?;
            address(review, Some(&explorations_dir))?;
            after = set(&after, "knowledge_status", &status)?;
            after = set(&after, "knowledge_review", &review)?;
        } else if change.knowledge_review.is_some() {
            return Err(invalid("knowledge_review requires knowledge_status"));
        }
        if staged.insert(path, (Some(before), after)).is_some() {
            return Err(invalid("duplicate curation target"));
        }
    }
    for review in &request.reviews {
        let path = address(&review.path, Some(&explorations_dir))?;
        let before = read(root, &path, review.revision.as_deref())?;
        if let Some(raw) = &before {
            let previous = crate::parse_page(raw).map_err(invalid)?;
            if previous
                .frontmatter
                .get("type")
                .and_then(serde_json::Value::as_str)
                != Some("exploration")
                || previous.frontmatter.get("llm_inputs").is_some()
                || previous.frontmatter.get("review").is_none()
            {
                return Err(invalid(
                    "curation can replace only an existing authored review",
                ));
            }
        }
        let page = crate::parse_page(&review.content).map_err(invalid)?;
        if page
            .frontmatter
            .get("type")
            .and_then(serde_json::Value::as_str)
            != Some("exploration")
            || page.frontmatter.get("llm_inputs").is_some()
        {
            return Err(invalid(
                "curation reviews must be complete authored explorations",
            ));
        }
        if staged
            .insert(path, (before, review.content.clone()))
            .is_some()
        {
            return Err(invalid("duplicate curation target"));
        }
    }
    let mut registry = ConceptRegistry::new();
    let mut identities = Vec::new();
    for page in crate::search::load_pages(root, dirs, false)?
        .into_iter()
        .filter(|page| page.concept)
    {
        let proposed = staged
            .get(&page.rel_path)
            .map(|(_, after)| crate::parse_page(after).map_err(invalid))
            .transpose()?;
        let page_data = proposed.as_ref().unwrap_or(&page.page);
        let title = page_data
            .frontmatter
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&page.title)
            .to_owned();
        let aliases = page_data
            .frontmatter
            .get("aliases")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let slug = page
            .rel_path
            .strip_prefix(&concepts_dir)
            .map_err(|error| invalid(error.to_string()))?
            .with_extension("")
            .to_string_lossy()
            .into_owned();
        registry.register(
            ConceptIdentity {
                slug: slug.clone(),
                title: title.clone(),
            },
            &aliases,
        );
        if staged.contains_key(&page.rel_path) {
            identities.push((slug, title, aliases));
        }
    }
    for (slug, title, aliases) in identities {
        for name in std::iter::once(&title).chain(&aliases) {
            if !matches!(registry.resolve(name), Resolution::Owned(owner) if owner.slug==slug) {
                return Err(invalid(format!(
                    "curation introduces ambiguous concept name: {name}"
                )));
            }
        }
    }
    let mut archive_paths = BTreeSet::new();
    let mut assessments: BTreeSet<String> = request
        .reviews
        .iter()
        .map(|review| review.path.clone())
        .collect();
    assessments.extend(
        request
            .concepts
            .iter()
            .filter_map(|change| change.knowledge_review.clone()),
    );
    for review_path in assessments {
        let path = address(&review_path, Some(&explorations_dir))?;
        let raw = if let Some((_, after)) = staged.get(&path) {
            after.clone()
        } else {
            let raw = std::fs::read_to_string(crate::page_path(root, &path)?)?;
            plan.require_unchanged(path.clone(), raw.clone());
            raw
        };
        let page = crate::parse_page(&raw).map_err(invalid)?;
        let assessment: KnowledgeReview = serde_json::from_value(
            page.frontmatter
                .get("review")
                .cloned()
                .ok_or_else(|| invalid("curation requires review metadata"))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let errors = assessment.validation_errors(&path, &page);
        if !errors.is_empty() {
            return Err(invalid(errors.join("; ")));
        }
        for change in request
            .concepts
            .iter()
            .filter(|change| change.knowledge_review.as_deref() == Some(&review_path))
        {
            if !assessment.subjects.contains(&change.path) {
                return Err(invalid(
                    "publication review must explicitly name its subject",
                ));
            }
        }
        for citation in &assessment.evidence {
            let path = address(&citation.path, None)?;
            let raw = if let Some((_, after)) = staged.get(&path) {
                after.clone()
            } else {
                match std::fs::read_to_string(crate::page_path(root, &path)?) {
                    Ok(raw) => {
                        plan.require_unchanged(path, raw.clone());
                        raw
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        plan.require_absent(path);
                        crate::load_archived_evidence(root, &citation.revision)?
                            .and_then(|page| page.original)
                            .ok_or_else(|| {
                                invalid("missing review evidence without a verified archive")
                            })?
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            if source_revision(&crate::parse_page(&raw).map_err(invalid)?) != citation.revision {
                return Err(invalid(format!(
                    "review evidence revision does not match proposed state: {}",
                    citation.path
                )));
            }
            let archive = crate::review::evidence_archive_path(&citation.revision)?;
            if archive_paths.insert(archive.clone()) {
                match crate::load_archived_evidence(root, &citation.revision)? {
                    Some(page) => plan.require_unchanged(
                        archive,
                        page.original
                            .ok_or_else(|| invalid("archive must retain its bytes"))?,
                    ),
                    None => plan.create(archive, raw),
                }
            }
        }
    }
    for (path, (before, after)) in &staged {
        if let Some(before) = before {
            if before == after {
                continue;
            }
            let revision = source_revision(&crate::parse_page(before).map_err(invalid)?);
            let archive = crate::review::evidence_archive_path(&revision)?;
            if archive_paths.insert(archive.clone()) {
                match crate::load_archived_evidence(root, &revision)? {
                    Some(page) => plan.require_unchanged(
                        archive,
                        page.original
                            .ok_or_else(|| invalid("archive must retain its bytes"))?,
                    ),
                    None => plan.create(archive, before.clone()),
                }
            }
        }
        plan.publish_expected(root, path.clone(), before.clone(), after.clone())?;
    }
    Ok((
        plan,
        CurationResult {
            concepts: request
                .concepts
                .into_iter()
                .map(|change| change.path)
                .collect(),
            reviews: request
                .reviews
                .into_iter()
                .map(|review| review.path)
                .collect(),
            evidence: request.evidence.len(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_curation_validates_vocabulary_and_preserves_semantic_sections() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/subject.md";
        let before = "---\ntype: concept\ntitle: Subject\ncategory: old\ntags: [old]\nllm_inputs: {synthesis_done: prior}\n---\n## Synthesis\nScoped judgment.\n\n## Sources\nRetained evidence.\n";
        crate::VaultWriter::new(root.path())
            .write_page_sync(Path::new(path), before)
            .unwrap();
        let categories = [ConceptCategory {
            id: "engineering".into(),
            label: "Engineering".into(),
        }];
        let request = |raw: &str, fields: serde_json::Value| {
            let mut change = json!({"path":path,"revision":file_revision(raw)});
            change
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            serde_json::from_value::<CurationPlan>(json!({
                "evidence":[{"path":path,"revision":file_revision(raw)}],
                "concepts":[change]
            }))
            .unwrap()
        };
        for fields in [
            json!({"category":"missing"}),
            json!({"tags":[" "]}),
            json!({"tags":["line\nbreak"]}),
            json!({"tags":["API","api"]}),
        ] {
            assert!(
                preview_curation(root.path(), &dirs, &categories, request(before, fields)).is_err()
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join(path)).unwrap(),
                before
            );
            assert!(!root.path().join(".lorekeeper/evidence").exists());
        }
        let fields = json!({"category":"engineering","tags":["검색","knowledge graph"]});
        preview_curation(
            root.path(),
            &dirs,
            &categories,
            request(before, fields.clone()),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            before
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        curate(&session, &dirs, &categories, request(before, fields)).unwrap();
        let updated = std::fs::read_to_string(root.path().join(path)).unwrap();
        let page = crate::parse_page(&updated).unwrap();
        assert_eq!(
            page.frontmatter.get("category"),
            Some(&json!("engineering"))
        );
        assert_eq!(
            page.frontmatter.get("tags"),
            Some(&json!(["검색", "knowledge graph"]))
        );
        assert_eq!(page.body, crate::parse_page(before).unwrap().body);
        assert_eq!(
            page.frontmatter.get("llm_inputs"),
            crate::parse_page(before)
                .unwrap()
                .frontmatter
                .get("llm_inputs")
        );
        assert!(
            curate(
                &session,
                &dirs,
                &categories,
                request(before, json!({"tags":[]}))
            )
            .is_err()
        );
        curate(
            &session,
            &dirs,
            &categories,
            request(&updated, json!({"aliases":[]})),
        )
        .unwrap();
        let kept = std::fs::read_to_string(root.path().join(path)).unwrap();
        assert_eq!(
            crate::parse_page(&kept)
                .unwrap()
                .frontmatter
                .get("category"),
            Some(&json!("engineering"))
        );
        curate(
            &session,
            &dirs,
            &categories,
            request(&kept, json!({"category":null,"tags":[]})),
        )
        .unwrap();
        let cleared =
            crate::parse_page(&std::fs::read_to_string(root.path().join(path)).unwrap()).unwrap();
        assert_eq!(
            cleared.frontmatter.get("category"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(cleared.frontmatter.get("tags"), Some(&json!([])));
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        assert_eq!(
            crate::search(
                root.path(),
                &dirs,
                &crate::SearchOptions {
                    snapshot: Some(&snapshot.snapshot),
                    ..Default::default()
                }
            )
            .unwrap()
            .hits[0]
                .tags,
            Vec::<String>::new()
        );
    }

    #[test]
    fn retirement_and_review_publish_atomically_without_changing_generated_evidence() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let path = "wiki/concepts/stored.md";
        let review_path = "wiki/explorations/gap.md";
        let before = "---\ntype: concept\ntitle: Stored\naliases: [Stored]\nsource_count: 0\nllm_inputs:\n  synthesis_done: old\n---\n# Stored\n\n## Summary\nUnverified old prose.\n\n## Sources\n";
        crate::VaultWriter::new(root.path())
            .write_page_sync(Path::new(path), before)
            .unwrap();
        let proposed = format!(
            "---\ntype: concept\ntitle: Stored\naliases: [Stored]\nsource_count: 0\nknowledge_status: retired\nknowledge_review: {review_path}\nllm_inputs:\n  synthesis_done: old\n---\n# Stored\n\n## Summary\nUnverified old prose.\n\n## Sources\n"
        );
        let revision = source_revision(&crate::parse_page(&proposed).unwrap());
        let review = format!(
            "---\ntype: exploration\ntitle: Evidence gap\nreview:\n  status: open\n  subjects: [{path}]\n  evidence:\n    - path: {path}\n      revision: {revision}\n  resolution: null\n---\n## Question\nCan this definition be supported?\n\n## Synthesis\nRetire pending direct evidence; this is not a falsehood verdict.\n"
        );
        let request:CurationPlan=serde_json::from_value(json!({"evidence":[{"path":path,"revision":file_revision(before)}],"concepts":[{"path":path,"revision":file_revision(before),"knowledge_status":"retired","knowledge_review":review_path}],"reviews":[{"path":review_path,"revision":null,"content":review}]})).unwrap();
        let preview:CurationPlan=serde_json::from_value(json!({"evidence":[{"path":path,"revision":file_revision(before)}],"concepts":[{"path":path,"revision":file_revision(before),"knowledge_status":"retired","knowledge_review":review_path}],"reviews":[{"path":review_path,"revision":null,"content":review}]})).unwrap();
        assert_eq!(
            preview_curation(root.path(), &VaultDirs::default(), &[], preview)
                .unwrap()
                .concepts,
            [path]
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            before
        );
        assert!(!root.path().join(".lorekeeper/evidence").exists());
        assert!(!root.path().join(review_path).exists());
        let result = curate(&session, &VaultDirs::default(), &[], request).unwrap();
        assert_eq!(result.concepts, [path]);
        let after = std::fs::read_to_string(root.path().join(path)).unwrap();
        let parsed = crate::parse_page(&after).unwrap();
        assert_eq!(
            parsed.frontmatter.knowledge_status().unwrap(),
            KnowledgeStatus::Retired
        );
        assert_eq!(parsed.frontmatter.source_count(), Some(0));
        assert_eq!(
            parsed.frontmatter.get("llm_inputs"),
            crate::parse_page(before)
                .unwrap()
                .frontmatter
                .get("llm_inputs")
        );
        assert_eq!(parsed.body, crate::parse_page(before).unwrap().body);
        assert_eq!(
            crate::load_archived_evidence(
                root.path(),
                &source_revision(&crate::parse_page(before).unwrap())
            )
            .unwrap()
            .unwrap()
            .original
            .unwrap(),
            before
        );
        assert_eq!(
            crate::list_reviews(root.path(), &VaultDirs::default()).unwrap()[0].effective_state,
            crate::ReviewEffectiveState::Open
        );
        let prepared = crate::prepare_search(&session, &VaultDirs::default()).unwrap();
        let options = crate::SearchOptions {
            query: Some("Stored"),
            snapshot: Some(&prepared.snapshot),
            ..Default::default()
        };
        assert_eq!(
            crate::search(root.path(), &VaultDirs::default(), &options)
                .unwrap()
                .total,
            0
        );
        let options = crate::SearchOptions {
            knowledge_status: None,
            ..options
        };
        let hits = crate::search(root.path(), &VaultDirs::default(), &options)
            .unwrap()
            .hits;
        assert_eq!(hits[0].reviews[0].path, review_path);
    }

    #[test]
    fn review_curation_cannot_take_over_generated_or_other_authored_pages() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let writer = crate::VaultWriter::new(root.path());
        let source = "wiki/documents/source.md";
        let evidence = "---\ntype: document\ntitle: Source\n---\nOriginal observation.\n";
        writer.write_page_sync(Path::new(source), evidence).unwrap();
        let target = "wiki/explorations/review.md";
        let content = format!(
            "---\ntype: exploration\ntitle: Assessment\nreview: {}\n---\n## Question\nWhat does the source support?\n",
            json!({"status":"open","evidence":[{"path":source,"revision":source_revision(&crate::parse_page(evidence).unwrap())}],"resolution":null})
        );
        for metadata in [
            "type: document\nreview: {}",
            "type: concept\nreview: {}",
            "type: exploration\nllm_inputs: {synthesis: pending}\nreview: {}",
            "type: exploration",
        ] {
            let before = format!(
                "---\n{metadata}\ntitle: Protected\n---\nProtected authored or generated content.\n"
            );
            std::fs::create_dir_all(root.path().join("wiki/explorations")).unwrap();
            std::fs::write(root.path().join(target), &before).unwrap();
            let request = serde_json::from_value::<CurationPlan>(json!({"evidence":[{"path":source,"revision":file_revision(evidence)}],"concepts":[],"reviews":[{"path":target,"revision":file_revision(&before),"content":content}]})).unwrap();
            assert!(
                preview_curation(root.path(), &dirs, &[], request)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("existing authored review")
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join(target)).unwrap(),
                before
            );
            assert!(!root.path().join(".lorekeeper/evidence").exists());
        }
    }

    #[test]
    fn lifecycle_review_requires_the_concept_as_subject_not_as_evidence() {
        let root = tempfile::tempdir().unwrap();
        let concept = "wiki/concepts/a.md";
        let review = "wiki/explorations/assesses-b.md";
        let before =
            "---\ntype: concept\ntitle: A\n---\n## Synthesis\nA supports another assessment.\n";
        crate::VaultWriter::new(root.path())
            .write_page_sync(Path::new(concept), before)
            .unwrap();
        let after = set(
            &set(before, "knowledge_status", &KnowledgeStatus::Retired).unwrap(),
            "knowledge_review",
            &review,
        )
        .unwrap();
        let content = format!(
            "---\ntype: exploration\ntitle: Assessment of B\nreview: {}\n---\n## Question\nWhat does A tell us about B?\n",
            json!({"status":"open","subjects":["wiki/concepts/b.md"],"evidence":[{"path":concept,"revision":source_revision(&crate::parse_page(&after).unwrap())}],"resolution":null})
        );
        let request = serde_json::from_value::<CurationPlan>(json!({"evidence":[{"path":concept,"revision":file_revision(before)}],"concepts":[{"path":concept,"revision":file_revision(before),"knowledge_status":"retired","knowledge_review":review}],"reviews":[{"path":review,"revision":null,"content":content}]})).unwrap();
        assert!(
            preview_curation(root.path(), &VaultDirs::default(), &[], request)
                .err()
                .unwrap()
                .to_string()
                .contains("explicitly name its subject")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(concept)).unwrap(),
            before
        );
        assert!(!root.path().join(review).exists());
    }

    #[test]
    fn curation_accepts_verified_archived_evidence_only_when_the_live_page_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let evidence = "wiki/documents/deleted.md";
        let raw = "---\ntype: document\ntitle: Deleted source\n---\n## Summary\nThe historical observation.\n";
        let revision = source_revision(&crate::parse_page(raw).unwrap());
        let archive = crate::review::evidence_archive_path(&revision).unwrap();
        crate::VaultWriter::new(root.path())
            .write_page_sync(&archive, raw)
            .unwrap();
        let review = "wiki/explorations/historical.md";
        let content = format!(
            "---\ntype: exploration\ntitle: Historical assessment\nreview: {}\n---\n## Question\nWhat did the retained observation support?\n",
            json!({"status":"open","evidence":[{"path":evidence,"revision":revision}],"resolution":null})
        );
        let request = || {
            serde_json::from_value::<CurationPlan>(json!({"evidence":[{"path":archive.to_str().unwrap(),"revision":file_revision(raw)}],"concepts":[],"reviews":[{"path":review,"revision":null,"content":content}]})).unwrap()
        };
        preview_curation(root.path(), &dirs, &[], request()).unwrap();
        crate::VaultWriter::new(root.path())
            .write_page_sync(Path::new(evidence), &raw.replace("historical", "changed"))
            .unwrap();
        assert!(preview_curation(root.path(), &dirs, &[], request()).is_err());
        assert!(!root.path().join(review).exists());
        std::fs::remove_file(root.path().join(evidence)).unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        curate(&session, &dirs, &[], request()).unwrap();
        let reviews = crate::list_reviews(root.path(), &dirs).unwrap();
        assert_eq!(
            reviews[0].effective_state,
            crate::ReviewEffectiveState::Open
        );
        assert_eq!(reviews[0].archived_evidence, [evidence]);
    }

    #[test]
    fn curation_refuses_stale_evidence_and_name_collisions_before_any_write() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let writer = crate::VaultWriter::new(root.path());
        let first = "---\ntype: concept\ntitle: First\naliases: [First]\n---\n# First\n\n## Summary\nFirst meaning.\n";
        let second = first.replace("First", "Second");
        writer
            .write_page_sync(Path::new("wiki/concepts/first.md"), first)
            .unwrap();
        writer
            .write_page_sync(Path::new("wiki/concepts/second.md"), &second)
            .unwrap();
        for (evidence_revision, aliases) in [
            ("stale".to_owned(), vec!["Allowed"]),
            (file_revision(first), vec!["Second"]),
        ] {
            let request:CurationPlan=serde_json::from_value(json!({"evidence":[{"path":"wiki/concepts/first.md","revision":evidence_revision}],"concepts":[{"path":"wiki/concepts/first.md","revision":file_revision(first),"aliases":aliases}]})).unwrap();
            assert!(curate(&session, &VaultDirs::default(), &[], request).is_err());
            assert_eq!(
                std::fs::read_to_string(root.path().join("wiki/concepts/first.md")).unwrap(),
                first
            );
            assert!(!root.path().join(".lorekeeper/evidence").exists());
        }
        let request:CurationPlan=serde_json::from_value(json!({"evidence":[{"path":"wiki/concepts/first.md","revision":file_revision(first)}],"concepts":[{"path":"wiki/concepts/first.md","revision":file_revision(first),"title":"Renamed","aliases":["First"]}]})).unwrap();
        curate(&session, &VaultDirs::default(), &[], request).unwrap();
        let after = std::fs::read_to_string(root.path().join("wiki/concepts/first.md")).unwrap();
        assert!(after.contains("# Renamed\n"));
        assert!(after.contains("First meaning."));
        assert!(!root.path().join("wiki/concepts/renamed.md").exists());
    }
}
