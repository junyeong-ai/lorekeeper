//! `lore resolve` — answer which concept page a name addresses.
//!
//! The ingest pipeline routes an extracted concept name to the page that owns it, through
//! `lk_core::concept::ConceptRegistry`. Nothing could ask that question from outside a run,
//! so a skill about to create a concept page had to load the whole registry and match names
//! by reading them — a judgment where the pipeline makes a decision, and the two can
//! disagree. They cannot now: this is the same registry, asked the same way.
//!
//! Exit code IS the answer, so a shell can branch on it without parsing: 0 a page owns the
//! name, 1 none does, 2 more than one does. Absence is not an error — it is the answer that
//! makes creating a page correct — but it is not success either, so `set -e` callers reach
//! for `--json`.

use std::path::PathBuf;

use lk_core::concept::{ConceptIdentity, ConceptRegistry, Resolution};
use lk_core::config::VaultDirs;
use lk_core::vault_path::VaultPath;
use serde::Serialize;

use super::GlobalOptions;

/// Exit codes, which are the whole verdict.
const OWNED: i32 = 0;
const ABSENT: i32 = 1;
const AMBIGUOUS: i32 = 2;
const FAILED: i32 = 3;

/// How the resolved page claims the name — an ADDRESS is the page's own location, a TITLE or
/// an ALIAS is a name it answers to. A caller deciding whether to write a page reads the
/// verdict; a caller explaining a surprising destination reads this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Claim {
    Address,
    Title,
    Alias,
}

#[derive(Serialize)]
struct ResolvedPage {
    slug: String,
    title: String,
    path: String,
    claim: Claim,
}

#[derive(Serialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
enum Answer {
    /// Exactly one page answers to the name.
    Owned {
        #[serde(flatten)]
        page: ResolvedPage,
    },
    /// No page does — writing one is correct.
    Absent { name: String },
    /// More than one does. `lore graph lint` reports the pair as a duplicate concept; until a
    /// human resolves it, `routed` is where a citation written now would land.
    Ambiguous {
        routed: ResolvedPage,
        claimants: Vec<ResolvedPage>,
    },
}

pub async fn run(opts: &GlobalOptions, name: String, json: bool, root: Option<PathBuf>) -> i32 {
    match answer(opts, &name, root).await {
        Ok(answer) => {
            if json {
                match serde_json::to_string_pretty(&answer) {
                    Ok(out) => println!("{out}"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return FAILED;
                    }
                }
            } else {
                print(&answer);
            }
            match answer {
                Answer::Owned { .. } => OWNED,
                Answer::Absent { .. } => ABSENT,
                Answer::Ambiguous { .. } => AMBIGUOUS,
            }
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            FAILED
        }
    }
}

async fn answer(opts: &GlobalOptions, name: &str, root: Option<PathBuf>) -> miette::Result<Answer> {
    let super::RootConfig { root, config } = super::resolve_root_config(opts, root)?;
    let dirs = config.map(|c| c.vault.dirs).unwrap_or_default();
    let registry = read_registry(&root, &dirs).await?;

    Ok(match registry.resolve(name) {
        Resolution::Absent => Answer::Absent {
            name: name.to_owned(),
        },
        Resolution::Owned(identity) => Answer::Owned {
            page: describe(&identity, name, &dirs),
        },
        Resolution::Ambiguous { routed, claimants } => Answer::Ambiguous {
            routed: describe(&routed, name, &dirs),
            claimants: claimants
                .iter()
                .map(|identity| describe(identity, name, &dirs))
                .collect(),
        },
    })
}

/// Read every concept page in the vault into the registry.
///
/// A page that will not parse still registers its address: the name it is reachable by is its
/// filename, which parsing has no say in, and dropping it would answer `absent` for a name the
/// vault already holds — the one answer that leads a caller to write a second page.
async fn read_registry(
    root: &std::path::Path,
    dirs: &VaultDirs,
) -> miette::Result<ConceptRegistry> {
    let dir = root.join(lk_core::vault_path::concepts_dir(dirs));
    let mut registry = ConceptRegistry::new();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(registry),
        Err(e) => return Err(miette::miette!("read {}: {e}", dir.display())),
    };

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| miette::miette!("read {}: {e}", dir.display()))?
    {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        let page = lk_core::frontmatter::parse_page(&content).ok();
        let frontmatter = page.as_ref().map(|p| &p.frontmatter);
        let title = frontmatter
            .and_then(|f| f.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or(slug);
        let aliases: Vec<String> = frontmatter
            .and_then(|f| f.get("aliases"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
        registry.register(
            ConceptIdentity {
                slug: slug.to_owned(),
                title: title.to_owned(),
            },
            &aliases,
        );
    }

    Ok(registry)
}

fn describe(identity: &ConceptIdentity, name: &str, dirs: &VaultDirs) -> ResolvedPage {
    let same =
        |a: &str, b: &str| lk_core::concept::identity_key(a) == lk_core::concept::identity_key(b);
    ResolvedPage {
        claim: if same(&identity.slug, name) {
            Claim::Address
        } else if same(&identity.title, name) {
            Claim::Title
        } else {
            Claim::Alias
        },
        path: VaultPath::concept(dirs, &identity.slug).to_string(),
        slug: identity.slug.clone(),
        title: identity.title.clone(),
    }
}

fn print(answer: &Answer) {
    match answer {
        Answer::Owned { page } => println!("{}\t{}\t{}", page.slug, page.title, page.path),
        Answer::Absent { name } => {
            eprintln!("no concept page answers to `{name}`");
        }
        Answer::Ambiguous { routed, claimants } => {
            eprintln!(
                "{} concept pages answer to one name; a citation written now lands on `{}`:",
                claimants.len(),
                routed.slug
            );
            for page in claimants {
                eprintln!("  {}\t{}", page.slug, page.path);
            }
            eprintln!("`lore graph lint` reports the pair; `lore graph merge` folds one in.");
        }
    }
}
