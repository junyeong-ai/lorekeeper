use super::*;
use serde_json::{Value, json};

fn install_response(fixture: &Fixture, engine: &str, output: &str, exit: i32) {
    let version = if engine == "codex" {
        "codex-cli 0.164.0"
    } else {
        "2.1.311 (Claude Code)"
    };
    let script = format!(
        "#!/usr/bin/env python3\nimport sys\nif sys.argv[1:]==['--version']:\n print({version:?});raise SystemExit()\nsys.stdin.read()\nprint({})\nraise SystemExit({exit})\n",
        serde_json::to_string(output).unwrap()
    );
    std::fs::write(fixture.root().join("agent executable"), script).unwrap();
}

fn assert_no_semantic_record(fixture: &Fixture, output: &std::process::Output, job: &Value) {
    assert!(!output.status.success());
    assert_eq!(job["halted"], true, "{job}");
    assert_eq!(job["cancelled"], false, "{job}");
    assert_eq!(job["budget_exhausted"], false, "{job}");
    assert!(
        !serde_json::to_string(job)
            .unwrap()
            .contains("PRIVATE_CANARY")
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_CANARY"));
    let step = &job["work_steps"][0];
    assert!(step["result_json_bytes"].is_null(), "{job}");
    let directory = fixture
        .root()
        .join("vault/.lorekeeper/work")
        .join(step["work_id"].as_str().unwrap());
    let files = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(files, vec![std::ffi::OsString::from("plan.json")]);
}

#[test]
fn failed_structured_hosts_retain_safe_diagnostics_without_adopting_the_reply() {
    for engine in ["codex", "claude-code"] {
        for success_looking in [false, true] {
            let fixture = bounded_audit_fixture(engine, 4, RECORD_BOUNDED_AUDIT);
            let native = json!({"status":"needs-action","observation":null,"action":{"reason":"PRIVATE_CANARY_BODY"}});
            let response = if engine == "claude-code" {
                if success_looking {
                    json!({"type":"result","subtype":"success","is_error":false,"structured_output":native})
                } else {
                    json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["PRIVATE_CANARY_ERROR"],"result":"PRIVATE_CANARY_RESULT","session_id":"PRIVATE_CANARY_SESSION"})
                }
            } else if success_looking {
                native
            } else {
                json!({"error":{"message":"PRIVATE_CANARY_ERROR"},"unknown":"PRIVATE_CANARY_UNKNOWN"})
            };
            let raw = serde_json::to_string(&response).unwrap();
            install_response(&fixture, engine, &raw, 1);
            let (output, job) = fixture.run("knowledge");
            assert_no_semantic_record(&fixture, &output, &job);
            assert_eq!(job["semantic_calls"], 1);
            let step = &job["work_steps"][0];
            assert_eq!(step["outcome"], "failed");
            let failure = &step["host_failure"];
            assert_eq!(failure["engine"], engine);
            assert_eq!(failure["phase"], "invocation");
            assert_eq!(failure["exit_code"], 1);
            assert_eq!(failure["stdout_capture"], "complete");
            assert_eq!(failure["stdout_format"], "json");
            assert_eq!(failure["stdout_bytes"], raw.len() + 1);
            if engine == "claude-code" {
                assert_eq!(failure["message_type"]["recognized"], "result");
                assert_eq!(
                    failure["subtype"]["recognized"],
                    if success_looking {
                        "success"
                    } else {
                        "error_during_execution"
                    }
                );
            }
        }
    }
}

#[test]
fn invalid_success_exit_responses_are_summarized_without_semantic_results() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 4, RECORD_BOUNDED_AUDIT);
        let raw = if engine == "claude-code" {
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["PRIVATE_CANARY_ERROR"]}"#
        } else {
            "PRIVATE_CANARY_NOT_JSON"
        };
        install_response(&fixture, engine, raw, 0);
        let (output, job) = fixture.run("knowledge");
        assert_no_semantic_record(&fixture, &output, &job);
        assert_eq!(job["semantic_calls"], 1);
        let failure = &job["work_steps"][0]["host_failure"];
        assert_eq!(failure["phase"], "response");
        assert_eq!(failure["exit_code"], 0);
        assert_eq!(failure["stdout_capture"], "complete");
        assert_eq!(
            failure["stdout_format"],
            if engine == "claude-code" {
                "json"
            } else {
                "invalid-json"
            }
        );
    }
}

#[test]
fn failed_version_preflight_is_distinct_from_a_semantic_attempt() {
    for (engine, exit) in [
        ("codex", 0),
        ("codex", 1),
        ("claude-code", 0),
        ("claude-code", 1),
    ] {
        let fixture = bounded_audit_fixture(engine, 4, RECORD_BOUNDED_AUDIT);
        let path = fixture.root().join("agent executable");
        let raw=std::fs::read_to_string(&path).unwrap().replace(
            "if [ \"$1\" = --version ]; then",
            &format!("if [ \"$1\" = --version ]; then printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"PRIVATE_CANARY_ERROR\"]}}'; exit {exit};"),
        );
        std::fs::write(path, raw).unwrap();
        let (output, job) = fixture.run("knowledge");
        assert_no_semantic_record(&fixture, &output, &job);
        assert_eq!(job["semantic_calls"], 0);
        let step = &job["work_steps"][0];
        assert!(step["agent_duration_ms"].is_null());
        assert_eq!(step["host_failure"]["phase"], "version-preflight");
        assert_eq!(
            step["host_failure"]["subtype"]["recognized"],
            "error_during_execution"
        );
        assert_eq!(step["host_failure"]["exit_code"], exit);
    }
}
