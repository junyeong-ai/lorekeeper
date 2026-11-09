use super::*;

#[derive(Default)]
pub struct PruneReport {
    pub states: usize,
    pub packets: usize,
    pub bytes: usize,
    pub empty_directories: usize,
}

pub fn present(root: &Path) -> Result<bool, GraphError> {
    let path = lk_vault::page_path(root, Path::new(DIRECTORY)).map_err(error)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(error("audit selection store must be a direct directory")),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(failure) => Err(error(failure)),
    }
}

struct Stored<T> {
    path: PathBuf,
    raw: Arc<String>,
    value: T,
}

#[derive(Default)]
struct Inventory {
    states: BTreeMap<StateReference, Stored<SelectionState>>,
    packets: BTreeMap<(String, String), Stored<PacketReceipt>>,
    directories: Vec<PathBuf>,
}

fn inventory(root: &Path) -> Result<Inventory, GraphError> {
    let mut inventory = Inventory::default();
    if !present(root)? {
        return Ok(inventory);
    }
    let read = ReadControl::default();
    for entry in walkdir::WalkDir::new(root.join(DIRECTORY))
        .max_depth(3)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(error)?;
        let path = entry
            .path()
            .strip_prefix(root)
            .map_err(error)?
            .to_path_buf();
        let parts = path
            .strip_prefix(DIRECTORY)
            .map_err(error)?
            .components()
            .collect::<Vec<_>>();
        let names = parts
            .iter()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .ok_or_else(|| error("audit selection address is not UTF-8"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if entry.file_type().is_symlink() {
            return Err(error("audit selection retention refuses symlink artifacts"));
        }
        if names.is_empty()
            || names.len() == 1 && is_revision(names[0])
            || names.len() == 2 && is_revision(names[0]) && matches!(names[1], "states" | "packets")
        {
            if !entry.file_type().is_dir() {
                return Err(error(
                    "audit selection namespace must be a direct directory",
                ));
            }
            inventory.directories.push(path);
            continue;
        }
        if names.len() != 3
            || !is_revision(names[0])
            || !matches!(names[1], "states" | "packets")
            || !entry.file_type().is_file()
            || path.extension().is_none_or(|value| value != "json")
        {
            return Err(error("unknown audit selection artifact prevents retention"));
        }
        let digest = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| error("invalid audit selection artifact address"))?;
        if !is_revision(digest) {
            return Err(error("invalid audit selection artifact revision"));
        }
        if names[1] == "states" {
            let (raw, value): (_, SelectionState) = read_artifact(root, &path, digest, &read)?;
            validate_state(&value)?;
            let reference = StateReference {
                subject: value.subject.clone(),
                revision: digest.into(),
            };
            if reference.path()? != path {
                return Err(error("audit selection state namespace differs"));
            }
            inventory
                .states
                .insert(reference, Stored { path, raw, value });
        } else {
            let (raw, value): (_, PacketReceipt) = read_artifact(root, &path, digest, &read)?;
            validate_packet(&value)?;
            if packet_path(&value.request.subject, digest)? != path {
                return Err(error("audit selection packet namespace differs"));
            }
            inventory.packets.insert(
                (value.request.subject.clone(), digest.into()),
                Stored { path, raw, value },
            );
        }
    }
    for state in inventory.states.values() {
        for row in &state.value.rows {
            if let Some(reference) = &row.decision {
                let packet = inventory
                    .packets
                    .get(&(
                        state.value.subject.clone(),
                        reference.packet_revision.clone(),
                    ))
                    .ok_or_else(|| error("referenced audit screening packet is missing"))?;
                validate_decision_reference(&state.value, row, reference, &packet.value)?;
            }
        }
    }
    Ok(inventory)
}

pub(super) struct PreparedPrune {
    pub(super) mutation: MutationPlan,
    report: PruneReport,
    directories: Vec<PathBuf>,
}

pub(super) fn plan_prune(
    root: &Path,
    retained_work: &BTreeSet<StateReference>,
    mut guards: MutationPlan,
    cutoff: i64,
) -> Result<PreparedPrune, GraphError> {
    match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
        Ok(_) => {
            return Err(error(
                "pending mutation journal prevents audit selection retention",
            ));
        }
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
        Err(failure) => return Err(error(failure)),
    }
    let (before, receipts) = super::super::load(root)?;
    let inventory = inventory(root)?;
    let mut retained = retained_work.clone();
    for (subject, plan) in &receipts.discovery {
        if let Some(reference) = plan
            .selection
            .as_ref()
            .and_then(|selection| selection.state.as_ref())
        {
            if &reference.subject != subject {
                return Err(error("audit discovery selection root differs"));
            }
            retained.insert(reference.clone());
        }
    }
    for (subject, reference) in &receipts.selection_assessments {
        if &reference.subject != subject {
            return Err(error("audit assessment selection root differs"));
        }
        retained.insert(reference.clone());
    }
    for reference in &retained {
        reference.validate()?;
        if !inventory.states.contains_key(reference) {
            return Err(error("referenced audit selection state is missing"));
        }
    }
    match before {
        Some(raw) => guards.require_unchanged(super::super::STATE, raw),
        None => guards.require_absent(super::super::STATE),
    }
    retained.extend(
        inventory
            .states
            .iter()
            .filter(|(_, stored)| stored.value.created_at.as_second() >= cutoff)
            .map(|(reference, _)| reference.clone()),
    );
    let mut retained_packets = BTreeSet::new();
    for reference in &retained {
        let state = &inventory.states[reference];
        guards.require_unchanged(&state.path, state.raw.as_ref().clone());
        for row in &state.value.rows {
            if let Some(reference) = &row.decision {
                retained_packets.insert((
                    state.value.subject.clone(),
                    reference.packet_revision.clone(),
                ));
            }
        }
    }
    retained_packets.extend(
        inventory
            .packets
            .iter()
            .filter(|(_, stored)| stored.value.created_at.as_second() >= cutoff)
            .map(|(reference, _)| reference.clone()),
    );
    for reference in &retained_packets {
        let packet = &inventory.packets[reference];
        guards.require_unchanged(&packet.path, packet.raw.as_ref().clone());
    }
    let mut report = PruneReport::default();
    for (reference, state) in &inventory.states {
        if !retained.contains(reference) {
            guards.remove(&state.path, state.raw.as_ref().clone());
            report.states += 1;
            report.bytes += state.raw.len();
        }
    }
    for (reference, packet) in &inventory.packets {
        if !retained_packets.contains(reference) {
            guards.remove(&packet.path, packet.raw.as_ref().clone());
            report.packets += 1;
            report.bytes += packet.raw.len();
        }
    }
    Ok(PreparedPrune {
        mutation: guards,
        report,
        directories: inventory.directories,
    })
}

/// Caller holds the selected vault's workflow and mutation claims for publication.
pub fn prune(
    root: &Path,
    session: Option<&MutationSession>,
    retained_work: &BTreeSet<StateReference>,
    guards: MutationPlan,
    cutoff: i64,
) -> Result<PruneReport, GraphError> {
    let root = root.canonicalize().map_err(error)?;
    if session.is_some_and(|session| session.root() != root) {
        return Err(error(
            "audit selection retention session belongs to another vault",
        ));
    }
    let PreparedPrune {
        mutation,
        mut report,
        directories,
    } = plan_prune(&root, retained_work, guards, cutoff)?;
    if let Some(session) = session {
        if report.states + report.packets != 0 {
            session.apply(mutation).map_err(error)?;
        }
        for directory in directories.iter().rev() {
            match std::fs::remove_dir(root.join(directory)) {
                Ok(()) => report.empty_directories += 1,
                Err(failure)
                    if matches!(
                        failure.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(failure) => return Err(error(failure)),
            }
        }
    }
    Ok(report)
}
