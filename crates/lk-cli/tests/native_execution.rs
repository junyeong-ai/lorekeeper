#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new(engine: &str, timeout: u64, behavior: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("vault/inbox")).unwrap();
        let agent = root.join("agent executable");
        let version = if engine == "codex" {
            "codex-cli 0.163.0"
        } else {
            "2.1.303 (Claude Code)"
        };
        std::fs::write(&agent, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; exit; fi\nprintf '%s\\n' \"$@\" > \"$TEST_EXECUTION_ROOT/arguments\"\n{behavior}\n")).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, format!("vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    labels: [knowledge]\n    extract_concepts: true\nexecution:\n  default_profile: primary\n  profiles:\n    primary:\n      engine: {engine}\n      executable: {}\n      timeout_seconds: {timeout}\n", serde_json::to_string(&agent).unwrap())).unwrap();
        let fixture = Self { directory, config };
        let result = fixture
            .command()
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .arg("--config")
            .arg(&self.config)
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("LORE_INSTALL_DATA_DIR", self.root().join("data"))
            .env("TEST_EXECUTION_ROOT", self.root());
        command
    }

    fn ingest(&self) {
        std::fs::write(self.root().join("vault/inbox/source.md"), "# Durable evidence\n\nA transactional outbox commits an event and its state change in one transaction.\n").unwrap();
        let result = self.command().arg("ingest").output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn run(&self, operation: &str) -> (std::process::Output, serde_json::Value) {
        let result = self
            .command()
            .args(["run", operation, "--json"])
            .output()
            .unwrap();
        let json = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            )
        });
        (result, json)
    }
}

#[test]
fn empty_work_does_not_launch_or_require_an_agent() {
    let fixture = Fixture::new("codex", 30, "exit 91");
    std::fs::remove_file(fixture.root().join("agent executable")).unwrap();
    let (output, result) = fixture.run("process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["ok"], true);
    assert!(result["execution"].is_null());
    for path in [
        "vault/wiki/AGENTS.md",
        "vault/wiki/index.md",
        "vault/.lorekeeper/search.sqlite",
    ] {
        assert!(fixture.root().join(path).is_file(), "missing {path}");
    }
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn both_hosts_receive_distinct_contracts_and_exit_zero_does_not_prove_completion() {
    for engine in ["claude-code", "codex"] {
        let fixture = Fixture::new(engine, 30, "exit 0");
        fixture.ingest();
        let (output, result) = fixture.run("process");
        assert!(!output.status.success());
        assert_eq!(result["ok"], false);
        assert_eq!(result["execution"]["engine"], engine);
        let arguments = std::fs::read_to_string(fixture.root().join("arguments")).unwrap();
        if engine == "codex" {
            assert!(arguments.starts_with("exec\n"));
            assert!(arguments.contains("--sandbox\nworkspace-write\n"));
            assert!(arguments.contains("--ephemeral\n"));
            assert!(!arguments.contains("--allowedTools"));
        } else {
            assert!(arguments.starts_with("-p\n/lore-process:lore-process\n"));
            assert!(arguments.contains("--permission-prompts\nnone\n"));
        }
        let pending = fixture.command().args(["queue", "count"]).output().unwrap();
        assert!(
            String::from_utf8(pending.stdout)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
                > 0
        );
    }
}

#[test]
fn daily_preserves_collection_when_the_selected_agent_is_unavailable() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    std::fs::remove_file(fixture.root().join("agent executable")).unwrap();
    std::fs::write(
        fixture.root().join("vault/inbox/source.md"),
        "# Source\n\nOriginal evidence must survive an authentication or execution failure.\n",
    )
    .unwrap();
    let (output, result) = fixture.run("daily");
    assert!(!output.status.success());
    assert_eq!(result["stages"][0]["name"], "ingest");
    assert_eq!(result["stages"][0]["state"], "completed");
    assert!(
        fixture
            .root()
            .join("vault")
            .join(lk_core::manual_original::ORIGINAL_STORE)
            .is_dir()
    );
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "wiki refresh" && stage["state"] == "completed")
    );
}

#[test]
fn missing_packaged_dependency_refuses_before_launch() {
    let fixture = Fixture::new("claude-code", 30, "exit 0");
    fixture.ingest();
    std::fs::remove_file(
        fixture
            .root()
            .join("data/skills/claude-code/lore-process/SKILL.md"),
    )
    .unwrap();
    let (output, result) = fixture.run("process");
    assert!(!output.status.success());
    assert_eq!(result["ok"], false);
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn audit_exit_zero_without_assessments_is_incomplete() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("unverified.md"),
        include_str!("behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md"),
    )
    .unwrap();
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(result["ok"], false);
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "audit completion" && stage["state"] == "failed"),
        "{}\n{}",
        result,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root().join("arguments").exists());
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/knowledge-audit.json")
            .exists()
    );
}

#[test]
fn a_failed_base_stage_does_not_start_dependent_periods() {
    let fixture = Fixture::new("codex", 30, "exit 23");
    fixture.ingest();
    let (output, result) = fixture.run("daily");
    assert!(!output.status.success());
    let names = result["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"period resume base"));
    assert!(!names.contains(&"period resume quarter"));
    assert!(!names.contains(&"period resume annual"));
}

#[test]
fn native_execution_preserves_the_users_git_index_and_commit() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    let root = fixture.root().join("vault");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.com"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let note = root.join("personal.md");
    std::fs::write(&note, "initial").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "personal.md"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(&note, "staged user text").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "personal.md"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(&note, "unstaged user text").unwrap();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let head = std::fs::read(
        root.join(".git/refs/heads").join(
            String::from_utf8(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["branch", "--show-current"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        ),
    )
    .unwrap();
    let (output, _) = fixture.run("process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    let branch = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(branch.stdout, head);
    assert_eq!(std::fs::read_to_string(note).unwrap(), "unstaged user text");
}

#[test]
fn cancellation_kills_foreground_children_and_preserves_cross_host_exclusion() {
    let fixture = Fixture::new(
        "codex",
        30,
        "printf '%s\\n' \"$$\" > \"$TEST_EXECUTION_ROOT/started\"\ntrap '' TERM\n/bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' &\nwait",
    );
    fixture.ingest();
    let stdout = std::fs::File::create(fixture.root().join("result.json")).unwrap();
    let stderr = std::fs::File::create(fixture.root().join("stderr.log")).unwrap();
    let mut child = fixture
        .command()
        .args(["run", "process", "--json"])
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fixture.root().join("started").exists() {
        assert!(Instant::now() < deadline, "agent never started");
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    let blocked = fixture
        .command()
        .args(["workflow", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "cancelled controller did not stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root().join("result.json")).unwrap())
            .unwrap();
    assert_eq!(result["cancelled"], true);
    assert_eq!(
        result["stages"].as_array().unwrap().last().unwrap()["state"],
        "cancelled"
    );
    loop {
        let released = fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .output()
            .unwrap();
        if released.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child retained the workflow descriptor after cancellation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
