use std::path::Path;
use std::process::{Command, Output};

fn check(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(["queue", "check", "--root"])
        .arg(root)
        .output()
        .unwrap()
}

#[test]
fn empty_queue_is_settled_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let output = check(dir.path());
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn malformed_work_and_unpublished_results_cannot_report_completion() {
    let dir = tempfile::tempdir().unwrap();
    let queue = dir.path().join(".lorekeeper/queue");
    std::fs::create_dir_all(queue.join("results")).unwrap();
    std::fs::write(queue.join("tasks.jsonl"), "unparseable\n").unwrap();
    std::fs::write(queue.join("results/answer.section.json"), "{}").unwrap();
    let output = check(dir.path());
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["data"]["current"], 0);
    assert_eq!(report["data"]["unparseable_lines"], 1);
    assert_eq!(
        report["data"]["staged_results"],
        serde_json::json!(["answer.section.json"])
    );
    assert_eq!(
        std::fs::read_to_string(queue.join("tasks.jsonl")).unwrap(),
        "unparseable\n"
    );
    assert!(queue.join("results/answer.section.json").exists());
}

#[test]
fn interrupted_mutation_is_reported_without_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join(".lorekeeper/mutation");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("pending.json"), "interrupted").unwrap();
    let output = check(dir.path());
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["data"]["pending_mutation"], true);
    assert_eq!(
        std::fs::read_to_string(state.join("pending.json")).unwrap(),
        "interrupted"
    );
}
