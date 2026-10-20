use std::io::{BufReader, BufWriter, Write};

use lk_core::captured_file::CapturedFile;
use lk_source::SourceError;

use super::SourceCapture;

pub(super) struct CapturedSource {
    observation: CapturedFile,
    originals: Vec<lk_core::manual_original::ManualBundleCapture>,
    retirement_blocked: bool,
}

fn failure(error: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("source acquisition storage: {error}"))
}

impl CapturedSource {
    pub(super) fn capture(source: SourceCapture) -> Result<Self, SourceError> {
        let mut file = tempfile::Builder::new()
            .prefix("lorekeeper-acquisition-")
            .tempfile()
            .map_err(failure)?;
        {
            let mut writer = BufWriter::new(file.as_file_mut());
            serde_json::to_writer(&mut writer, &source.observation).map_err(failure)?;
            writer.flush().map_err(failure)?;
        }
        Ok(Self {
            observation: CapturedFile::from_file(file).map_err(failure)?,
            originals: source.originals,
            retirement_blocked: source.retirement_blocked,
        })
    }

    pub(super) fn restore(self) -> Result<SourceCapture, SourceError> {
        let reader = self.observation.reader().map_err(failure)?;
        let observation = serde_json::from_reader(BufReader::new(reader)).map_err(failure)?;
        Ok(SourceCapture {
            observation,
            originals: self.originals,
            retirement_blocked: self.retirement_blocked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::event::{OpenWork, RawItem, WorkState};
    use lk_source::{ObservationOutcome, ObservationScope, SourceObservation};

    fn source() -> SourceCapture {
        SourceCapture {
            observation: SourceObservation {
                items: vec![RawItem {
                    external_id: Some("versioned-identity".into()),
                    title: "운영 정책".into(),
                    body: "Original\nUnicode 😀\n".repeat(64),
                    url: Some("https://example.com/policy".into()),
                    author: None,
                    timestamp: "2026-10-19T12:00:00Z".parse().unwrap(),
                    is_self: true,
                    work: Some(WorkState::Open(OpenWork {
                        summary: "Check policy".into(),
                        url: "https://example.com/task".into(),
                    })),
                    labels: vec!["decision".into()],
                    metadata: serde_json::json!({"version": 17, "nested": [null, true]}),
                }],
                scope: ObservationScope::Window {
                    start: "2026-10-19T00:00:00Z".parse().unwrap(),
                    end: "2026-10-20T00:00:00Z".parse().unwrap(),
                },
                outcome: ObservationOutcome::Partial,
            },
            originals: Vec::new(),
            retirement_blocked: true,
        }
    }

    #[test]
    fn acquisition_preserves_exact_items_and_scope_and_removes_private_scratch() {
        let expected = source();
        let stored = CapturedSource::capture(source()).unwrap();
        let path = stored.observation.path().to_owned();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let actual = stored.restore().unwrap();
        assert!(!path.exists());
        assert_eq!(
            serde_json::to_value(actual.observation).unwrap(),
            serde_json::to_value(expected.observation).unwrap()
        );
        assert!(actual.retirement_blocked);
    }

    #[test]
    fn corrupted_capture_refuses_restore_and_is_removed() {
        let stored = CapturedSource::capture(source()).unwrap();
        let path = stored.observation.path().to_owned();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("Original", "Modified")).unwrap();
        assert!(stored.restore().is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn storing_an_observation_does_not_start_a_fresh_collection_budget() {
        let budget = std::num::NonZeroU64::new(1).unwrap();
        let started = std::time::Instant::now();
        let source = super::super::collect_with_budget(started, budget, async { Ok(source()) })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1010)).await;
        let result = super::super::collect_with_budget(started, budget, async {
            CapturedSource::capture(source)
        })
        .await;
        assert!(matches!(result, Err(SourceError::CollectionTimeout { seconds: 1 })));
    }
}
