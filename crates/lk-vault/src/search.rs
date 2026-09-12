//! Vault search — the lookup a reader does before writing, and the one an agent does
//! instead of reading the catalog.
//!
//! `index.md` is a catalog of every page and grows with the vault; past a few thousand pages
//! reading it to locate one concept costs more than the answer is worth, and an agent has no
//! way to read part of it. This answers the same question by query instead: which pages
//! ANSWER to this, and which merely mention it.
//!
//! There is no score. A hit is ordered by WHERE it matched — the page's own names, then the
//! line it opens with, then anywhere in its prose — and within that by how much evidence the
//! page carries. Both are facts the page states, so the ordering is explainable hit by hit and
//! reproducible across runs, which a weighted blend of the two would not be.

use std::path::{Path, PathBuf};

use lk_core::concept::identity_key;
use lk_core::config::VaultDirs;
use lk_core::frontmatter::parse_page;
use lk_core::link;
use lk_core::vault_path::{concepts_dir, documents_dir, explorations_dir};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::VaultError;

/// Where a query matched a page, from the strongest claim to the weakest. The derived
/// ordering is the search's ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchField {
    /// The query IS one of the page's names, by the same exact fold `lore resolve` uses. The
    /// page does not merely discuss the subject; it is the page the name addresses.
    Identity,
    /// Every term appears among the page's names — its title, its aliases, or its address.
    Name,
    /// Every term appears in the page's opening statement — the first line of its first
    /// section, which is what every format leads with and what the catalog shows.
    Summary,
    /// Every term appears somewhere in the page's prose. Link destinations are not prose: a
    /// concept page carries one per citation, so matching them would make every page in the
    /// vault answer to `daily` or to any source id.
    Text,
}

/// One page a query reached.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub id: String,
    pub path: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub source_count: u64,
    pub matched: MatchField,
    pub excerpt: String,
}

/// Every knowledge page whose names or prose hold every term of `query`, best match first.
///
/// The scope is the wiki — concepts, documents and explorations — and not the daily pages
/// under them. A daily page is provenance: it holds the raw material a concept was read out
/// of, in bulk, so admitting it would bury every page that ANSWERS a query under the pages
/// that merely mentioned it once. The concept layer is the index into that material, and a
/// hit's `## Sources` is how a reader gets back to it.
pub fn search(
    vault_root: &Path,
    dirs: &VaultDirs,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, VaultError> {
    let terms: Vec<String> = query.split_whitespace().map(fold).collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let query_identity = identity_key(query);

    let mut hits: Vec<SearchHit> = Vec::new();
    for rel_dir in [
        concepts_dir(dirs),
        documents_dir(dirs),
        explorations_dir(dirs),
    ] {
        for path in markdown_files(&vault_root.join(&rel_dir)) {
            let Ok(content) = std::fs::read_to_string(&path) else {
                tracing::warn!(path = %path.display(), "skipping unreadable page");
                continue;
            };
            let Ok(page) = parse_page(&content) else {
                tracing::warn!(path = %path.display(), "skipping page whose frontmatter will not parse");
                continue;
            };
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = page
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let aliases = string_array(&page, "aliases");
            // Taken from where the file actually sits rather than assembled from the directory
            // and the name: the walk is recursive, so a page a person filed in a subdirectory
            // would otherwise be reported at an address that resolves nowhere.
            let rel_path = path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf();

            let names: Vec<&str> = std::iter::once(id.as_str())
                .chain(std::iter::once(title.as_str()))
                .chain(aliases.iter().map(String::as_str))
                .collect();
            let Some(matched) = match_page(&terms, query_identity.as_deref(), &names, &page.body)
            else {
                continue;
            };

            hits.push(SearchHit {
                excerpt: excerpt(&page.body, matched, &terms),
                id,
                path: rel_path.to_string_lossy().into_owned(),
                title,
                aliases,
                format: page
                    .frontmatter
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                category: page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                source_count: page.frontmatter.source_count().unwrap_or(0),
                matched,
            });
        }
    }

    hits.sort_by(|a, b| {
        a.matched
            .cmp(&b.matched)
            .then(b.source_count.cmp(&a.source_count))
            .then(a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// The strongest claim this page has on the query, or `None` when some term is nowhere on it.
///
/// Every term must appear, and the page is ranked by the WEAKEST field it needed — a query
/// whose second word only turns up deep in the prose is a prose match however well the first
/// one matched the title. Ranking by the strongest field instead would let one common word in
/// a title outrank a page that actually discusses the whole phrase.
///
/// The summary field is what keeps a long page from answering everything. A page states its
/// subject in the line it OPENS with, so a page whose subject is the query outranks one that
/// merely grew long enough to contain the words — which, ordered by evidence, is otherwise
/// every hub page in the vault. It has to be that line rather than the whole section: a
/// concept cited a hundred times has a synthesis long enough to hold any three common words.
fn match_page(
    terms: &[String],
    query_identity: Option<&str>,
    names: &[&str],
    body: &str,
) -> Option<MatchField> {
    if let Some(key) = query_identity
        && names
            .iter()
            .any(|name| identity_key(name).as_deref() == Some(key))
    {
        return Some(MatchField::Identity);
    }
    // Link DESTINATIONS are addresses rather than prose, and a concept page carries one per
    // citation — so matching them makes every page in the vault answer to `daily`, to any
    // source id, and to a date fragment. What a reader sees is the display text, which is what
    // `strip_links` leaves.
    let prose = link::strip_links(body);
    let fields = [
        (MatchField::Name, fold(&names.join(" "))),
        (
            MatchField::Summary,
            fold(&opening_statement(&prose).unwrap_or_default()),
        ),
        (MatchField::Text, fold(&prose)),
    ];
    terms
        .iter()
        .map(|term| {
            fields
                .iter()
                .find(|(_, text)| text.contains(term.as_str()))
                .map(|(field, _)| *field)
        })
        .try_fold(MatchField::Name, |worst, field| Some(worst.max(field?)))
}

/// What to show beside the hit. A page matched by name is introduced by what it says first;
/// one matched in its prose is shown the line that matched, because the title already failed
/// to explain why it is here.
fn excerpt(body: &str, matched: MatchField, terms: &[String]) -> String {
    // The same prose the match was taken over, so the line shown is the line that matched.
    let prose = link::strip_links(body);
    let text = match matched {
        MatchField::Identity | MatchField::Name | MatchField::Summary => opening_statement(&prose),
        MatchField::Text => prose
            .lines()
            .map(str::trim)
            .find(|line| {
                let folded = fold(line);
                terms.iter().all(|t| folded.contains(t.as_str()))
            })
            .map(str::to_owned)
            .or_else(|| opening_statement(&prose)),
    };
    text.map(|t| crate::index::truncate_summary(&t))
        .unwrap_or_default()
}

/// What the page says it is: the body of its first `## ` section.
///
/// Read by POSITION, which is type- and locale-independent on purpose. A concept leads with
/// its synthesis, a document with its summary, an exploration with its question — the
/// section's name differs with both the format and `vault.locale`, its position with neither,
/// so nothing here has a table to keep in step with either.
fn first_section(body: &str) -> Option<&str> {
    let start = body
        .find("\n## ")
        .map(|i| i + 1)
        .or_else(|| body.starts_with("## ").then_some(0))?;
    let rest = &body[start..];
    let after_heading = rest.find('\n').map(|i| start + i + 1)?;
    Some(match body[after_heading..].find("\n## ") {
        Some(end) => &body[after_heading..after_heading + end],
        None => &body[after_heading..],
    })
}

/// The page's opening statement: the first paragraph of [`first_section`].
///
/// A paragraph rather than a line, because a hard-wrapped page would otherwise have its
/// opening sentence split across two of them — the terms of a query landing on either side of
/// a break that carries no meaning.
pub(crate) fn opening_statement(body: &str) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    for line in first_section(body)?.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        paragraph.push(line);
    }
    (!paragraph.is_empty()).then(|| paragraph.join(" "))
}

/// Case- and width-insensitive comparison text. NFKC first, so a full-width or decomposed
/// spelling matches the composed one a page was written with — the same normalization every
/// slug in the vault goes through.
fn fold(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
}

fn string_array(page: &lk_core::frontmatter::VaultPage, key: &str) -> Vec<String> {
    page.frontmatter
        .get(key)
        .and_then(|v| v.as_array())
        .map(|seq| {
            seq.iter()
                .filter_map(|x| x.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn vault() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "agent-capability.md",
            "---\ntype: concept\ntitle: \"Agent Capability\"\naliases: [\"Agent Capability\", \"에이전트 호출 가능 애플리케이션 단위\"]\ncategory: agent-architecture\nsource_count: 3\n---\n\n# Agent Capability\n\n## 핵심\n\n애플리케이션의 일부를 에이전트가 직접 호출할 수 있게 노출한 단위.\n",
        );
        write(
            &concepts,
            "tool-exposure.md",
            "---\ntype: concept\ntitle: \"Tool Exposure\"\nsource_count: 9\n---\n\n# Tool Exposure\n\n## 핵심\n\n외부 기능을 호출 가능한 단위로 여는 방식이다.\n\n호출자는 대체로 에이전트이고, 도구를 여는 쪽은 권한을 함께 정한다.\n",
        );
        tmp
    }

    /// The ordering the whole command rests on: a page the query NAMES outranks one that
    /// merely carries the words, whatever the evidence behind either.
    #[test]
    fn a_name_outranks_prose_and_evidence_orders_the_rest() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "에이전트", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["agent-capability", "tool-exposure"],
            "the alias carries the term; the other page only says it in prose"
        );
        assert_eq!(hits[0].matched, MatchField::Name);
        assert_eq!(hits[1].matched, MatchField::Text);
        assert!(
            hits[1].excerpt.contains("호출자는 대체로"),
            "a prose hit is shown the line that matched, not the page's opening"
        );
    }

    /// The rank that keeps a well-cited page from answering every query. A concept with a
    /// hundred citations accumulates a synthesis long enough to contain any three common
    /// words, and ordering by evidence then puts it above the page the words actually name.
    #[test]
    fn a_page_that_opens_with_the_query_outranks_a_longer_one_that_merely_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "fail-open-fail-closed.md",
            "---\ntype: concept\ntitle: \"Fail-Open / Fail-Closed\"\nsource_count: 8\n---\n\n## 핵심\n\n품질 게이트의 실패 정책을 에러 성격에 따라 이분하는 설계 원칙.\n",
        );
        write(
            &concepts,
            "md-wisely.md",
            "---\ntype: concept\ntitle: \"MD Wisely\"\nsource_count: 146\n---\n\n## 핵심\n\nMD AX 프로젝트의 사내 제품이다.\n\n배포 게이트를 도입했고, 실패 시 롤백한다. 운영 정책은 별도로 관리한다.\n",
        );
        let hits = search(tmp.path(), &dirs, "게이트 실패 정책", 10).unwrap();
        assert_eq!(hits[0].id, "fail-open-fail-closed");
        assert_eq!(hits[0].matched, MatchField::Summary);
        assert_eq!(
            hits[1].matched,
            MatchField::Text,
            "the longer page still answers, as the weaker kind of hit it is"
        );
    }

    /// The question `lore resolve` answers, asked of a reader who does not know the name yet:
    /// an exact name reaches its page ahead of every page that discusses it.
    #[test]
    fn an_exact_name_is_the_strongest_match_however_it_is_spelled() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "AgentCapability", 10).unwrap();
        assert_eq!(hits[0].id, "agent-capability");
        assert_eq!(hits[0].matched, MatchField::Identity);
    }

    /// Every term has to land somewhere, so a query is narrowed by adding words rather than
    /// widened — the property that makes a two-word query useful at all.
    #[test]
    fn a_term_matching_nothing_excludes_the_page() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "에이전트 없는단어", 10).unwrap();
        assert!(hits.is_empty());
    }
}
