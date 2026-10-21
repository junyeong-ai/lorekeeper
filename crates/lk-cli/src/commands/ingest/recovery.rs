use std::collections::BTreeMap;

use jiff::civil::Date;
use lk_core::config::{Config, SourceRecovery};

use crate::commands::synthesis::pending::{CollectionWindow, source_coverage};
use crate::commands::{GlobalOptions, find_config, load_config_with_revision};

#[derive(Debug, PartialEq, Eq)]
struct SourcePlan {
    id: String,
    dates: Vec<Date>,
}

fn uncovered_days(
    start: Date,
    today: Date,
    budget: usize,
    windows: &[CollectionWindow],
) -> miette::Result<Vec<Date>> {
    let mut date = start;
    let mut selected = Vec::new();
    let mut index = 0;
    while date < today && selected.len() < budget {
        while windows.get(index).is_some_and(|window| window.end <= date) {
            index += 1;
        }
        if let Some(window) = windows.get(index)
            && window.start <= date
        {
            date = window.end;
        } else {
            selected.push(date);
            date = date
                .tomorrow()
                .map_err(|e| miette::miette!("source recovery date: {e}"))?;
        }
    }
    Ok(selected)
}

fn plan(
    config: &Config,
    today: Date,
    coverage: &BTreeMap<String, Vec<CollectionWindow>>,
) -> miette::Result<Vec<SourcePlan>> {
    let recovery = config.ingest.recovery.as_ref().ok_or_else(|| {
        miette::miette!("source recovery is not enabled; set ingest.recovery.start_date explicitly")
    })?;
    config
        .enabled_sources()
        .filter(|(_, source)| {
            source.source_type.descriptor().recovery == SourceRecovery::DateWindow
        })
        .map(|(id, _)| {
            Ok(SourcePlan {
                id: id.to_owned(),
                dates: uncovered_days(
                    recovery.start_date,
                    today,
                    recovery.max_days_per_source_per_run.get(),
                    coverage.get(id).map(Vec::as_slice).unwrap_or_default(),
                )?,
            })
        })
        .collect()
}

pub async fn run(opts: &GlobalOptions, dry_run: bool, wait_for_vault: bool) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let (config, revision) = load_config_with_revision(&config_path)?;
    let today = jiff::Timestamp::now()
        .to_zoned(config.vault.timezone())
        .date();
    // Validate opt-in before provider authentication or identity requests.
    let unverified = plan(&config, today, &BTreeMap::new())?;
    let (identities, identity_failure) = if unverified.iter().any(|source| !source.dates.is_empty())
    {
        current_identities(&config).await?
    } else {
        (BTreeMap::new(), false)
    };
    super::ensure_config_current(&config_path, &revision)?;
    let coverage = source_coverage(&config, &identities)?;
    let plans = plan(&config, today, &coverage)?;
    let pinned_opts = GlobalOptions {
        config: Some(config_path.clone()),
        template_dir: opts.template_dir.clone(),
    };
    for (id, source) in config.enabled_sources() {
        match source.source_type.descriptor().recovery {
            SourceRecovery::DateWindow => {}
            SourceRecovery::ConfiguredQuery => eprintln!(
                "[recovery] {id}: historical coverage unverified; configured queries/finite listings cannot establish dated replay; ordinary collection remains enabled"
            ),
            SourceRecovery::CurrentSnapshot => eprintln!(
                "[recovery] {id}: current snapshot/inbox only; ordinary collection handles retained current inputs, not historic provider states"
            ),
        }
    }
    let mut failed = identity_failure;
    let mut attempts = 0;
    for source in plans {
        let Some(identity) = identities.get(&source.id) else {
            continue;
        };
        if source.dates.is_empty() {
            eprintln!("[recovery] {}: no uncovered closed query days", source.id);
        }
        for date in source.dates {
            super::ensure_config_current(&config_path, &revision)?;
            eprintln!(
                "[recovery] {} {date}: {}dated provider query; deleted/intermediate revisions remain outside this coverage",
                source.id,
                if dry_run {
                    "would collect "
                } else {
                    "collecting "
                },
            );
            attempts += 1;
            if dry_run {
                continue;
            }
            let result = super::run_with_identity(
                &pinned_opts,
                Some(source.id.clone()),
                Some(date.to_string()),
                false,
                wait_for_vault,
                Some(identity),
                Some(&revision),
            )
            .await;
            super::ensure_config_current(&config_path, &revision)?;
            if let Err(error) = result {
                failed = true;
                eprintln!(
                    "[recovery] {} {date}: failed; coverage remains pending: {error}",
                    source.id
                );
            }
        }
    }
    eprintln!(
        "[recovery] {attempts} source-day {} within configured per-source budget; filtered recovery does not rebuild the cross-source work-log",
        if dry_run { "selections" } else { "attempts" },
    );
    if failed {
        return Err(miette::miette!(
            "source recovery had incomplete attempts; accepted publications are retained and remaining days retry on the next run"
        ));
    }
    Ok(())
}

async fn current_identities(
    config: &Config,
) -> miette::Result<(BTreeMap<String, lk_source::CollectionIdentity>, bool)> {
    let sources = config
        .enabled_sources()
        .filter(|(_, source)| {
            source.source_type.descriptor().recovery == SourceRecovery::DateWindow
        })
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Ok((BTreeMap::new(), false));
    }
    let http = lk_source::build_http_client().map_err(|error| miette::miette!("{error}"))?;
    let credentials = lk_source::credentials::CredentialStore::new(&config.credentials_path())
        .and_then(|store| store.load().map(|credentials| (store, credentials)));
    let atlassian = credentials
        .as_ref()
        .map(|(store, credentials)| {
            lk_source::build_atlassian_registry_with_store(credentials, &http, store)
        })
        .unwrap_or_default();
    let mut identities = BTreeMap::new();
    let mut failed = false;
    for (id, source) in sources {
        let started_at = std::time::Instant::now();
        let result = async {
            lk_source::validate_params(source.source_type, &source.params)?;
            let empty = lk_source::credentials::Credentials::default();
            let credentials = if source.source_type == lk_core::config::SourceType::Tasks {
                &empty
            } else {
                &credentials
                    .as_ref()
                    .map_err(|error| lk_source::SourceError::Auth(error.to_string()))?
                    .1
            };
            let adapter = lk_source::build_source(
                source.source_type,
                http.clone(),
                credentials,
                &atlassian,
                source.instance.as_deref(),
            )?;
            adapter
                .collection_identity()
                .await?
                .ok_or(lk_source::SourceError::CollectionIdentityUnavailable)
        };
        match super::collect_with_budget(
            started_at,
            source
                .collection_timeout_seconds
                .unwrap_or(config.ingest.source_timeout_seconds),
            result,
        )
        .await
        {
            Ok(identity) => {
                identities.insert(id.to_owned(), identity);
            }
            Err(error) => {
                failed = true;
                eprintln!(
                    "[recovery] {id}: collection identity unavailable; previous coverage is unverified and this source was not replayed: {error}. Check the intended provider account, dataset and identity-read permissions; explicit dated ingest remains available."
                );
            }
        }
    }
    Ok((identities, failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn recovery_selects_only_missing_closed_days_and_skips_covered_ranges() {
        let windows = [
            CollectionWindow {
                start: date(2020, 1, 1),
                end: date(2026, 10, 17),
            },
            CollectionWindow {
                start: date(2026, 10, 18),
                end: date(2026, 10, 19),
            },
        ];
        assert_eq!(
            uncovered_days(date(2020, 1, 1), date(2026, 10, 20), 3, &windows).unwrap(),
            vec![date(2026, 10, 17), date(2026, 10, 19)]
        );
        assert_eq!(
            uncovered_days(date(2026, 10, 15), date(2026, 10, 20), 2, &[]).unwrap(),
            vec![date(2026, 10, 15), date(2026, 10, 16)]
        );
        assert!(
            uncovered_days(date(2026, 10, 20), date(2026, 10, 20), 2, &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn recovery_plans_sources_independently_and_preserves_current_snapshot_limits() {
        let mut config: Config = lk_core::yaml::from_str(
            "vault: {root: vault}\nidentity: {name: Test, email: test@example.invalid}\ningest:\n  recovery: {start_date: '2026-10-15', max_days_per_source_per_run: 2}\nsources:\n  mail: {type: gmail}\n  tasks: {type: tasks}\n  disabled: {type: gmail, enabled: false}\n  notes: {type: manual}\n  repo: {type: nodex}\n  news: {type: rss}\n  jira: {type: jira}\n  search: {type: slack-search}\n",
        ).unwrap();
        let coverage = BTreeMap::from([(
            "mail".into(),
            vec![CollectionWindow {
                start: date(2026, 10, 15),
                end: date(2026, 10, 19),
            }],
        )]);
        assert_eq!(
            plan(&config, date(2026, 10, 20), &coverage).unwrap(),
            vec![
                SourcePlan {
                    id: "mail".into(),
                    dates: vec![date(2026, 10, 19)]
                },
                SourcePlan {
                    id: "tasks".into(),
                    dates: vec![date(2026, 10, 15), date(2026, 10, 16)]
                },
            ]
        );
        config.ingest.recovery = None;
        assert!(plan(&config, date(2026, 10, 20), &coverage).is_err());
    }
}
