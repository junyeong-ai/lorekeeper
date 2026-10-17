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

/// Whether the resolved page declares the queried name as its title or an alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Claim {
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
    Absent {
        name: String,
        proposed_path: Option<String>,
    },
    /// Multiple owners; a writer must resolve the ambiguity before applying changes.
    Ambiguous { claimants: Vec<ResolvedPage> },
}

pub async fn run(
    opts: &GlobalOptions,
    names: Vec<String>,
    json: bool,
    root: Option<PathBuf>,
) -> i32 {
    match answers(opts, &names, root).await {
        Ok(answers) => {
            if json {
                let output = if answers.len() == 1 {
                    serde_json::to_value(&answers[0])
                } else {
                    Ok(serde_json::Value::Array(names.iter().zip(&answers).map(|(name, answer)| {
                        serde_json::json!({"name": name, "answer": answer})
                    }).collect()))
                };
                match output.and_then(|value| serde_json::to_string_pretty(&value)) {
                    Ok(out) => println!("{out}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return FAILED;
                    }
                }
            } else {
                for answer in &answers {
                    print(answer);
                }
            }
            answers
                .iter()
                .map(|answer| match answer {
                    Answer::Owned { .. } => OWNED,
                    Answer::Absent { .. } => ABSENT,
                    Answer::Ambiguous { .. } => AMBIGUOUS,
                })
                .max()
                .unwrap_or(OWNED)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            FAILED
        }
    }
}

async fn answers(
    opts: &GlobalOptions,
    names: &[String],
    root: Option<PathBuf>,
) -> miette::Result<Vec<Answer>> {
    let super::RootConfig { root, config } = super::resolve_root_config(opts, root)?;
    let dirs = config.map(|config| config.vault.dirs).unwrap_or_default();
    let registry = lk_vault::load_concept_registry(&lk_vault::FsVault::new(root), &dirs)
        .await
        .map_err(|error| miette::miette!("read concept registry: {error}"))?;
    Ok(names
        .iter()
        .map(|name| answer(&registry, name, &dirs))
        .collect())
}

fn answer(registry: &ConceptRegistry, name: &str, dirs: &VaultDirs) -> Answer {
    match registry.resolve(name) {
        Resolution::Absent => Answer::Absent {
            name: name.to_owned(),
            proposed_path: registry
                .available_slug(name)
                .map(|slug| VaultPath::concept(dirs, &slug).to_string()),
        },
        Resolution::Owned(identity) => Answer::Owned {
            page: describe(&identity, name, dirs),
        },
        Resolution::Ambiguous { claimants } => Answer::Ambiguous {
            claimants: claimants
                .iter()
                .map(|identity| describe(identity, name, dirs))
                .collect(),
        },
    }
}

fn describe(identity: &ConceptIdentity, name: &str, dirs: &VaultDirs) -> ResolvedPage {
    let same =
        |a: &str, b: &str| lk_core::concept::identity_key(a) == lk_core::concept::identity_key(b);
    ResolvedPage {
        claim: if same(&identity.title, name) {
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
        Answer::Absent { name, .. } => {
            eprintln!("no concept page answers to `{name}`");
        }
        Answer::Ambiguous { claimants } => {
            eprintln!(
                "{} concept pages answer to one name; resolve the ambiguity before writing:",
                claimants.len()
            );
            for page in claimants {
                eprintln!("  {}\t{}", page.slug, page.path);
            }
            eprintln!("`lore graph lint` reports the pair; `lore graph merge` folds one in.");
        }
    }
}
