//! `lore status` — the one command that answers "is anything wrong, and what do I run".
//!
//! One row per subsystem, each verdict computed by the code its owning command exits on, each
//! naming that command. It reports and does not gate: the owning command's exit code is the
//! verdict a caller acts on, so a summary that also failed the shell would be one more gate
//! rather than a front door.

use std::fmt::Write as _;

use serde::Serialize;

use super::{find_config, load_config};

#[derive(Serialize)]
struct Subsystem {
    name: &'static str,
    ok: bool,
    state: String,
    next: &'static str,
}

pub async fn run(opts: &super::GlobalOptions, json: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let now = jiff::Timestamp::now();
    let mut subsystems = Vec::new();

    let installation = lk_dist::Installation::detect().map_err(|e| miette::miette!("{e}"))?;
    let agents = vault_root
        .join(&config.vault.dirs.wiki)
        .join(lk_core::vault_path::SCHEMA_FILE);
    let schema = lk_dist::SchemaState::read(&agents, &super::schema::generator());
    let install = lk_dist::InstallationReport::build(&installation, schema);
    subsystems.push(Subsystem {
        name: "install",
        ok: install.stale() == 0,
        state: match install.stale() {
            0 => "current".to_string(),
            1 => "1 deployed copy differs".to_string(),
            n => format!("{n} deployed copies differ"),
        },
        next: install.repair(),
    });

    // `lore agenda` is a view that always succeeds, so this row's mark is the board's own
    // judgment that a person has something to do: a page a write cannot land on, or tasks no
    // rule can reach.
    if let Some(board) = super::task::IntentPlane::survey(opts) {
        subsystems.push(Subsystem {
            name: "board",
            ok: board.ok,
            state: board.state,
            next: "lore agenda",
        });
    }

    let freshness = super::health::freshness(&config, &vault_root, now).await?;
    let (fresh, stale, never) = (freshness.fresh(), freshness.stale(), freshness.never());
    let mut currency = format!("{fresh} fresh");
    if stale > 0 {
        let _ = write!(currency, " · {stale} stale");
    }
    if never > 0 {
        let _ = write!(currency, " · {never} never ingested");
    }
    subsystems.push(Subsystem {
        name: "sources",
        ok: stale == 0,
        state: currency,
        next: "lore health",
    });

    // The deterministic half of extraction currency: whether the repository moved under the
    // paths each manifest declared. Judging the extracted knowledge needs `/lore-extract audit`.
    let extracts =
        super::extract::survey(&vault_root, now.to_zoned(config.vault.timezone()).date())?;
    if !extracts.is_empty() {
        let tally = super::extract::tally(&extracts);
        subsystems.push(Subsystem {
            name: "extract",
            ok: tally.behind == 0,
            state: super::extract::coverage(&tally),
            next: "lore extract status",
        });
    }

    let queue = super::queue::check::inspect(&vault_root)?;
    let (state, next) = queue_row(&queue);
    subsystems.push(Subsystem {
        name: "queue",
        ok: queue.is_settled(),
        state,
        next,
    });

    let in_flight = super::queue::work_in_flight(&vault_root);
    let audit = super::doctor::audit(
        &super::doctor::managed_roots(&vault_root, &config.vault.dirs),
        &vault_root,
        &in_flight.keys,
    );
    let mut pages = match (audit.defects(), audit.errors()) {
        (0, 0) => format!("{} clean", audit.scanned),
        (n, _) => format!("{n} defect(s) in {} pages", audit.scanned),
    };
    if audit.errors() > 0 {
        let _ = write!(pages, " · {} check error(s)", audit.errors());
    }
    if audit.credentials() > 0 {
        let _ = write!(pages, " · {} credential form(s)", audit.credentials());
    }
    subsystems.push(Subsystem {
        name: "pages",
        ok: audit.defects() == 0 && audit.errors() == 0,
        state: pages,
        next: "lore doctor",
    });

    // `lore graph` resolves its own root and scope, so the row reuses that resolution and
    // stays the verdict `lore graph lint` exits on.
    let (ok, state) = match super::graph::lint(opts, None) {
        Ok(report) if !report.errors.is_empty() => {
            (false, format!("incomplete: {}", report.errors.join("; ")))
        }
        Ok(report) => match report.violations.count() {
            0 => (
                true,
                format!("sound · {} pages, {} links", report.pages, report.links),
            ),
            n => (false, format!("{n} violation(s)")),
        },
        Err(e) => (false, format!("unreadable: {e}")),
    };
    subsystems.push(Subsystem {
        name: "graph",
        ok,
        state,
        next: "lore graph lint",
    });

    match lk_vault::list_reviews(&vault_root, &config.vault.dirs) {
        Ok(reports) if reports.is_empty() => {}
        Ok(reports) => {
            let health = lk_vault::ReviewHealth::from_reports(&reports);
            subsystems.push(Subsystem {
                name: "reviews",
                ok: !health.requires_attention(),
                state: review_state(&health),
                next: "lore wiki reviews --check",
            });
        }
        Err(e) => subsystems.push(Subsystem {
            name: "reviews",
            ok: false,
            state: format!("unreadable: {e}"),
            next: "lore wiki reviews --check",
        }),
    }

    if json {
        let envelope = serde_json::json!({
            "ok": true,
            "data": {
                "version": env!("CARGO_PKG_VERSION"),
                "vault": vault_root.display().to_string(),
                "subsystems": subsystems,
            }
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).map_err(|e| miette::miette!("{e}"))?
        );
    } else {
        eprintln!(
            "lore {}  {}",
            env!("CARGO_PKG_VERSION"),
            vault_root.display()
        );
        for subsystem in &subsystems {
            line(subsystem);
        }
    }
    Ok(())
}

/// The verdict `lore queue check` exits on. An interrupted publication, a staged result or an
/// unreadable task is unsettled without being work a drain picks up, so those states route to
/// `queue check`; only waiting work routes to the drain.
fn queue_row(queue: &super::queue::check::QueueSettlement) -> (String, &'static str) {
    let mut parts = Vec::new();
    if queue.current > 0 {
        parts.push(format!(
            "{} task(s) waiting for an LLM session",
            queue.current
        ));
    }
    if queue.unreadable > 0 {
        parts.push(format!("{} unreadable", queue.unreadable));
    }
    if queue.unparseable_lines > 0 {
        parts.push(format!("{} unparseable line(s)", queue.unparseable_lines));
    }
    if !queue.staged_results.is_empty() {
        parts.push(format!("{} staged result(s)", queue.staged_results.len()));
    }
    if queue.pending_mutation {
        parts.push("interrupted publication".to_string());
    }
    if parts.is_empty() {
        return ("drained".to_string(), "/lore-process");
    }
    let only_waiting = queue.unreadable == 0
        && queue.unparseable_lines == 0
        && queue.staged_results.is_empty()
        && !queue.pending_mutation;
    let next = if only_waiting {
        "/lore-process"
    } else {
        "lore queue check"
    };
    (parts.join(" · "), next)
}

fn review_state(health: &lk_vault::ReviewHealth) -> String {
    [
        (health.needs_reassessment, "need reassessment"),
        (health.invalid, "invalid"),
        (health.open, "open"),
        (health.resolved, "resolved"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The gap before the command is added rather than padded into, so a state long enough to fill
/// the column never runs into the command name.
fn line(subsystem: &Subsystem) {
    let mark = if subsystem.ok { '·' } else { '!' };
    eprintln!(
        "  {mark} {}{}  {}",
        super::pad(subsystem.name, 9),
        super::pad(&subsystem.state, 42),
        subsystem.next
    );
}

#[cfg(test)]
mod tests {
    use super::{queue_row, review_state};
    use crate::commands::queue::check::QueueSettlement;

    fn settled() -> QueueSettlement {
        QueueSettlement {
            current: 0,
            unreadable: 0,
            unparseable_lines: 0,
            staged_results: Vec::new(),
            pending_mutation: false,
        }
    }

    #[test]
    fn a_settled_queue_is_drained() {
        let queue = settled();
        assert!(queue.is_settled());
        assert_eq!(queue_row(&queue), ("drained".to_string(), "/lore-process"));
    }

    #[test]
    fn waiting_work_alone_names_the_drain() {
        let queue = QueueSettlement {
            current: 3,
            ..settled()
        };
        assert!(!queue.is_settled());
        assert_eq!(
            queue_row(&queue),
            (
                "3 task(s) waiting for an LLM session".to_string(),
                "/lore-process"
            )
        );
    }

    #[test]
    fn every_unsettled_state_is_named_and_routes_to_queue_check() {
        let queue = QueueSettlement {
            current: 1,
            unreadable: 2,
            unparseable_lines: 3,
            staged_results: vec!["result.json".to_string()],
            pending_mutation: true,
        };
        assert_eq!(
            queue_row(&queue),
            (
                "1 task(s) waiting for an LLM session · 2 unreadable · 3 unparseable line(s) \
                 · 1 staged result(s) · interrupted publication"
                    .to_string(),
                "lore queue check"
            )
        );
    }

    #[test]
    fn reviews_needing_attention_lead_and_empty_states_are_omitted() {
        let health = lk_vault::ReviewHealth {
            open: 2,
            resolved: 5,
            needs_reassessment: 1,
            invalid: 0,
        };
        assert!(health.requires_attention());
        assert_eq!(
            review_state(&health),
            "1 need reassessment · 2 open · 5 resolved"
        );
    }
}
