use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// A concept the LLM surfaced from a source. The page filename is always
/// `slugify(name)`, so the address a concept link points at is derivable from the
/// name alone — a citation and its page can never disagree on the slug.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedConcept {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
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
/// and ASCII `AB` both become `ab`). All concept slugs and graph node ids flow through
/// this one rule, so there is exactly one CJK-correct normalization in the workspace.
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

/// The identity a name claims, as opposed to the ADDRESS [`slugify`] writes it at.
///
/// The two answer different questions. A slug has to stay readable as a filename, so it keeps
/// every separator; identity keeps only the separators that mean something. Every break is
/// typography — `Vector DB`, `vector-db` and `vectordb` are one name written three ways, and so
/// are `claude-35` and `claude35` — EXCEPT one between two numerals, which is the name itself:
/// positional notation makes `3-5` two numerals and `35` one, so `Claude 3.5` and `Claude 35`
/// are different names, as are `GPT-4.1`/`GPT-41` and `Web 2.0`/`web20`. Dropping every
/// separator would fold those together, and version-numbered names are the single most common
/// shape in a technology vault.
///
/// Nothing else is folded. Word order and every character are identity, so `agent-harness` and
/// `harness-agent`, `http` and `https`, `doc-hub` and `docs-hub` are all DIFFERENT names —
/// whether they name the same concept is a question about meaning, which this cannot and must
/// not answer.
///
/// Single-sourced because two consumers must agree exactly: `lk_pipeline`'s alias index resolves
/// an extracted name to the page that owns it, and `lk_graph`'s duplicate lint reports two pages
/// owning one name. The lint only reports, but the index ACTS — it can fold an extraction into an
/// established page — so the fold has to be one no reviewer would overturn.
///
/// # Boundaries
///
/// **A break between digits is kept even when it only GROUPS one number for reading**, so
/// `978-0-13-468599-1` and `9780134685991` are two names. That is the price of telling
/// `Claude 3.5` from `Claude 35`, and version numbers are what a technology vault is full of
/// while grouped identifiers are not.
///
/// **Separator TYPE is not identity.** [`slugify`] maps `:`, `.`, `/` and space alike, so `16:9`
/// and `16-9` share one. Distinguishing them would claim that punctuation CHOICE is the name,
/// which is the typography claim this fold exists to deny.
///
/// **A symbol that is the whole difference between two names is lost, and it is lost in the
/// ADDRESS, not here.** [`slugify`] maps every non-alphanumeric to a separator and trims the
/// ends, so `C`, `C++` and `C#` all slugify to `c` — one address for three names. This fold
/// inherits that, and cannot undo it: `ConceptDrafts::resolve_identity` looks a name up by
/// `identity_key(slugify(name))`, so all three arrive at the same key however this function
/// behaves. Teaching it to read the NAME would only move the collision — the three would resolve
/// to different keys, miss each other in the index, and each mint a page addressed `c`,
/// overwriting rather than mis-routing.
///
/// The address cannot represent them either: `#` in a slug makes `wiki/concepts/c#.md` parse as
/// an anchor, so a link to it would not resolve, and a content-derived suffix would put a hash in
/// a slug humans read and cite. So two names that slugify alike cannot both be pages. That is a
/// property of the vault's addressing, the duplicate lint reports the pair, and the remedy is the
/// one `/lore-wiki audit` already prescribes — disambiguate the NAME (`Go (programming language)`
/// beside `Go (board game)`), which is a human's call about what the concepts are called.
pub fn identity_key(name: &str) -> Option<String> {
    let slug = slugify(name)?;
    let mut key = String::with_capacity(slug.len());
    let mut rest = slug.chars().peekable();
    while let Some(c) = rest.next() {
        // slugify leaves no leading, trailing or repeated separator, so a `-` always sits
        // between two characters and both sides are readable here.
        if c != '-' {
            key.push(c);
        } else if key.chars().next_back().is_some_and(char::is_numeric)
            && rest.peek().copied().is_some_and(char::is_numeric)
        {
            key.push('-');
        }
    }
    Some(key)
}

/// The identity of a concept's EVIDENCE: BLAKE3-128 over the set of pages that cite it,
/// in the same 32-hex form every other cache key in the vault takes.
///
/// The SET, never the rendered citation list. A source page that is retitled changes how a
/// citation reads without changing what it is, and a digest taken over the rendered text
/// would call that a change of evidence — resurfacing a concept whose material is identical.
/// Sorted and deduplicated before hashing, so the same evidence digests identically however
/// it was collected, and serialized as a JSON array so no id can be confused with a pair of
/// shorter ones.
///
/// Single-sourced because two crates must produce the same string: `lore graph
/// backlinks-sync` records it on the concept page as the input its `## Synthesis` is owed
/// against, and the queued task carries it as the input it answers.
pub fn citation_digest(citations: &[String]) -> String {
    let mut set: Vec<&str> = citations.iter().map(String::as_str).collect();
    set.sort_unstable();
    set.dedup();
    let bytes = serde_json::to_vec(&set).expect("a string array always serializes");
    blake3::hash(&bytes).to_hex()[..32].to_string()
}

/// What a name resolves to in a [`ConceptRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one page answers to this name.
    Owned(ConceptIdentity),
    /// More than one page answers to it — the vault defect `lore graph lint` reports as a
    /// duplicate concept. A writer still has to address the citation somewhere, so the
    /// deterministic choice is named alongside every claimant rather than left implicit.
    Ambiguous {
        routed: ConceptIdentity,
        claimants: Vec<ConceptIdentity>,
    },
    /// No page answers to this name.
    Absent,
}

impl Resolution {
    /// The page a citation of this name addresses, or `None` when nothing answers to it.
    pub fn routed(&self) -> Option<&ConceptIdentity> {
        match self {
            Resolution::Owned(identity)
            | Resolution::Ambiguous {
                routed: identity, ..
            } => Some(identity),
            Resolution::Absent => None,
        }
    }
}

/// Every name the vault's concept pages answer to, keyed by [`identity_key`].
///
/// One name reaches one page however its separators fall (`VectorDB` finds `vector-db.md`),
/// and a page answers to its own address, its title, and each of its aliases. The lookup is
/// EXACT on both sides: a name either IS one this vault already answers to, or it is a new
/// concept. There is no score, no threshold and no morphology — whether two different names
/// mean one concept is a judgment about meaning, and it lives in `/lore-wiki audit`.
///
/// Single-sourced because the two planes must agree: the ingest pipeline routes an extracted
/// name to the page owning it, and a reader asks whether a name is already taken before
/// writing a second page beside the canonical one. Computing the answer twice is what splits
/// a concept's citations by spelling.
#[derive(Debug, Default)]
pub struct ConceptRegistry {
    entries: BTreeMap<String, Claims>,
}

/// The pages answering to one identity, split by HOW they claim it. Both lists keep every
/// claimant in registration order, and routing takes the first of the first non-empty one:
/// a page's own ADDRESS outranks any other page's name, so a stale alias can never redirect
/// a concept away from its own page, and among equals the earliest registration wins. One
/// rule, so a citation's destination is reproducible without knowing how many pages claim
/// the name or in which order they were read.
#[derive(Debug, Default)]
struct Claims {
    address: Vec<ConceptIdentity>,
    named: Vec<ConceptIdentity>,
}

impl Claims {
    fn routed(&self) -> Option<&ConceptIdentity> {
        self.address.first().or_else(|| self.named.first())
    }

    fn claimants(&self) -> Vec<ConceptIdentity> {
        let mut out = self.address.clone();
        for identity in &self.named {
            if !out.iter().any(|held| held.slug == identity.slug) {
                out.push(identity.clone());
            }
        }
        out
    }

    fn holds(&self, slug: &str) -> bool {
        self.address
            .iter()
            .chain(&self.named)
            .any(|c| c.slug == slug)
    }
}

impl ConceptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register every name one concept page answers to.
    ///
    /// A page claims its own ADDRESS unconditionally, whatever order pages are registered in.
    /// Deriving the claim from names alone would protect only pages whose title or an alias
    /// happens to reproduce the stem — a page titled more descriptively than its file
    /// (`access-ingress-2axis-model` ← "Access × Ingress 2-Axis Deployment Model") has no such
    /// name, leaving its address free for another page's alias to take and sending every
    /// citation of its own name elsewhere. A stem carrying no identity has no address to seed;
    /// its names still register, so the page keeps answering to them.
    pub fn register(&mut self, identity: ConceptIdentity, aliases: &[String]) {
        if let Some(key) = identity_key(&identity.slug) {
            let claims = self.entries.entry(key).or_default();
            if !claims.address.iter().any(|a| a.slug == identity.slug) {
                claims.address.push(identity.clone());
            }
        }
        for name in std::iter::once(&identity.title).chain(aliases) {
            let Some(key) = identity_key(name) else {
                continue;
            };
            let claims = self.entries.entry(key).or_default();
            if !claims.holds(&identity.slug) {
                claims.named.push(identity.clone());
            }
        }
    }

    /// The page answering to `name`.
    pub fn resolve(&self, name: &str) -> Resolution {
        let Some(claims) = identity_key(name).and_then(|key| self.entries.get(&key)) else {
            return Resolution::Absent;
        };
        let claimants = claims.claimants();
        match claims.routed() {
            None => Resolution::Absent,
            Some(routed) if claimants.len() == 1 => Resolution::Owned(routed.clone()),
            Some(routed) => Resolution::Ambiguous {
                routed: routed.clone(),
                claimants,
            },
        }
    }

    /// Resolve `name`, recording `identity` as the page that answers to it when nothing does
    /// yet, and return the page a citation of it addresses.
    ///
    /// Recording the decision is what makes resolution self-consistent for the rest of a run:
    /// a caller that renders a citation before folding anything, and a caller that stages a
    /// whole batch before folding any of it, both need every spelling of one name to reach the
    /// same page. The first spelling seen fixes the address; the title is whichever spelling
    /// created the page.
    pub fn resolve_or_claim(&mut self, name: &str, identity: ConceptIdentity) -> ConceptIdentity {
        let resolved = self.resolve(name);
        if let Some(routed) = resolved.routed() {
            return routed.clone();
        }
        self.register(identity.clone(), &[]);
        identity
    }

    /// Every name the vault answers to, in identity order, with the page each resolves to.
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
    fn identity_key_folds_every_break_but_one_between_numerals() {
        // Everything slugify folds, plus every break that is not between two numerals.
        let vector_db = identity_key("Vector DB");
        assert_eq!(identity_key("vector-db"), vector_db);
        assert_eq!(identity_key("vectordb"), vector_db);
        assert_eq!(identity_key("VectorDB"), vector_db);
        assert_eq!(
            identity_key("Chain of Thought"),
            identity_key("chain-of-thought")
        );
        assert_eq!(identity_key("ＲＡＧ"), identity_key("rag")); // NFKC full-width
        assert_eq!(identity_key("Vite+"), identity_key("vite"));
        // A break on only ONE side of a digit is still typography.
        assert_eq!(identity_key("GPT-4o"), identity_key("gpt4o"));
        assert_eq!(identity_key("H-2"), identity_key("h2"));
        assert_eq!(identity_key("ISO-8601"), identity_key("iso8601"));
        assert_eq!(identity_key("S3 bucket"), identity_key("s3bucket"));
        assert_eq!(identity_key("---"), None);
    }

    #[test]
    fn slugify_and_identity_key_are_idempotent() {
        // `lk_pipeline` keys the alias index on `identity_key(name)` and looks a resolved
        // page up by `identity_key(slug)`. Those two agree only because re-normalizing a
        // slug is a no-op, so an accidental non-idempotent rule would silently split the
        // index into a write key and a read key that never meet.
        for name in [
            "Claude 3.5",
            "Vector DB",
            "GPT-4o",
            "RAG (Retrieval)",
            "ＲＡＧ",
            "8비트 색상 정규화",
        ] {
            let slug = slugify(name).unwrap();
            assert_eq!(slugify(&slug).as_deref(), Some(slug.as_str()), "{name}");
            assert_eq!(identity_key(&slug), identity_key(name), "{name}");
        }
    }

    #[test]
    fn identity_key_keeps_a_break_between_digits() {
        // Positional notation: `3-5` is two numerals, `35` is one, so the break IS the
        // name. Version-numbered names are the most common shape in a technology vault,
        // and folding these together would silently route one concept onto the other.
        assert_ne!(identity_key("Claude 3.5"), identity_key("Claude 35"));
        assert_ne!(identity_key("GPT-4.1"), identity_key("GPT-41"));
        assert_ne!(identity_key("Web 2.0"), identity_key("web20"));
        assert_ne!(identity_key("v1-0"), identity_key("v10"));
        // Spelling the same version differently still folds.
        assert_eq!(identity_key("Claude 3.5"), identity_key("claude-3-5"));
        assert_eq!(
            identity_key("Gemini 3.1 Flash"),
            identity_key("gemini-3-1-flash")
        );
    }

    #[test]
    fn a_numeral_is_a_numeral_in_any_script() {
        // The break test asks `is_numeric`, not `is_ascii_digit`: NFKC folds full-width,
        // superscript and enclosed forms to ASCII, but Arabic-Indic and Devanagari digits
        // survive it and are still numerals. `is_numeric` is Nd ∪ Nl ∪ No, so a numeral
        // that NFKC leaves alone stays one — `Ⅴ` decomposes to the letter `v`, while `ↀ`
        // (U+2180, no decomposition) does not and keeps its break. Korean number words are
        // letters. None of this is reachable from a real concept name; it is pinned so the
        // predicate is a decision rather than an accident.
        assert_ne!(identity_key("x ٥ ٦"), identity_key("x ٥٦")); // Arabic-Indic
        assert_ne!(identity_key("x ५ ६"), identity_key("x ५६")); // Devanagari
        assert_ne!(identity_key("x ５ ６"), identity_key("x ５６")); // full-width
        assert_ne!(identity_key("x²·³"), identity_key("x²³")); // superscript
        assert_eq!(identity_key("x Ⅴ Ⅵ"), identity_key("xⅤⅥ")); // U+2160 → letters
        assert_ne!(identity_key("x ↀ ↀ"), identity_key("xↀↀ")); // U+2180 stays a numeral
        assert_eq!(identity_key("x 오 육"), identity_key("x오육")); // Hangul → letters
    }

    #[test]
    fn identity_key_folds_nothing_about_meaning() {
        // Order and every character are identity; whether two of these name one concept
        // is a question about meaning, which this must not answer.
        assert_ne!(identity_key("agent harness"), identity_key("harness agent"));
        assert_ne!(identity_key("http"), identity_key("https"));
        assert_ne!(identity_key("doc-hub"), identity_key("docs-hub"));
        assert_ne!(identity_key("k8s"), identity_key("kubernetes"));
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
    fn a_page_answers_to_its_address_its_title_and_every_alias() {
        let mut registry = ConceptRegistry::new();
        registry.register(
            identity(
                "retrieval-augmented-generation",
                "Retrieval-Augmented Generation",
            ),
            &["RAG".to_owned()],
        );

        for name in [
            "retrieval-augmented-generation",
            "Retrieval-Augmented Generation",
            "RAG",
            "rag",
        ] {
            assert_eq!(
                registry.resolve(name).routed().map(|i| i.slug.as_str()),
                Some("retrieval-augmented-generation"),
                "{name}"
            );
        }
        assert_eq!(registry.resolve("vector-db"), Resolution::Absent);
    }

    /// A page whose title reproduces nothing of its filename still owns its address, so a
    /// citation written at that address reaches it rather than whichever page happens to
    /// alias the same identity.
    #[test]
    fn an_address_outranks_another_pages_alias() {
        let mut registry = ConceptRegistry::new();
        registry.register(
            identity(
                "access-ingress-2axis-model",
                "Access × Ingress 2-Axis Deployment Model",
            ),
            &[],
        );
        registry.register(
            identity("deployment-topology", "Deployment Topology"),
            &["access ingress 2axis model".to_owned()],
        );

        let resolved = registry.resolve("access-ingress-2axis-model");
        assert_eq!(
            resolved.routed().map(|i| i.slug.as_str()),
            Some("access-ingress-2axis-model")
        );
        assert!(
            matches!(resolved, Resolution::Ambiguous { .. }),
            "both pages answer to the name, and a reader has to be told"
        );
    }

    /// Registration order decides nothing a reader can see: whichever page is registered
    /// first, the answer names every claimant.
    #[test]
    fn two_pages_answering_to_one_name_resolve_ambiguous_either_way() {
        let register = |registry: &mut ConceptRegistry, which| match which {
            0 => registry.register(identity("doc-hub", "Doc Hub"), &[]),
            _ => registry.register(
                identity("docs-portal", "Docs Portal"),
                &["Doc Hub".to_owned()],
            ),
        };

        for order in [[0, 1], [1, 0]] {
            let mut registry = ConceptRegistry::new();
            for which in order {
                register(&mut registry, which);
            }

            let Resolution::Ambiguous { claimants, .. } = registry.resolve("Doc Hub") else {
                panic!("two pages answer to `Doc Hub`, registered {order:?}");
            };
            let mut slugs: Vec<&str> = claimants.iter().map(|i| i.slug.as_str()).collect();
            slugs.sort_unstable();
            assert_eq!(slugs, vec!["doc-hub", "docs-portal"]);
        }
    }

    /// The name a page is claimed under is the one every later spelling of it resolves to,
    /// so a batch that sees `VectorDB` and `Vector DB` addresses one page rather than two.
    #[test]
    fn a_claimed_name_answers_every_spelling_of_itself() {
        let mut registry = ConceptRegistry::new();
        let claimed = registry.resolve_or_claim("VectorDB", identity("vectordb", "VectorDB"));
        assert_eq!(claimed.slug, "vectordb");

        assert_eq!(
            registry
                .resolve_or_claim("Vector DB", identity("vector-db", "Vector DB"))
                .slug,
            "vectordb",
            "the second spelling must not mint a rival page"
        );
        assert_eq!(
            registry
                .resolve("vector-db")
                .routed()
                .map(|i| i.slug.as_str()),
            Some("vectordb")
        );
    }

    #[test]
    fn claiming_never_displaces_an_established_page() {
        let mut registry = ConceptRegistry::new();
        registry.register(identity("vector-db", "Vector DB"), &[]);

        let resolved = registry.resolve_or_claim("VectorDB", identity("vectordb", "VectorDB"));
        assert_eq!(resolved.slug, "vector-db");
        assert_eq!(resolved.title, "Vector DB");
    }

    /// Two FILES whose stems reduce to one identity both keep their claim. Collapsing them
    /// to the survivor would leave a citation landing on the page a reader did not expect
    /// with nothing to explain why, and `lore graph lint` reporting a pair the resolver
    /// cannot see.
    #[test]
    fn two_files_addressed_by_one_identity_both_stay_claimants() {
        let mut registry = ConceptRegistry::new();
        registry.register(identity("claude-35", "claude-35"), &[]);
        registry.register(identity("claude35", "claude35"), &[]);

        let Resolution::Ambiguous { routed, claimants } = registry.resolve("claude 35") else {
            panic!("two files answer to this address");
        };
        assert_eq!(routed.slug, "claude-35", "the earliest registration routes");
        assert_eq!(
            claimants
                .iter()
                .map(|c| c.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-35", "claude35"]
        );
    }

    #[test]
    fn a_citation_digest_is_the_set_not_the_order_or_the_repetition() {
        let a = citation_digest(&["daily/x/2026-01-01".into(), "wiki/documents/spec".into()]);
        let b = citation_digest(&["wiki/documents/spec".into(), "daily/x/2026-01-01".into()]);
        let c = citation_digest(&[
            "daily/x/2026-01-01".into(),
            "wiki/documents/spec".into(),
            "daily/x/2026-01-01".into(),
        ]);
        assert_eq!(a, b, "collection order is not evidence");
        assert_eq!(a, c, "one page cited twice is one citation");
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn a_citation_digest_separates_every_distinct_set() {
        let empty = citation_digest(&[]);
        let one = citation_digest(&["daily/x/2026-01-01".into()]);
        let other = citation_digest(&["daily/x/2026-01-02".into()]);
        let both = citation_digest(&["daily/x/2026-01-01".into(), "daily/x/2026-01-02".into()]);
        let all = [&empty, &one, &other, &both];
        for (i, left) in all.iter().enumerate() {
            for right in &all[i + 1..] {
                assert_ne!(left, right);
            }
        }

        // Ids are joined through a JSON array, so no split of one id reproduces another set.
        assert_ne!(
            citation_digest(&["a/b".into()]),
            citation_digest(&["a".into(), "b".into()])
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
