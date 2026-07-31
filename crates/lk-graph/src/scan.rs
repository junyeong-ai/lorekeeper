//! Vault scan: walk markdown files and build [`ScannedPage`] records.
//!
//! The pure domain logic — slug normalization, frontmatter parsing, and markdown-link
//! extraction/resolution — lives in `lk-core` (`slugify`, `frontmatter::parse_page`,
//! `link`). This module keeps only the I/O concerns: filesystem walking (walkdir +
//! rayon) and assembling [`ScannedPage`]s.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lk_core::concept::slugify;
use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::frontmatter::{self, Frontmatter};
use lk_core::link;
use lk_core::vault_path::{CONCEPTS_SUBDIR, DOCUMENTS_SUBDIR, EXPLORATIONS_SUBDIR};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::GraphError;

/// A scanned vault page: its slug id, vault-relative path, display title, and its outgoing
/// links (deduped by address, first-appearance order).
///
/// Resolution happens at scan time: each internal markdown-link destination is resolved
/// against the page's own location ([`link::resolve_dest`]), so every downstream consumer is a
/// plain lookup with no second resolution to disagree about.
#[derive(Debug, Clone, Default)]
pub struct ScannedPage {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub outgoing: Vec<Link>,
}

/// One resolved link, carried as both of the things a consumer asks it for.
///
/// The two are not interchangeable, and conflating them answers the wrong question. `dest` is
/// an ADDRESS — the destination as the page named it — and only a path can answer "is there a
/// file there". `id` is the page id that address belongs to, which is the graph's node key.
/// Slugifying a path is lossy, so `Bad_Name.md` and `bad-name.md` share an id while only one of
/// them is a file: checking existence by id reports a link to the other one sound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    pub dest: String,
    /// `None` when the destination is not a vault address at all — it escapes the root. Such a
    /// link addresses no page, so it must not carry one: `path_slug` DROPS `..` segments, so
    /// `../../../wiki/concepts/kubernetes.md` would fold onto the real concept's id and be
    /// credited as a citation of it, exempt it from orphan detection, and add a graph edge —
    /// while `broken` reported the same link dead.
    pub id: Option<String>,
}

impl Link {
    /// A link to a vault-relative address. The id is DERIVED, so the pair cannot drift.
    pub fn to(dest: &str) -> Self {
        Self {
            id: Some(path_slug(Path::new(dest))),
            dest: dest.to_owned(),
        }
    }

    /// A destination that leaves the vault: an address, and no page.
    pub fn outside(dest: &str) -> Self {
        Self {
            id: None,
            dest: dest.to_owned(),
        }
    }
}

/// Walk the vault at `root` and parse every markdown file it holds.
///
/// The walk is the WHOLE vault, always. Which pages are analysed, and whose links are checked,
/// are questions about the result — [`VaultViews`] answers them by filtering — because a walk
/// that had already dropped a page cannot tell "not there" from "not looked at", in whichever
/// direction a caller happens to resolve it.
pub fn scan_vault(root: &Path, follow_links: bool) -> Result<Vec<ScannedPage>, GraphError> {
    if !root.exists() {
        return Err(GraphError::ScanDirNotFound(root.to_path_buf()));
    }

    // Dot-directories are never knowledge: `.obsidian` is the app's own config, `.trash` is
    // where Obsidian puts a DELETED page — resolving a link to one would report a deleted
    // page as present — and `.lorekeeper` is this tool's state.
    let walker = WalkDir::new(root)
        .follow_links(follow_links)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
        });

    let mut file_paths: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unreadable path");
                continue;
            }
        };
        let path = entry.path();
        // `is_file` FOLLOWS the link, so a symlinked page is a page: it is readable at exactly
        // this address, and a link to it resolves in every editor. `follow_links` decides
        // whether the walk descends symlinked directories, which is a separate question.
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            file_paths.push(path.to_path_buf());
        }
    }
    file_paths.sort();

    // A page that cannot be READ still EXISTS, and its id comes from the path rather than from
    // anything inside it — so it yields a page with no title and no links rather than vanishing.
    // Dropping it would make a link to a real file on disk read as broken, and leave it out of
    // every count of what the vault holds.
    let mut pages: Vec<ScannedPage> = file_paths
        .par_iter()
        .map(|path| {
            parse_file(path, root).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "unreadable page: id only");
                let rel = path.strip_prefix(root).unwrap_or(path);
                ScannedPage {
                    id: path_slug(rel),
                    path: rel.to_path_buf(),
                    title: path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("untitled")
                        .to_owned(),
                    outgoing: Vec::new(),
                }
            })
        })
        .collect();

    pages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pages)
}

/// ONE scan of the vault, partitioned into the three views that answer three different
/// questions. Conflating any two of them reports something false.
pub struct VaultViews {
    /// Every page in the vault. The existence universe, and the view the mutating commands
    /// repoint links across — a citation of a renamed or merged page lives wherever it was
    /// written.
    pub scanned: Vec<ScannedPage>,
    pub existence: VaultExistence,
    /// The pages this tool writes and manages: the analysis scope ∪ every page directory. A
    /// vault also holds the user's own folders, and a stray link in a hand-written note is
    /// neither this tool's output nor something it can repair.
    pub link_sources: Vec<ScannedPage>,
    /// The analysis scope — the graph's nodes and edges.
    pub pages: Vec<ScannedPage>,
}

impl VaultViews {
    /// `scope.exclude` and `scope.dirs` narrow the last two views only: an excluded page still
    /// exists, so it still resolves a link and still has its own links repointed by a rename.
    pub fn resolve(root: &Path, graph: &GraphConfig, dirs: &VaultDirs) -> Result<Self, GraphError> {
        // A configured directory must name exactly one directory on disk. Absent is a typo —
        // silently yielding nothing lets `wiki map` overwrite the map with an empty catalog and
        // exit 0, and every analysis command agree the vault has no pages. Two spellings
        // answering to it is a vault split in half: the pages under one would be analysed while
        // `wiki index` writes its catalog under the other. Where the filesystem folds the
        // spellings, both names reach one directory and the vault genuinely works.
        for dir in &graph.scope.dirs {
            resolve_dir(root, dir)?;
        }

        let scanned = scan_vault(root, graph.scope.follow_links)?;
        let existence = VaultExistence::build(&scanned, dirs);
        let excluded = Excludes::compile(&graph.scope.exclude)?;

        let managed = page_dirs(dirs);
        let in_scope = |page: &ScannedPage, scope: &[PathBuf]| {
            scope.iter().any(|dir| under(&page.id, dir)) && !excluded.matches(&page.path)
        };

        Ok(Self {
            link_sources: scanned
                .iter()
                .filter(|p| in_scope(p, &graph.scope.dirs) || in_scope(p, &managed))
                .cloned()
                .collect(),
            pages: scanned
                .iter()
                .filter(|p| in_scope(p, &graph.scope.dirs))
                .cloned()
                .collect(),
            scanned,
            existence,
        })
    }
}

/// Whether a page id sits under a configured vault-relative directory.
///
/// THE definition, shared by the analysis scope and by every consumer that asks what kind of
/// page a path is. Two of them comparing differently is how a vault gets analysed by one and
/// ignored by the other.
///
/// Compared on the ID, not the raw path: an id is the address after normalization, so a vault
/// whose directory on disk is spelled `Wiki` while the config says `wiki` still matches. A raw
/// prefix test answers no there — `is_dir` passes on a case-insensitive volume, so the directory
/// "exists", no page ever matches it, and every graph command reports an empty vault and exits 0.
pub(crate) fn under(id: &str, dir: &Path) -> bool {
    let prefix = dir_slug(dir);
    if prefix.is_empty() {
        return true;
    }
    id == prefix || id.starts_with(&format!("{prefix}/"))
}

/// The directory a configured vault-relative path names, matched segment by segment under the
/// same folding that decides scope membership — so a configured `wiki` finds a `Wiki/` on disk
/// whether or not the filesystem folds the spelling for us.
fn resolve_dir(root: &Path, dir: &Path) -> Result<PathBuf, GraphError> {
    let mut at = root.to_path_buf();
    for segment in dir.components() {
        let Component::Normal(segment) = segment else {
            continue;
        };
        let name = segment.to_string_lossy();
        // The configured path resolving IS the answer, and nothing beside it can make it
        // ambiguous: reads and writes both go to this directory, so the vault works. Asking a
        // further question here is what made an empty `.daily` — a directory the scan refuses to
        // look inside — remove the whole `daily/` tree from the checked set, and `graph broken`
        // report 0 on a vault with dead links.
        let exact = at.join(&*name);
        if exact.is_dir() {
            at = exact;
            continue;
        }
        // It does not resolve, so the analysis would be empty. A directory the FILESYSTEM would
        // call the same name is worth naming — that is a vault split in half, pages under one
        // spelling and the catalog written under the other. Anything else is a typo.
        let siblings: Vec<String> = std::fs::read_dir(&at)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        return match match_segment(&siblings, &name) {
            SegmentMatch::None => Err(GraphError::ScanDirNotFound(root.join(dir))),
            SegmentMatch::One(found) | SegmentMatch::Ambiguous(found, _) => {
                Err(GraphError::DirSpelling {
                    configured: dir.to_path_buf(),
                    on_disk: PathBuf::from(found),
                })
            }
        };
    }
    Ok(at)
}

/// Which directory a configured name reaches when the configured path itself does not resolve.
///
/// Pure, so the rule can be tested where the filesystem cannot say it: the cases that matter
/// only arise where two spellings stay apart, which the development machine folds.
///
/// Compared with [`lk_core::fs::names_fold_together`] — case and Unicode normalization, the
/// union of what the filesystems this ships to collapse. NOT `slugify`, which is the CONCEPT
/// name rule: it folds every run of non-alphanumerics too, so it called `daily_`, `daily-` and
/// `.daily` spellings of `daily` — names no filesystem confuses.
#[derive(Debug, PartialEq, Eq)]
enum SegmentMatch {
    None,
    One(String),
    Ambiguous(String, String),
}

fn match_segment(siblings: &[String], want: &str) -> SegmentMatch {
    let mut folded = siblings
        .iter()
        .filter(|s| lk_core::fs::names_fold_together(s, want));
    match (folded.next(), folded.next()) {
        (None, _) => SegmentMatch::None,
        (Some(one), None) => SegmentMatch::One(one.clone()),
        (Some(a), Some(b)) => SegmentMatch::Ambiguous(a.clone(), b.clone()),
    }
}

/// Every vault-relative page directory that exists on disk — anything this tool writes a page
/// into. Missing directories are skipped so a partially-populated vault doesn't error out.
fn page_dirs(dirs: &VaultDirs) -> Vec<PathBuf> {
    // Every one of them, unconditionally. Membership is decided by `under` over page ids, so a
    // directory the vault has not created yet simply matches nothing — while a filesystem check
    // here could DROP a directory that does exist, and dropping `daily` takes every link written
    // on a daily page out of the checked set without saying so.
    [&dirs.wiki, &dirs.daily, &dirs.personal, &dirs.synthesis]
        .iter()
        .map(|s| PathBuf::from(s.as_str()))
        .collect()
}

fn parse_file(path: &Path, root: &Path) -> Result<ScannedPage, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    // A page's links live in its BODY, so the body comes from the page's own delimiters and a
    // frontmatter that does not parse costs only the title. Taking the whole page as unreadable
    // instead loses every link it wrote: a broken one goes unreported, and each page it cited
    // reads as an orphan. That the frontmatter is unparseable is a defect in its own right, and
    // `lore doctor` is what reports it — the scan's question is what the page points at.
    let parts = frontmatter::split_page(&raw);
    let title = frontmatter::parse_page(&raw)
        .ok()
        .as_ref()
        .and_then(|page| frontmatter_title(&page.frontmatter))
        .or_else(|| extract_first_heading(&parts.body))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_owned()
        });

    let rel = path.strip_prefix(root).unwrap_or(path);

    let mut outgoing = Vec::new();
    let mut seen = HashSet::new();
    for dest in link::extract_dests(&parts.body) {
        // Only `.md` destinations address knowledge pages; a link to any other file
        // (an attachment) is not a graph edge.
        if !Path::new(&dest).extension().is_some_and(|ext| ext == "md") {
            continue;
        }
        // Escapes the vault root: keep the destination as written — it names no file in the
        // vault, so it is reported broken rather than dropped, and it carries no page id.
        let link = match link::resolve_dest(rel, &dest) {
            Some(resolved) => Link::to(&rel_str(&resolved)),
            None => Link::outside(&dest),
        };
        // Deduped by ADDRESS, not by id: two dead spellings of one id are two broken links,
        // and each names its own repair.
        if !link.dest.is_empty() && seen.insert(link.dest.clone()) {
            outgoing.push(link);
        }
    }

    Ok(ScannedPage {
        id: path_slug(rel),
        path: rel.to_path_buf(),
        title,
        outgoing,
    })
}

fn frontmatter_title(fm: &Frontmatter) -> Option<String> {
    fm.get("title")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn extract_first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return Some(heading.trim().to_owned());
        }
    }
    None
}

/// Every page on disk, for the integrity checks that must reason about the vault rather than
/// the analysis scope (`graph.scope.dirs`): a `wiki/` concept linking a `daily/` page is not a
/// broken link, and a concept linked only from `daily/` is not an orphan.
///
/// Built from a scan of the vault ROOT, so "absent" means absent from the vault.
#[derive(Debug, Clone)]
pub struct VaultExistence {
    /// Every page ADDRESS the scan found, keyed by [`lk_core::fs::fold_name`] and holding the
    /// spelling as it is on disk. Catalogs INCLUDED, since a catalog is a file a page may
    /// legitimately link.
    ///
    /// Keyed folded because that is how an address resolves; the value is kept because the
    /// difference between the two is itself reportable — a link that only resolves under the
    /// fold is dead on a filesystem that does not fold.
    paths: HashMap<String, String>,
    /// Every page id, minus the generated catalogs: the connectivity question orphan detection
    /// asks.
    /// `index.md` links every page it catalogs, so counting a link to it as a connection would
    /// mask exactly the orphans that detection exists to find.
    knowledge: HashSet<String>,
    /// Page ids that are the resolved target of a link from *another* page
    /// (self-links excluded). Drives orphan inbound exemption.
    linked: HashSet<String>,
}

impl VaultExistence {
    /// Derive the universe from a page scan. The caller passes every page the vault has —
    /// integrity commands scan from the vault ROOT for exactly this reason — so an id absent
    /// from `ids` is absent from the vault, not merely from somewhere nobody looked.
    pub fn build(pages: &[ScannedPage], dirs: &VaultDirs) -> Self {
        // Navigation/catalog meta-files (index.md, log.md, map.md, AGENTS.md) are generated
        // artifacts, not knowledge. They are real files, so they RESOLVE as link targets; what
        // they must not do is count as connectivity, since index.md links every page it catalogs
        // and would mark every concept "linked", defeating orphan detection.
        let is_reserved = reserved_page_predicate(Path::new(&dirs.wiki));
        let mut paths: HashMap<String, String> = HashMap::with_capacity(pages.len());
        let mut knowledge = HashSet::with_capacity(pages.len());
        for page in pages {
            let spelled = rel_str(&page.path);
            // First spelling wins, and the scan is sorted, so two files that fold together
            // resolve to the same one every run. That they are two is `address_collisions`.
            paths
                .entry(lk_core::fs::fold_name(&spelled))
                .or_insert(spelled);
            if !is_reserved(page.id.as_str()) {
                knowledge.insert(page.id.clone());
            }
        }

        // Connectivity asks the same question `reached` answers, so it asks `reached` — a
        // second spelling of the rule here is a second answer to "what does this link reach",
        // and the three consumers that disagreed over exactly that is what the rule exists to
        // prevent. Collected first because reading it borrows the set being filled.
        let mut existence = Self {
            paths,
            knowledge,
            linked: HashSet::new(),
        };
        existence.linked = pages
            .iter()
            .filter(|page| !is_reserved(page.id.as_str()))
            .flat_map(|page| {
                page.outgoing.iter().filter_map(|link| {
                    let id = existence.reached(link)?;
                    (id != page.id && existence.is_knowledge(id)).then(|| id.to_owned())
                })
            })
            .collect();
        existence
    }

    /// Whether a link's address names a file in the vault — a knowledge page or a generated
    /// catalog, both of which are files on disk.
    ///
    /// Compared under [`lk_core::fs::fold_name`], the rule the directory segments of an address
    /// already resolve by, so one address resolves one way along its whole path:
    /// `wiki/concepts/foo.md` and a file stored as `.../Foo.md` are one file on the filesystem
    /// this vault is most often edited on, and `cat` opens it at either spelling.
    ///
    /// What a CASE difference costs when the vault syncs to a filesystem that keeps the two
    /// apart is [`Self::respelled`]'s question, reported under its own violation channel with the
    /// repair the link needs. Neither `unnormalized` nor `address_collisions` covers it — the
    /// first judges a FILE's name and the file here is already its own slug, the second needs two
    /// files and there is one. A NORMALIZATION difference is `unnormalized`'s: both sides here
    /// have been through [`rel_str`], so by this point there is none left to see.
    pub fn is_resolvable(&self, dest: &str) -> bool {
        self.paths.contains_key(&lk_core::fs::fold_name(dest))
    }

    /// The on-disk spelling of the file `dest` reaches, when it differs from `dest` itself.
    ///
    /// The other half of the fold: `wiki/concepts/ALPHA.md` opens `alpha.md` here and opens
    /// nothing on ext4, and neither `unnormalized` (which judges a FILE's name) nor
    /// `address_collisions` (which needs two files) has anything to say about it — the file is
    /// its own slug and there is only one of it. Reporting the difference is what makes folding
    /// a resolution rule rather than a way to lose the question.
    pub fn respelled<'a>(&'a self, dest: &str) -> Option<&'a str> {
        let on_disk = self.paths.get(&lk_core::fs::fold_name(dest))?;
        (on_disk != dest).then_some(on_disk.as_str())
    }

    /// The page a link actually REACHES: its id, and only when the address it names is a file.
    ///
    /// A link is one thing to a reader — it opens a page or it does not — so it has to be one
    /// thing to the graph. Reading the id alone lets a dead spelling that slugifies onto a real
    /// page become an edge, a citation in that page's `## Sources`, and an orphan exemption,
    /// while `broken` reports the very same link missing: one vault, three answers. Every
    /// consumer that turns a link into a graph fact goes through here.
    pub fn reached<'a>(&self, link: &'a Link) -> Option<&'a str> {
        if !self.is_resolvable(&link.dest) {
            return None;
        }
        link.id.as_deref()
    }

    /// Whether a resolved link target addresses a KNOWLEDGE page — the question orphan
    /// connectivity asks, where reaching a generated catalog is not reaching anything.
    pub fn is_knowledge(&self, target: &str) -> bool {
        self.knowledge.contains(target)
    }

    /// Whether `page_id` is the resolved target of a link from another page.
    pub fn is_linked(&self, page_id: &str) -> bool {
        self.linked.contains(page_id)
    }
}

/// Whether a page is a concept page (`{dirs.wiki}/concepts/{slug}.md`). Anchored to
/// the configured `dirs.wiki` — never a hardcoded path segment — and shared by
/// `backlinks` and the concept lints.
pub(crate) fn is_concept_page(path: &Path, dirs: &VaultDirs) -> bool {
    under(
        &path_slug(path),
        &Path::new(&dirs.wiki).join(CONCEPTS_SUBDIR),
    ) && path.extension().is_some_and(|ext| ext == "md")
}

/// True iff this vault-relative path can act as a citation *source* — an event,
/// work-log, synthesis, document, or exploration page where a concept appearance is
/// meaningful provenance. Concept-to-concept links and navigation pages are excluded:
/// cross-references between concepts are curated structure (`## Related`), not activity.
/// Single-sourced here as THE definition of "a real source" — `backlinks` (what fills
/// `## Sources`) resolves citations through it.
pub(crate) fn is_valid_source(path: &Path, dirs: &VaultDirs) -> bool {
    let id = path_slug(path);
    let wiki = Path::new(&dirs.wiki);
    [
        Path::new(&dirs.daily).to_path_buf(),
        Path::new(&dirs.personal).to_path_buf(),
        Path::new(&dirs.synthesis).to_path_buf(),
        wiki.join(DOCUMENTS_SUBDIR),
        wiki.join(EXPLORATIONS_SUBDIR),
    ]
    .iter()
    .any(|dir| under(&id, dir))
}

/// Page ids of Lorekeeper's reserved wiki meta files (the index catalog, the time log,
/// the navigation map, and the AGENTS.md schema doc) under `wiki_dir`, e.g. `wiki/index`,
/// `wiki/log`, `wiki/map`, `wiki/agents`.
/// Single-sourced from [`lk_core::vault_path::RESERVED_WIKI_FILES`] so the graph's
/// orphan / index-drift checks exclude exactly what the index builder skips.
pub fn reserved_page_ids(wiki_dir: &Path) -> Vec<String> {
    lk_core::vault_path::RESERVED_WIKI_FILES
        .iter()
        .map(|name| {
            let stem = Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(name);
            path_slug(&wiki_dir.join(stem))
        })
        .collect()
}

/// Returns a predicate matching any reserved navigation/catalog meta page
/// (`index.md`/`log.md`/`map.md`/`AGENTS.md`) that must stay out of the analysis graph
/// (nodes AND edges): they link every page, so as nodes they would be spurious mega-hubs
/// that merge separate communities and mask real orphans.
pub fn reserved_page_predicate(wiki_dir: &Path) -> impl Fn(&str) -> bool {
    let ids: HashSet<String> = reserved_page_ids(wiki_dir).into_iter().collect();
    move |id: &str| ids.contains(id)
}

/// A vault-relative path as one comparable address: separators normalized to `/`, and the text
/// to Unicode NFC.
///
/// The spelling IS the address — a link resolves to a file only if the path it names is the one
/// on disk — but two byte sequences can spell the same name. A composed and a decomposed Hangul
/// filename look identical, address the same file on every filesystem that stores either, and
/// differ as strings; HFS+ decomposed on write, APFS keeps what it is given, and a sync tool
/// can hand a vault both. Comparing without normalizing would report every link into such a
/// directory broken, which is Unicode canonical equivalence being ignored rather than a defect
/// in the vault. Both sides of every comparison come through here, so they cannot disagree.
pub fn rel_str(rel: &Path) -> String {
    use unicode_normalization::UnicodeNormalization;
    rel.to_string_lossy().replace('\\', "/").nfc().collect()
}

/// Two files whose paths slugify to ONE page id.
///
/// The id is the graph's node key, so only one of them can hold the node: the other's edges are
/// attributed to its twin, and it is absent from every list derived from the node map while
/// still counted in the totals. `wiki/documents/A B.md` beside `wiki/documents/a-b.md` is enough
/// — no exotic filesystem needed — which is why this is reported rather than resolved: which of
/// the two the tool should keep is not its call to make.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AddressCollision {
    pub id: String,
    pub paths: Vec<String>,
}

pub fn address_collisions(pages: &[ScannedPage]) -> Vec<AddressCollision> {
    let mut by_id: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for page in pages {
        by_id
            .entry(page.id.as_str())
            .or_default()
            .push(rel_str(&page.path));
    }
    by_id
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(id, mut paths)| {
            paths.sort();
            AddressCollision {
                id: id.to_owned(),
                paths,
            }
        })
        .collect()
}

/// Node key for a vault-relative path: drop the extension, normalize separators to `/`, then
/// name each segment by the rule that segment is named under — directories by the filesystem
/// (`lk_core::fs::fold_name`), the file by the concept slug (so `wiki/Concept A.md` →
/// `wiki/concept-a`).
///
/// A concept page's FILE is named by its slug, which is why two files whose stems slugify alike
/// are one address and a violation. A DIRECTORY is not: the vault addresses it literally, so
/// `slugify` applied there answers a question nobody asked and folds directories the filesystem
/// keeps apart. `daily_/` beside `daily/` became one id: a broken link written on a page under
/// `daily_/` was reported against the page under `daily/`, where a reader looking for it finds
/// no such link, and the two real files at two real addresses were reported as one address with
/// two files.
pub fn path_slug(rel: &Path) -> String {
    let no_ext = rel.with_extension("");
    let s = no_ext.to_string_lossy().replace('\\', "/");
    let mut segments: Vec<&str> = s.split('/').collect();
    let stem = segments.pop();
    join_segments(
        segments
            .into_iter()
            .map(lk_core::fs::fold_name)
            .chain(stem.and_then(slugify)),
    )
}

/// The id prefix a vault-relative DIRECTORY contributes: every segment is a directory, so every
/// segment is named by the filesystem rule. Sharing [`path_slug`] here would slugify the last
/// segment as though it were a page's file, and a configured `daily_` would then match no page
/// at all — it would ask for the prefix `daily`, which is not what any page under it carries.
pub(crate) fn dir_slug(dir: &Path) -> String {
    let s = dir.to_string_lossy().replace('\\', "/");
    join_segments(s.split('/').map(lk_core::fs::fold_name))
}

fn join_segments(segments: impl Iterator<Item = String>) -> String {
    segments
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// The `graph.scope.exclude` globs as a predicate over a vault-relative path.
///
/// Exclusion narrows the ANALYSIS — which pages are graph nodes — not the vault. An excluded
/// page still exists, so a link to it resolves; its existence is not the exclusion's business.
/// Integrity commands therefore scan without the globs and apply this to the node set.
pub struct Excludes(GlobSet);

impl Excludes {
    pub fn compile(patterns: &[String]) -> Result<Self, GraphError> {
        build_exclude_set(patterns).map(Self)
    }

    /// Matched against the same form the scan uses: vault-relative, `/`-separated.
    pub fn matches(&self, rel: &Path) -> bool {
        self.0
            .is_match(rel.to_string_lossy().replace('\\', "/").as_str())
    }
}

fn build_exclude_set(patterns: &[String]) -> Result<GlobSet, GraphError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| GraphError::InvalidExclude(pattern.clone(), e.to_string()))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| GraphError::InvalidExclude("<set>".to_string(), e.to_string()))
}

#[cfg(test)]
mod tests {
    /// Links as the ADDRESSES they name, which is what existence is asked about.
    fn dests(page: &ScannedPage) -> Vec<&str> {
        page.outgoing.iter().map(|l| l.dest.as_str()).collect()
    }

    /// A directory symlink, or `false` where the platform or filesystem refuses one. Windows
    /// needs a privilege most accounts lack, so the caller treats absence as "skip this case".
    fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link);
        made.is_ok()
    }

    use super::*;

    #[test]
    fn reserved_predicate_excludes_only_meta_files() {
        let is_reserved = reserved_page_predicate(Path::new("wiki"));
        assert!(is_reserved("wiki/index"));
        assert!(is_reserved("wiki/log"));
        assert!(is_reserved("wiki/map"));
        assert!(is_reserved("wiki/agents"));
        // Real knowledge nodes are NOT reserved.
        assert!(!is_reserved("wiki/concepts/rag"));
        assert!(!is_reserved("wiki/documents/report"));
        // A concept whose slug merely contains "index" is not a meta page.
        assert!(!is_reserved("wiki/concepts/index-fund"));
    }

    /// `is_valid_source` is the single definition of what may cite a concept, and
    /// `backlinks-sync` derives every concept's sources section and `source_count` from
    /// exactly the pages it admits — wiping any entry not backed by an admitted one. So a
    /// page class dropped here does not merely go uncounted: its citations are erased from
    /// the concept pages that carry them, with nothing left recording that they existed.
    /// Each accepted root therefore needs its own case; covering one incidentally leaves the
    /// rest free to be narrowed silently.
    #[test]
    fn every_page_class_that_may_cite_a_concept_is_admitted() {
        let dirs = VaultDirs::default();
        for cites in [
            "daily/team-slack/2026-05-22.md",
            "me/work-log/2026-05-22.md",
            "synthesis/weekly/2026-W21.md",
            "wiki/documents/report.md",
            "wiki/explorations/why-rag.md",
        ] {
            assert!(
                is_valid_source(Path::new(cites), &dirs),
                "{cites} may cite a concept"
            );
        }
        // A concept's own links are curated structure, not provenance, and the meta pages
        // are generated FROM the graph — admitting either would cite from the output.
        for not_a_source in [
            "wiki/concepts/rag.md",
            "wiki/index.md",
            "wiki/log.md",
            "wiki/map.md",
            "wiki/AGENTS.md",
        ] {
            assert!(
                !is_valid_source(Path::new(not_a_source), &dirs),
                "{not_a_source} is not provenance"
            );
        }
    }

    #[test]
    fn an_unparseable_page_still_exists() {
        // Its id comes from the PATH, so nothing inside the file is needed to know it is there.
        // Dropping it would make a link to a real file on disk read as broken.
        let tmp = tempfile::TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("good.md"), "---\nid: good\n---\n\n# Good\n").unwrap();
        std::fs::write(wiki.join("broken.md"), "---\nid: broken\ntitle: unclosed\n").unwrap();

        let pages = scan_vault(tmp.path(), false).unwrap();

        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["wiki/broken", "wiki/good"]);
        let unparseable = pages.iter().find(|p| p.id == "wiki/broken").unwrap();
        assert!(
            unparseable.outgoing.is_empty(),
            "a page whose body did not parse contributes no links"
        );
    }

    #[test]
    fn a_link_that_leaves_the_vault_addresses_no_page() {
        // `path_slug` DROPS `..` segments, so an escaping destination would fold onto whatever
        // real page its tail names: credited as a citation of it, exempting it from orphan
        // detection and adding a graph edge — while `broken` reported the same link dead.
        let tmp = tempfile::TempDir::new().unwrap();
        let daily = tmp.path().join("daily/src");
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki/concepts")).unwrap();
        std::fs::write(tmp.path().join("wiki/concepts/kubernetes.md"), "# K8s\n").unwrap();
        std::fs::write(
            daily.join("2026-01-03.md"),
            "# Day\n\n[K8s](../../../wiki/concepts/kubernetes.md)\n",
        )
        .unwrap();

        let pages = scan_vault(tmp.path(), false).unwrap();
        let citer = pages
            .iter()
            .find(|p| p.id == "daily/src/2026-01-03")
            .unwrap();
        assert_eq!(citer.outgoing.len(), 1);
        assert_eq!(citer.outgoing[0].id, None, "it names no page in this vault");

        let existence = VaultExistence::build(&pages, &VaultDirs::default());
        assert!(!existence.is_resolvable(&citer.outgoing[0].dest));
        assert!(
            !existence.is_linked("wiki/concepts/kubernetes"),
            "a link that leaves the vault is not a link to the page it happens to name"
        );
    }

    #[test]
    fn a_folded_directory_spelling_is_the_same_directory_to_every_consumer() {
        // Scope membership folds the spelling; `is_concept_page`/`is_valid_source` must fold it
        // the same way, or the vault is analysed by one and invisible to the other —
        // `backlinks-sync` recognizing no concept page at all.
        let dirs = VaultDirs::default();
        assert!(is_concept_page(Path::new("Wiki/concepts/x.md"), &dirs));
        assert!(is_concept_page(Path::new("wiki/concepts/x.md"), &dirs));
        assert!(!is_concept_page(Path::new("wiki/documents/x.md"), &dirs));
        assert!(is_valid_source(Path::new("Daily/src/2026-01-01.md"), &dirs));
        assert!(is_valid_source(Path::new("Wiki/documents/x.md"), &dirs));
        assert!(!is_valid_source(Path::new("wiki/concepts/x.md"), &dirs));
    }

    #[test]
    fn only_a_name_a_filesystem_could_confuse_answers_for_another() {
        // The rule, tested where the filesystem cannot say it: this machine folds `wiki` onto
        // `Wiki`, so a vault holding both as SEPARATE directories cannot be built here.
        //
        // `slugify` was the wrong comparison and cost the most: it folds every run of
        // non-alphanumerics, so an empty `.daily` — a directory the scan refuses to look inside —
        // read as a rival spelling of the daily page dir, took `daily` out of the checked set,
        // and `graph broken` reported 0 on a vault with dead links.
        let dirs = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        for imposter in [".daily", "daily_", "daily-", "_daily", "daily.md", "dai-ly"] {
            assert_eq!(
                match_segment(&dirs(&[imposter]), "daily"),
                SegmentMatch::None,
                "{imposter} is a different name to every filesystem"
            );
        }
        assert_eq!(
            match_segment(&dirs(&["Wiki", "daily"]), "wiki"),
            SegmentMatch::One("Wiki".into()),
            "case is folded by APFS and NTFS"
        );
        assert_eq!(
            match_segment(&dirs(&["Wiki", "WIKI"]), "wiki"),
            SegmentMatch::Ambiguous("Wiki".into(), "WIKI".into())
        );
        assert_eq!(match_segment(&dirs(&["daily"]), "wiki"), SegmentMatch::None);
        assert_eq!(match_segment(&dirs(&[]), "wiki"), SegmentMatch::None);
    }

    #[test]
    fn a_configured_directory_is_resolved_the_way_the_filesystem_answers() {
        // Where both spellings reach one directory the vault works and its pages are analysed;
        // where they are kept apart the vault is split and running is worse than refusing.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Wiki/concepts")).unwrap();
        std::fs::write(tmp.path().join("Wiki/concepts/x.md"), "# X\n").unwrap();
        let folds = tmp.path().join("wiki").is_dir();

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki")];
        let resolved = VaultViews::resolve(tmp.path(), &config, &VaultDirs::default());

        if folds {
            let views = resolved.expect("one directory under two names");
            assert_eq!(
                views
                    .pages
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["wiki/concepts/x"]
            );
        } else {
            assert!(
                matches!(resolved, Err(GraphError::DirSpelling { .. })),
                "two directories must be refused, naming the spelling on disk"
            );
        }
    }

    #[test]
    fn a_sibling_no_filesystem_confuses_leaves_the_page_dirs_alone() {
        // The regression this replaces: an empty `.daily` beside `daily/` dropped every page
        // under `daily/` from the set whose links are checked, silently.
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["wiki/concepts", "daily/notes", ".daily", "daily_", "_daily"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(
            tmp.path().join("daily/notes/2026-05-24.md"),
            "# Day\n\n[Ghost](../../wiki/concepts/ghost.md)\n",
        )
        .unwrap();

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki")];
        let views = VaultViews::resolve(tmp.path(), &config, &VaultDirs::default()).unwrap();
        assert!(
            views
                .link_sources
                .iter()
                .any(|p| p.id == "daily/notes/2026-05-24"),
            "the daily page is still this tool's to check"
        );
    }

    #[test]
    fn a_configured_scope_directory_that_is_not_there_is_an_error() {
        // Not an empty analysis: `wiki map` would overwrite the map with an empty catalog and
        // exit 0, and every analysis command would agree the vault has no pages.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wikii")];
        assert!(VaultViews::resolve(tmp.path(), &config, &VaultDirs::default()).is_err());
    }

    #[test]
    fn two_files_whose_names_differ_only_by_normalization_are_an_address_collision() {
        // Comparing addresses under NFC is what makes a composed link reach a decomposed file.
        // The price is that a vault holding BOTH spellings as separate files — possible only
        // where the filesystem preserves and distinguishes them — has two files at one address.
        // That is reported, by the channel whose subject it is, rather than silently resolved.
        use unicode_normalization::UnicodeNormalization;
        let composed = "wiki/concepts/개념.md".to_string();
        let decomposed: String = composed.nfd().collect();
        assert_ne!(composed, decomposed, "the two spellings differ as bytes");

        let pages = vec![
            ScannedPage {
                id: path_slug(Path::new(&composed)),
                path: PathBuf::from(&composed),
                ..ScannedPage::default()
            },
            ScannedPage {
                id: path_slug(Path::new(&decomposed)),
                path: PathBuf::from(&decomposed),
                ..ScannedPage::default()
            },
        ];
        let collisions = address_collisions(&pages);
        assert_eq!(collisions.len(), 1, "one address, two files");
        assert_eq!(collisions[0].paths.len(), 2);
    }

    #[test]
    fn a_decomposed_filename_answers_a_composed_link() {
        // HFS+ decomposed on write and APFS keeps what it is given, so one vault can hold a
        // decomposed filename and a composed link to it. They name the same file everywhere;
        // comparing the bytes would report every link into such a directory broken.
        let tmp = tempfile::TempDir::new().unwrap();
        let concepts = tmp.path().join("wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        let decomposed: String = {
            use unicode_normalization::UnicodeNormalization;
            "개념".nfd().collect()
        };
        std::fs::write(concepts.join(format!("{decomposed}.md")), "# 개념\n").unwrap();
        std::fs::write(concepts.join("cites.md"), "# Cites\n\n[개념](개념.md)\n").unwrap();

        let pages = scan_vault(tmp.path(), false).unwrap();
        let existence = VaultExistence::build(&pages, &VaultDirs::default());
        let citer = pages
            .iter()
            .find(|p| p.id == "wiki/concepts/cites")
            .unwrap();
        assert!(
            existence.is_resolvable(&citer.outgoing[0].dest),
            "a composed link must resolve to the decomposed file it names"
        );
    }

    #[test]
    fn a_page_whose_frontmatter_does_not_parse_still_links_what_it_links() {
        // The frontmatter is a mapping `serde_yaml` refuses (`title: Notes: today`), which is
        // exactly what an agent's `Edit` on a marker line produces. Dropping the body's links
        // with it reports one broken link fewer and one orphan more, both silently.
        let tmp = tempfile::TempDir::new().unwrap();
        let daily = tmp.path().join("daily/notes");
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::write(
            daily.join("2026-05-24.md"),
            "---\nid: n\ntitle: Notes: today\n---\n\n# Notes\n\n[Cited](../../wiki/c.md)\n",
        )
        .unwrap();

        let pages = scan_vault(tmp.path(), false).unwrap();
        let page = pages
            .iter()
            .find(|p| p.id == "daily/notes/2026-05-24")
            .unwrap();
        assert_eq!(dests(page), vec!["wiki/c.md"]);
        assert_eq!(
            page.title, "Notes",
            "the title falls back to the body heading"
        );
    }

    #[test]
    fn vault_existence_tracks_ids_and_linked() {
        let pages = vec![
            ScannedPage {
                id: "daily/team-slack/2026-05-22".to_owned(),
                path: PathBuf::from("daily/team-slack/2026-05-22.md"),
                title: "t".to_owned(),
                outgoing: vec![Link::to("wiki/concepts/confluence-cloud.md")],
            },
            ScannedPage {
                id: "wiki/concepts/confluence-cloud".to_owned(),
                path: PathBuf::from("wiki/concepts/confluence-cloud.md"),
                title: "Confluence Cloud".to_owned(),
                outgoing: vec![],
            },
        ];
        let ex = VaultExistence::build(&pages, &VaultDirs::default());
        assert!(ex.is_resolvable("daily/team-slack/2026-05-22.md"));
        assert!(ex.is_resolvable("wiki/concepts/confluence-cloud.md"));
        assert!(!ex.is_resolvable("nope.md"));
        // The daily page links the concept → its page id is a link target.
        assert!(ex.is_linked("wiki/concepts/confluence-cloud"));
        // The daily page itself is linked by nobody.
        assert!(!ex.is_linked("daily/team-slack/2026-05-22"));
    }

    #[test]
    fn path_slug_basic() {
        assert_eq!(path_slug(Path::new("wiki/Concept A.md")), "wiki/concept-a");
        assert_eq!(path_slug(Path::new("wiki/Bad_Name.md")), "wiki/bad-name");
    }

    #[test]
    fn path_slug_preserves_directory_structure() {
        assert_eq!(
            path_slug(Path::new("wiki/sub/Topic Name.md")),
            "wiki/sub/topic-name"
        );
    }

    /// A directory is addressed literally, so two directories the filesystem keeps apart stay
    /// two ids. Slugifying them folded `daily_/` onto `daily/`: a broken link written under one
    /// was reported against a page in the other, where nothing of the kind is written, and the
    /// two real files were reported as one address holding two.
    #[test]
    fn a_directory_is_named_by_the_filesystem_not_by_the_concept_rule() {
        assert_ne!(
            path_slug(Path::new("daily_/notes/2026-07-30.md")),
            path_slug(Path::new("daily/notes/2026-07-30.md"))
        );
        assert_eq!(
            path_slug(Path::new("daily_/notes/2026-07-30.md")),
            "daily_/notes/2026-07-30"
        );
        // The FILE is still named by the concept rule, which is what makes two stems that
        // slugify alike one address and a violation.
        assert_eq!(
            path_slug(Path::new("wiki/concepts/Bad_Name.md")),
            path_slug(Path::new("wiki/concepts/bad-name.md"))
        );
        // And a configured directory asks for the prefix its own pages carry.
        assert!(under("daily_/notes/2026-07-30", Path::new("daily_")));
        assert!(!under("daily_/notes/2026-07-30", Path::new("daily")));
        assert!(under("daily/notes/2026-07-30", Path::new("daily")));
    }

    #[test]
    fn parse_file_resolves_links_against_page_location() {
        let tmp = tempfile::tempdir().unwrap();
        let daily = tmp.path().join("daily/x");
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::write(
            daily.join("2026-05-22.md"),
            "---\ntitle: Day\n---\n\n# Day\n\n\
             [K](../../wiki/concepts/kubernetes.md) and again \
             [K](../../wiki/concepts/kubernetes.md), plus [ext](https://x.y/z.md).\n",
        )
        .unwrap();
        let page = parse_file(&daily.join("2026-05-22.md"), tmp.path()).unwrap();
        assert_eq!(page.id, "daily/x/2026-05-22");
        assert_eq!(page.title, "Day");
        // Both links resolve to the same page id and dedupe; the external is skipped.
        assert_eq!(dests(&page), vec!["wiki/concepts/kubernetes.md"]);
    }

    #[test]
    fn parse_file_skips_non_md_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("a.md"),
            "[pdf](../files/report.pdf) and [page](concepts/b.md)\n",
        )
        .unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        assert_eq!(dests(&page), vec!["wiki/concepts/b.md"]);
    }

    #[test]
    fn parse_file_keeps_escaping_dest_as_broken_target() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "[out](../../outside/x.md)\n").unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        // The destination escapes the vault root: kept as written, never resolvable.
        assert_eq!(dests(&page), vec!["../../outside/x.md"]);
    }

    #[test]
    fn title_falls_back_to_heading_then_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("h.md"), "# Just A Heading\n\nbody\n").unwrap();
        std::fs::write(wiki.join("s.md"), "no heading here\n").unwrap();
        let h = parse_file(&wiki.join("h.md"), tmp.path()).unwrap();
        let s = parse_file(&wiki.join("s.md"), tmp.path()).unwrap();
        assert_eq!(h.title, "Just A Heading");
        assert_eq!(s.title, "s");
    }

    /// Two scope directories that name ONE directory must yield each page once. The
    /// filesystem calls them the same directory for spellings the config sees as distinct —
    /// case on macOS, NFC against NFD on APFS, a `.` segment — and a page scanned twice is
    /// scanned under two prefixes, so one copy types as a concept page and the other as a
    /// page citing it. `backlinks-sync` then writes the concept's own curated
    /// cross-references into its sources, which is the one thing citation derivation must
    /// never do. A `.` segment will not stand in for the case here: Rust's `Path` equality
    /// already folds it, so the text dedup this replaced would pass such a case while still
    /// failing on the spellings that actually occur. A second name for one directory is the
    /// real shape, and a symlink is the portable way to make one.
    #[test]
    fn a_page_reachable_at_two_addresses_exists_at_both() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n\n[b](b.md)\n").unwrap();
        std::fs::write(wiki.join("b.md"), "# B\n").unwrap();
        if !symlink_dir(&wiki, &tmp.path().join("notes")) {
            return;
        }

        // `follow_links` is the vault owner declaring that a symlink is part of the vault, so
        // both spellings are addresses a reader can open — and a link to either resolves.
        // Collapsing them onto one would make the other read as broken, and which of the two
        // survived would depend on readdir order.
        let pages = scan_vault(tmp.path(), true).unwrap();
        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["notes/a", "notes/b", "wiki/a", "wiki/b"]);

        // Left alone, a symlink is not walked into, so the vault is what the walk finds.
        let pages = scan_vault(tmp.path(), false).unwrap();
        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["wiki/a", "wiki/b"]);
    }

    #[test]
    fn the_views_narrow_what_is_analysed_and_never_what_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("keep.md"), "# Keep\n").unwrap();
        std::fs::write(wiki.join("skip.md"), "# Skip\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("private")).unwrap();
        std::fs::write(tmp.path().join("private/note.md"), "# Note\n").unwrap();

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki")];
        config.scope.exclude = vec!["wiki/skip.md".to_string()];
        let views = VaultViews::resolve(tmp.path(), &config, &VaultDirs::default()).unwrap();

        assert_eq!(
            views
                .pages
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["wiki/keep"],
            "the analysis scope is narrowed by both settings"
        );
        for id in ["wiki/keep", "wiki/skip", "private/note"] {
            assert!(
                views.existence.is_resolvable(&format!("{id}.md")),
                "{id} exists whatever the analysis scope is"
            );
        }
        assert!(
            !views
                .link_sources
                .iter()
                .any(|p| p.id == "private/note" || p.id == "wiki/skip"),
            "neither a user's own folder nor an excluded page is this tool's to lint"
        );
    }

    #[test]
    fn a_vault_root_that_is_not_there_is_an_error() {
        assert!(scan_vault(Path::new("/nonexistent-vault-root"), false).is_err());
    }

    #[test]
    fn a_directory_matches_the_configured_spelling_the_filesystem_folded() {
        // `is_dir` passes on a case-insensitive volume, so the directory "exists" while a raw
        // prefix test matches no page — every graph command then reports an empty vault, green.
        assert!(under("wiki/concepts/x", Path::new("Wiki")));
        assert!(under("wiki/concepts/x", Path::new("wiki")));
        assert!(!under("wikipedia/x", Path::new("wiki")));
        assert!(under("anything", Path::new("")));
    }

    #[test]
    fn two_files_at_one_address_are_reported() {
        let pages = vec![
            ScannedPage {
                id: "wiki/documents/a-b".to_owned(),
                path: PathBuf::from("wiki/documents/A B.md"),
                ..ScannedPage::default()
            },
            ScannedPage {
                id: "wiki/documents/a-b".to_owned(),
                path: PathBuf::from("wiki/documents/a-b.md"),
                ..ScannedPage::default()
            },
            ScannedPage {
                id: "wiki/documents/c".to_owned(),
                path: PathBuf::from("wiki/documents/c.md"),
                ..ScannedPage::default()
            },
        ];

        let collisions = address_collisions(&pages);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].id, "wiki/documents/a-b");
        assert_eq!(
            collisions[0].paths,
            vec!["wiki/documents/A B.md", "wiki/documents/a-b.md"]
        );
    }
}
