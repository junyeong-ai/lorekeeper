use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// A concept surfaced from a source. Resolve its declared name through the registry
/// before constructing a page or a citation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedConcept {
    /// The concept's name, and nothing else. [`identity_key`] is exact, so a name carrying a
    /// parenthetical gloss (`Agent Capability (에이전트 호출 가능 애플리케이션 단위)`) answers
    /// to neither the term nor the gloss: the page becomes unreachable by anything a person
    /// or a later extraction would write, and the next mention of the bare term mints a rival
    /// page beside it. The other spellings belong in [`Self::aliases`].
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Every other name this concept answers to — a translation, an expanded acronym, the
    /// original-language term. Each is registered against the page so a citation written in
    /// any of them resolves to it, which is what lets one concept named differently in two
    /// projects converge on a single page instead of accumulating a synthesis per spelling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// The concept page a name resolves to: the ADDRESS it is written at and the TITLE it
/// is displayed under. The two are independent — a renamed or merged concept keeps its
/// original slug and records the new name as an alias, which is what keeps every existing
/// citation resolving.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConceptIdentity {
    pub slug: String,
    pub title: String,
}

/// Normalize an arbitrary name into a path-safe slug.
///
/// Pipeline: **NFKC normalize → lowercase → map every non-alphanumeric char
/// (whitespace, punctuation, including literal `-`) to a separator → collapse runs of
/// separators into a single `-` and trim leading/trailing `-`**.
///
/// NFKC folds compatibility forms first, so composed and decomposed Hangul and
/// full-width characters all reduce to the same canonical slug (e.g. full-width `ＡＢ`
/// and ASCII `AB` both become `ab`). This proposes normalized filenames; existing page
/// addresses and graph node identities retain their exact spelling.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut pending_sep = false;
    for c in name.nfkc().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(c);
        } else {
            // '-', whitespace, and punctuation are all separators; runs collapse to one.
            pending_sep = true;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// A declared name, independent of its filesystem address. NFC and whitespace
/// normalize canonical spelling; case, compatibility characters and punctuation remain significant.
/// Different spellings converge only through explicitly registered aliases.
pub fn identity_key(name: &str) -> Option<String> {
    let normalized: String = name.nfc().collect();
    let key = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    (!key.is_empty()).then_some(key)
}

/// BLAKE3-128 of the sorted, deduplicated source paths and their authored revisions.
/// Source ordering and repeated citations do not change the input.
pub fn citation_digest(citations: &[Citation]) -> String {
    let mut set: Vec<&Citation> = citations.iter().collect();
    set.sort_unstable();
    set.dedup();
    let bytes = serde_json::to_vec(&set).expect("citations serialize");
    blake3::hash(&bytes).to_hex()[..32].to_string()
}

/// A source page at the revision read by a synthesis.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Citation {
    pub path: String,
    pub revision: String,
}

impl Citation {
    pub fn new(path: &std::path::Path, page: &crate::frontmatter::VaultPage) -> Self {
        Self::at_revision(path, source_revision(page))
    }

    pub fn at_revision(path: &std::path::Path, revision: String) -> Self {
        Self {
            path: path.to_string_lossy().replace('\\', "/"),
            revision,
        }
    }
}

/// Authored input, excluding bookkeeping that does not change what a source states.
pub fn source_revision(page: &crate::frontmatter::VaultPage) -> String {
    let mut fields = page.frontmatter.fields.clone();
    for key in [
        crate::frontmatter::field::LLM_INPUTS,
        "updated",
        crate::frontmatter::field::SOURCE_COUNT,
    ] {
        fields.remove(key);
    }
    let bytes = serde_json::to_vec(&(fields, &page.body)).expect("page values serialize");
    blake3::hash(&bytes).to_hex()[..32].to_string()
}

/// Exact file bytes, including bookkeeping, used to guard publication against edits.
pub fn file_revision(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

/// What a name resolves to in a [`ConceptRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one page answers to this name.
    Owned(ConceptIdentity),
    /// Multiple declared owners; no page is selected for writing.
    Ambiguous { claimants: Vec<ConceptIdentity> },
    /// No page answers to this name.
    Absent,
}

impl Resolution {
    /// The page a citation of this name addresses, or `None` when nothing answers to it.
    pub fn routed(&self) -> Option<&ConceptIdentity> {
        match self {
            Resolution::Owned(identity) => Some(identity),
            Resolution::Ambiguous { .. } | Resolution::Absent => None,
        }
    }
}

/// Declared names and filesystem addresses are separate namespaces.
#[derive(Debug, Default)]
pub struct ConceptRegistry {
    entries: BTreeMap<String, BTreeMap<String, ConceptIdentity>>,
    addresses: BTreeMap<String, ConceptIdentity>,
}

impl ConceptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, identity: ConceptIdentity, aliases: &[String]) {
        self.addresses.insert(
            identity.slug.nfkc().flat_map(char::to_lowercase).collect(),
            identity.clone(),
        );
        for name in std::iter::once(&identity.title).chain(aliases) {
            if let Some(key) = identity_key(name) {
                self.entries
                    .entry(key)
                    .or_default()
                    .insert(identity.slug.clone(), identity.clone());
            }
        }
    }

    pub fn resolve(&self, name: &str) -> Resolution {
        let Some(claims) = identity_key(name).and_then(|key| self.entries.get(&key)) else {
            return Resolution::Absent;
        };
        let claimants: Vec<_> = claims.values().cloned().collect();
        match claimants.as_slice() {
            [] => Resolution::Absent,
            [identity] => Resolution::Owned(identity.clone()),
            [_, _, ..] => Resolution::Ambiguous { claimants },
        }
    }

    /// Allocate a safe address without interpreting an occupied filename as a synonym.
    /// The digest disambiguates addresses, never semantic identity. Existing files retain
    /// their addresses; an occupied disambiguated address is refused rather than overwritten.
    pub fn available_slug(&self, name: &str) -> Option<String> {
        let base = slugify(name)?;
        if base.len() <= 220 && !self.addresses.contains_key(&base) {
            return Some(base);
        }
        let key = identity_key(name)?;
        let digest = blake3::hash(key.as_bytes()).to_hex();
        let mut end = base.len().min(220);
        while !base.is_char_boundary(end) {
            end -= 1;
        }
        let slug = format!("{}-{}", base[..end].trim_end_matches('-'), &digest[..16]);
        (!self.addresses.contains_key(&slug)).then_some(slug)
    }

    pub fn names(&self) -> impl Iterator<Item = (&str, Resolution)> {
        self.entries
            .keys()
            .map(|key| (key.as_str(), self.resolve(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(slug: &str, title: &str) -> ConceptIdentity {
        ConceptIdentity {
            slug: slug.to_owned(),
            title: title.to_owned(),
        }
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Claude Code"), Some("claude-code".into()));
        assert_eq!(slugify("GPT-4o"), Some("gpt-4o".into()));
        assert_eq!(slugify("RAG (Retrieval)"), Some("rag-retrieval".into()));
    }

    #[test]
    fn slugify_collapses_and_trims_separators() {
        assert_eq!(slugify("  spaced  out  "), Some("spaced-out".into()));
        assert_eq!(slugify("a---b"), Some("a-b".into()));
        assert_eq!(
            slugify("--leading-and-trailing--"),
            Some("leading-and-trailing".into())
        );
        assert_eq!(slugify("under_score"), Some("under-score".into()));
    }

    #[test]
    fn declared_names_preserve_symbols_and_word_boundaries() {
        for (a, b) in [
            ("C", "C++"),
            ("C++", "C#"),
            ("CO", "Co"),
            ("x²", "x2"),
            ("ＲＡＧ", "RAG"),
            ("Vite", "Vite+"),
            ("A/I", "A I"),
            ("VectorDB", "Vector DB"),
            ("Claude 3.5", "Claude 35"),
            ("16:9", "16-9"),
            ("x ٥ ٦", "x ٥٦"),
        ] {
            assert_ne!(identity_key(a), identity_key(b), "{a} / {b}");
        }
        assert_eq!(identity_key("  RAG  "), identity_key("RAG"));
        assert_eq!(identity_key("한글"), identity_key("한글"));
        assert_eq!(identity_key("Vector  DB"), identity_key("Vector DB"));
        assert_eq!(identity_key("   "), None);
    }

    #[test]
    fn slugify_returns_none_for_empty() {
        assert_eq!(slugify(""), None);
        assert_eq!(slugify("---"), None);
        assert_eq!(slugify("   "), None);
    }

    #[test]
    fn slugify_nfkc_fullwidth_matches_ascii() {
        // Full-width latin letters fold to ASCII under NFKC, so the slug matches.
        assert_eq!(slugify("\u{ff21}\u{ff22}"), Some("ab".into())); // Ａ Ｂ
        assert_eq!(slugify("\u{ff21}\u{ff22}"), slugify("AB"));
    }

    #[test]
    fn slugify_nfkc_hangul_composed_equals_decomposed() {
        // Composed (precomposed syllable) and decomposed (jamo sequence) Hangul must
        // produce the same slug once NFKC-normalized — the real CJK bug this fixes.
        let composed = "\u{d55c}\u{ae00}"; // 한글, precomposed
        let decomposed = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}"; // ㅎㅏㄴㄱㅡㄹ jamo
        assert_eq!(slugify(composed), slugify(decomposed));
        assert!(slugify(composed).is_some());
    }

    #[test]
    fn names_resolve_only_through_declared_titles_and_aliases() {
        let mut registry = ConceptRegistry::new();
        registry.register(identity("c", "C++"), &["CPP".into()]);
        assert_eq!(registry.resolve("C"), Resolution::Absent);
        assert_eq!(registry.resolve("CPP").routed().unwrap().title, "C++");
        let slug = registry.available_slug("C").unwrap();
        assert_ne!(slug, "c");
        registry.register(identity(&slug, "C"), &[]);
        assert_eq!(registry.resolve("C").routed().unwrap().slug, slug);
        registry.register(identity("other", "Other"), &["CPP".into()]);
        assert!(matches!(
            registry.resolve("CPP"),
            Resolution::Ambiguous { .. }
        ));
        registry.register(identity(&registry.available_slug("C#").unwrap(), "C#"), &[]);
        for name in ["C", "C++", "C#"] {
            assert!(matches!(registry.resolve(name), Resolution::Owned(_)));
        }
    }

    #[test]
    fn address_allocation_respects_occupied_case_variants_and_bounds_unicode_names() {
        let mut registry = ConceptRegistry::new();
        registry.register(identity("C", "C++"), &[]);
        assert_ne!(registry.available_slug("C#").unwrap(), "c");
        let boundary = format!("{} {}", "a".repeat(219), "b".repeat(40));
        let address = registry.available_slug(&boundary).unwrap();
        assert_eq!(slugify(&address).as_deref(), Some(address.as_str()));
        let long = "지식".repeat(100);
        let address = registry.available_slug(&long).unwrap();
        assert!(address.len() <= 237);
        assert_eq!(slugify(&address).as_deref(), Some(address.as_str()));
        registry.register(identity(&address, "Reserved"), &[]);
        assert_eq!(registry.available_slug(&long), None);
    }

    fn citation(path: &str) -> Citation {
        Citation {
            path: path.into(),
            revision: "revision".into(),
        }
    }

    #[test]
    fn source_revisions_track_authored_facts_not_serialization_or_bookkeeping() {
        let revision = |text: &str| source_revision(&crate::frontmatter::parse_page(text).unwrap());
        let original = "---\ntitle: Contract\nstatus: active\nupdated: 2026-01-01\nllm_inputs:\n  summary: old\n---\nTimeout is 30 seconds.\n";
        let bookkeeping = "---\nllm_inputs: {summary: new, summary_done: new}\nsource_count: 9\nupdated: 2026-10-15\nstatus: active\ntitle: 'Contract'\n---\nTimeout is 30 seconds.\n";
        assert_eq!(revision(original), revision(bookkeeping));
        assert_eq!(
            revision(original),
            revision(&original.replace('\n', "\r\n"))
        );
        for changed in [
            original.replace("30 seconds", "5 seconds"),
            original.replace("active", "superseded"),
            original.replace("Contract", "Proposal"),
        ] {
            assert_ne!(revision(original), revision(&changed));
        }
    }

    #[test]
    fn a_citation_digest_is_the_set_not_the_order_or_the_repetition() {
        let a = citation_digest(&[
            citation("daily/x/2026-01-01"),
            citation("wiki/documents/spec"),
        ]);
        let b = citation_digest(&[
            citation("wiki/documents/spec"),
            citation("daily/x/2026-01-01"),
        ]);
        let c = citation_digest(&[
            citation("daily/x/2026-01-01"),
            citation("wiki/documents/spec"),
            citation("daily/x/2026-01-01"),
        ]);
        assert_eq!(a, b, "collection order is not evidence");
        assert_eq!(a, c, "one page cited twice is one citation");
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn a_citation_digest_separates_every_distinct_set() {
        let empty = citation_digest(&[]);
        let one = citation_digest(&[citation("daily/x/2026-01-01")]);
        let other = citation_digest(&[citation("daily/x/2026-01-02")]);
        let both = citation_digest(&[
            citation("daily/x/2026-01-01"),
            citation("daily/x/2026-01-02"),
        ]);
        let all = [&empty, &one, &other, &both];
        for (i, left) in all.iter().enumerate() {
            for right in &all[i + 1..] {
                assert_ne!(left, right);
            }
        }

        // Ids are joined through a JSON array, so no split of one id reproduces another set.
        assert_ne!(
            citation_digest(&[citation("a/b")]),
            citation_digest(&[citation("a"), citation("b")])
        );
    }

    #[test]
    fn a_name_carrying_no_identity_resolves_to_nothing() {
        let mut registry = ConceptRegistry::new();
        registry.register(identity("rag", "RAG"), &[]);
        assert_eq!(registry.resolve("---"), Resolution::Absent);
        assert_eq!(registry.resolve(""), Resolution::Absent);
    }
}
