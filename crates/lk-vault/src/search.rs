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
//!
//! A page is reached two ways and keeps the better one: every term of the query appearing on
//! it, or — when the query IS a concept's name — any of that concept's OTHER declared names
//! appearing on it. The second is what lets one question be asked in one spelling and answered
//! from prose written in another, which a vault holding one language's writing about another
//! language's terms needs in both directions. It reads only names the vault wrote down, so it
//! is not morphology and not a guess; see [`spellings_of`].

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

    let mut pages: Vec<Page> = Vec::new();
    for rel_dir in [
        concepts_dir(dirs),
        documents_dir(dirs),
        explorations_dir(dirs),
    ] {
        let concept = rel_dir == concepts_dir(dirs);
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
            pages.push(Page {
                // Taken from where the file actually sits rather than assembled from the
                // directory and the name: the walk is recursive, so a page filed in a
                // subdirectory would otherwise be reported at an address resolving nowhere.
                rel_path: path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf(),
                aliases: string_array(&page, "aliases"),
                id,
                title,
                concept,
                page,
            });
        }
    }

    let spellings = spellings_of(query_identity.as_deref(), &pages);

    let mut hits: Vec<SearchHit> = Vec::new();
    for entry in &pages {
        let names: Vec<&str> = entry.names().collect();
        let Some(matched) = match_page(
            &terms,
            query_identity.as_deref(),
            &spellings,
            &names,
            &entry.page.body,
        ) else {
            continue;
        };
        hits.push(SearchHit {
            excerpt: excerpt(&entry.page.body, matched, &terms, &spellings),
            id: entry.id.clone(),
            path: entry.rel_path.to_string_lossy().into_owned(),
            title: entry.title.clone(),
            aliases: entry.aliases.clone(),
            format: entry
                .page
                .frontmatter
                .get("type")
                .and_then(|v| v.as_str())
                .map(String::from),
            category: entry
                .page
                .frontmatter
                .get("category")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            source_count: entry.page.frontmatter.source_count().unwrap_or(0),
            matched,
        });
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

/// One knowledge page, read and parsed once.
///
/// Buffered rather than matched as it is read, because the query has to be read against the
/// vault's vocabulary before any page is judged, and that vocabulary is on the pages.
struct Page {
    id: String,
    title: String,
    aliases: Vec<String>,
    rel_path: PathBuf,
    /// Whether this page is a concept. Concepts are the vault's vocabulary — the pages a name
    /// addresses, and the only ones [`spellings_of`] reads a query's other spellings from.
    concept: bool,
    page: lk_core::frontmatter::VaultPage,
}

impl Page {
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.id.as_str())
            .chain(std::iter::once(self.title.as_str()))
            .chain(self.aliases.iter().map(String::as_str))
    }
}

/// Every spelling the vault declares for the concept the QUERY names, folded for matching.
///
/// A query that IS a concept's name asks what the vault knows about that concept, and what
/// that concept is CALLED is already written down: the page's title and its aliases. So the
/// prose search runs under those spellings too, and a page discussing the subject under
/// another of its names is reached rather than missed. Nothing here folds a space, stems a
/// word or matches a pattern — every spelling is one a person or an extraction recorded on
/// the page, so a hit reached this way was reached by a declared name and the line shown
/// contains it.
///
/// It matters most in a Korean vault and it is not a Korean feature. Korean word spacing
/// genuinely varies, so `지식그래프` and `지식 그래프` are ONE name to `identity_key` and two
/// strings to prose matching; `RAG` and `Retrieval-Augmented Generation` are the same gap in
/// an English one. Both close here, and in both directions — a query in either language reaches the
/// pages written in the other, which is what a vault holding one language's prose about
/// another language's terms needs.
///
/// Empty when the query names nothing, which leaves the term-by-term match as the only path
/// and is the answer for a query that is a phrase rather than a name.
///
/// Where this reaches too far: an alias that is ALSO a common word in another sense. Querying
/// the concept then reads every page using that word in the other sense — `측정 모집단` reads
/// the pages saying `분모`, which is the alias and is also just the word for a denominator.
/// The reach belongs to the alias rather than to the matching, which is where it can be
/// fixed: the excerpt shows the line and the name that reached it, and judging whether a name
/// belongs to a concept is `/lore-wiki audit`'s. What is ruled out is the other failure, the
/// one no edit can reach — a page matched by a spelling nobody wrote down.
fn spellings_of(query_identity: Option<&str>, pages: &[Page]) -> Vec<String> {
    let Some(key) = query_identity else {
        return Vec::new();
    };
    pages
        .iter()
        .filter(|p| p.concept)
        .filter(|p| {
            p.names()
                .any(|name| identity_key(name).as_deref() == Some(key))
        })
        .flat_map(Page::names)
        .map(fold)
        .filter(|name| !name.is_empty())
        .collect()
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
    spellings: &[String],
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
    let field_of = |needle: &str| {
        fields
            .iter()
            .find(|(_, text)| contains_term(text, needle))
            .map(|(field, _)| *field)
    };
    // Two independent routes to the page, and the page keeps the better of them. Every term
    // appearing is the general answer; the query's own name appearing as a whole is the answer
    // for a page that spells the concept differently, where the terms it was typed as are
    // nowhere on the page.
    let by_terms = terms
        .iter()
        .map(|term| field_of(term))
        .try_fold(MatchField::Name, |worst, field| Some(worst.max(field?)));
    let by_name = spellings.iter().filter_map(|name| field_of(name)).min();
    by_terms.into_iter().chain(by_name).min()
}

/// Does `text` hold `term` as a word of its own, rather than inside a longer one?
///
/// Plain containment is what a Korean query needs: a noun carries its particle inside the same
/// word (`에이전트를`, `게이트가`), so requiring a boundary there would make the language's
/// ordinary spelling unsearchable. A Latin-script query needs the opposite, because spaces do
/// separate words there — `ai` matched `daily`, `chain` and `guardrail`, and did so at the
/// second-strongest rank, since those spellings are page NAMES.
///
/// So a boundary is required per EDGE and decided by the TERM: where a term begins or ends
/// with an ASCII letter or digit, the character beside the match may not be one. Nothing here
/// inspects the page's language, and a query mixing the two scripts is judged edge by edge.
/// The limit is the other scripts that DO separate words — Cyrillic, Greek, an accented Latin
/// word — which keep plain containment: unconstrained rather than wrongly constrained, which
/// is the state this started from.
fn contains_term(text: &str, term: &str) -> bool {
    let (Some(first), Some(last)) = (term.chars().next(), term.chars().last()) else {
        return false;
    };
    let open_anywhere = !first.is_ascii_alphanumeric();
    let close_anywhere = !last.is_ascii_alphanumeric();
    text.match_indices(term).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + term.len()..].chars().next();
        (open_anywhere || !before.is_some_and(|c| c.is_ascii_alphanumeric()))
            && (close_anywhere || !after.is_some_and(|c| c.is_ascii_alphanumeric()))
    })
}

/// What to show beside the hit. A page matched by name is introduced by what it says first;
/// one matched in its prose is shown the line that matched, because the title already failed
/// to explain why it is here.
fn excerpt(body: &str, matched: MatchField, terms: &[String], spellings: &[String]) -> String {
    // The same prose the match was taken over, so the line shown is the line that matched —
    // under whichever of the two routes reached the page, so a page found by another of the
    // concept's names shows the line carrying that name rather than the page's opening.
    let prose = link::strip_links(body);
    let text = match matched {
        MatchField::Identity | MatchField::Name | MatchField::Summary => opening_statement(&prose),
        MatchField::Text => prose
            .lines()
            .map(str::trim)
            .find(|line| {
                let folded = fold(line);
                terms.iter().all(|t| contains_term(&folded, t))
                    || spellings.iter().any(|n| contains_term(&folded, n))
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

    /// The reason a Korean vault needed the query read against its own vocabulary. Korean word
    /// spacing varies, so the same name is written both ways, and `identity_key` folds the two
    /// while prose matching cannot. On the reference vault `지식그래프` reached 4 pages where
    /// `지식 그래프` reached 29 — the same question, the same subject, a quarter of the answer.
    #[test]
    fn a_query_spelled_solid_reaches_the_prose_spelled_with_a_space() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "knowledge-graph.md",
            "---\ntype: concept\ntitle: \"지식 그래프\"\naliases: [\"Knowledge Graph\"]\nsource_count: 9\n---\n\n## 핵심\n\n개체와 관계를 그래프로 구조화한 지식 표현.\n",
        );
        write(
            &concepts,
            "graphrag.md",
            "---\ntype: concept\ntitle: \"GraphRAG\"\nsource_count: 5\n---\n\n## 핵심\n\n검색 단계에서 지식 그래프의 구조를 활용하는 RAG 계열 접근이다.\n",
        );
        let ids = |q: &str| {
            search(tmp.path(), &dirs, q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("지식 그래프"), ["knowledge-graph", "graphrag"]);
        assert_eq!(
            ids("지식그래프"),
            ["knowledge-graph", "graphrag"],
            "the solid spelling names the same concept, so it reads the same prose"
        );
        assert!(
            search(tmp.path(), &dirs, "지식그래프", 10).unwrap()[1]
                .excerpt
                .contains("지식 그래프"),
            "the line shown carries the declared name that reached the page"
        );
    }

    /// The same mechanism in the other direction, which is what makes it not a Korean feature:
    /// an English query reaches prose written in Korean, and a full name reaches the prose that
    /// only ever writes the acronym. On the reference vault `Model Context Protocol` reached 9
    /// pages where `MCP` reached 202.
    #[test]
    fn a_query_reaches_the_prose_written_under_the_concepts_other_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "prompt-injection.md",
            "---\ntype: concept\ntitle: \"Prompt Injection\"\naliases: [\"프롬프트 인젝션\"]\nsource_count: 31\n---\n\n## 핵심\n\n외부 입력이 모델의 지시로 읽히는 취약점.\n",
        );
        write(
            &concepts,
            "agent-sandbox.md",
            "---\ntype: concept\ntitle: \"Agent Sandbox\"\nsource_count: 4\n---\n\n## 핵심\n\n에이전트 실행을 격리하는 방식이다.\n\n프롬프트 인젝션이 성립해도 피해를 가둔다.\n",
        );
        let hits = search(tmp.path(), &dirs, "prompt injection", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["prompt-injection", "agent-sandbox"],
            "the English query reaches the page that only writes the Korean name"
        );
        assert_eq!(hits[1].matched, MatchField::Text);
    }

    /// A query that names nothing is a phrase, and a phrase has no other spellings to read —
    /// so it is matched term by term exactly as before. Without this the expansion would be
    /// unbounded: every query would look for a concept to borrow names from.
    #[test]
    fn a_query_that_names_no_concept_is_matched_term_by_term() {
        let tmp = vault();
        let hits = search(
            tmp.path(),
            &VaultDirs::default(),
            "호출 가능한 단위로 여는 권한",
            10,
        )
        .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["tool-exposure"]
        );
        assert_eq!(hits[0].matched, MatchField::Text);
    }

    /// The acronym is the commonest shape a technical query takes, and plain containment made
    /// it the noisiest: on the reference vault `ai` reached `md-wisely-daily-scrum`,
    /// `supply-chain-attack` and `pure-code-guardrail-chain` — all at the NAME rank, above
    /// every page that actually discusses the subject.
    #[test]
    fn a_latin_term_matches_a_word_and_not_the_middle_of_one() {
        assert!(contains_term("ai-slop", "ai"));
        assert!(contains_term("amazon sagemaker ai", "ai"));
        assert!(
            contains_term("ai기반 파이프라인", "ai"),
            "no space is needed where the neighbouring script separates words on its own"
        );
        assert!(!contains_term("md-wisely-daily-scrum", "ai"));
        assert!(!contains_term("supply-chain-attack", "ai"));
        assert!(
            !contains_term("openai-codex", "ai"),
            "a longer name is a different name"
        );
        assert!(
            !contains_term("claude-35", "5"),
            "a numeral inside a number is not the number"
        );
    }

    /// The other half of the same rule: Korean writes a noun and its particle as one word, so
    /// a boundary requirement would leave the vault's own language unsearchable.
    #[test]
    fn a_korean_term_still_matches_inside_an_inflected_word() {
        assert!(contains_term("호출자는 대체로 에이전트이고", "에이전트"));
        assert!(contains_term("게이트가 실패하면", "게이트"));
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
