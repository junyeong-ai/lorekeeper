//! A plane that cannot be held refuses every WRITE and no READ.
//!
//! The rule is one line at the call site and the failure it prevents is silent: writing an
//! unheld plane lost board lines and transition records while reporting success. Nothing
//! measured that, which is how it shipped, and nothing measured the fix either — so a read
//! quietly joining the refusal would ship the same way.

use std::path::Path;
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .arg("--config")
        .arg(config)
        .output()
        .expect("spawn lore")
}

/// What a command SAID, as one line. `miette` wraps to the terminal's width, so a phrase read
/// back out of the rendering breaks on a path long enough to push it over — which is a test that
/// passes or fails on where the temp directory happened to be.
fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("│ ", "")
}

fn vault(root: &Path) -> std::path::PathBuf {
    let config = root.join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "vault:\n  root: {}/vault\n  timezone: UTC\nidentity:\n  name: t\n  email: t@t.com\n\
             sources:\n  s1:\n    type: tasks\npersonal:\n  tracked_sources: [s1]\n  \
             performance_categories: [pd]\n  tasks:\n    board: tasks.md\n",
            root.display()
        ),
    )
    .expect("write config");
    std::fs::create_dir_all(root.join("vault/personal")).expect("create vault");
    config
}

/// A DIRECTORY where the lock file goes: the plane can never be held, and it is the one cause
/// reproducible without a filesystem that lacks locking.
fn wedge(root: &Path) {
    let lock = root.join("vault/.lorekeeper/tasks.lock");
    std::fs::create_dir_all(lock.parent().expect("parent")).expect("create dir");
    let _ = std::fs::remove_file(&lock);
    std::fs::create_dir(&lock).expect("wedge the lock");
}

#[test]
fn every_top_level_command_can_render_help() {
    use clap::CommandFactory;
    let tmp = tempfile::tempdir().unwrap();
    let config = vault(tmp.path());
    let command = lore::Cli::command();
    for subcommand in command.get_subcommands() {
        let name = subcommand.get_name();
        let output = run(&config, &[name, "--help"]);
        assert!(output.status.success(), "{name}: {}", said(&output));
        assert!(!output.stdout.is_empty(), "{name}");
    }
}

#[test]
fn fresh_vault_reads_leave_it_absent_and_first_task_write_creates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let config = vault(tmp.path());
    let root = tmp.path().join("vault");
    std::fs::remove_dir_all(&root).unwrap();
    assert!(run(&config, &["task", "list"]).status.success());
    assert!(!root.exists());
    let output = run(&config, &["task", "add", "First task"]);
    assert!(output.status.success(), "{}", said(&output));
    let output = run(&config, &["task", "list", "--json"]);
    assert!(output.status.success(), "{}", said(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("First task"));
}

#[test]
fn task_writers_recover_history_and_board_together_and_preserve_intentional_reopening() {
    let tmp = tempfile::tempdir().unwrap();
    let config = vault(tmp.path());
    let root = tmp.path().join("vault");
    let today = jiff::Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .date();
    let yesterday = today.yesterday().unwrap();
    let id = "7k2p".parse().unwrap();
    let before = format!("## Today\n\n- [ ] Ship docs <!--t:7k2p since:{yesterday}-->\n");
    let board_path = lk_core::vault_path::VaultPath::task_board(
        &lk_core::config::VaultDirs::default(),
        "tasks.md",
    )
    .to_string();
    std::fs::create_dir_all(root.join(&board_path).parent().unwrap()).unwrap();
    std::fs::write(root.join(&board_path), &before).unwrap();
    let mut board = lk_task::Board::parse(&before);
    board.remove(&id).unwrap();
    let after = board.render(lk_core::i18n::Locale::Ko, today);
    let transition = lk_task::Transition::new(
        id,
        lk_task::TransitionKind::Done,
        "Ship docs",
        yesterday
            .at(12, 0, 0, 0)
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp(),
    )
    .with_note(Some("shipped".into()));
    let history = format!("{}\n", serde_json::to_string(&transition).unwrap());
    let history_path = format!(".lorekeeper/tasks/{yesterday}.jsonl");
    let mut plan = lk_vault::MutationPlan::default();
    plan.create(&history_path, history.clone());
    plan.edit(&board_path, before.clone(), after);
    std::fs::create_dir_all(root.join(".lorekeeper/mutation")).unwrap();
    std::fs::create_dir_all(root.join(".lorekeeper/tasks")).unwrap();
    std::fs::write(root.join(&history_path), &history).unwrap();
    let journal = root.join(".lorekeeper/mutation/pending.json");
    std::fs::write(
        &journal,
        serde_json::to_vec(&serde_json::json!({"version":1,"plan":plan})).unwrap(),
    )
    .unwrap();
    assert!(run(&config, &["task", "list"]).status.success());
    assert_eq!(
        std::fs::read_to_string(root.join(&board_path)).unwrap(),
        before
    );
    assert!(journal.exists());
    let output = run(&config, &["task", "sync"]);
    assert!(output.status.success(), "{}", said(&output));
    assert!(
        !std::fs::read_to_string(root.join(&board_path))
            .unwrap()
            .contains("t:7k2p")
    );
    assert_eq!(
        std::fs::read_to_string(root.join(&history_path)).unwrap(),
        history
    );
    assert!(!journal.exists());
    std::fs::write(root.join(&board_path), before).unwrap();
    let output = run(
        &config,
        &["task", "done", "7k2p", "--note", "shipped again"],
    );
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(
        std::fs::read_to_string(root.join(history_path)).unwrap(),
        history
    );
    let current =
        std::fs::read_to_string(root.join(format!(".lorekeeper/tasks/{today}.jsonl"))).unwrap();
    assert_eq!(current.lines().count(), 1);
    let done: serde_json::Value = serde_json::from_str(current.trim()).unwrap();
    assert_eq!(done["kind"], "done");
    assert_eq!(done["note"], "shipped again");
}

#[test]
fn a_plane_that_cannot_be_held_refuses_writes_and_serves_reads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = vault(tmp.path());
    assert!(
        run(&config, &["task", "add", "a task"]).status.success(),
        "the vault works before it is wedged"
    );
    wedge(tmp.path());

    for args in [
        vec!["task", "add", "another"],
        vec!["task", "sync"],
        vec!["task", "rollover"],
        vec!["task", "propose"],
        vec!["task", "remind", "due"],
    ] {
        let out = run(&config, &args);
        assert!(!out.status.success(), "{args:?} must refuse");
        let told = said(&out);
        assert!(
            told.contains("Clear whatever sits at that path"),
            "{args:?} must name the repair its cause calls for: {told}"
        );
    }

    for args in [
        vec!["task", "list"],
        vec!["task", "remind", "list"],
        vec!["agenda"],
        vec!["agenda", "--json"],
        vec!["status"],
    ] {
        let out = run(&config, &args);
        assert!(
            out.status.success(),
            "{args:?} reads and must not be refused: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn task_writers_keep_claimed_root_and_refuse_inherited_drift_before_recovery() {
    use std::os::unix::fs::symlink;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct WaitingChild(Option<Child>);

    impl Drop for WaitingChild {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Change {
        Vault,
        Alias,
        Configuration,
    }

    for (inherited, change) in [
        (false, Change::Vault),
        (true, Change::Vault),
        (true, Change::Alias),
        (true, Change::Configuration),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let config = vault(tmp.path());
        let first = tmp.path().join("vault");
        let second = tmp.path().join("other-vault");
        std::fs::create_dir(&second).unwrap();
        let selected = tmp.path().join("selected");
        let equivalent = tmp.path().join("equivalent");
        symlink(&first, &selected).unwrap();
        symlink(&first, &equivalent).unwrap();
        let raw = std::fs::read_to_string(&config)
            .unwrap()
            .replace(
                &format!("root: {}/vault", tmp.path().display()),
                &format!("root: {}", selected.display()),
            )
            .replace(
                "    board: tasks.md\n",
                "    board: tasks.md\n    propose_from: [s1]\n",
            );
        std::fs::write(&config, &raw).unwrap();
        let today = jiff::Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .date();
        for (root, title, origin) in [
            (&first, "Owned proposal", "owned"),
            (&second, "Foreign proposal", "foreign"),
        ] {
            lk_task::Judged::new(root)
                .add(
                    today,
                    &lk_task::Candidate {
                        source_id: "s1".into(),
                        summary: title.into(),
                        url: format!("https://example.test/{origin}"),
                    },
                )
                .unwrap();
        }
        let judged = format!(".lorekeeper/proposals/judged/{today}.jsonl");
        let owned_before = std::fs::read(first.join(&judged)).unwrap();
        let foreign_before = std::fs::read(second.join(&judged)).unwrap();
        let plane = lk_task::PlaneLock::hold(&first).unwrap();
        let mut recovery = lk_vault::MutationPlan::default();
        recovery.create("recovered-intent.txt", "Owned recovery".into());
        let journal = first.join(".lorekeeper/mutation/pending.json");
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        let pending =
            serde_json::to_vec(&serde_json::json!({"version":1,"plan":recovery})).unwrap();
        std::fs::write(&journal, &pending).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .env("LORE_INSTALL_DATA_DIR", tmp.path().join("data"))
            .arg("--config")
            .arg(&config);
        if inherited {
            command
                .args(["workflow", "--"])
                .arg(env!("CARGO_BIN_EXE_lore"));
        }
        let mut child = WaitingChild(Some(
            command
                .args(["task", "propose"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
            match lk_vault::MutationClaim::acquire(&first) {
                Err(lk_vault::VaultError::MutationBusy) => break,
                Ok(claim) => drop(claim),
                Err(error) => panic!("mutation claim probe: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "child did not acquire the vault claim"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        match change {
            Change::Vault | Change::Alias => {
                std::fs::remove_file(&selected).unwrap();
                symlink(
                    if matches!(change, Change::Alias) {
                        &equivalent
                    } else {
                        &second
                    },
                    &selected,
                )
                .unwrap();
            }
            Change::Configuration => {
                std::fs::write(&config, raw + "# changed configuration\n").unwrap();
            }
        }
        drop(plane);
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "task writer did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        let board = lk_core::vault_path::VaultPath::task_board(
            &lk_core::config::VaultDirs::default(),
            "tasks.md",
        )
        .to_string();
        let refused = inherited && !matches!(change, Change::Alias);
        assert_eq!(output.status.success(), !refused, "{}", said(&output));
        assert_eq!(std::fs::read(second.join(&judged)).unwrap(), foreign_before);
        assert!(!second.join(&board).exists());
        assert!(!second.join("recovered-intent.txt").exists());
        if refused {
            assert!(said(&output).contains("changed"), "{}", said(&output));
            assert_eq!(std::fs::read(&journal).unwrap(), pending);
            assert_eq!(std::fs::read(first.join(&judged)).unwrap(), owned_before);
            assert!(!first.join(&board).exists());
            assert!(!first.join("recovered-intent.txt").exists());
        } else {
            let page = std::fs::read_to_string(first.join(&board)).unwrap();
            assert!(page.contains("Owned proposal"), "{page}");
            assert!(!page.contains("Foreign proposal"), "{page}");
            assert!(!first.join(&judged).exists());
            assert!(!journal.exists());
            assert_eq!(
                std::fs::read_to_string(first.join("recovered-intent.txt")).unwrap(),
                "Owned recovery"
            );
        }
    }
}

/// A view that could not be written to says so, rather than reporting a quiet day over a vault
/// where the nightly close, every command and the reminder timer are all being turned away.
#[test]
fn a_view_says_the_plane_cannot_be_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = vault(tmp.path());
    run(&config, &["task", "add", "a task"]);
    wedge(tmp.path());

    let json = run(&config, &["agenda", "--json"]);
    let day: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("the contract is still JSON");
    assert!(
        day["unwritable"]
            .as_str()
            .is_some_and(|why| why.contains("Clear whatever sits at that path")),
        "unwritable: {}",
        day["unwritable"]
    );

    assert!(
        said(&run(&config, &["status"])).contains("! board"),
        "the front door marks it"
    );
}

#[test]
fn completed_editor_tasks_do_not_fire_reminders_before_sync() {
    let checked = "- [x] Finish the task <!--t:7k2p since:2026-08-19-->\n";
    let open = "- [ ] Finish the task <!--t:7k2p since:2026-08-19-->\n";
    for (board, fires, retained) in [
        (Some(format!("## Today\n{checked}")), false, false),
        (Some(format!("## Today\n{open}")), true, false),
        (Some(format!("## Today\n{checked}{open}")), true, false),
        (
            Some(format!(
                "## Today\n{checked}- [ ] Unreadable stamp <!--t:3b8q since:invalid-->\n"
            )),
            false,
            true,
        ),
        (None, false, true),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let config = vault(tmp.path());
        let board_path = tmp.path().join("vault").join(
            lk_core::vault_path::VaultPath::task_board(
                &lk_core::config::VaultDirs::default(),
                "tasks.md",
            )
            .to_string(),
        );
        if let Some(raw) = &board {
            std::fs::create_dir_all(board_path.parent().unwrap()).unwrap();
            std::fs::write(&board_path, raw).unwrap();
        }
        let store = lk_task::Reminders::new(&tmp.path().join("vault"));
        store
            .add(lk_task::Reminder {
                id: "3b8q".parse().unwrap(),
                at: "2026-01-01T00:00:00Z".parse().unwrap(),
                text: "promised task follow-up".into(),
                task: Some("7k2p".parse().unwrap()),
            })
            .unwrap();
        let result = run(&config, &["task", "remind", "due"]);
        assert!(result.status.success(), "{}", said(&result));
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).contains("promised task follow-up"),
            fires,
            "{board:?}"
        );
        assert_eq!(!store.read().unwrap().is_empty(), retained, "{board:?}");
        assert_eq!(std::fs::read_to_string(board_path).ok(), board);
    }
}
