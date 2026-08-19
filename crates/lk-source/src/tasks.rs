//! The user's own completed tasks.
//!
//! The only source whose items this tool produced itself: a task finished on the board is work
//! performed, and it re-enters here through the same door Gmail and Jira use. That is the whole
//! archive — the daily page, the work-log, the contribution categories, the concept extraction
//! and every review already consume events, so a completion becomes all of them without any of
//! them changing.
//!
//! It reads the intent plane's transition log rather than the board, because the board holds
//! what is still OPEN and a completed task has left it. The log is a closed record per date, so
//! a past day re-reads complete and `lore ingest --date <past>` reproduces its page exactly.

use async_trait::async_trait;
use lk_core::event::RawItem;
use serde::Deserialize;

use crate::{ExtractContext, Source, SourceError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TasksParams {
    /// How far back to read, like every other source's window.
    ///
    /// A day is closed out in the evening and the pipeline runs the next morning, so a source
    /// reading only the target date would archive a day that has barely started and never the
    /// one that just ended. Rounded outward to whole days, because each date's record is
    /// complete on its own and a partial day would overwrite that date's page with a fragment.
    #[serde(default = "default_lookback_hours")]
    lookback_hours: u32,
}

fn default_lookback_hours() -> u32 {
    24
}

impl crate::ValidatedParams for TasksParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.lookback_hours == 0 {
            return Err(SourceError::InvalidParams(
                "tasks `lookback_hours` must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<TasksParams>(params).map(|_| ())
}

/// The dates a window covers, oldest first — whole days, rounded outward.
fn window(target: jiff::civil::Date, lookback_hours: u32) -> Vec<jiff::civil::Date> {
    let days_back = i32::try_from(lookback_hours.div_ceil(24)).unwrap_or(i32::MAX);
    (0..=days_back)
        .rev()
        .filter_map(|back| target.checked_sub(jiff::Span::new().days(back)).ok())
        .collect()
}

pub struct TasksSource;

#[async_trait]
impl Source for TasksSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let params = crate::parse_validated::<TasksParams>(params)?;
        let log = lk_task::TransitionLog::new(&ctx.vault_root);

        let mut items = Vec::new();
        for date in window(ctx.target_date, params.lookback_hours) {
            // A day with no transitions is a quiet day, not a failure: nothing was attempted,
            // so an empty answer is hiding nothing — unlike a source that reached a network and
            // came back with none of what it asked for.
            let transitions = log
                .read(date)
                .map_err(|e| SourceError::Parse(format!("{e}")))?;
            items.extend(transitions.iter().filter_map(|t| t.observation()));
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_task::{Transition, TransitionKind};

    fn context(root: &std::path::Path, date: jiff::civil::Date) -> ExtractContext {
        ExtractContext {
            target_date: date,
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::En,
            identity: lk_core::config::Identity {
                name: "t".into(),
                email: "t@t.com".into(),
                ..Default::default()
            },
            vault_root: root.to_path_buf(),
        }
    }

    fn at(hour: i8) -> jiff::Timestamp {
        jiff::civil::date(2026, 8, 19)
            .at(hour, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    #[tokio::test]
    async fn a_days_completions_are_the_days_items() {
        let tmp = tempfile::tempdir().unwrap();
        let log = lk_task::TransitionLog::new(tmp.path());
        log.record(
            &[
                Transition::new(
                    "7k2p".parse().unwrap(),
                    TransitionKind::Done,
                    "read the spec",
                    at(9),
                )
                .with_note(Some("tokens rotate on use".into())),
                Transition::new(
                    "3b8q".parse().unwrap(),
                    TransitionKind::Carried,
                    "other",
                    at(18),
                ),
            ],
            &jiff::tz::TimeZone::UTC,
        )
        .unwrap();

        let items = TasksSource
            .extract(
                &serde_json::json!({}),
                &context(tmp.path(), jiff::civil::date(2026, 8, 19)),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1, "only a completion is an observation");
        assert_eq!(items[0].title, "read the spec");
        assert_eq!(items[0].body, "tokens rotate on use");
        assert!(items[0].is_self);
    }

    /// Nothing was attempted, so an empty answer hides nothing — unlike a source that reaches a
    /// network and comes back with none of what it asked for.
    #[tokio::test]
    async fn a_day_with_no_transitions_is_a_quiet_day() {
        let tmp = tempfile::tempdir().unwrap();
        let items = TasksSource
            .extract(
                &serde_json::json!({}),
                &context(tmp.path(), jiff::civil::date(2026, 8, 19)),
            )
            .await
            .unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn a_key_that_does_nothing_is_refused_rather_than_ignored() {
        assert!(validate_params(&serde_json::json!({ "inbox_dir": "x" })).is_err());
        assert!(validate_params(&serde_json::json!({})).is_ok());
        assert!(validate_params(&serde_json::json!({ "lookback_hours": 0 })).is_err());
    }

    /// The day is closed out in the evening and the pipeline runs the next morning, so the
    /// window has to reach back over the day that just ended.
    #[test]
    fn the_window_is_whole_days_reaching_back_over_the_one_that_ended() {
        let target = jiff::civil::date(2026, 8, 19);
        assert_eq!(window(target, 24), [jiff::civil::date(2026, 8, 18), target]);
        // Rounded outward: a partial day would hand the pipeline a fragment of a date and
        // overwrite that date's page with it.
        assert_eq!(window(target, 1), [jiff::civil::date(2026, 8, 18), target]);
        assert_eq!(window(target, 48).len(), 3);
    }

    #[tokio::test]
    async fn yesterdays_completions_reach_this_mornings_run() {
        let tmp = tempfile::tempdir().unwrap();
        let log = lk_task::TransitionLog::new(tmp.path());
        let yesterday = jiff::civil::date(2026, 8, 18)
            .at(22, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp();
        log.record(
            &[Transition::new(
                "7k2p".parse().unwrap(),
                TransitionKind::Done,
                "closed last night",
                yesterday,
            )],
            &jiff::tz::TimeZone::UTC,
        )
        .unwrap();

        let items = TasksSource
            .extract(
                &serde_json::json!({}),
                &context(tmp.path(), jiff::civil::date(2026, 8, 19)),
            )
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].timestamp, yesterday);
    }
}
