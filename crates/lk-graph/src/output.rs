use serde::Serialize;

use crate::backlinks::{BacklinksSyncResult, ConceptUpdate};
use crate::cluster::{ClusterResult, LinkSuggestion};
use crate::concept_lint::{DuplicateConcept, InvalidCategoryConcept, UnresolvedConflict};
use crate::export::GraphExport;
use crate::graph::{BrokenLink, HubPageReference};
use crate::merge::MergeResult;
use crate::scan::AddressCollision;

/// Wraps the hub list so `--json` emits a named object (`{"hubs": [...]}`) consistent with
/// the other graph reports, rather than a bare top-level array — the wrapper IS the JSON
/// presentation structure, not an empty pass-through. No `count` field: `hubs` is already a
/// top-N list, so (unlike Orphans/Broken, where the total is the salient number) a count
/// would be redundant.
#[derive(Debug, Serialize)]
pub struct HubsReport {
    pub hubs: Vec<HubPageReference>,
}

#[derive(Debug, Serialize)]
pub struct OrphansReport {
    pub orphans: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct BrokenReport {
    pub broken: Vec<BrokenLink>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexSyncReport {
    pub missing_from_index: Vec<String>,
    pub missing_from_disk: Vec<String>,
    /// The catalog differs from a re-derivation in a way the two lists do not name — a title or
    /// a summary that changed while the page set did not.
    pub stale: bool,
    /// There is no catalog on disk. Distinct from `stale`, which the lists explain: in an empty
    /// vault they name nothing, and a report reading the state off them alone describes a file
    /// that was never built as one that disagrees with itself.
    pub absent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct NormalizeReport {
    pub renames: Vec<RenameSuggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RenameSuggestion {
    pub from: String,
    pub to: String,
}

/// A claim the vault makes about itself that does not hold: a link whose destination is not
/// there, a catalog that disagrees with the disk, a category outside the configured
/// vocabulary, one name answering to two pages. Each entry names its own repair, so this
/// channel — and only this channel — decides the exit code.
#[derive(Debug, Serialize)]
pub struct Violations {
    pub broken: Vec<BrokenLink>,
    pub index: IndexSyncReport,
    pub invalid_categories: Vec<InvalidCategoryConcept>,
    pub duplicate_concepts: Vec<DuplicateConcept>,
    pub address_collisions: Vec<AddressCollision>,
    /// Filenames that disagree with their own normalized slug. A link is written from the slug,
    /// so the page is addressed by a name its file does not have.
    pub unnormalized: Vec<RenameSuggestion>,
    /// Links that resolve only because the filesystem folds case. Dead on one that does not,
    /// which is where a synced vault ends up.
    pub respelled_links: Vec<crate::graph::RespelledLink>,
}

/// True statements about a vault in good standing, reported because they guide a human's next
/// decision rather than because anything is wrong: the concepts nothing cites yet, the pages
/// everything cites, a disagreement between sources that an audit deliberately recorded.
///
/// Deliberately out of the exit code. A living vault always holds some of these — every
/// extraction mints concepts before anything cites them — so counting them would make the exit
/// code permanently non-zero, and a verdict that is never clean carries no information.
#[derive(Debug, Serialize)]
pub struct Observations {
    pub hubs: Vec<HubPageReference>,
    pub orphans: Vec<String>,
    pub unresolved_conflicts: Vec<UnresolvedConflict>,
}

#[derive(Debug, Serialize)]
pub struct LintReport {
    pub pages: usize,
    pub links: usize,
    pub components: usize,
    pub violations: Violations,
    pub observations: Observations,
}

impl Violations {
    /// Destructured, so adding a field to this channel does not compile until it is counted.
    /// This number is what the exit code is derived from, and a test over a hand-built fixture
    /// cannot establish that — a new field satisfies the compiler as `Vec::new()` — so the guard
    /// has to be the pattern itself. A `..` would silence the error, which no type system
    /// prevents; what the pattern removes is the silent omission, not the deliberate one.
    pub fn count(&self) -> usize {
        let Self {
            broken,
            index,
            invalid_categories,
            duplicate_concepts,
            address_collisions,
            unnormalized,
            respelled_links,
        } = self;
        broken.len()
            + index.count()
            + invalid_categories.len()
            + duplicate_concepts.len()
            + address_collisions.len()
            + unnormalized.len()
            + respelled_links.len()
    }
}

impl IndexSyncReport {
    /// Its own method for the same reason [`Violations::count`] destructures: a pattern one
    /// level up never mentions a field added here, so each struct counts what it holds and the
    /// guard reaches every level.
    pub fn count(&self) -> usize {
        let Self {
            missing_from_index,
            missing_from_disk,
            stale,
            // Why the catalog is stale, not a second finding: a catalog that is absent differs
            // from its re-derivation, so `stale` already counts it.
            absent: _,
            // Not a finding: how many entries `index-sync --fix` ADDED. `lint` never fixes, and
            // the standalone command reports it separately.
            fixed: _,
        } = self;
        // `stale` is the verdict for the whole channel and the lists are its explanation: a
        // catalog stale in a way neither list names counts once, and one they DO name counts
        // what they name — folding the flag into either list would count it twice.
        (missing_from_index.len() + missing_from_disk.len()).max(usize::from(*stale))
    }
}

impl Observations {
    /// Destructured for the same reason as [`Violations::count`] — see there.
    pub fn count(&self) -> usize {
        let Self {
            hubs,
            orphans,
            unresolved_conflicts,
        } = self;
        hubs.len() + orphans.len() + unresolved_conflicts.len()
    }
}

#[derive(Debug, Serialize)]
pub struct SuggestLinksReport {
    pub pairs: Vec<LinkSuggestion>,
    pub count: usize,
}

#[derive(Serialize)]
struct Envelope<T: Serialize> {
    ok: bool,
    data: T,
}

/// Print a report inside the `{"ok": …, "data": …}` envelope.
///
/// `ok` is the verdict the exit code carries: false exactly when the vault contradicts itself.
/// The exit code is a shell fact and this is the parsed one, and a consumer reads whichever its
/// language makes easy — so a constant here would hand half of them the opposite answer.
pub fn print_json<T: Serialize>(data: &T, violated: bool) -> Result<(), String> {
    let envelope = Envelope {
        ok: !violated,
        data,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())?
    );
    Ok(())
}

pub fn print_hubs(r: &HubsReport) {
    if r.hubs.is_empty() {
        println!("(no hubs found)");
        return;
    }
    for hub in &r.hubs {
        println!(
            "{:>4} links  {} (in:{} out:{})",
            hub.degree, hub.id, hub.incoming, hub.outgoing
        );
    }
}

pub fn print_orphans(r: &OrphansReport) {
    for id in &r.orphans {
        println!("  {id}");
    }
    println!("{} orphan(s)", r.count);
}

pub fn print_broken(r: &BrokenReport) {
    for link in &r.broken {
        println!("  {} -> {} (not found)", link.source, link.target);
    }
    println!("{} broken link(s)", r.count);
}

pub fn print_index_sync(r: &IndexSyncReport) {
    if r.count() == 0 {
        println!("index.md is in sync");
        return;
    }
    for p in &r.missing_from_index {
        println!("  +index  {p}");
    }
    for p in &r.missing_from_disk {
        println!("  -disk   {p}");
    }
    if let Some(reason) = unlisted_reason(r) {
        println!("  {reason}");
    }
    match r.fixed {
        Some(n) => println!("index.md rewritten ({n} page(s) settled)"),
        None => println!("  repair: `lore wiki index` (or re-run with --fix)"),
    }
}

/// Why the catalog drifted when neither list says — printed by both reports, so they cannot
/// describe one state in two ways.
///
/// A catalog holding every page can still state a title or a summary no page does, and then
/// neither list names anything; saying "in sync" there is the report contradicting its own exit
/// code. But in a vault with no pages, neither list names anything EITHER while the catalog is
/// simply unbuilt, and the same sentence then describes a file that is not there.
fn unlisted_reason(r: &IndexSyncReport) -> Option<&'static str> {
    if !r.missing_from_index.is_empty() || !r.missing_from_disk.is_empty() {
        return None;
    }
    Some(if r.absent {
        "no catalog has been built yet"
    } else {
        "the catalog holds every page, but states something no page does"
    })
}

pub fn print_normalize(r: &NormalizeReport) {
    if r.renames.is_empty() {
        println!("all slugs normalized");
        return;
    }
    for entry in &r.renames {
        if r.applied.is_some() {
            println!("  renamed: {} -> {}", entry.from, entry.to);
        } else {
            println!("  would rename: {} -> {}", entry.from, entry.to);
        }
    }
    if let Some(n) = r.applied {
        println!("{n} file(s) renamed");
    }
}

pub fn print_cluster(r: &ClusterResult) {
    if r.communities.is_empty() {
        println!("(no communities found)");
        return;
    }
    for community in &r.communities {
        let label = community
            .label
            .as_deref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        println!("  #{:<3} size={:<4}{label}", community.id, community.size);
    }
    println!(
        "{} community(ies), modularity={:.4}, iterations={}",
        r.communities.len(),
        r.modularity,
        r.iterations
    );
}

pub fn print_export(r: &GraphExport) {
    println!("nodes: {}", r.nodes.len());
    println!("edges: {}", r.edges.len());
    let with_clusters = r.nodes.iter().any(|n| n.community.is_some());
    if with_clusters {
        println!("clusters: included");
    }
    println!("(use `lore graph --json export` for full graph data)");
}

pub fn print_lint(r: &LintReport) {
    println!("=== Lint Report ===");
    println!(
        "pages: {}, links: {}, components: {}",
        r.pages, r.links, r.components
    );

    print_violations(&r.violations);
    print_observations(&r.observations);

    let violations = r.violations.count();
    let observations = r.observations.count();
    if violations == 0 {
        println!("\nNo violations ({observations} observation(s))");
    } else {
        println!("\n{violations} violation(s), {observations} observation(s)");
    }
}

/// Destructured like [`Violations::count`], and for the same reason: a channel counted but not
/// printed gates the pipeline with nothing naming the repair.
fn print_violations(v: &Violations) {
    let Violations {
        broken,
        index,
        invalid_categories,
        duplicate_concepts,
        address_collisions,
        unnormalized,
        respelled_links,
    } = v;
    if v.count() == 0 {
        return;
    }
    println!("\n--- Violations ---");

    if !broken.is_empty() {
        println!("\nBroken links ({}):", broken.len());
        for link in broken {
            println!("  {} -> {}", link.source, link.target);
        }
    }

    if index.count() > 0 {
        println!("\nIndex drift:");
        for p in &index.missing_from_index {
            println!("  +index  {p}");
        }
        for p in &index.missing_from_disk {
            println!("  -disk   {p}");
        }
        if let Some(reason) = unlisted_reason(index) {
            println!("  {reason}");
        }
        println!("  repair: `lore wiki index` (or `lore graph index-sync --fix`)");
    }

    if !invalid_categories.is_empty() {
        println!(
            "\nInvalid concept categories ({}):",
            invalid_categories.len()
        );
        for c in invalid_categories {
            println!(
                "  {}  category={}  ({})",
                c.slug,
                c.category,
                c.path.display()
            );
        }
    }

    if !duplicate_concepts.is_empty() {
        println!("\nDuplicate concepts ({}):", duplicate_concepts.len());
        for d in duplicate_concepts {
            println!("  {} ~ {}  (\"{}\" = \"{}\")", d.a, d.b, d.a_name, d.b_name);
        }
    }

    if !address_collisions.is_empty() {
        println!("\nOne address, two files ({}):", address_collisions.len());
        for c in address_collisions {
            println!("  {}  <-  {}", c.id, c.paths.join(", "));
        }
    }

    if !respelled_links.is_empty() {
        println!(
            "\nLinks the filesystem forgives ({}):",
            respelled_links.len()
        );
        for link in respelled_links {
            println!(
                "  {} -> {}  (the file is {})",
                link.source, link.target, link.on_disk
            );
        }
        println!(
            "  repair: rewrite each destination as the file spells it — this resolves only on a \
             filesystem that folds case, and is dead on one that does not"
        );
    }

    if !unnormalized.is_empty() {
        println!(
            "\nFilenames that are not their own slug ({}):",
            unnormalized.len()
        );
        for r in unnormalized {
            println!("  {} -> {}{}", r.from, r.to, normalization_note(r));
        }
    }
}

fn print_observations(o: &Observations) {
    let Observations {
        hubs,
        orphans,
        unresolved_conflicts,
    } = o;
    if o.count() == 0 {
        return;
    }
    println!("\n--- Observations (do not affect the exit code) ---");

    if !hubs.is_empty() {
        println!("\nHub pages:");
        for hub in hubs {
            println!("  {:>4} links  {}", hub.degree, hub.id);
        }
    }

    if !orphans.is_empty() {
        println!("\nOrphans ({}):", orphans.len());
        for id in orphans {
            println!("  {id}");
        }
    }

    if !unresolved_conflicts.is_empty() {
        println!(
            "\nUnresolved concept conflicts ({}):",
            unresolved_conflicts.len()
        );
        for c in unresolved_conflicts {
            if c.note.is_empty() {
                println!("  {}  ({})", c.slug, c.path.display());
            } else {
                println!("  {}  {}  ({})", c.slug, c.note, c.path.display());
            }
        }
    }
}

pub fn print_suggest_links(r: &SuggestLinksReport) {
    if r.pairs.is_empty() {
        println!("(no link suggestions)");
        return;
    }
    for s in &r.pairs {
        println!(
            "  {} <-> {} (score: {:.3}, shared neighbors: {})",
            s.a, s.b, s.score, s.shared_neighbors
        );
    }
    println!("{} suggestion(s)", r.count);
}

#[derive(Debug, Serialize)]
pub struct BacklinksSyncReport {
    #[serde(flatten)]
    pub sync: BacklinksSyncResult,
    pub changed: usize,
    /// Synthesis tasks this run handed to the queue — always the length of
    /// `resynthesize`, except under `--dry-run`, where nothing is queued.
    pub queued: usize,
}

pub fn print_merge(result: &MergeResult) {
    let mode = if result.dry_run { " (dry-run)" } else { "" };
    let total: usize = result.rewritten.iter().map(|r| r.links).sum();
    println!(
        "Merge{mode}: {} → {} — {total} link(s) across {} page(s)",
        result.from_slug,
        result.into_slug,
        result.rewritten.len()
    );
    for r in &result.rewritten {
        println!("  {} ({} link(s))", r.path.display(), r.links);
    }
    if result.deleted {
        let verb = if result.dry_run {
            "would delete"
        } else {
            "deleted"
        };
        println!("  {verb} concept page: {}", result.from_slug);
    }
    if result.from_authored {
        if result.dry_run {
            println!(
                "  ! '{}' has authored body content a merge would discard — salvage it \
                 into '{}', then re-run with --force.",
                result.from_slug, result.into_slug
            );
        } else {
            println!(
                "  ! '{}' had authored body content; it was discarded (--force). \
                 Confirm '{}' captured anything worth keeping.",
                result.from_slug, result.into_slug
            );
        }
    }
    println!("  next: run `lore graph backlinks-sync` to re-derive sources + source_count");
}

pub fn print_backlinks(r: &BacklinksSyncReport) {
    println!("=== Backlinks sync ===");

    if r.sync.dry_run {
        println!("(dry-run: no files written)");
    }

    if r.sync.updated.is_empty() {
        println!("\nAll {} concept page(s) in sync.", r.sync.unchanged);
    } else {
        println!("\nUpdated: {} concept page(s)", r.sync.updated.len());
        for entry in &r.sync.updated {
            println!("  {}{}", entry.path.display(), format_diff(entry));
        }
        println!("Unchanged: {} concept page(s)", r.sync.unchanged);
    }

    if !r.sync.resynthesize.is_empty() {
        // The count comes from what was ACTUALLY queued when that differs from what is owed:
        // a task already waiting for the same input is not queued again, and a line that
        // always said "queued" would describe a run that queued nothing.
        let fate = if r.sync.dry_run {
            "would be queued".to_owned()
        } else if r.queued == r.sync.resynthesize.len() {
            "queued for the next drain".to_owned()
        } else {
            format!(
                "{} queued for the next drain, the rest already waiting",
                r.queued
            )
        };
        println!(
            "\nSynthesis owed: {} concept page(s) whose evidence has moved since the section was \
             written — {fate}",
            r.sync.resynthesize.len()
        );
        for entry in &r.sync.resynthesize {
            println!(
                "  {} ({} citation(s))",
                entry.path.display(),
                entry.citations.len()
            );
        }
        let replacing = r
            .sync
            .resynthesize
            .iter()
            .filter(|entry| entry.discarding.is_some())
            .count();
        if replacing > 0 {
            // Named for the same reason every other LLM-owned section names it: a body
            // somebody wrote without recording it does not survive the rewrite, and the page
            // is the only copy.
            println!(
                "  of those, {replacing} already hold a written synthesis, which the rewrite \
                 REPLACES — copy anything you need out first"
            );
        }
    }

    if !r.sync.adopted.is_empty() {
        println!(
            "\nAdopted: {} concept page(s) already held a written synthesis and had recorded no \
             input — their prose stands as the answer for the evidence they carry now, and a \
             rewrite is owed the next time that evidence moves. Delete a page's \
             `llm_inputs.synthesis_done` to have one written sooner — deleting BOTH keys \
             re-adopts it instead, because a page that records no input is one this has never \
             seen",
            r.sync.adopted.len()
        );
        for path in &r.sync.adopted {
            println!("  {}", path.display());
        }
    }

    if !r.sync.headless.is_empty() {
        println!(
            "\nNo synthesis heading: {} cited concept page(s). Citations and count are written; \
             nothing can be owed against evidence the page has nowhere to answer — add the \
             section, in any locale's spelling",
            r.sync.headless.len()
        );
        for path in &r.sync.headless {
            println!("  {}", path.display());
        }
    }

    if !r.sync.skipped.is_empty() {
        // Four states reach this list and the repair differs for each, so the message names
        // all four rather than the one it happened to be written for. A page missing only its
        // SYNTHESIS heading is not among them — that one keeps its citations and is reported
        // above — so naming it here would send a reader after a repair that cannot apply.
        println!(
            "\nSkipped: {} concept page(s) with nowhere to record what the citation graph says \
             about them — the sources list, source_count and the synthesis input all stay stale \
             until the page is repaired. A page with no frontmatter block needs one added; \
             frontmatter that will not parse needs the YAML fixed; a page missing its sources \
             heading needs it back, in any locale's spelling; and an `llm_inputs:` written as \
             an inline `{{…}}` mapping needs rewriting as a block mapping — the line is there, \
             but nothing can add a key to it without dropping the ones it holds",
            r.sync.skipped.len()
        );
        for path in &r.sync.skipped {
            println!("  {}", path.display());
        }
    }
}

/// Said when a rename's two sides LOOK identical.
///
/// A filename in NFD and its NFC slug render the same glyphs, so `개념 -> 개념` reads as a
/// no-op and any tooling that string-compares them concludes there is nothing to do — while on
/// a filesystem that keeps the two apart they are different files, which is the whole finding.
fn normalization_note(rename: &RenameSuggestion) -> &'static str {
    use unicode_normalization::UnicodeNormalization;
    let same_glyphs = |a: &str, b: &str| a.nfc().collect::<String>() == b.nfc().collect::<String>();
    if same_glyphs(&rename.from, &rename.to) {
        "  (same glyphs, different bytes: the name is not in Unicode NFC)"
    } else {
        ""
    }
}

fn format_diff(update: &ConceptUpdate) -> String {
    let mut parts = Vec::new();
    if !update.added.is_empty() {
        parts.push(format!("+{} source(s)", update.added.len()));
    }
    if !update.removed.is_empty() {
        parts.push(format!("-{} source(s)", update.removed.len()));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  ({})", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many entries a serialized channel actually lists: every array, counted where it
    /// sits, without descending into the entries themselves — one finding is one entry in one
    /// list, and a list inside an entry belongs to that finding rather than being another one.
    fn listed(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Array(items) => items.len(),
            serde_json::Value::Object(fields) => fields.values().map(listed).sum(),
            _ => 0,
        }
    }

    /// Which sentence explains a drift neither list names. The two states are indistinguishable
    /// from the lists — both leave them empty — and only one of them is about a file that exists.
    #[test]
    fn an_unbuilt_catalog_is_not_described_as_one_that_disagrees() {
        let drifted = |absent, missing_from_index: Vec<String>| IndexSyncReport {
            stale: true,
            absent,
            missing_from_index,
            missing_from_disk: Vec::new(),
            fixed: None,
        };
        assert_eq!(
            unlisted_reason(&drifted(false, Vec::new())),
            Some("the catalog holds every page, but states something no page does")
        );
        assert_eq!(
            unlisted_reason(&drifted(true, Vec::new())),
            Some("no catalog has been built yet")
        );
        assert_eq!(
            unlisted_reason(&drifted(true, vec!["wiki/concepts/a".into()])),
            None,
            "a list that names the drift explains it on its own"
        );
    }

    /// `count`'s destructuring refuses a new field until it is mentioned; this is the other
    /// half, that the fields are counted CORRECTLY — one `.len()` per list, none doubled, none
    /// read off the wrong field — against the entries serde can see in a full channel.
    #[test]
    fn the_violation_channel_counts_every_list_it_carries() {
        let v = Violations {
            address_collisions: Vec::new(),
            unnormalized: Vec::new(),
            respelled_links: vec![crate::graph::RespelledLink {
                source: "daily/notes/2026-05-23".into(),
                target: "../../wiki/concepts/ALPHA.md".into(),
                on_disk: "wiki/concepts/alpha.md".into(),
            }],
            broken: vec![BrokenLink {
                source: "a".into(),
                target: "b".into(),
            }],
            index: IndexSyncReport {
                stale: false,
                absent: false,
                missing_from_index: vec!["c".into()],
                missing_from_disk: vec!["d".into()],
                fixed: None,
            },
            invalid_categories: vec![InvalidCategoryConcept {
                path: "wiki/concepts/e.md".into(),
                slug: "e".into(),
                category: "nope".into(),
            }],
            duplicate_concepts: vec![DuplicateConcept {
                a: "f".into(),
                b: "g".into(),
                a_name: "F".into(),
                b_name: "F".into(),
            }],
        };
        assert_eq!(v.count(), listed(&serde_json::to_value(&v).unwrap()));
    }

    #[test]
    fn the_observation_channel_counts_every_list_it_carries() {
        let o = Observations {
            hubs: vec![HubPageReference {
                id: "a".into(),
                title: "A".into(),
                degree: 3,
                outgoing: 1,
                incoming: 2,
            }],
            orphans: vec!["b".into()],
            unresolved_conflicts: vec![UnresolvedConflict {
                path: "wiki/concepts/c.md".into(),
                slug: "c".into(),
                note: "sources disagree".into(),
            }],
        };
        assert_eq!(o.count(), listed(&serde_json::to_value(&o).unwrap()));
    }

    /// A rename whose two sides render the same glyphs says which difference it means.
    #[test]
    fn a_normalization_only_rename_says_the_bytes_differ() {
        let nfd = "\u{1100}\u{1162}\u{1102}\u{1167}\u{11b7}";
        let nfc = "\u{ac1c}\u{b150}";
        assert_ne!(nfd, nfc, "the fixture must be two spellings of one word");
        assert!(
            normalization_note(&RenameSuggestion {
                from: nfd.into(),
                to: nfc.into(),
            })
            .contains("not in Unicode NFC")
        );
        assert!(
            normalization_note(&RenameSuggestion {
                from: "Bad_Name".into(),
                to: "bad-name".into(),
            })
            .is_empty(),
            "a rename a reader can see needs no note"
        );
    }
}
