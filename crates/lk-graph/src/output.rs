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
/// there, a category outside the configured
/// vocabulary, one name answering to two pages. Each entry names its own repair, so this
/// channel — and only this channel — decides the exit code.
#[derive(Debug, Serialize)]
pub struct Violations {
    pub broken: Vec<BrokenLink>,
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
    /// Checks that could not complete; their absence of findings is not a clean verdict.
    pub errors: Vec<String>,
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
            invalid_categories,
            duplicate_concepts,
            address_collisions,
            unnormalized,
            respelled_links,
        } = self;
        broken.len()
            + invalid_categories.len()
            + duplicate_concepts.len()
            + address_collisions.len()
            + unnormalized.len()
            + respelled_links.len()
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
    pub revision: String,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
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
    if !r.errors.is_empty() {
        println!("\nIncomplete checks:");
        for error in &r.errors {
            println!("  {error}");
        }
        println!(
            "\n{violations} known violation(s), {observations} observation(s); verdict incomplete"
        );
    } else if violations == 0 {
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
    if r.total == 0 {
        println!("(no link suggestions)");
        return;
    }
    for s in &r.pairs {
        println!(
            "  {} <-> {} (score: {:.3}, shared neighbors: {})",
            s.a, s.b, s.score, s.shared_neighbors
        );
    }
    println!(
        "{} of {} candidate(s), offset {}",
        r.pairs.len(),
        r.total,
        r.offset
    );
    if let Some(offset) = r.next_offset {
        eprintln!("next page: --offset {offset} --revision {}", r.revision);
    }
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
    println!("  from revision: {}", result.from_revision);
    println!("  into revision: {}", result.into_revision);
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

    if !r.sync.preserved_sources.is_empty() {
        println!(
            "\nCustom Sources preserved: {} concept page(s)",
            r.sync.preserved_sources.len()
        );
        for path in &r.sync.preserved_sources {
            println!("  {}", path.display());
        }
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
            .filter(|entry| !entry.discarding.is_empty())
            .count();
        if replacing > 0 {
            // Named for the same reason every other LLM-owned section names it: a body
            // somebody wrote without recording it does not survive the rewrite, and the page
            // is the only copy. Counted over BOTH sections the one act writes, because a page
            // whose relations were authored and whose synthesis is empty is a page this line
            // used to pass over in silence.
            let sections: usize = r
                .sync
                .resynthesize
                .iter()
                .map(|entry| entry.discarding.len())
                .sum();
            println!(
                "  of those, {replacing} already hold written prose in {sections} section(s), \
                 which the rewrite REPLACES — copy anything you need out first"
            );
        }
        // The relations answer to the same evidence and are written by the same act, so a page
        // carrying no heading for them loses half of what it is owed — and loses it silently,
        // since the synthesis still lands and the task still succeeds. Named here because this
        // is the only moment anything knows.
        let unrelatable: Vec<&std::path::Path> = r
            .sync
            .resynthesize
            .iter()
            .filter(|entry| entry.related_anchor.is_none())
            .map(|entry| entry.path.as_path())
            .collect();
        if !unrelatable.is_empty() {
            println!(
                "  of those, {} carry no related-concepts heading, so their relations cannot be \
                 written — add the section and the next rewrite fills it:",
                unrelatable.len()
            );
            for path in unrelatable {
                println!("    {}", path.display());
            }
        }
    }

    if !r.sync.adopted.is_empty() {
        println!(
            "\nAdopted: {} concept page(s) already held a written synthesis and had recorded no \
             input — their prose stands as the answer for the evidence they carry now, and a \
             rewrite is owed the next time that evidence moves. Use \
             `lore graph resynthesize <vault-relative-concept.md>` to request a rewrite \
             from current citations without editing completion markers",
            r.sync.adopted.len()
        );
        for adopted in &r.sync.adopted {
            println!(
                "  {} ({} citation(s))",
                adopted.path.display(),
                adopted.citations
            );
        }
        // Adoption is only obviously right at one citation, where the prose was written from
        // the single source there is. Above one it defers: the page reads as fully answered
        // and nothing is queued, so this is the only place that says how much was deferred.
        let deferred = r
            .sync
            .adopted
            .iter()
            .filter(|adopted| adopted.citations > 1)
            .count();
        if deferred > 0 {
            println!(
                "  {deferred} of those answer to MORE than one citation — the coverage of their \
                 adopted prose has not been verified. Use \
                 `lore graph resynthesize <vault-relative-concept.md>` for an explicit rewrite"
            );
        }
    }

    if !r.sync.withdrawn.is_empty() {
        println!(
            "\nWithdrawn: {} concept page(s) recorded a synthesis input nothing answered, and \
             nothing cites them any more — the rewrite they were promised can never be written \
             from a set that is now empty, so the input is removed. A task queued for one stops \
             being work; `lore queue prune` drops it",
            r.sync.withdrawn.len()
        );
        for path in &r.sync.withdrawn {
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
        // A missing synthesis heading is reported above while its count still reconciles.
        // These pages cannot safely publish their derived state and remain unchanged.
        println!(
            "\nSkipped: {} concept page(s) could not safely publish derived state and were \
             left unchanged. A page with no frontmatter block needs one added; \
             frontmatter that will not parse needs the YAML fixed; a page missing its sources \
             heading needs it back, in any locale's spelling; and an `llm_inputs:` written as \
             an inline `{{…}}` mapping needs rewriting as a block mapping — the line is there, \
             but nothing can add a key to it without dropping the ones it holds. A legacy page \
             needing a new synthesis must explicitly declare `type: concept`",
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
    let (Some(added), Some(removed)) = (&update.added, &update.removed) else {
        return "  (previous complete source set unavailable)".into();
    };
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("+{} source(s)", added.len()));
    }
    if !removed.is_empty() {
        parts.push(format!("-{} source(s)", removed.len()));
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

    #[test]
    fn unavailable_source_deltas_are_explicit_in_json_and_terminal_output() {
        let mut update = ConceptUpdate {
            path: "wiki/concepts/topic.md".into(),
            added: None,
            removed: None,
        };
        assert!(format_diff(&update).contains("unavailable"));
        let json = serde_json::to_value(&update).unwrap();
        assert!(json["added"].is_null());
        assert!(json["removed"].is_null());
        update.added = Some(vec!["daily/new".into()]);
        update.removed = Some(Vec::new());
        assert_eq!(format_diff(&update), "  (+1 source(s))");
    }

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

    #[test]
    fn lint_violations_do_not_expose_a_catalog_currency_channel() {
        let violations = Violations {
            broken: Vec::new(),
            invalid_categories: Vec::new(),
            duplicate_concepts: Vec::new(),
            address_collisions: Vec::new(),
            unnormalized: Vec::new(),
            respelled_links: Vec::new(),
        };
        let value = serde_json::to_value(&violations).unwrap();
        assert!(value.get("index").is_none());
        assert_eq!(violations.count(), 0);
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
