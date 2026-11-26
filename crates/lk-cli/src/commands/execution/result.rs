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
    pub fn response(&self, stdout: &[u8], max_result_bytes: usize) -> Result<Value, ResponseError> {
        let bytes = match self.engine {
            AgentEngine::ClaudeCode => stdout.to_vec(),
            AgentEngine::Codex => {
                let directory = std::fs::symlink_metadata(self._directory.path())
                    .into_diagnostic()
                    .map_err(ResponseError::Local)?;
                if !directory.is_dir() || directory.file_type().is_symlink() {
                    return Err(ResponseError::Local(miette::miette!(
                        "native result directory must remain a direct directory"
                    )));
                }
                let metadata = match std::fs::symlink_metadata(&self.result_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(ResponseError::Remote {
                            failure:
                                super::super::work::state::InvocationFailure::MissingResponse {
                                    engine: self.engine,
                                },
                            detail: miette::miette!(
                                "native host returned no required terminal result"
                            ),
                        });
                    }
                    Err(error) => {
                        return Err(ResponseError::Local(miette::miette!(
                            "read native terminal result metadata: {error}"
                        )));
                    }
                };
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(ResponseError::Local(miette::miette!(
                        "native result must be a direct regular file"
                    )));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&self.result_path)
                    .into_diagnostic()
                    .map_err(ResponseError::Local)?
                    .take(self.transport_limit.saturating_add(1) as u64)
                    .read_to_end(&mut bytes)
                    .into_diagnostic()
                    .map_err(ResponseError::Local)?;
                if bytes.len() > self.transport_limit {
                    return Err(ResponseError::Remote {
                        failure: super::super::work::state::InvocationFailure::InvalidResponse {
                            engine: self.engine,
                            revision: lk_core::rss_original::digest(&bytes),
                            bytes: bytes.len(),
                        },
                        detail: miette::miette!(
                            "native terminal result exceeds its transport bound"
                        ),
                    });
                }
                bytes
            }
        };
        super::observation::decode_response(self.engine, &bytes, max_result_bytes).map_err(
            |detail| ResponseError::Remote {
                failure: super::observation::response_failure(self.engine, &bytes),
                detail,
            },
        )
    }
}

#[derive(Debug)]
pub(in crate::commands) enum ResponseError {
    Local(miette::Report),
    Remote {
        failure: super::super::work::state::InvocationFailure,
        detail: miette::Report,
    },
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(error) => write!(f, "{error}"),
            Self::Remote { detail, .. } => write!(f, "{detail}"),
        }
    }
}

pub(in crate::commands) struct Contract<'a> {
    pub schema: &'a Value,
    pub max_result_bytes: usize,
    pub access: super::AgentAccess,
}

pub(in crate::commands) fn build(
    profile: &ExecutionProfile,
    skill: &str,
    installation: &lk_dist::Installation,
    config: &Path,
    request: &Value,
    contract: Contract<'_>,
) -> miette::Result<Invocation> {
    let Contract {
        schema,
        max_result_bytes,
        access,
    } = contract;
    let directory =
        crate::commands::private_directory::temporary("lorekeeper-result-").into_diagnostic()?;
    let schema = serde_json::to_string(schema).into_diagnostic()?;
    let schema_path = directory.path().join("result.schema.json");
    std::fs::write(&schema_path, &schema).into_diagnostic()?;
    let result_path = directory.path().join("result.json");
    let agent = super::result_invocation(
        profile,
        skill,
        installation,
        config,
        directory.path(),
        request,
        super::ResultContract {
            schema: &schema,
            schema_path: &schema_path,
            result_path: &result_path,
            access,
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
        assert!(matches!(
            invocation.response(br#"{"type":"item.completed"}"#, 64),
            Err(ResponseError::Remote {
                failure: super::super::super::work::state::InvocationFailure::MissingResponse { .. },
                ..
            })
        ));
        std::fs::write(&path, br#"{"value":"exact"}"#).unwrap();
        assert_eq!(
            invocation.response(b"unbounded tool events", 64).unwrap(),
            json!({"value":"exact"})
        );
        invocation.transport_limit = usize::MAX;
        assert_eq!(
            invocation.response(b"", 64).unwrap(),
            json!({"value":"exact"})
        );
        invocation.transport_limit = 128;
        std::fs::write(&path, "x".repeat(129)).unwrap();
        assert!(matches!(
            invocation.response(b"", 64),
            Err(ResponseError::Remote { .. })
        ));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            invocation.response(b"", 64),
            Err(ResponseError::Local(_))
        ));
        std::fs::remove_dir(&path).unwrap();
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
