use std::io::Read;
use std::path::{Path, PathBuf};

use lk_core::execution::{AgentEngine, ExecutionProfile};
use miette::IntoDiagnostic;
use serde_json::Value;

pub(in crate::commands) struct Invocation {
    pub agent: super::AgentInvocation,
    pub output: Output,
}

pub(in crate::commands) struct Output {
    engine: AgentEngine,
    pub capture_limit: Option<usize>,
    result_path: PathBuf,
    transport_limit: usize,
    _directory: tempfile::TempDir,
}

impl Output {
    pub fn response(&self, stdout: &[u8], max_result_bytes: usize) -> miette::Result<Value> {
        let bytes = match self.engine {
            AgentEngine::ClaudeCode => stdout.to_vec(),
            AgentEngine::Codex => {
                let metadata = std::fs::symlink_metadata(&self.result_path).into_diagnostic()?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(miette::miette!(
                        "native result must be a direct regular file"
                    ));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&self.result_path)
                    .into_diagnostic()?
                    .take(self.transport_limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .into_diagnostic()?;
                if bytes.len() > self.transport_limit {
                    return Err(miette::miette!(
                        "native terminal result exceeds its transport bound"
                    ));
                }
                bytes
            }
        };
        super::observation::decode_response(self.engine, &bytes, max_result_bytes)
    }
}

pub(in crate::commands) fn build(
    profile: &ExecutionProfile,
    installation: &lk_dist::Installation,
    config: &Path,
    request: &Value,
    schema: &Value,
    max_result_bytes: usize,
) -> miette::Result<Invocation> {
    let directory = tempfile::Builder::new()
        .prefix("lorekeeper-result-")
        .tempdir()
        .into_diagnostic()?;
    let schema = serde_json::to_string(schema).into_diagnostic()?;
    let schema_path = directory.path().join("result.schema.json");
    std::fs::write(&schema_path, &schema).into_diagnostic()?;
    let result_path = directory.path().join("result.json");
    let agent = super::agent_invocation_with_output(
        profile,
        "lore-process",
        "Complete only the supplied native semantic result request.",
        installation,
        config,
        Some(directory.path()),
        Some(request),
        super::AgentOutput::Structured {
            schema: &schema,
            schema_path: &schema_path,
            result_path: &result_path,
        },
    )?;
    // JSON transport includes formatting and host metadata; the decoded semantic
    // value and assembled result retain their separate exact byte limits.
    let transport_limit = max_result_bytes.saturating_mul(8).saturating_add(65536);
    Ok(Invocation {
        agent,
        output: Output {
            engine: profile.engine(),
            result_path,
            transport_limit,
            capture_limit: (profile.engine() == AgentEngine::ClaudeCode).then_some(transport_limit),
            _directory: directory,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_results_are_explicit_bounded_and_never_scraped_from_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("result.json");
        let mut invocation = Output {
            engine: AgentEngine::Codex,
            capture_limit: None,
            result_path: path.clone(),
            transport_limit: 128,
            _directory: directory,
        };
        assert!(
            invocation
                .response(br#"{"type":"item.completed"}"#, 64)
                .is_err()
        );
        std::fs::write(&path, br#"{"value":"exact"}"#).unwrap();
        assert_eq!(
            invocation.response(b"unbounded tool events", 64).unwrap(),
            json!({"value":"exact"})
        );
        std::fs::write(&path, "x".repeat(129)).unwrap();
        assert!(invocation.response(b"", 64).is_err());
        invocation.engine = AgentEngine::ClaudeCode;
        let result = br#"{"type":"result","subtype":"success","is_error":false,"structured_output":{"value":"exact"}}"#;
        assert_eq!(
            invocation.response(result, 64).unwrap(),
            json!({"value":"exact"})
        );
        assert!(invocation.response(result, 2).is_err());
        assert!(
            invocation
                .response(
                    br#"{"type":"result","subtype":"success","is_error":false,"result":"text"}"#,
                    64
                )
                .is_err()
        );
        assert!(invocation.response(b"{}\n{}", 64).is_err());
    }
}
