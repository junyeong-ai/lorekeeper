use lk_core::execution::AgentEngine;
use serde::Serialize;
use serde_json::Value;

use super::{ChildResult, StdoutCapture};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum Phase {
    VersionPreflight,
    Invocation,
    Response,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum OutputFormat {
    Empty,
    InvalidJson,
    Json,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum JsonType {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl JsonType {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Boolean,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonField {
    kind: JsonType,
    string_utf8_bytes: Option<usize>,
    members: Option<usize>,
    recognized: Option<&'static str>,
}

impl JsonField {
    fn from_value(value: &Value, known: &[&'static str]) -> Self {
        Self {
            kind: JsonType::from_value(value),
            string_utf8_bytes: value.as_str().map(str::len),
            members: match value {
                Value::Array(items) => Some(items.len()),
                Value::Object(fields) => Some(fields.len()),
                _ => None,
            },
            recognized: value
                .as_str()
                .and_then(|actual| known.iter().copied().find(|known| *known == actual)),
        }
    }
}

#[derive(Debug, Serialize)]
pub(in crate::commands) struct HostFailure {
    phase: Phase,
    engine: AgentEngine,
    exit_code: Option<i32>,
    stdout_capture: StdoutCapture,
    stdout_bytes: Option<usize>,
    stdout_format: Option<OutputFormat>,
    json_type: Option<JsonType>,
    message_type: Option<JsonField>,
    subtype: Option<JsonField>,
    status: Option<JsonField>,
    is_error: Option<bool>,
    is_error_type: Option<JsonType>,
    error: Option<JsonField>,
    errors: Option<JsonField>,
    result: Option<JsonField>,
    structured_output: Option<JsonField>,
}

impl HostFailure {
    pub fn from_child(engine: AgentEngine, phase: Phase, child: &ChildResult) -> Self {
        Self::from_output(
            engine,
            phase,
            child.exit_code,
            child.stdout_capture,
            &child.stdout,
        )
    }

    pub fn from_response(engine: AgentEngine, bytes: &[u8]) -> Self {
        Self::from_output(
            engine,
            Phase::Response,
            Some(0),
            StdoutCapture::Complete,
            bytes,
        )
    }

    fn from_output(
        engine: AgentEngine,
        phase: Phase,
        exit_code: Option<i32>,
        capture: StdoutCapture,
        bytes: &[u8],
    ) -> Self {
        let (format, value) = if capture != StdoutCapture::Complete {
            (None, None)
        } else if bytes.is_empty() {
            (Some(OutputFormat::Empty), None)
        } else {
            match serde_json::from_slice::<Value>(bytes) {
                Ok(value) => (Some(OutputFormat::Json), Some(value)),
                Err(_) => (Some(OutputFormat::InvalidJson), None),
            }
        };
        let field = |name: &str, known: &[&'static str]| {
            value
                .as_ref()?
                .get(name)
                .map(|value| JsonField::from_value(value, known))
        };
        let is_error = value.as_ref().and_then(|value| value.get("is_error"));
        Self {
            phase,
            engine,
            exit_code,
            stdout_capture: capture,
            stdout_bytes: (capture == StdoutCapture::Complete).then_some(bytes.len()),
            stdout_format: format,
            json_type: value.as_ref().map(JsonType::from_value),
            message_type: field("type", &["result", "error"]),
            subtype: field(
                "subtype",
                &[
                    "success",
                    "error_during_execution",
                    "error_max_turns",
                    "error_max_budget_usd",
                    "error_max_structured_output_retries",
                ],
            ),
            status: field("status", &["complete", "needs-action", "needs-discovery"]),
            is_error: is_error.and_then(Value::as_bool),
            is_error_type: is_error.map(JsonType::from_value),
            error: field("error", &[]),
            errors: field("errors", &[]),
            result: field("result", &[]),
            structured_output: field("structured_output", &[]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(bytes: &[u8]) -> Value {
        serde_json::to_value(HostFailure::from_response(AgentEngine::ClaudeCode, bytes)).unwrap()
    }

    #[test]
    fn failed_envelopes_expose_only_fixed_shape_and_known_status() {
        let raw = serde_json::to_vec(&json!({
            "type":"result", "subtype":"error_during_execution", "is_error":true,
            "errors":["SECRET_SOURCE", {"SECRET_KEY":"SECRET_VALUE"}],
            "result":"SECRET_RESULT", "structured_output":{"secret":"SECRET_BODY"},
            "session_id":"SECRET_SESSION", "SECRET_UNKNOWN":"SECRET_UNKNOWN_VALUE"
        }))
        .unwrap();
        let out = summary(&raw);
        let encoded = serde_json::to_string(&out).unwrap();
        assert!(!encoded.contains("SECRET"), "{encoded}");
        assert_eq!(out["message_type"]["recognized"], "result");
        assert_eq!(out["subtype"]["recognized"], "error_during_execution");
        assert_eq!(out["is_error"], true);
        assert_eq!(out["errors"]["members"], 2);
        assert_eq!(out["structured_output"]["kind"], "object");
        assert_eq!(out["stdout_bytes"], raw.len());
        assert_eq!(out["phase"], "response");
    }

    #[test]
    fn unknown_tokens_and_invalid_or_nonobject_json_do_not_leak_values() {
        for raw in [
            b"not JSON SECRET".as_slice(),
            b"\"SECRET\"",
            b"[\"SECRET\"]",
            b"",
            br#"{"type":"SECRET","subtype":"SECRET","status":"SECRET","is_error":"SECRET"}"#,
        ] {
            let out = summary(raw);
            assert!(!serde_json::to_string(&out).unwrap().contains("SECRET"));
        }
        let out = summary(br#"{"type":17,"subtype":[],"is_error":"private"}"#);
        assert_eq!(out["message_type"]["kind"], "number");
        assert!(out["message_type"]["recognized"].is_null());
        assert_eq!(out["subtype"]["kind"], "array");
        assert_eq!(out["is_error_type"], "string");
        assert!(out["is_error"].is_null());
        assert_eq!(summary(b" ")["stdout_format"], "invalid-json");
        assert_eq!(summary(b"")["stdout_format"], "empty");
    }

    #[test]
    fn unobserved_and_discarded_output_remain_unknown() {
        for capture in [StdoutCapture::NotCaptured, StdoutCapture::Unavailable] {
            let child = ChildResult {
                success: false,
                exit_code: Some(1),
                cancelled: false,
                detail: None,
                stdout: b"SECRET".to_vec(),
                stdout_capture: capture,
            };
            let out = serde_json::to_value(HostFailure::from_child(
                AgentEngine::Codex,
                Phase::VersionPreflight,
                &child,
            ))
            .unwrap();
            assert!(out["stdout_bytes"].is_null());
            assert!(out["stdout_format"].is_null());
            assert!(out["json_type"].is_null());
            assert!(!serde_json::to_string(&out).unwrap().contains("SECRET"));
            assert_eq!(out["phase"], "version-preflight");
            assert_eq!(out["exit_code"], 1);
        }
    }
}
