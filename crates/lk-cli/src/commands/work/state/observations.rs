//! Content-addressed index of fresh inspection observations. A new Work starts from
//! the recorded observation of every inspection whose exact evidence and purpose
//! another stored Work already observed, through attributed reuse. The index is
//! disposable: a missing origin is a miss, and maintenance rebuilds it from the store.
use super::*;

const DIRECTORY: &str = ".lorekeeper/cache/observations";
const SCHEMA: &str = "observation-origin-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    schema: String,
    work_id: String,
    node_id: String,
}

impl Origin {
    fn new(work_id: &str, node_id: &str) -> Self {
        Self {
            schema: SCHEMA.into(),
            work_id: work_id.into(),
            node_id: node_id.into(),
        }
    }

    fn load(root: &Path, path: &Path) -> miette::Result<Option<(String, Self)>> {
        let Some(raw) = read_optional(root, path)? else {
            return Ok(None);
        };
        let origin = Self::decode(&raw).map_err(|error| {
            fail(format!(
                "observation index entry {} is invalid: {error}; the index is disposable and maintenance rebuilds it",
                path.display()
            ))
        })?;
        Ok(Some((raw, origin)))
    }

    fn decode(raw: &str) -> miette::Result<Self> {
        let origin: Self = super::super::receipt::decode(raw).map_err(fail)?;
        if origin.schema != SCHEMA {
            return Err(fail(format!("unsupported schema {}", origin.schema)));
        }
        require_id(&origin.work_id)?;
        require_id(&origin.node_id)?;
        Ok(origin)
    }

    fn is_stored(&self, root: &Path) -> miette::Result<bool> {
        Ok(is_file(root, &work_path(&self.work_id, "plan.json")?)?
            && is_file(root, &output_path(&self.work_id, &self.node_id)?)?)
    }
}

fn path(content_revision: &str) -> miette::Result<PathBuf> {
    require_id(content_revision)?;
    Ok(PathBuf::from(DIRECTORY).join(format!("{content_revision}.json")))
}

fn is_file(root: &Path, path: &Path) -> miette::Result<bool> {
    let full = lk_vault::page_path(root, path).map_err(fail)?;
    match std::fs::symlink_metadata(full) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(fail("work artifact must be a direct regular file"))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(fail(error)),
    }
}

/// Sorted identifiers named by a store directory's entries, each `<id><suffix>`.
fn identifiers(root: &Path, directory: &str, suffix: &str) -> miette::Result<Vec<String>> {
    let full = lk_vault::page_path(root, Path::new(directory)).map_err(fail)?;
    let entries = match std::fs::read_dir(full) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(fail(error)),
    };
    let mut identifiers = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(fail)?
            .file_name()
            .into_string()
            .map_err(|_| fail(format!("{directory} entry is not UTF-8")))?;
        let identifier = name
            .strip_suffix(suffix)
            .ok_or_else(|| fail(format!("unexpected {directory} entry: {name}")))?;
        require_id(identifier)?;
        identifiers.push(identifier.to_owned());
    }
    identifiers.sort();
    Ok(identifiers)
}

impl Work {
    /// Index a fresh unrefined inspection record as its content's origin. An entry
    /// whose origin is still stored keeps it; a missing origin is replaced.
    pub(super) fn plan_observation_origin(
        &self,
        root: &Path,
        node: &Node,
        mutation: &mut MutationPlan,
    ) -> miette::Result<()> {
        let NodeKind::Inspect { part_id } = &node.kind else {
            return Ok(());
        };
        if node.refinement.is_some() {
            return Ok(());
        }
        let path = path(&self.content_revision(part_id)?)?;
        let before = match Origin::load(root, &path)? {
            Some((_, origin)) if origin.is_stored(root)? => return Ok(()),
            Some((raw, _)) => Some(raw),
            None => None,
        };
        mutation
            .publish_expected(
                root,
                path,
                before,
                encoded(&Origin::new(&self.plan.id, &node.id))?,
            )
            .map_err(fail)
    }

    /// Reuse the indexed observation of every unobserved unrefined inspection. Each
    /// reuse is planned against the state the previous one published.
    pub fn reuse_recorded(
        &self,
        session: &MutationSession,
        resources: &[SemanticResource],
        guard: impl Fn(&mut MutationPlan),
    ) -> miette::Result<usize> {
        let root = session.root();
        let evaluated = self.evaluate(root, resources)?;
        let unobserved = evaluated
            .nodes
            .iter()
            .filter(|node| node.refinement.is_none() && !evaluated.current.contains_key(&node.id))
            .filter_map(|node| match &node.kind {
                NodeKind::Inspect { part_id } => Some((node.id.clone(), part_id.clone())),
                NodeKind::Reduce { .. } => None,
            })
            .collect::<Vec<_>>();
        let mut reused = 0;
        for (node, part) in unobserved {
            let Some((_, origin)) = Origin::load(root, &path(&self.content_revision(&part)?)?)?
            else {
                continue;
            };
            if !origin.is_stored(root)? {
                continue;
            }
            let prior = Work::load(root, &origin.work_id)?;
            let mut mutation =
                self.plan_reuse(session, &prior, &origin.node_id, &node, resources)?;
            guard(&mut mutation);
            session.apply(mutation).map_err(fail)?;
            reused += 1;
        }
        Ok(reused)
    }
}

#[derive(Default)]
pub(crate) struct ObservationIndexReport {
    pub indexed: usize,
    pub removed: usize,
}

/// Index every stored fresh unrefined inspection whose content has no stored origin
/// and remove entries whose origin is gone. Without a session nothing is written.
pub(crate) fn reconcile_observation_index(
    root: &Path,
    session: Option<&MutationSession>,
) -> miette::Result<ObservationIndexReport> {
    require_settled(root)?;
    let mut recorded = BTreeMap::<String, BTreeSet<(String, String)>>::new();
    for id in identifiers(root, ".lorekeeper/work", "")? {
        if !is_file(root, &work_path(&id, "plan.json")?)? {
            continue;
        }
        let work = Work::load(root, &id)?;
        for node in work.capture_refinements(root)?.nodes {
            let NodeKind::Inspect { part_id } = &node.kind else {
                continue;
            };
            if node.refinement.is_some() {
                continue;
            }
            let Some(raw) = read_optional(root, &output_path(&id, &node.id)?)? else {
                continue;
            };
            let stored: StoredOutput = serde_json::from_str(&raw).map_err(fail)?;
            if stored.reused_from.is_none() && stored.refinement.is_none() {
                recorded
                    .entry(work.content_revision(part_id)?)
                    .or_default()
                    .insert((id.clone(), node.id));
            }
        }
    }
    let mut report = ObservationIndexReport::default();
    let mut mutation = MutationPlan::default();
    for content in identifiers(root, DIRECTORY, ".json")? {
        let path = path(&content)?;
        let (raw, origin) = Origin::load(root, &path)?
            .ok_or_else(|| fail("observation index entry disappeared"))?;
        match recorded.remove(&content) {
            Some(origins) if origins.contains(&(origin.work_id, origin.node_id)) => {}
            Some(origins) => {
                let (work, node) = origins.first().expect("recorded origins are nonempty");
                mutation
                    .publish_expected(root, path, Some(raw), encoded(&Origin::new(work, node))?)
                    .map_err(fail)?;
                report.indexed += 1;
            }
            None => {
                mutation.remove(path, raw);
                report.removed += 1;
            }
        }
    }
    for (content, origins) in recorded {
        let (work, node) = origins.first().expect("recorded origins are nonempty");
        mutation
            .publish_expected(
                root,
                path(&content)?,
                None,
                encoded(&Origin::new(work, node))?,
            )
            .map_err(fail)?;
        report.indexed += 1;
    }
    if let Some(session) = session
        && mutation.has_changes()
    {
        session.apply(mutation).map_err(fail)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{context, draft, limits, resources};
    use super::*;
    use lk_core::semantic::SemanticAuthority;

    fn sources() -> Vec<SemanticResource> {
        let mut sources = resources();
        sources.push(SemanticResource {
            address: "wiki/concepts/owner.md".into(),
            revision: "owner-1".into(),
            authority: SemanticAuthority::Page,
            text: "owner paragraph ".repeat(300),
        });
        sources
    }

    fn observe_all(work: &Work, session: &MutationSession, resources: &[SemanticResource]) {
        for part in &work.plan.parts {
            let output = draft(work, session.root(), &part.id, resources);
            work.record(session, output, resources).unwrap();
        }
    }

    fn index_entries(root: &Path) -> Vec<String> {
        identifiers(root, DIRECTORY, ".json").unwrap()
    }

    #[test]
    fn new_work_reuses_indexed_observations_only_for_unchanged_parts_and_purpose() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = sources();
        let old = Work::create(&session, &source, "old", context("old"), limits()).unwrap();
        observe_all(&old, &session, &source);
        assert_eq!(index_entries(root.path()).len(), old.plan.parts.len());

        let mut changed = source.clone();
        changed[1].revision = "owner-2".into();
        changed[1].text.push('!');
        let new = Work::create(&session, &changed, "new", context("new"), limits()).unwrap();
        let unchanged = new
            .plan
            .parts
            .iter()
            .filter(|part| old.plan.parts.iter().any(|prior| prior.id == part.id))
            .map(|part| part.id.clone())
            .collect::<Vec<_>>();
        assert!(!unchanged.is_empty() && unchanged.len() < new.plan.parts.len());
        assert_eq!(
            new.reuse_recorded(&session, &changed, |_| {}).unwrap(),
            unchanged.len()
        );
        let progress = new.progress(root.path(), &changed).unwrap();
        assert_eq!(progress.completed, unchanged.len());
        for part in &unchanged {
            let raw = read_optional(root.path(), &output_path(&new.plan.id, part).unwrap())
                .unwrap()
                .unwrap();
            let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
            assert_eq!(stored.reused_from.unwrap().work_id, old.plan.id);
        }
        assert_eq!(index_entries(root.path()).len(), old.plan.parts.len());

        let mut purpose = context("other");
        purpose["purpose"]["kind"] = "extraction".into();
        let other = Work::create(&session, &changed, "other", purpose, limits()).unwrap();
        assert_eq!(other.reuse_recorded(&session, &changed, |_| {}).unwrap(), 0);
    }

    #[test]
    fn maintenance_rebuilds_the_index_and_drops_origins_that_are_gone() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = sources();
        let old = Work::create(&session, &source, "old", context("old"), limits()).unwrap();
        observe_all(&old, &session, &source);
        let indexed = index_entries(root.path());

        std::fs::remove_dir_all(root.path().join(DIRECTORY)).unwrap();
        let preview = reconcile_observation_index(root.path(), None).unwrap();
        assert_eq!((preview.indexed, preview.removed), (indexed.len(), 0));
        assert!(index_entries(root.path()).is_empty());
        let rebuilt = reconcile_observation_index(root.path(), Some(&session)).unwrap();
        assert_eq!((rebuilt.indexed, rebuilt.removed), (indexed.len(), 0));
        assert_eq!(index_entries(root.path()), indexed);

        std::fs::remove_dir_all(root.path().join(work_path(&old.plan.id, "").unwrap())).unwrap();
        let fresh = Work::create(&session, &source, "fresh", context("fresh"), limits()).unwrap();
        assert_eq!(fresh.reuse_recorded(&session, &source, |_| {}).unwrap(), 0);
        let pruned = reconcile_observation_index(root.path(), Some(&session)).unwrap();
        assert_eq!((pruned.indexed, pruned.removed), (0, indexed.len()));
        assert!(index_entries(root.path()).is_empty());
    }
}
