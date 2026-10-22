//! End-to-end contract of `lore queue apply`, exercised through the real binary.
//!
//! The case that matters is several results landing on ONE page in a single batch. It is
//! not exotic: `lore ingest` warns when pending queue files exist precisely because running
//! it twice before a drain enqueues the same target again, under the same input hash, and
//! both tasks classify current. Drain both and `queue apply` gets two results for one page.

use lk_queue::TaskRequest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn extraction_request(seed: &str) -> lk_queue::ExtractConceptsRequest {
    lk_queue::ExtractConceptsRequest {
        concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
        rss_original: None,
        text: seed.into(),
        source_id: "notes".into(),
        source_type: lk_core::config::SourceType::Manual,
        date: jiff::civil::date(2026, 5, 23),
        focus: None,
        locale: lk_core::i18n::Locale::En,
        categories: vec![],
        target: lk_queue::TaskTarget {
            vault_path: "unused.md".into(),
            kind: lk_queue::TargetKind::DailyConcepts,
            anchor: "## Related Concepts".into(),
        },
    }
}

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("vault")).expect("vault dir");
        std::fs::write(
            root.path().join("config.yaml"),
            "vault:\n  root: vault\n  locale: en\n\
             identity:\n  name: Tester\n  email: tester@example.com\n\
             execution:\n  work: {extraction_context: full-context}\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
        )
        .expect("config");
        Self { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.vault().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.vault().join(rel)).expect("read page")
    }

    fn capture_extraction_scope(
        &self,
        task_id: &str,
        queries: &[lk_core::discovery::DiscoveryQuery],
    ) -> String {
        let status = work_json(self, &["queue", "status", "--json"]);
        let target = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["task_id"] == task_id)
            .unwrap();
        assert!(matches!(
            target["status"].as_str(),
            Some("current" | "done")
        ));
        self.write(
            ".lorekeeper/discovery-request.json",
            &serde_json::json!({
                "target_revision":target["target_revision"],"queries":queries,
            })
            .to_string(),
        );
        let file = self.vault().join(".lorekeeper/discovery-request.json");
        work_json(
            self,
            &[
                "work",
                "discover",
                "--task-id",
                task_id,
                "--file",
                file.to_str().unwrap(),
            ],
        );
        work_json(self, &["work", "context", "--task-id", task_id])["context_revision"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// One `extract-concepts` result naming a single concept, as a drain writes it.
    /// Serialized from the real type so the fixture cannot drift from the wire format.
    fn drop_result(&self, task_id: &str, target: &str, hash: &str, concept: &str) {
        self.drop_grounded_result(task_id, target, hash, concept, None);
    }

    /// The same result carrying the grounding sentence a drain writes when the concept is one
    /// the vault does not hold yet.
    fn drop_grounded_result(
        &self,
        task_id: &str,
        target: &str,
        hash: &str,
        concept: &str,
        synthesis: Option<&str>,
    ) {
        let target_revision = std::fs::read_to_string(self.vault().join(target))
            .map(|content| lk_core::concept::file_revision(&content))
            .unwrap_or_default();
        let task_target = lk_queue::TaskTarget {
            vault_path: target.into(),
            kind: lk_queue::TargetKind::DailyConcepts,
            anchor: "## Related Concepts".into(),
        };
        let mut request = extraction_request(hash);
        request.target = task_target.clone();
        let mut task = lk_queue::QueueLlmClient::new(self.vault().join(".lorekeeper/queue"))
            .build_task("fixture", lk_queue::TaskKind::ExtractConcepts, request);
        task.task_id = task_id.into();
        self.write(
            &format!(".lorekeeper/queue/{task_id}.jsonl"),
            &serde_json::to_string(&task).unwrap(),
        );
        let discovery = vec![lk_core::discovery::DiscoveryQuery {
            query: concept.into(),
            phrase: true,
            spacing: false,
        }];
        let status = work_json(self, &["queue", "status", "--json"]);
        let selected = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["task_id"] == task_id)
            .unwrap();
        let context_revision = if matches!(selected["status"].as_str(), Some("current" | "done")) {
            self.write(
                ".lorekeeper/discovery-request.json",
                &serde_json::json!({
                    "target_revision":target_revision,"queries":discovery,
                })
                .to_string(),
            );
            let file = self.vault().join(".lorekeeper/discovery-request.json");
            work_json(
                self,
                &[
                    "work",
                    "discover",
                    "--task-id",
                    task_id,
                    "--file",
                    file.to_str().unwrap(),
                ],
            );
            work_json(self, &["work", "context", "--task-id", task_id])["context_revision"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            assert!(matches!(
                selected["status"].as_str(),
                Some("unreadable" | "missing-target" | "stale")
            ));
            "unavailable-for-noncurrent-target".into()
        };
        let resolved = self.run(&["resolve", concept, "--json"]);
        let resolved: serde_json::Value = serde_json::from_slice(&resolved.stdout).unwrap();
        let admission = if resolved["verdict"] == "owned" {
            let path = resolved["path"].as_str().unwrap();
            lk_queue::ConceptAdmission::Reuse {
                owner: lk_queue::ConceptOwner {
                    path: path.into(),
                    revision: lk_core::concept::file_revision(&self.read(path)),
                },
            }
        } else {
            assert!(matches!(
                resolved["verdict"].as_str(),
                Some("absent" | "ambiguous")
            ));
            lk_queue::ConceptAdmission::New
        };
        let grounding = synthesis.map(str::to_owned).or_else(|| {
            matches!(admission, lk_queue::ConceptAdmission::New).then(|| {
                format!(
                    "{concept} is the scoped concept declared by this fixture's retained source."
                )
            })
        });
        let result = lk_queue::TaskResult {
            selection: None,
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            target_revision,
            context_revision,
            discovery,
            task_id: task_id.into(),
            cache_hash: task.cache_hash,
            target: task_target,
            date: jiff::civil::date(2026, 5, 23),
            concepts: vec![lk_queue::ReportedConcept {
                admission,
                rss_evidence: Vec::new(),
                concept: lk_core::concept::ExtractedConcept {
                    name: concept.into(),
                    category: None,
                    aliases: Vec::new(),
                },
                synthesis: grounding,
            }],
        };
        let dir = self
            .vault()
            .join(".lorekeeper/queue")
            .join(lk_queue::RESULTS_SUBDIR);
        std::fs::create_dir_all(&dir).expect("results dir");
        std::fs::write(
            dir.join(format!("{task_id}.json")),
            serde_json::to_vec(&result).unwrap(),
        )
        .expect("result");
    }

    /// Run `queue apply` and require it to have APPLIED `applied` results and dropped
    /// `dropped`.
    ///
    /// Asserting the resulting page alone cannot tell a correct re-apply from a silent
    /// drop — both leave it unchanged — so an idempotency test written that way passes
    /// against the very bug it is named after. What happened is part of the assertion.
    fn apply_expecting(&self, applied: usize, dropped: usize) {
        let out = self.run(&["queue", "apply"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "queue apply failed: {err}");
        // Parsed, not substring-matched: `"1 applied"` is a substring of `"11 applied"`,
        // so a run that applied ten times too many would satisfy a `contains` check.
        let tally = err
            .lines()
            .find_map(|line| line.strip_prefix("queue apply: "))
            .unwrap_or_else(|| panic!("no summary line in: {err}"));
        let counted = |what: &str| -> usize {
            tally
                .split(", ")
                .find_map(|part| part.strip_suffix(&format!(" {what}")))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("no `{what}` count in: {tally}"))
        };
        assert_eq!(
            (counted("applied"), counted("dropped")),
            (applied, dropped),
            "in: {tally}"
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                cmd.env_remove(key);
            }
        }
        cmd.arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .args(args)
            .output()
            .expect("spawn lore")
    }
}

const PAGE: &str = "daily/notes/2026-05-23.md";

fn daily_page(hash: &str) -> String {
    dated_daily_page("2026-05-23", hash)
}

fn dated_daily_page(date: &str, hash: &str) -> String {
    let hash = extraction_request(hash).cache_hash();
    format!(
        "---\nid: notes-{date}\ntype: daily\nllm_inputs:\n  concepts: \"{hash}\"\n---\n\n\
         ## Summary\n\nbody\n\n## Related Concepts\n\n## Key Events\n\n- a\n"
    )
}

fn results_dir(ws: &Workspace) -> PathBuf {
    ws.vault()
        .join(".lorekeeper")
        .join("queue")
        .join(lk_queue::RESULTS_SUBDIR)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Two results for one page in one batch must BOTH be cited. Every write lands after the
/// apply loop, so a second fold reading the page from disk would see a version predating
/// the first, and its write would drop the first's citation — the same silent loss the
/// accumulating render exists to prevent, reached through batching instead of re-extraction.
#[test]
fn two_results_for_one_page_both_land() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    // Same target, same hash — what two ingests before one drain produce.
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.drop_result("ext-2", PAGE, "h", "Concept B");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        out.status.success(),
        "queue apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page = ws.read(PAGE);
    for slug in ["concept-a", "concept-b"] {
        assert!(
            page.contains(&format!("wiki/concepts/{slug}.md")),
            "{slug} must be cited after applying both results:\n{page}"
        );
    }
    // Both concept pages exist, so both citations resolve.
    for slug in ["concept-a", "concept-b"] {
        assert!(
            ws.vault()
                .join("wiki/concepts")
                .join(format!("{slug}.md"))
                .exists(),
            "{slug} page must be written"
        );
    }
    assert!(
        json_files(&results_dir(&ws)).is_empty(),
        "every applied result must be consumed"
    );
}

#[test]
#[cfg(unix)]
fn directory_alias_targets_keep_one_publication_owner_and_refuse_duplicate_owners() {
    for duplicate in [false, true] {
        let ws = Workspace::new();
        let seed = "Selected Candidate A and Selected Candidate B describe distinct mechanisms.";
        let before = daily_page(seed);
        ws.write(PAGE, &before);
        std::os::unix::fs::symlink(ws.vault().join("daily"), ws.vault().join("daily-alias"))
            .unwrap();
        let alias = "daily-alias/notes/2026-05-23.md";
        let owner_a = "wiki/concepts/first.md";
        let owner_b = "wiki/concepts/second.md";
        let definition = |title: &str| {
            format!(
                "---\ntype: concept\ntitle: {title}\n---\n## Synthesis\nEstablished scope.\n\n## Sources\n\n## Related\n"
            )
        };
        let first_before = definition("Selected Candidate A");
        let second_before = definition("Selected Candidate B");
        ws.write(owner_a, &first_before);
        ws.write(owner_b, &second_before);
        ws.drop_result("alias", alias, seed, "Selected Candidate B");
        if duplicate {
            ws.drop_result("original", PAGE, seed, "Selected Candidate A");
        }
        let status = work_json(&ws, &["queue", "status", "--json"]);
        assert_eq!(
            status["data"]["tasks"].as_array().unwrap().len(),
            if duplicate { 2 } else { 1 }
        );
        assert!(
            status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|task| task["status"] == "current")
        );
        assert_eq!(
            ws.vault().join(alias).canonicalize().unwrap(),
            ws.vault().join(PAGE).canonicalize().unwrap()
        );
        let results: Vec<_> = json_files(&results_dir(&ws))
            .into_iter()
            .map(|path| {
                let raw = std::fs::read(&path).unwrap();
                (path, raw)
            })
            .collect();
        for dry_run in [true, false] {
            let output = ws.run(if dry_run {
                &["queue", "apply", "--dry-run"]
            } else {
                &["queue", "apply"]
            });
            assert_eq!(
                output.status.success(),
                !duplicate,
                "duplicate={duplicate}, dry={dry_run}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if duplicate {
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("multiple publication addresses resolve to the same page")
                );
            }
            if dry_run || duplicate {
                assert_eq!(ws.read(PAGE), before);
                assert_eq!(ws.read(owner_a), first_before);
                assert_eq!(ws.read(owner_b), second_before);
                for (path, raw) in &results {
                    assert_eq!(std::fs::read(path).unwrap(), *raw);
                }
            }
            assert!(
                !ws.vault()
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
        }
        if !duplicate {
            assert!(ws.read(PAGE).contains("concepts_done:"));
            assert!(ws.read(PAGE).contains("wiki/concepts/second.md"));
            assert!(json_files(&results_dir(&ws)).is_empty());
        }
    }
}

#[test]
fn duplicate_extraction_files_fold_once_and_conflicting_proposals_stay_unpublished() {
    for conflicting in [false, true] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.drop_result("duplicate", PAGE, "h", "Shared Subject");
        let original_path = ".lorekeeper/queue/results/duplicate.json";
        let original = ws.read(original_path);
        let mut alias: serde_json::Value = serde_json::from_str(&original).unwrap();
        if conflicting {
            alias["concepts"][0]["synthesis"] =
                "An incompatible proposal for the same retained task.".into();
        }
        let alias = serde_json::to_string_pretty(&alias).unwrap();
        let alias_path = ".lorekeeper/queue/results/another-name.json";
        ws.write(alias_path, &alias);
        let other = "daily/notes/other.md";
        ws.write(other, &daily_page("other"));
        ws.drop_result("independent", other, "other", "Independent Subject");
        let before = ws.read(PAGE);
        let preview = ws.run(&["queue", "apply", "--dry-run"]);
        assert_eq!(
            preview.status.success(),
            !conflicting,
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(ws.read(PAGE), before);
        assert_eq!(ws.read(original_path), original);
        assert_eq!(ws.read(alias_path), alias);
        if conflicting {
            let output = ws.run(&["queue", "apply"]);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("conflicting extraction results")
            );
            assert_eq!(ws.read(PAGE), before);
            assert_eq!(ws.read(original_path), original);
            assert_eq!(ws.read(alias_path), alias);
            assert!(!ws.vault().join("wiki/concepts/shared-subject.md").exists());
        } else {
            ws.apply_expecting(2, 0);
            assert!(ws.read(PAGE).contains("concepts_done:"));
            assert!(!ws.vault().join(original_path).exists());
            assert!(!ws.vault().join(alias_path).exists());
            assert_eq!(
                ws.read(PAGE)
                    .matches("wiki/concepts/shared-subject.md")
                    .count(),
                1
            );
        }
        assert!(ws.read(other).contains("concepts_done:"));
        assert!(
            ws.vault()
                .join("wiki/concepts/independent-subject.md")
                .exists()
        );
    }
}

#[test]
fn extraction_proposal_groups_classify_before_comparing_answers() {
    for scenario in [
        "stale",
        "missing-target",
        "done",
        "orphan",
        "unparseable-pending",
        "unreadable",
        "invalid-binding-current",
        "invalid-binding-done",
        "invalid-input",
        "done-with-fresh-proposal",
    ] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.drop_result("proposal", PAGE, "h", "Shared Subject");
        let original_path = ".lorekeeper/queue/results/proposal.json";
        let original = ws.read(original_path);
        if scenario.starts_with("done") || scenario == "invalid-binding-done" {
            ws.apply_expecting(1, 0);
            ws.write(original_path, &original);
        }
        let mut alias: serde_json::Value = serde_json::from_str(&original).unwrap();
        alias["concepts"][0]["synthesis"] = "A conflicting retained interpretation.".into();
        if scenario.starts_with("invalid-binding") {
            alias["target"]["vault_path"] = "daily/notes/missing.md".into();
        }
        if scenario == "done-with-fresh-proposal" {
            alias["target_revision"] = lk_core::concept::file_revision(&ws.read(PAGE)).into();
        }
        let alias = serde_json::to_string_pretty(&alias).unwrap();
        let alias_path = if scenario == "invalid-binding-done" {
            ".lorekeeper/queue/results/z-another-name.json"
        } else {
            ".lorekeeper/queue/results/another-name.json"
        };
        ws.write(alias_path, &alias);
        let other = "daily/notes/other.md";
        ws.write(other, &daily_page("other"));
        ws.drop_result("independent", other, "other", "Independent Subject");

        match scenario {
            "stale" => ws.write(PAGE, &daily_page("newer-input")),
            "missing-target" => std::fs::remove_file(ws.vault().join(PAGE)).unwrap(),
            "orphan" => {
                std::fs::remove_file(ws.vault().join(".lorekeeper/queue/proposal.jsonl")).unwrap();
            }
            "unparseable-pending" => {
                ws.write(".lorekeeper/queue/proposal.jsonl", "{unfinished task\n");
            }
            "unreadable" => ws.write(PAGE, "---\ntype: [\n---\n## Related Concepts\n"),
            "invalid-input" => {
                let task_path = ".lorekeeper/queue/proposal.jsonl";
                let mut task: serde_json::Value =
                    serde_json::from_str(&ws.read(task_path)).unwrap();
                task["input"]["text"] = "Unbound replacement input.".into();
                ws.write(task_path, &task.to_string());
            }
            _ => {}
        }
        let before = std::fs::read(ws.vault().join(PAGE)).ok();
        let retired = matches!(scenario, "stale" | "missing-target" | "done");
        let preview = ws.run(&["queue", "apply", "--dry-run"]);
        assert_eq!(
            preview.status.success(),
            retired,
            "{scenario}: {}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(std::fs::read(ws.vault().join(PAGE)).ok(), before);
        assert_eq!(ws.read(original_path), original);
        assert_eq!(ws.read(alias_path), alias);

        let output = ws.run(&["queue", "apply"]);
        assert_eq!(
            output.status.success(),
            retired,
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(ws.vault().join(PAGE)).ok(), before);
        if scenario == "orphan" {
            for (name, raw) in [("proposal.json", &original), ("another-name.json", &alias)] {
                assert_eq!(
                    ws.read(&format!(".lorekeeper/queue/results/orphaned/{name}")),
                    *raw
                );
                assert!(
                    ws.read(&format!(
                        ".lorekeeper/queue/results/orphaned/{name}.reason.txt"
                    ))
                    .contains("no matching pending task")
                );
            }
        } else if !retired {
            assert_eq!(ws.read(original_path), original, "{scenario}");
            assert_eq!(ws.read(alias_path), alias, "{scenario}");
        }
        if retired || scenario == "orphan" {
            assert!(!ws.vault().join(original_path).exists(), "{scenario}");
            assert!(!ws.vault().join(alias_path).exists(), "{scenario}");
        }
        assert!(ws.read(other).contains("concepts_done:"), "{scenario}");
        assert!(
            ws.vault()
                .join("wiki/concepts/independent-subject.md")
                .exists(),
            "{scenario}"
        );
    }
}

#[test]
fn extraction_discovery_guards_complete_candidates_and_explicit_admission() {
    for scenario in [
        "current",
        "new-match",
        "changed-match",
        "unrelated",
        "missing-context",
        "missing-admission",
        "missing-grounding",
        "no-queries",
        "empty-extraction",
    ] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.write(
            "wiki/documents/context.md",
            "---\ntype: document\n---\nConcept A has a scoped neighboring claim.\n",
        );
        ws.drop_grounded_result(
            "discovery",
            PAGE,
            "h",
            "Concept A",
            Some("Concept A is supported by this fixture's retained source."),
        );
        let path = ".lorekeeper/queue/results/discovery.json";
        let mut result: serde_json::Value = serde_json::from_str(&ws.read(path)).unwrap();
        let before = ws.read(PAGE);
        match scenario {
            "new-match" => ws.write(
                "wiki/documents/new.md",
                "---\ntype: document\n---\nConcept A has another source-scoped claim.\n",
            ),
            "changed-match" => ws.write(
                "wiki/documents/context.md",
                "---\ntype: document\n---\nConcept A applies only in development.\n",
            ),
            "unrelated" => ws.write(
                "wiki/documents/unrelated.md",
                "---\ntype: document\n---\nA separate subject.\n",
            ),
            "missing-context" => {
                result.as_object_mut().unwrap().remove("context_revision");
            }
            "missing-admission" => {
                result["concepts"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("admission");
            }
            "missing-grounding" => {
                result["concepts"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("synthesis");
            }
            "no-queries" => result["discovery"] = serde_json::json!([]),
            "empty-extraction" => {
                result["concepts"] = serde_json::json!([]);
                result["discovery"] = serde_json::json!([]);
                result["context_revision"] = ws.capture_extraction_scope("discovery", &[]).into();
            }
            _ => {}
        }
        ws.write(path, &result.to_string());
        let should_apply = matches!(scenario, "current" | "unrelated" | "empty-extraction");
        let output = ws.run(&["queue", "apply"]);
        assert_eq!(
            output.status.success(),
            should_apply,
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records = ws.vault().join(".lorekeeper/queue/discovery");
        let recipes = std::fs::read_dir(records)
            .unwrap()
            .filter_map(Result::ok)
            .count();
        if should_apply {
            assert!(ws.read(PAGE).contains("concepts_done:"));
            assert_eq!(recipes, 0, "successful publication retires its recipe");
        } else {
            assert_eq!(ws.read(PAGE), before);
            assert!(!ws.vault().join("wiki/concepts/concept-a.md").exists());
            assert_eq!(
                recipes, 1,
                "unfinished extraction keeps its discovery recipe"
            );
        }
        if matches!(scenario, "new-match" | "changed-match") {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("semantic read scope changed")
            );
        }
        if scenario == "no-queries" {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("requires discovery before drafting")
            );
        }
        if matches!(scenario, "missing-context" | "missing-admission") {
            assert!(
                ws.vault()
                    .join(".lorekeeper/queue/results/corrupt/discovery.json")
                    .exists()
            );
        }
    }
}

#[test]
fn explicit_discovery_scope_replaces_the_recorded_recipe_instead_of_adding_to_it() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.write(
        "wiki/documents/recorded.md",
        "---\ntype: document\n---\nRecorded Candidate has a separate claim.\n",
    );
    ws.write(
        "wiki/documents/selected.md",
        "---\ntype: document\n---\nSelected Candidate has a scoped claim.\n",
    );
    ws.drop_result("explicit-discovery", PAGE, "h", "Selected Candidate");
    ws.capture_extraction_scope(
        "explicit-discovery",
        &[lk_core::discovery::DiscoveryQuery {
            query: "Recorded Candidate".into(),
            phrase: true,
            spacing: false,
        }],
    );
    let queries = vec![lk_core::discovery::DiscoveryQuery {
        query: "Selected Candidate".into(),
        phrase: true,
        spacing: false,
    }];
    ws.write(
        ".lorekeeper/explicit-discovery.json",
        &serde_json::json!({"queries":queries}).to_string(),
    );
    let file = ws.vault().join(".lorekeeper/explicit-discovery.json");
    let context = work_json(
        &ws,
        &[
            "work",
            "context",
            "--task-id",
            "explicit-discovery",
            "--discovery-file",
            file.to_str().unwrap(),
        ],
    );
    let addresses: Vec<_> = context["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["address"].as_str().unwrap())
        .collect();
    assert!(addresses.contains(&"wiki/documents/selected.md"));
    assert!(!addresses.contains(&"wiki/documents/recorded.md"));
    let result_path = ".lorekeeper/queue/results/explicit-discovery.json";
    let mut result: lk_queue::TaskResult = serde_json::from_str(&ws.read(result_path)).unwrap();
    result.context_revision = context["context_revision"].as_str().unwrap().into();
    result.discovery = queries;
    ws.write(result_path, &serde_json::to_string(&result).unwrap());
    ws.write(
        "wiki/documents/recorded.md",
        "---\ntype: document\n---\nRecorded Candidate changed outside the explicit read scope.\n",
    );
    ws.apply_expecting(1, 0);
    assert!(
        ws.read(PAGE)
            .contains("wiki/concepts/selected-candidate.md")
    );
}

#[test]
fn multipart_context_discloses_an_exact_plan_without_a_large_resource_roster() {
    let ws = Workspace::new();
    bounded_config(&ws);
    let text = "Complete retained source.\n".repeat(600);
    ws.write(PAGE, &daily_page(&text));
    ws.drop_result("context-plan", PAGE, &text, "Selected Candidate");
    let query = lk_core::discovery::DiscoveryQuery {
        query: "Selected Candidate".into(),
        phrase: true,
        spacing: false,
    };
    ws.write(
        ".lorekeeper/explicit-discovery.json",
        &serde_json::json!({"queries":[query]}).to_string(),
    );
    let file = ws.vault().join(".lorekeeper/explicit-discovery.json");
    let context = work_json(
        &ws,
        &[
            "work",
            "context",
            "--task-id",
            "context-plan",
            "--discovery-file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(context["requires_bounded_work"], true);
    assert!(context["resources"].is_null());
    assert!(context["resource_count"].as_u64().unwrap() > 0);
    assert!(context["part_count"].as_u64().unwrap() > 1);
    assert!(
        !ws.vault().join(".lorekeeper/work").exists(),
        "context capture remains read-only"
    );
    let command: Vec<_> = context["plan_command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    assert_eq!(command[0], "lore");
    assert_eq!(command[command.len() - 2], "--discovery-file");
    assert_eq!(
        Path::new(command.last().unwrap()),
        file.canonicalize().unwrap()
    );
    let planned = work_json(&ws, &command[1..]);
    assert_eq!(planned["context_revision"], context["context_revision"]);
    assert_eq!(planned["parts"], context["part_count"]);
}

#[test]
fn extraction_scope_tracks_declared_reviews_without_a_lexical_match() {
    let owner = "wiki/concepts/canonical.md";
    for (bounded, subject, scenario) in [
        (false, owner, "current"),
        (false, owner, "new"),
        (false, owner, "changed"),
        (false, owner, "deleted"),
        (false, owner, "unrelated"),
        (false, PAGE, "new"),
        (false, PAGE, "unrelated"),
        (true, owner, "current"),
        (true, owner, "changed"),
        (true, owner, "unrelated"),
    ] {
        let ws = Workspace::new();
        if bounded {
            bounded_config(&ws);
        }
        let text = if bounded {
            "Retained source material.\n".repeat(500)
        } else {
            "h".into()
        };
        ws.write(PAGE, &daily_page(&text));
        ws.write(owner, "---\ntype: concept\ntitle: Selected Candidate\n---\n## Synthesis\nEstablished scoped definition.\n\n## Sources\n\n## Related\n");
        ws.write(
            "wiki/concepts/unrelated.md",
            "---\ntype: concept\n---\nSeparate subject.\n",
        );
        let evidence_path = "wiki/documents/proof.md";
        let evidence = "---\ntype: document\n---\nAn independent recorded observation.\n";
        ws.write(evidence_path, evidence);
        let citation = lk_core::concept::Citation::new(
            Path::new(evidence_path),
            &lk_vault::parse_page(evidence).unwrap(),
        );
        let review_path = "wiki/explorations/judgment.md";
        let review = |subject: &str, judgment: &str| {
            format!(
                "---\ntype: exploration\nreview: {}\n---\n## Judgment\n{judgment}\n",
                serde_json::json!({"status":"open","subjects":[subject],"evidence":[citation],"resolution":null}),
            )
        };
        if matches!(scenario, "current" | "changed" | "deleted") {
            ws.write(
                review_path,
                &review(subject, "The recorded scope needs clarification."),
            );
        }
        ws.drop_result("review-scope", PAGE, &text, "Selected Candidate");
        let result_path = ".lorekeeper/queue/results/review-scope.json";
        let result = ws.read(result_path);
        if bounded {
            let planned = work_json(&ws, &["work", "plan", "--task-id", "review-scope"]);
            assert!(planned["parts"].as_u64().unwrap() > 1);
            let work_id = planned["work_id"].as_str().unwrap();
            loop {
                let progress = work_json(&ws, &["work", "status", work_id]);
                if progress["complete"] == true {
                    break;
                }
                for node in progress["ready"].as_array().unwrap() {
                    let node_id = node.as_str().unwrap();
                    let input = work_json(&ws, &["work", "read", work_id, node_id]);
                    ws.write(".lorekeeper/observation.json", &serde_json::json!({
                        "work_id":work_id,"node_id":node_id,"input_revision":input["input_revision"],
                        "summary":"Fixture observation preserves the assigned source and scoped review context.",
                        "findings":[],"unresolved":[],"resolutions":[],
                    }).to_string());
                    let file = ws.vault().join(".lorekeeper/observation.json");
                    work_json(&ws, &["work", "record", "--file", file.to_str().unwrap()]);
                }
            }
            ws.write(".lorekeeper/final.json", &result);
            let file = ws.vault().join(".lorekeeper/final.json");
            let finished = work_json(
                &ws,
                &["work", "finish", work_id, "--file", file.to_str().unwrap()],
            );
            assert_ne!(finished["staged_result"], result_path);
            let aliases = json_files(&results_dir(&ws));
            assert_eq!(
                aliases.len(),
                2,
                "manual staging and work finish use different result filenames"
            );
            for path in aliases {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        &std::fs::read_to_string(path).unwrap()
                    )
                    .unwrap(),
                    serde_json::from_str::<serde_json::Value>(&result).unwrap()
                );
            }
        }
        let target_before = ws.read(PAGE);
        let owner_before = ws.read(owner);
        match scenario {
            "new" => ws.write(review_path, &review(subject, "The source is disputed.")),
            "changed" => ws.write(
                review_path,
                &review(subject, "The assessment now limits the applicable scope."),
            ),
            "deleted" => std::fs::remove_file(ws.vault().join(review_path)).unwrap(),
            "unrelated" => ws.write(
                review_path,
                &review("wiki/concepts/unrelated.md", "An unrelated assessment."),
            ),
            _ => {}
        }
        let should_apply = matches!(scenario, "current" | "unrelated");
        for dry_run in [true, false] {
            let output = ws.run(if dry_run {
                &["queue", "apply", "--dry-run"]
            } else {
                &["queue", "apply"]
            });
            assert_eq!(
                output.status.success(),
                should_apply,
                "bounded={bounded}, subject={subject}, scenario={scenario}, dry={dry_run}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if !should_apply {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("semantic read scope changed")
                );
            }
            if dry_run || !should_apply {
                assert_eq!(ws.read(PAGE), target_before);
                assert_eq!(ws.read(owner), owner_before);
            }
        }
    }
}

#[test]
fn reuse_requires_the_exact_owner_inside_the_captured_discovery_scope() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    let owner = "wiki/concepts/canonical.md";
    let original = "---\ntype: concept\ntitle: Canonical Owner\n---\n## Synthesis\nEstablished scope.\n\n## Sources\n\n## Related\n";
    ws.write(owner, original);
    ws.drop_result("owner-scope", PAGE, "h", "Source Term");
    let path = ".lorekeeper/queue/results/owner-scope.json";
    let mut result: lk_queue::TaskResult = serde_json::from_str(&ws.read(path)).unwrap();
    result.concepts[0].admission = lk_queue::ConceptAdmission::Reuse {
        owner: lk_queue::ConceptOwner {
            path: owner.into(),
            revision: lk_core::concept::file_revision(original),
        },
    };
    ws.write(path, &serde_json::to_string(&result).unwrap());
    let before = ws.read(PAGE);
    let refused = ws.run(&["queue", "apply"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("outside the captured discovery context")
    );
    assert_eq!(ws.read(PAGE), before);
    assert_eq!(ws.read(owner), original);
    result.discovery.push(lk_core::discovery::DiscoveryQuery {
        query: "Canonical Owner".into(),
        phrase: true,
        spacing: false,
    });
    result.context_revision = ws.capture_extraction_scope(&result.task_id, &result.discovery);
    ws.write(path, &serde_json::to_string(&result).unwrap());
    ws.apply_expecting(1, 0);
    assert!(ws.read(PAGE).contains("wiki/concepts/canonical.md"));
    assert!(ws.read(owner).contains("Established scope."));
    assert!(!ws.vault().join("wiki/concepts/source-term.md").exists());
}

#[test]
fn existing_only_policy_requires_explicit_reuse_after_an_owner_appears() {
    let ws = Workspace::new();
    let mut request = extraction_request("policy-proof");
    request.concept_policy = lk_core::config::ConceptPolicy::ExistingOnly;
    let hash = request.cache_hash();
    ws.write(
        PAGE,
        &daily_page("policy-proof")
            .replace(&extraction_request("policy-proof").cache_hash(), &hash),
    );
    ws.drop_result("policy-proof", PAGE, "policy-proof", "Known alias");
    let task_path = ".lorekeeper/queue/policy-proof.jsonl";
    let mut task: lk_queue::QueueTask = serde_json::from_str(&ws.read(task_path)).unwrap();
    task.input
        .insert("concept_policy".into(), serde_json::json!("existing-only"));
    task.cache_hash = hash.clone();
    ws.write(task_path, &serde_json::to_string(&task).unwrap());
    let result_path = ".lorekeeper/queue/results/policy-proof.json";
    let mut result: lk_queue::TaskResult = serde_json::from_str(&ws.read(result_path)).unwrap();
    result.concept_policy = lk_core::config::ConceptPolicy::ExistingOnly;
    result.cache_hash = hash;
    result.context_revision = ws.capture_extraction_scope(&result.task_id, &result.discovery);
    ws.write(result_path, &serde_json::to_string(&result).unwrap());

    let before = ws.read(PAGE);
    let rejected = ws.run(&["queue", "apply"]);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("existing-only concept policy refuses new concept")
    );
    assert_eq!(ws.read(PAGE), before);
    assert!(!ws.vault().join("wiki/concepts/known-alias.md").exists());
    assert!(ws.vault().join(result_path).exists());
    assert!(ws.vault().join(task_path).exists());

    ws.write("wiki/concepts/stable-address.md", "---\ntype: concept\ntitle: Existing subject\naliases: [Known alias]\n---\n## Synthesis\nA stable scoped definition.\n\n## Sources\n\n## Related\n");
    let outdated = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(!outdated.status.success());
    assert!(String::from_utf8_lossy(&outdated.stderr).contains("semantic read scope changed"));
    result.concepts[0].admission = lk_queue::ConceptAdmission::Reuse {
        owner: lk_queue::ConceptOwner {
            path: "wiki/concepts/stable-address.md".into(),
            revision: lk_core::concept::file_revision(&ws.read("wiki/concepts/stable-address.md")),
        },
    };
    result.context_revision = ws.capture_extraction_scope(&result.task_id, &result.discovery);
    ws.write(result_path, &serde_json::to_string(&result).unwrap());
    ws.apply_expecting(1, 0);
    assert!(ws.read(PAGE).contains("wiki/concepts/stable-address.md"));
    assert!(!ws.vault().join("wiki/concepts/known-alias.md").exists());
    assert!(
        ws.read("wiki/concepts/stable-address.md")
            .contains("A stable scoped definition.")
    );
}

/// A result whose target page will not parse is kept, and does not strand the batch.
///
/// The likeliest author of unparseable frontmatter is a drain stamping a marker, and the
/// repair is a line. So the page is a question nobody can answer yet rather than a dead
/// result: consuming it would spend the extraction on a page that could not receive it, with
/// nothing left to say what was lost. And every other page's citations must not wait for that
/// repair — aborting would hold them on this run and on every run after it, since nothing
/// prunes results.
#[test]
fn a_result_whose_page_will_not_parse_is_kept_and_holds_up_nothing() {
    const BROKEN: &str = "daily/notes/2026-05-24.md";
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.write(
        BROKEN,
        "---\nid: notes-2026-05-24\ntype: daily\nllm_inputs:\n   [[[\n---\n\n\
         ## Related Concepts\n\n",
    );
    ws.drop_result("ext-good", PAGE, "h", "Concept A");
    ws.drop_result("ext-broken", BROKEN, "h", "Concept B");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        !out.status.success(),
        "a result nothing could apply must fail the run"
    );

    assert!(
        ws.read(PAGE).contains("wiki/concepts/concept-a.md"),
        "the page that could be written must be cited:\n{}",
        ws.read(PAGE)
    );
    assert_eq!(
        json_files(&results_dir(&ws))
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        vec!["ext-broken.json".to_string()],
        "the unapplicable result is kept and the applied one consumed"
    );
}

/// `--dry-run` reports without touching the vault or consuming a result.
#[test]
fn dry_run_applies_nothing_and_consumes_nothing() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    let before = ws.read(PAGE);

    let out = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(out.status.success(), "dry-run must succeed");
    assert_eq!(ws.read(PAGE), before, "dry-run must not rewrite the page");
    assert_eq!(
        json_files(&results_dir(&ws)).len(),
        1,
        "dry-run must leave the result for a real run"
    );
    assert!(
        !ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "dry-run must not write concept pages"
    );
}

#[test]
fn extraction_refuses_a_manual_source_edit_without_a_cache_marker_change() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_grounded_result("ext-1", PAGE, "h", "Concept A", Some("Original claim."));
    let revised = format!("{}\nThe original claim was withdrawn.\n", ws.read(PAGE));
    ws.write(PAGE, &revised);
    let output = ws.run(&["queue", "apply"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("target changed"));
    assert_eq!(ws.read(PAGE), revised);
    assert!(!ws.vault().join("wiki/concepts/concept-a.md").exists());
    assert!(results_dir(&ws).join("ext-1.json").exists());
}

#[test]
fn an_extraction_result_left_after_publication_is_retired_without_rewriting() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    let result_path = ".lorekeeper/queue/results/ext-1.json";
    let original = ws.read(result_path);
    ws.apply_expecting(1, 0);
    let page = ws.read(PAGE);
    let concept = ws.read("wiki/concepts/concept-a.md");
    ws.write(result_path, &original);
    ws.apply_expecting(0, 1);
    assert_eq!(ws.read(PAGE), page);
    assert_eq!(ws.read("wiki/concepts/concept-a.md"), concept);
    assert!(!ws.vault().join(result_path).exists());
}

#[test]
fn extraction_cannot_retarget_a_pending_task_or_apply_without_one() {
    for scenario in ["retargeted", "orphaned"] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.drop_result("ext-1", PAGE, "h", "Concept A");
        let task_path = ws.vault().join(".lorekeeper/queue/ext-1.jsonl");
        if scenario == "orphaned" {
            std::fs::remove_file(task_path).unwrap();
        } else {
            let mut task: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
            task["target"]["vault_path"] = "daily/another.md".into();
            std::fs::write(task_path, serde_json::to_vec(&task).unwrap()).unwrap();
        }
        let before = ws.read(PAGE);
        let output = ws.run(&["queue", "apply"]);
        assert!(!output.status.success(), "{scenario}");
        assert_eq!(ws.read(PAGE), before);
        assert!(!ws.vault().join("wiki/concepts/concept-a.md").exists());
        if scenario == "orphaned" {
            assert!(results_dir(&ws).join("orphaned/ext-1.json").exists());
        } else {
            assert!(results_dir(&ws).join("ext-1.json").exists());
        }
    }
}

#[test]
fn a_result_whose_page_moved_on_is_dropped() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("newer-hash"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(out.status.success(), "a dropped result is not a failure");
    assert!(
        !ws.read(PAGE).contains("concept-a.md"),
        "a stale result must not edit the page"
    );
    assert!(json_files(&results_dir(&ws)).is_empty(), "and is consumed");
}

/// A `vault.locale` switch re-renders every heading but leaves the concept input hash
/// untouched, so a result drained before the switch stays hash-current while naming a
/// section that no longer exists. It used to fail `queue apply` — and so the whole
/// scheduled pipeline — on every run forever, since `queue prune` classifies tasks and
/// never results. It is dropped instead: the page carries no completion marker, so the
/// next ingest re-enqueues the work under the heading the page now has.
#[test]
fn a_result_whose_section_no_longer_exists_is_dropped_not_retried_forever() {
    let ws = Workspace::new();
    // Same input hash; the section is now named in another locale.
    ws.write(
        PAGE,
        "---\nid: notes-2026-05-23\ntype: daily\nllm_inputs:\n  concepts: \"h\"\n---\n\n\
         ## 관련 개념\n\n## Key Events\n\n- a\n",
    );
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        out.status.success(),
        "a result with nowhere to land is dropped, not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        json_files(&results_dir(&ws)).is_empty(),
        "it must be consumed, or it blocks every later run"
    );
    assert!(
        !ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "and nothing may be written from it"
    );
    assert_eq!(
        ws.read(PAGE).matches("## ").count(),
        2,
        "the page is untouched:\n{}",
        ws.read(PAGE)
    );
}

/// A result is a VALUE IN FLIGHT, not a request for work. The page already carrying the
/// completion marker for this input does not make the result redundant — for concepts the
/// marker is stamped by the very edit that writes them, and a drain following the skill's
/// own step-3c table stamps it before `queue apply` ever runs. Dropping such a result
/// discards the extraction permanently and silently: the marker keeps the empty section
/// looking cached, so nothing re-enqueues, and the command still exits 0.
#[test]
fn a_result_lands_even_when_its_page_is_already_marked_done() {
    let ws = Workspace::new();
    let hash = extraction_request("h").cache_hash();
    ws.write(PAGE, &format!("---\nid: notes-2026-05-23\ntype: daily\nllm_inputs:\n  concepts: {hash}\n  concepts_done: {hash}\n---\n\n## Related Concepts\n\n## Key Events\n\n- a\n"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(out.status.success(), "queue apply must succeed");
    assert!(
        ws.read(PAGE).contains("wiki/concepts/concept-a.md"),
        "the citation must be written:\n{}",
        ws.read(PAGE)
    );
    assert!(
        ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "and the concept page it points at must exist"
    );
}

/// Re-applying a result that already landed reproduces the page rather than doubling it —
/// which is what makes it safe for a result to ignore the completion marker.
#[test]
fn applying_the_same_result_twice_is_idempotent() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.apply_expecting(1, 0);
    let once = ws.read(PAGE);

    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    // APPLIED again, not dropped: the first apply stamped the completion marker, so a
    // result judged by that marker would be discarded here — leaving the page identical,
    // which is exactly what idempotence looks like from the bytes alone.
    ws.apply_expecting(1, 0);
    assert_eq!(
        ws.read(PAGE),
        once,
        "a re-applied result must change nothing"
    );
}

/// The safety argument for a result ignoring the completion marker is that re-applying is
/// idempotent — so it has to hold for the CONCEPT page too, not just the origin. A merge
/// that reset an established page's accumulated state would trade a silent loss for a
/// silent corruption, which is worse.
#[test]
fn re_applying_preserves_everything_an_established_concept_page_accumulated() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.apply_expecting(1, 0);

    // Stand in for what the rest of the system writes onto an established page:
    // `backlinks-sync` owns the count, a human or a later audit owns the synthesis and
    // aliases, and `created` is the day it entered the vault.
    let concept = "wiki/concepts/concept-a.md";
    let enriched = ws
        .read(concept)
        .replace("source_count: 0", "source_count: 3")
        .replace("created: 2026-05-23", "created: 2020-01-01")
        .replace(
            "aliases: [\"Concept A\"]",
            "aliases: [\"Concept A\", \"CA\"]",
        )
        + "\nA human wrote this.\n";
    ws.write(concept, &enriched);

    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    // Applied, not dropped — see `applying_the_same_result_twice_is_idempotent`.
    ws.apply_expecting(1, 0);

    let after = ws.read(concept);
    assert!(
        after.contains("source_count: 3"),
        "count preserved:\n{after}"
    );
    assert!(
        after.contains("created: 2020-01-01"),
        "origin date preserved:\n{after}"
    );
    assert!(after.contains("\"CA\""), "alias preserved:\n{after}");
    assert!(
        after.contains("A human wrote this."),
        "authored prose preserved:\n{after}"
    );
}

#[test]
fn missing_retained_extraction_refuses_small_and_large_sources_without_publication() {
    for large in [false, true] {
        let ws = Workspace::new();
        bounded_config(&ws);
        let source = format!(
            "{}{}",
            daily_page("orphan"),
            if large {
                "Complete retained source.\n".repeat(1000)
            } else {
                String::new()
            }
        );
        ws.write(PAGE, &source);
        ws.drop_result("orphan", PAGE, "orphan", "Unpublished Subject");
        std::fs::remove_file(ws.vault().join(".lorekeeper/queue/orphan.jsonl")).unwrap();
        let result = ws.read(".lorekeeper/queue/results/orphan.json");
        let dry = ws.run(&["queue", "apply", "--dry-run"]);
        assert!(!dry.status.success());
        assert!(String::from_utf8_lossy(&dry.stderr).contains("no matching pending task"));
        assert_eq!(ws.read(".lorekeeper/queue/results/orphan.json"), result);
        let applied = ws.run(&["queue", "apply"]);
        assert!(!applied.status.success());
        assert_eq!(ws.read(PAGE), source);
        assert!(
            !ws.vault()
                .join("wiki/concepts/unpublished-subject.md")
                .exists()
        );
        assert_eq!(
            ws.read(".lorekeeper/queue/results/orphaned/orphan.json"),
            result
        );
        assert!(!ws.vault().join(".lorekeeper/work").exists());
    }
}

fn bounded_config(ws: &Workspace) {
    let path = ws.root.path().join("config.yaml");
    let config = std::fs::read_to_string(&path).unwrap().replace(
        "work: {extraction_context: full-context}",
        "work: {extraction_context: full-context, max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 100}",
    );
    std::fs::write(path, config).unwrap();
}

fn work_json(ws: &Workspace, args: &[&str]) -> serde_json::Value {
    let output = ws.run(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn synthesis_context(ws: &Workspace, task_id: &str) -> serde_json::Value {
    work_json(ws, &["work", "context", "--task-id", task_id])["context_revision"].clone()
}

#[test]
fn multipart_extraction_requires_completed_work_without_poisoning_valid_results() {
    for prepared in [false, true] {
        let ws = Workspace::new();
        bounded_config(&ws);
        let large = "Complete retained evidence.\n\n".repeat(2000);
        ws.write(PAGE, &daily_page(&large));
        ws.drop_result("a-large", PAGE, &large, "Blocked Subject");
        let small = "daily/notes/small.md";
        ws.write(small, &daily_page("small"));
        ws.drop_result("z-small", small, "small", "Published Subject");
        let original = ws.read(PAGE);
        if prepared {
            let plan = work_json(&ws, &["work", "plan", "--task-id", "a-large"]);
            assert!(plan["parts"].as_u64().unwrap() > 1);
            assert_eq!(plan["completed"], 0);
        }
        let output = ws.run(&["queue", "apply"]);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(if prepared {
                "work reduction is incomplete"
            } else {
                "bounded evidence work is required"
            }),
            "{diagnostic}"
        );
        assert_eq!(ws.read(PAGE), original);
        assert!(!ws.vault().join("wiki/concepts/blocked-subject.md").exists());
        assert!(
            ws.vault()
                .join("wiki/concepts/published-subject.md")
                .exists()
        );
        assert!(ws.read(small).contains("concepts_done:"));
        assert!(results_dir(&ws).join("a-large.json").exists());
        assert!(!results_dir(&ws).join("z-small.json").exists());
    }
}

#[test]
fn sealed_synthesis_requires_exact_read_source_bytes_even_when_authored_revision_is_unchanged() {
    let ws = Workspace::new();
    bounded_config(&ws);
    let source_path = "wiki/documents/source.md";
    let source = format!(
        "---\ntype: document\nupdated: 2026-01-01\n---\n{}",
        "Complete source evidence.\n\n".repeat(1000)
    );
    ws.write(source_path, &source);
    let parsed = lk_core::frontmatter::parse_page(&source).unwrap();
    let citation = lk_core::concept::Citation::new(Path::new(source_path), &parsed);
    let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
    let target = "wiki/concepts/topic.md";
    let body = format!(
        "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nPrior claim.\n\n## Related\n"
    );
    ws.write(target, &body);
    let task = serde_json::json!({"task_id":"syn-large", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
        "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":target,"kind":"concept-synthesis","anchor":"## Synthesis"}});
    ws.write(".lorekeeper/queue/syn-large.jsonl", &task.to_string());
    let planned = work_json(&ws, &["work", "plan", "--task-id", "syn-large"]);
    assert!(planned["parts"].as_u64().unwrap() > 1);
    let work_id = planned["work_id"].as_str().unwrap();
    loop {
        let progress = work_json(&ws, &["work", "status", work_id]);
        if progress["complete"] == true {
            break;
        }
        let nodes = progress["ready"].as_array().unwrap();
        assert!(!nodes.is_empty());
        for node in nodes {
            let node_id = node.as_str().unwrap();
            let input = work_json(&ws, &["work", "read", work_id, node_id]);
            let observation = serde_json::json!({"work_id":work_id,"node_id":node_id,
                "input_revision":input["input_revision"],"summary":"Deterministic fixture observation for the complete selected ranges.",
                "findings":[],"unresolved":[],"resolutions":[]});
            ws.write(
                ".lorekeeper/fixture-observation.json",
                &observation.to_string(),
            );
            let file = ws.vault().join(".lorekeeper/fixture-observation.json");
            work_json(&ws, &["work", "record", "--file", file.to_str().unwrap()]);
        }
    }
    let result = serde_json::json!({"task_id":"syn-large","cache_hash":hash,"context_revision":planned["context_revision"],
        "target_revision":blake3::hash(body.as_bytes()).to_hex().to_string(),
        "synthesis":"The retained source supports the scoped claim.","related":""});
    ws.write(".lorekeeper/fixture-final.json", &result.to_string());
    let file = ws.vault().join(".lorekeeper/fixture-final.json");
    work_json(
        &ws,
        &["work", "finish", work_id, "--file", file.to_str().unwrap()],
    );
    let preview = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let changed = source.replace("updated: 2026-01-01", "updated: 2026-01-02");
    assert_eq!(
        lk_core::concept::source_revision(&parsed),
        lk_core::concept::source_revision(&lk_core::frontmatter::parse_page(&changed).unwrap())
    );
    ws.write(source_path, &changed);
    let output = ws.run(&["queue", "apply"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("semantic read scope changed"));
    assert_eq!(ws.read(target), body);
    assert!(!json_files(&results_dir(&ws)).is_empty());
    ws.write(source_path, &source);
    let applied = ws.run(&["queue", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        ws.read(target)
            .contains("The retained source supports the scoped claim.")
    );
    assert!(ws.read(target).contains("synthesis_done:"));
    assert!(json_files(&results_dir(&ws)).is_empty());
}

/// `queue prune`'s `--json` envelope carries the same verdict as its exit code.
///
/// The janitor is the only place a blocked queue surfaces — `queue count` omits it so a
/// scheduled run never spends a session on work no session can do — so a caller that parses
/// `ok` is the one that has to be told, and telling it the opposite of the exit code is worse
/// than saying nothing.
#[test]
fn prune_reports_the_same_verdict_in_json_as_in_its_exit_code() {
    let ws = Workspace::new();
    let queue = ws.vault().join(".lorekeeper/queue");
    std::fs::create_dir_all(&queue).expect("queue dir");

    let verdict = |args: &[&str]| {
        let out = ws.run(args);
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
        (
            parsed["ok"].as_bool().expect("ok is a bool"),
            out.status.code() == Some(0),
            parsed,
        )
    };

    // Nothing to do: green, and it says so.
    let (ok, exited_zero, _) = verdict(&["queue", "prune", "--json"]);
    assert!(ok && exited_zero, "an empty queue is not a finding");

    // One line nobody can parse: the file is kept whole, and so is any task beside it.
    std::fs::write(queue.join("run-1.jsonl"), "{not json\n").expect("queue file");
    let (ok, exited_zero, parsed) = verdict(&["queue", "prune", "--json", "--dry-run"]);
    assert_eq!(
        ok, exited_zero,
        "the parsed verdict and the exit code are one answer\n{parsed}"
    );
    assert!(!ok, "a queue nothing can drain is a finding\n{parsed}");
    assert_eq!(parsed["data"]["kept_unparseable"].as_u64(), Some(1));
}

/// A grounding sentence answers the ONE page whose material produced it, and `queue apply`
/// records that page as the concept's synthesis input. Without the record the sentence is
/// indistinguishable from an authored synthesis, and `backlinks-sync` adopts it as the answer
/// for every page citing the concept by the time it first runs — which in a backfill is the
/// whole citation set, permanently, since adoption also stamps the answer.
#[test]
fn a_grounding_answers_one_page_and_a_second_citation_owes_a_rewrite() {
    const SECOND: &str = "daily/notes/2026-05-24.md";
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.write(SECOND, &dated_daily_page("2026-05-24", "h"));
    ws.drop_grounded_result("ext-1", PAGE, "h", "Concept A", Some("What Concept A is."));
    ws.apply_expecting(1, 0);
    ws.drop_result("ext-2", SECOND, "h", "Concept A");
    ws.apply_expecting(1, 0);

    let concept = ws.read("wiki/concepts/concept-a.md");
    let grounded = lk_core::concept::citation_digest(&[lk_core::concept::Citation::new(
        std::path::Path::new(PAGE),
        &lk_core::frontmatter::parse_page(&ws.read(PAGE)).unwrap(),
    )]);
    assert!(
        concept.contains(&format!("synthesis_done: \"{grounded}\"")),
        "the seeded sentence must answer the page it was written from:\n{concept}"
    );

    let out = ws.run(&["graph", "backlinks-sync"]);
    assert!(
        out.status.success(),
        "backlinks-sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = ws.run(&["queue", "count"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1",
        "two pages cite a concept whose synthesis answers one, so the rewrite is owed"
    );
}

#[test]
fn source_revisions_drive_synthesis_without_reprocessing_bookkeeping() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_grounded_result(
        "ext-1",
        PAGE,
        "h",
        "Concept A",
        Some("Timeout is 30 seconds."),
    );
    ws.apply_expecting(1, 0);

    let sync = || {
        let out = ws.run(&["graph", "--json", "backlinks-sync"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["data"].clone()
    };
    assert!(ws.run(&["queue", "prune"]).status.success());
    assert_eq!(sync()["queued"], 0);
    let page = ws.read(PAGE);
    let changed_metadata = page.replacen("---\n", "---\nupdated: 2026-10-15\n", 1);
    ws.write(PAGE, &changed_metadata);
    assert_eq!(sync()["queued"], 0);

    ws.write(
        PAGE,
        &format!("{changed_metadata}\nTimeout is 5 seconds.\n"),
    );
    assert_eq!(sync()["queued"], 1);
    assert_eq!(sync()["queued"], 0, "a pending revision is queued once");
    let status = || {
        let out = ws.run(&["queue", "status", "--json"]);
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["data"].clone()
    };
    assert_eq!(status()["current"], 1);

    let latest = ws.read(PAGE);
    ws.write(PAGE, &latest.replacen("---\n", "---\nbroken: [\n", 1));
    assert_eq!(status()["unreadable"], 1);
    let before = ws.read("wiki/concepts/concept-a.md");
    let out = ws.run(&["graph", "--json", "backlinks-sync"]);
    assert!(!out.status.success());
    assert_eq!(ws.read("wiki/concepts/concept-a.md"), before);

    ws.write(PAGE, &latest);
    assert_eq!(status()["current"], 1, "repair restores the pending task");
    ws.write(PAGE, &latest.replace("5 seconds", "10 seconds"));
    assert_eq!(
        status()["stale"],
        1,
        "a queued draft cannot use changed evidence"
    );
    assert_eq!(sync()["queued"], 1);
    assert_eq!(status()["current"], 1);
}

#[test]
fn synthesis_results_preserve_edits_and_apply_both_sections_only_to_current_inputs() {
    for scenario in [
        "current",
        "source-changed",
        "target-changed",
        "missing-context",
        "missing-related",
        "missing-link",
        "existing-link",
    ] {
        let ws = Workspace::new();
        let source = "---\ntype: document\n---\nTimeout is 30 seconds.\n";
        ws.write("wiki/documents/spec.md", source);
        let citation = lk_core::concept::Citation::new(
            Path::new("wiki/documents/spec.md"),
            &lk_core::frontmatter::parse_page(source).unwrap(),
        );
        let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
        let concept = format!(
            "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nOld synthesis.\n\n## Related\nOld relation.\n\n## Notes\nKeep this.\n"
        );
        ws.write("wiki/concepts/timeout.md", &concept);
        let task = serde_json::json!({"task_id":"syn-1", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
            "input":{"locale":"en","citations":[citation], "related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/timeout.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status = ws.run(&["queue", "status", "--json"]);
        assert!(status.status.success());
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        let mut result = serde_json::json!({"task_id":"syn-1", "cache_hash":hash, "context_revision":synthesis_context(&ws, "syn-1"), "target_revision":status["data"]["tasks"][0]["target_revision"], "synthesis":"Timeout is 30 seconds.", "related":"A sourced relationship."});
        match scenario {
            "source-changed" => ws.write("wiki/documents/spec.md", &source.replace("30", "5")),
            "target-changed" => ws.write(
                "wiki/concepts/timeout.md",
                &concept.replace("Keep this.", "New human decision."),
            ),
            "missing-related" => {
                result.as_object_mut().unwrap().remove("related");
            }
            "missing-context" => {
                result.as_object_mut().unwrap().remove("context_revision");
            }
            "missing-link" => {
                result["synthesis"] =
                    serde_json::json!("Conflict: [review](../explorations/missing.md).");
            }
            "existing-link" => {
                ws.write(
                    "wiki/explorations/review.md",
                    "---\ntype: exploration\n---\nReview\n",
                );
                result["synthesis"] = serde_json::json!(
                    "Timeout is 30 seconds. [Review](../explorations/review.md#claims). [External](https://example.com) [Here](#local) ` [example](missing.md)`"
                );
            }
            _ => {}
        }
        let before = ws.read("wiki/concepts/timeout.md");
        ws.write(
            ".lorekeeper/queue/results/syn-1.synthesis.json",
            &result.to_string(),
        );
        if matches!(scenario, "missing-link" | "existing-link") {
            let dry_run = ws.run(&["queue", "apply", "--dry-run"]);
            assert_eq!(dry_run.status.success(), scenario == "existing-link");
            assert_eq!(ws.read("wiki/concepts/timeout.md"), before);
            assert!(
                ws.vault()
                    .join(".lorekeeper/queue/results/syn-1.synthesis.json")
                    .exists()
            );
        }
        let out = ws.run(&["queue", "apply"]);
        let after = ws.read("wiki/concepts/timeout.md");
        if matches!(scenario, "current" | "existing-link") {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(after.contains("Timeout is 30 seconds."));
            assert!(after.contains("A sourced relationship."));
            assert!(after.contains("Keep this."));
            assert!(after.contains("synthesis_done:"));
            assert!(ws.run(&["queue", "apply"]).status.success());
            assert_eq!(after, ws.read("wiki/concepts/timeout.md"));
        } else {
            assert_eq!(after, before, "{scenario}");
            assert_eq!(
                out.status.success(),
                scenario == "source-changed",
                "{scenario}"
            );
        }
    }
}

#[test]
fn duplicate_sections_fold_once_and_conflicting_typed_proposals_remain_for_review() {
    for synthesis in [false, true] {
        for conflicting in [false, true] {
            let ws = Workspace::new();
            let target = if synthesis {
                "wiki/concepts/subject.md"
            } else {
                "wiki/documents/subject.md"
            };
            let source = "---\ntype: document\n---\nTimeout is 30 seconds.\n";
            ws.write("wiki/documents/spec.md", source);
            let citation = lk_core::concept::Citation::new(
                Path::new("wiki/documents/spec.md"),
                &lk_core::frontmatter::parse_page(source).unwrap(),
            );
            let input = if synthesis {
                serde_json::json!({"locale":"en", "citations":[citation], "related_anchor":"## Related"})
            } else {
                serde_json::json!({"locale":"en", "text":"Retained source.", "max_sentences":3})
            };
            let hash = if synthesis {
                lk_core::concept::citation_digest(&[citation])
            } else {
                lk_queue::cache_hash(input.as_object().unwrap())
            };
            let (kind, target_kind, key, page_format, heading, suffix) = if synthesis {
                (
                    "synthesize-concept",
                    "concept-synthesis",
                    "synthesis",
                    "concept",
                    "Synthesis",
                    "synthesis",
                )
            } else {
                (
                    "summarize",
                    "document-summary",
                    "summary",
                    "document",
                    "Summary",
                    "section",
                )
            };
            let page = format!(
                "---\ntype: {page_format}\nllm_inputs:\n  {key}: {hash}\n---\n## {heading}\nPrevious body.\n\n## Related\nPrevious relation.\n"
            );
            ws.write(target, &page);
            let task = serde_json::json!({"task_id":"subject", "kind":kind, "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash, "input":input, "target":{"vault_path":target,"kind":target_kind,"anchor":format!("## {heading}")}});
            ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
            let mut result = serde_json::json!({"task_id":"subject", "cache_hash":hash, "target_revision":lk_core::concept::file_revision(&page)});
            if synthesis {
                result["context_revision"] = synthesis_context(&ws, "subject");
                result["synthesis"] = "Grounded synthesis.".into();
                result["related"] = "Grounded relation.".into();
            } else {
                result["body"] = "Grounded summary.".into();
            }
            let original = result.to_string();
            let original_path = format!(".lorekeeper/queue/results/subject.{suffix}.json");
            ws.write(&original_path, &original);
            if conflicting {
                result[if synthesis { "related" } else { "body" }] = "A conflicting answer.".into();
            }
            let alias = serde_json::to_string_pretty(&result).unwrap();
            let alias_path = format!(".lorekeeper/queue/results/another-name.{suffix}.json");
            ws.write(&alias_path, &alias);

            let mut independent = task.clone();
            independent["task_id"] = "independent".into();
            independent["kind"] = "summarize".into();
            independent["input"] =
                serde_json::json!({"locale":"en","text":"Independent source.","max_sentences":3});
            independent["cache_hash"] =
                lk_queue::cache_hash(independent["input"].as_object().unwrap()).into();
            independent["target"] = serde_json::json!({"vault_path":"wiki/documents/independent.md","kind":"document-summary","anchor":"## Summary"});
            let independent_page = format!(
                "---\ntype: document\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
                independent["cache_hash"]
            );
            ws.write("wiki/documents/independent.md", &independent_page);
            ws.write(
                ".lorekeeper/queue/independent.jsonl",
                &independent.to_string(),
            );
            ws.write(".lorekeeper/queue/results/independent.section.json", &serde_json::json!({"task_id":"independent","cache_hash":independent["cache_hash"],"target_revision":lk_core::concept::file_revision(&independent_page),"body":"Independent answer."}).to_string());

            let preview = ws.run(&["queue", "apply", "--dry-run"]);
            assert_eq!(
                preview.status.success(),
                !conflicting,
                "{}",
                String::from_utf8_lossy(&preview.stderr)
            );
            assert_eq!(ws.read(target), page);
            assert_eq!(ws.read(&original_path), original);
            assert_eq!(ws.read(&alias_path), alias);
            let applied = ws.run(&["queue", "apply"]);
            assert_eq!(
                applied.status.success(),
                !conflicting,
                "{}",
                String::from_utf8_lossy(&applied.stderr)
            );
            if conflicting {
                assert!(
                    String::from_utf8_lossy(&applied.stderr)
                        .contains("conflicting section/synthesis results")
                );
                assert_eq!(ws.read(target), page);
                assert_eq!(ws.read(&original_path), original);
                assert_eq!(ws.read(&alias_path), alias);
            } else {
                let answered = lk_vault::parse_page(&ws.read(target)).unwrap();
                assert_eq!(
                    answered.frontmatter.get("llm_inputs").unwrap()[format!("{key}_done")],
                    hash
                );
                assert!(!ws.vault().join(&original_path).exists());
                assert!(!ws.vault().join(&alias_path).exists());
                assert!(
                    String::from_utf8_lossy(&applied.stderr).contains("2 staged sections applied")
                );
                let answered = ws.read(target);
                result[if synthesis { "related" } else { "body" }] = "Late obsolete answer.".into();
                ws.write(&original_path, &original);
                ws.write(&alias_path, &result.to_string());
                assert!(ws.run(&["queue", "apply"]).status.success());
                assert_eq!(ws.read(target), answered);
                assert!(!ws.vault().join(original_path).exists());
                assert!(!ws.vault().join(alias_path).exists());
            }
            assert!(
                ws.read("wiki/documents/independent.md")
                    .contains("Independent answer.")
            );
        }
    }
}

#[test]
fn synthesis_read_scope_guards_scoped_reviews_and_context_for_whole_and_bounded_work() {
    for bounded in [false, true] {
        for scenario in [
            "current",
            "new-review",
            "changed-review",
            "deleted-review",
            "unrelated-review",
            "related-changed",
            "config-changed",
        ] {
            let ws = Workspace::new();
            if bounded {
                bounded_config(&ws);
            }
            let source_path = "wiki/documents/policy.md";
            let source = format!(
                "---\ntype: document\n---\nRetention is 30 days.\n{}",
                if bounded {
                    "Exact supporting context.\n\n".repeat(700)
                } else {
                    String::new()
                }
            );
            ws.write(source_path, &source);
            let citation = lk_core::concept::Citation::new(
                Path::new(source_path),
                &lk_core::frontmatter::parse_page(&source).unwrap(),
            );
            let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
            let target = "wiki/concepts/retention.md";
            let body = format!(
                "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nPrior scope.\n\n## Related\n[Related](related.md)\n"
            );
            ws.write(target, &body);
            ws.write(
                "wiki/concepts/related.md",
                "---\ntype: concept\n---\n## Synthesis\nRelated context.\n",
            );
            ws.write(
                "wiki/concepts/unrelated.md",
                "---\ntype: concept\n---\n## Synthesis\nSeparate scope.\n",
            );
            let review_path = "wiki/explorations/retention-review.md";
            let review = |subject: &str, reason: &str| {
                format!(
                    "---\ntype: exploration\nreview: {}\n---\n## Judgment\n{reason}\n",
                    serde_json::json!({"status":"open","subjects":[subject],"evidence":[citation],"resolution":null})
                )
            };
            if matches!(scenario, "changed-review" | "deleted-review") {
                ws.write(review_path, &review(target, "Initial scoped uncertainty."));
            }
            let task = serde_json::json!({"task_id":"scope", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
                "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":target,"kind":"concept-synthesis","anchor":"## Synthesis"}});
            ws.write(".lorekeeper/queue/scope.jsonl", &task.to_string());
            let context = work_json(&ws, &["work", "context", "--task-id", "scope"]);
            assert!(!ws.vault().join(".lorekeeper/work").exists());
            let result = serde_json::json!({"task_id":"scope","cache_hash":hash,
                "target_revision":context["subject"]["target_revision"],"context_revision":context["context_revision"],
                "synthesis":"The source declares retention of 30 days.","related":"Related context remains separate from supporting evidence."});
            if bounded {
                let planned = work_json(&ws, &["work", "plan", "--task-id", "scope"]);
                assert!(planned["parts"].as_u64().unwrap() > 1);
                assert_eq!(planned["context_revision"], context["context_revision"]);
                let work_id = planned["work_id"].as_str().unwrap();
                loop {
                    let progress = work_json(&ws, &["work", "status", work_id]);
                    if progress["complete"] == true {
                        break;
                    }
                    for node in progress["ready"].as_array().unwrap() {
                        let node_id = node.as_str().unwrap();
                        let input = work_json(&ws, &["work", "read", work_id, node_id]);
                        let observation = serde_json::json!({"work_id":work_id,"node_id":node_id,"input_revision":input["input_revision"],
                            "summary":"Fixture observation preserves the exact assigned evidence scope.","findings":[],"unresolved":[],"resolutions":[]});
                        ws.write(".lorekeeper/observation.json", &observation.to_string());
                        let file = ws.vault().join(".lorekeeper/observation.json");
                        work_json(&ws, &["work", "record", "--file", file.to_str().unwrap()]);
                    }
                }
                ws.write(".lorekeeper/final.json", &result.to_string());
                let file = ws.vault().join(".lorekeeper/final.json");
                work_json(
                    &ws,
                    &["work", "finish", work_id, "--file", file.to_str().unwrap()],
                );
            } else {
                ws.write(
                    ".lorekeeper/queue/results/scope.synthesis.json",
                    &result.to_string(),
                );
            }
            match scenario {
                "new-review" => ws.write(
                    review_path,
                    &review(target, "New scoped uncertainty after drafting."),
                ),
                "changed-review" => ws.write(
                    review_path,
                    &review(target, "Changed judgment after drafting."),
                ),
                "deleted-review" => std::fs::remove_file(ws.vault().join(review_path)).unwrap(),
                "unrelated-review" => ws.write(
                    review_path,
                    &review("wiki/concepts/unrelated.md", "Separate assessment."),
                ),
                "related-changed" => ws.write(
                    "wiki/concepts/related.md",
                    "---\ntype: concept\n---\n## Synthesis\nChanged related context.\n",
                ),
                "config-changed" => {
                    let path = ws.root.path().join("config.yaml");
                    let mut config = std::fs::read_to_string(&path).unwrap();
                    config.push_str("\n# Selected configuration revision changed.\n");
                    std::fs::write(path, config).unwrap();
                }
                _ => {}
            }
            let should_apply = matches!(scenario, "current" | "unrelated-review");
            for dry in [true, false] {
                let out = if dry {
                    ws.run(&["queue", "apply", "--dry-run"])
                } else {
                    ws.run(&["queue", "apply"])
                };
                assert_eq!(
                    out.status.success(),
                    should_apply,
                    "bounded={bounded}, scenario={scenario}, dry={dry}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                if !should_apply {
                    assert!(
                        String::from_utf8_lossy(&out.stderr)
                            .contains("semantic read scope changed")
                    );
                }
                if dry || !should_apply {
                    assert_eq!(ws.read(target), body);
                }
            }
            if should_apply {
                assert!(ws.read(target).contains("synthesis_done:"));
            }
        }
    }
}

#[test]
fn orphan_synthesis_is_preserved_with_a_reason_and_does_not_fail_forever() {
    let ws = Workspace::new();
    let result = serde_json::json!({"task_id":"retired", "cache_hash":"old", "context_revision":"unavailable", "target_revision":"old", "synthesis":"Evidence.", "related":null}).to_string();
    let path = ".lorekeeper/queue/results/retired.synthesis.json";
    ws.write(path, &result);
    let out = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(!out.status.success());
    assert_eq!(ws.read(path), result);
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert!(!ws.vault().join(path).exists());
    assert_eq!(
        ws.read(".lorekeeper/queue/results/orphaned/retired.synthesis.json"),
        result
    );
    assert!(
        ws.read(".lorekeeper/queue/results/orphaned/retired.synthesis.json.reason.txt")
            .contains("no matching pending task")
    );
    assert!(ws.run(&["queue", "apply"]).status.success());
}

#[test]
fn orphan_quarantine_failure_preserves_bytes_and_allows_independent_extraction() {
    for suffix in ["json", "section.json", "synthesis.json"] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("independent"));
        ws.drop_result("z-independent", PAGE, "independent", "Independent Subject");
        let identity = serde_json::json!({
            "task_id": "orphan", "cache_hash": "retired", "target_revision": "captured",
        });
        let orphan = match suffix {
            "json" => {
                let mut result: serde_json::Value =
                    serde_json::from_str(&ws.read(".lorekeeper/queue/results/z-independent.json"))
                        .unwrap();
                result["task_id"] = "orphan".into();
                result
            }
            "section.json" => {
                let mut result = identity;
                result["body"] = "Unpublished section.".into();
                result
            }
            "synthesis.json" => {
                let mut result = identity;
                result["context_revision"] = "captured".into();
                result["synthesis"] = "Unpublished synthesis.".into();
                result["related"] = serde_json::Value::Null;
                result
            }
            _ => unreachable!(),
        };
        let orphan = serde_json::to_string_pretty(&orphan).unwrap() + "\n";
        let path = format!(".lorekeeper/queue/results/a-orphan.{suffix}");
        let quarantine = ".lorekeeper/queue/results/orphaned";
        ws.write(&path, &orphan);
        ws.write(quarantine, "occupied quarantine address");

        let out = ws.run(&["queue", "apply"]);
        assert!(!out.status.success(), "{suffix}");
        assert_eq!(ws.read(&path), orphan, "{suffix}");
        assert_eq!(ws.read(quarantine), "occupied quarantine address");
        let published = ws.read(PAGE);
        assert!(
            published.contains("concepts_done:"),
            "{suffix}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            ws.vault()
                .join("wiki/concepts/independent-subject.md")
                .exists()
        );
        assert!(!results_dir(&ws).join("z-independent.json").exists());

        std::fs::remove_file(ws.vault().join(quarantine)).unwrap();
        let retried = ws.run(&["queue", "apply"]);
        assert!(
            !retried.status.success(),
            "orphan remains a reported failure"
        );
        assert!(!ws.vault().join(&path).exists());
        assert_eq!(ws.read(&format!("{quarantine}/a-orphan.{suffix}")), orphan);
        assert!(
            ws.read(&format!("{quarantine}/a-orphan.{suffix}.reason.txt"))
                .contains("no matching pending task")
        );
        assert_eq!(
            ws.read(PAGE),
            published,
            "retry must not reapply completed work"
        );
        assert!(ws.run(&["queue", "apply"]).status.success());
    }
}

#[test]
fn malformed_pending_task_prevents_orphan_disposition() {
    let ws = Workspace::new();
    ws.write(".lorekeeper/queue/tasks.jsonl", "{truncated task");
    let path = ".lorekeeper/queue/results/unknown.synthesis.json";
    let result = serde_json::json!({"task_id":"unknown", "cache_hash":"old", "context_revision":"unavailable", "target_revision":"old", "synthesis":"Evidence.", "related":null}).to_string();
    ws.write(path, &result);
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert_eq!(ws.read(path), result);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unparseable queue lines"));
}

#[test]
fn corrupt_result_payloads_are_preserved_once_without_blocking_valid_synthesis() {
    for suffix in ["json", "synthesis.json", "section.json"] {
        for bytes in [
            b"{\"task_id\":".as_slice(),
            b"{\"task_id\":\"\xea\xb0".as_slice(),
        ] {
            let ws = Workspace::new();
            let source = "---\ntype: document\n---\nEvidence.\n";
            ws.write("wiki/documents/spec.md", source);
            let citation = lk_core::concept::Citation::new(
                Path::new("wiki/documents/spec.md"),
                &lk_core::frontmatter::parse_page(source).unwrap(),
            );
            let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
            ws.write("wiki/concepts/example.md", &format!("---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nOld.\n\n## Related\n\n"));
            let task = serde_json::json!({"task_id":"good", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
                "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/example.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
            ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
            let status: serde_json::Value =
                serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
            let result = serde_json::json!({"task_id":"good","cache_hash":hash,"context_revision":synthesis_context(&ws, "good"),"target_revision":status["data"]["tasks"][0]["target_revision"],"synthesis":"Grounded evidence.","related":""});
            ws.write(
                ".lorekeeper/queue/results/good.synthesis.json",
                &result.to_string(),
            );
            let bad = ws
                .vault()
                .join(format!(".lorekeeper/queue/results/bad.{suffix}"));
            std::fs::write(&bad, bytes).unwrap();
            let dry = ws.run(&["queue", "apply", "--dry-run"]);
            assert!(!dry.status.success());
            assert_eq!(std::fs::read(&bad).unwrap(), bytes);
            assert!(ws.read("wiki/concepts/example.md").contains("Old."));
            assert!(!ws.run(&["queue", "apply"]).status.success());
            assert!(
                ws.read("wiki/concepts/example.md")
                    .contains("Grounded evidence.")
            );
            assert!(!bad.exists());
            let preserved = ws
                .vault()
                .join(format!(".lorekeeper/queue/results/corrupt/bad.{suffix}"));
            assert_eq!(std::fs::read(&preserved).unwrap(), bytes);
            assert!(
                preserved
                    .with_file_name(format!("bad.{suffix}.reason.txt"))
                    .is_file()
            );
            assert!(ws.run(&["queue", "apply"]).status.success());
        }
    }
}

#[test]
fn shortened_synthesis_citations_cannot_bypass_source_validation_or_be_pruned() {
    for malformed in [
        serde_json::json!([]),
        serde_json::json!(null),
        serde_json::json!({"bad":"shape"}),
    ] {
        let ws = Workspace::new();
        ws.write("wiki/concepts/example.md", "---\ntype: concept\nllm_inputs:\n  synthesis: \"00000000000000000000000000000001\"\n---\n## Synthesis\nKeep.\n\n## Related\n\n");
        let mut task = serde_json::json!({"task_id":"bad-input", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":"00000000000000000000000000000001",
        "input":{"locale":"en","citations":[],"related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/example.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
        task["input"]["citations"] = malformed;
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        assert_eq!(status["data"]["unreadable"], 1);
        let result = serde_json::json!({"task_id":"bad-input","cache_hash":task["cache_hash"],"context_revision":"unreadable","target_revision":status["data"]["tasks"][0]["target_revision"],"synthesis":"Unsupported.","related":""});
        ws.write(
            ".lorekeeper/queue/results/bad-input.synthesis.json",
            &result.to_string(),
        );
        assert!(!ws.run(&["queue", "apply"]).status.success());
        assert!(ws.read("wiki/concepts/example.md").contains("Keep."));
        assert!(!ws.run(&["queue", "prune"]).status.success());
        assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
    }
}

#[test]
fn staged_sections_are_snapshot_guarded_and_only_the_applier_stamps_completion() {
    for scenario in [
        "summary",
        "empty",
        "theme",
        "refine",
        "changed-target",
        "stale",
        "wrong-kind",
        "missing-anchor",
        "changed-event-headings",
        "sibling-heading",
        "unknown-policy",
        "mutated-input",
        "missing-input",
        "wrong-page-format",
    ] {
        let ws = Workspace::new();
        let (kind, target_kind, key) = match scenario {
            "theme" => ("identify-themes", "weekly-synthesis-themes", "themes"),
            "refine" | "changed-event-headings" => {
                ("refine-events", "daily-refine-events", "refine_events")
            }
            _ => ("summarize", "daily-summary", "summary"),
        };
        let input = if scenario == "theme" {
            serde_json::json!({"text":"Source material", "max_themes":3, "locale":"en"})
        } else {
            serde_json::json!({"text":"Source material", "max_sentences":3, "locale":"en"})
        };
        let hash = lk_queue::cache_hash(input.as_object().unwrap());
        let page_format = match scenario {
            "theme" => "weekly-synthesis",
            "wrong-page-format" => "weekly-review",
            _ => "daily",
        };
        let page = format!(
            "---\ntype: {page_format}\nllm_inputs:\n  {key}: {hash}\n---\n## Target\n### Original Event\nRaw source.\n\n## Notes\nPreserve.\n"
        );
        ws.write("daily/source/page.md", &page);
        let mut task = serde_json::json!({"task_id":"section-1","kind":kind,"created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":{"vault_path":"daily/source/page.md","kind":target_kind,"anchor":"## Target"}});
        if scenario == "mutated-input" {
            task["input"]["text"] = "Substituted source".into();
        }
        if scenario == "missing-input" {
            task["input"].as_object_mut().unwrap().remove("text");
        }
        if scenario == "wrong-kind" {
            task["kind"] = serde_json::json!("extract-concepts");
        }
        if scenario == "unknown-policy" {
            task["input"]["generation_policy"] = serde_json::json!("future-policy");
        }
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        assert!(status["data"]["tasks"][0]["target_revision"].is_string());
        let body = match scenario {
            "empty" => "",
            "refine" => "### Original Event\nRefined evidence.",
            "changed-event-headings" => "### Different Event\nWrong.",
            "sibling-heading" => "## Notes\nOverwrite.",
            "theme" => "### 1. Theme\nGrounded theme.",
            _ => "Grounded summary.",
        };
        let result = serde_json::json!({"task_id":"section-1","cache_hash":hash,"target_revision":status["data"]["tasks"][0]["target_revision"],"body":body});
        ws.write(
            ".lorekeeper/queue/results/section-1.section.json",
            &result.to_string(),
        );
        match scenario {
            "changed-target" => ws.write(
                "daily/source/page.md",
                &page.replace("Preserve.", "Human edit."),
            ),
            "stale" => ws.write("daily/source/page.md", &page.replace(&hash, "newer")),
            "missing-anchor" => ws.write(
                "daily/source/page.md",
                &page.replace("## Target", "## Renamed"),
            ),
            _ => {}
        }
        let before = ws.read("daily/source/page.md");
        let accepted = matches!(scenario, "summary" | "empty" | "theme" | "refine");
        let dropped = matches!(scenario, "stale" | "missing-anchor");
        let dry = ws.run(&["queue", "apply", "--dry-run"]);
        assert_eq!(
            dry.status.success(),
            accepted || dropped,
            "{scenario}: {}",
            String::from_utf8_lossy(&dry.stderr)
        );
        assert_eq!(ws.read("daily/source/page.md"), before);
        let applied = ws.run(&["queue", "apply"]);
        assert_eq!(
            applied.status.success(),
            accepted || dropped,
            "{scenario}: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        if accepted {
            let after = ws.read("daily/source/page.md");
            let parsed = lk_vault::parse_page(&after).unwrap();
            assert_eq!(
                parsed.frontmatter.get("llm_inputs").unwrap()[format!("{key}_done")],
                hash
            );
            assert!(after.contains("Preserve."));
            assert!(after.contains(body));
            assert!(ws.run(&["queue", "apply"]).status.success());
            assert_eq!(ws.read("daily/source/page.md"), after);
        } else {
            assert_eq!(ws.read("daily/source/page.md"), before);
        }
        assert_eq!(
            ws.vault()
                .join(".lorekeeper/queue/results/section-1.section.json")
                .exists(),
            !accepted && !dropped
        );
        if matches!(scenario, "mutated-input" | "missing-input") {
            assert_eq!(status["data"]["tasks"][0]["status"], "unreadable");
            assert!(!ws.run(&["queue", "prune"]).status.success());
            assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
        }
    }
}

#[test]
fn staged_sections_retain_work_when_the_target_declares_an_incompatible_format() {
    for completed in [false, true] {
        let ws = Workspace::new();
        let request = lk_queue::SummarizeRequest {
            text: "Retained source evidence.".into(),
            max_sentences: 3,
            focus: None,
            locale: lk_core::i18n::Locale::En,
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/source.md".into(),
                kind: lk_queue::TargetKind::DocumentSummary,
                anchor: "## Summary".into(),
            },
        };
        let task = lk_queue::QueueLlmClient::new(ws.vault().join(".lorekeeper/queue")).build_task(
            "fixture",
            lk_queue::TaskKind::Summarize,
            request,
        );
        let marker = if completed {
            format!("  summary_done: {}\n", task.cache_hash)
        } else {
            String::new()
        };
        let page = format!(
            "---\ntype: weekly-review\nllm_inputs:\n  summary: {}\n{marker}---\n## Summary\nPreserve this body.\n",
            task.cache_hash
        );
        ws.write(&task.target.vault_path, &page);
        let pending = serde_json::to_string(&task).unwrap();
        ws.write(".lorekeeper/queue/run.jsonl", &pending);
        let result = lk_queue::SectionResult {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: lk_core::concept::file_revision(&page),
            body: "A freshly generated interpretation.".into(),
        };
        let staged = serde_json::to_string(&result).unwrap();
        let result_path = ".lorekeeper/queue/results/result.section.json";
        ws.write(result_path, &staged);

        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        assert_eq!(status["data"]["tasks"][0]["status"], "unreadable");
        for args in [vec!["queue", "apply", "--dry-run"], vec!["queue", "apply"]] {
            assert!(!ws.run(&args).status.success());
            assert_eq!(ws.read(&task.target.vault_path), page);
            assert_eq!(ws.read(result_path), staged);
        }
        assert!(!ws.run(&["queue", "check"]).status.success());
        assert!(!ws.run(&["queue", "prune"]).status.success());
        assert_eq!(ws.read(".lorekeeper/queue/run.jsonl"), pending);
        assert_eq!(ws.read(result_path), staged);
    }
}

#[test]
fn queue_apply_refuses_a_busy_vault_mutation_session_before_consuming_results() {
    let ws = Workspace::new();
    ws.write(".lorekeeper/queue/results/bad.section.json", "{truncated");
    let _held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("vault mutation"));
    assert!(
        ws.vault()
            .join(".lorekeeper/queue/results/bad.section.json")
            .exists()
    );
    assert!(
        !ws.vault()
            .join(".lorekeeper/queue/results/corrupt")
            .exists()
    );
}

#[test]
fn explicit_regeneration_reuses_retained_input_and_stamps_policy_without_global_invalidation() {
    use lk_queue::TaskRequest;
    for scenario in [
        "processed",
        "historical-policy",
        "pending",
        "pending-historical-policy",
        "wrong-page-format",
        "pending-wrong-page-format",
        "changed-input",
        "missing",
        "ambiguous",
        "missing-input",
    ] {
        let ws = Workspace::new();
        let req = lk_queue::SummarizeRequest {
            text: "Retained complete source text.".into(),
            max_sentences: 5,
            focus: None,
            locale: lk_core::i18n::Locale::En,
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: "daily/source/day.md".into(),
                kind: lk_queue::TargetKind::DailySummary,
                anchor: "## Summary".into(),
            },
        };
        let hash = req.cache_hash();
        let mut input = req.task_input();
        input.insert("locale".into(), "en".into());
        let mut task = serde_json::json!({"task_id":"retained","kind":"summarize","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":req.target});
        if matches!(scenario, "historical-policy" | "pending-historical-policy") {
            task["input"]["generation_policy"] = serde_json::json!("previous-policy-v0");
        }
        if scenario == "missing-input" {
            task["input"].as_object_mut().unwrap().remove("text");
        }
        let page = format!(
            "---\ntype: daily\nllm_inputs:\n  summary: \"{hash}\"\n  summary_done: \"{hash}\"\n---\n## Summary\nPreserved authored summary.\n"
        );
        ws.write("daily/source/day.md", &page);
        let retained = if scenario.starts_with("pending") {
            ".lorekeeper/queue/retained.jsonl"
        } else {
            ".lorekeeper/queue/processed/retained.jsonl"
        };
        if scenario != "missing" {
            ws.write(retained, &task.to_string());
        }
        if scenario == "ambiguous" {
            ws.write(".lorekeeper/queue/duplicate.jsonl", &task.to_string());
        }
        if scenario == "changed-input" {
            ws.write("daily/source/day.md", &page.replace(&hash, "changed"));
        }
        if matches!(scenario, "wrong-page-format" | "pending-wrong-page-format") {
            ws.write(
                "daily/source/day.md",
                &page.replace("type: daily", "type: weekly-review"),
            );
        }
        let before = ws.read("daily/source/day.md");
        let args = ["queue", "regenerate", "--task-id", "retained"];
        let preview = ws.run(&[args.as_slice(), &["--dry-run"]].concat());
        let accepted = matches!(
            scenario,
            "processed" | "pending" | "historical-policy" | "pending-historical-policy"
        );
        assert_eq!(
            preview.status.success(),
            accepted,
            "{scenario}: {}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(ws.read("daily/source/day.md"), before);
        let out = ws.run(&args);
        assert_eq!(
            out.status.success(),
            accepted,
            "{scenario}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if !accepted {
            assert_eq!(ws.read("daily/source/day.md"), before);
            if scenario.starts_with("pending") {
                assert_eq!(ws.read(retained), task.to_string());
            }
            continue;
        }
        let selected: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let new_id = selected["data"]["task_id"].as_str().unwrap();
        assert!(
            ws.read("daily/source/day.md")
                .contains("Preserved authored summary.")
        );
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        let row = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["task_id"] == new_id)
            .unwrap();
        assert_eq!(row["status"], "current");
        if scenario.starts_with("pending") {
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["task_id"] != "retained")
            );
            assert!(!ws.vault().join(retained).exists());
            let archived = std::fs::read_dir(ws.vault().join(".lorekeeper/queue/processed"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(std::fs::read_to_string(archived).unwrap(), task.to_string());
        }
        let waiting_page = ws.read("daily/source/day.md");
        let repeated = ws.run(&args);
        assert!(!repeated.status.success());
        assert!(String::from_utf8_lossy(&repeated.stderr).contains("regeneration already pending"));
        assert_eq!(ws.read("daily/source/day.md"), waiting_page);
        let result = serde_json::json!({"task_id":new_id,"cache_hash":hash,"target_revision":row["target_revision"],"body":"Newly reviewed summary."});
        ws.write(
            &format!(".lorekeeper/queue/results/{new_id}.section.json"),
            &result.to_string(),
        );
        let applied = ws.run(&["queue", "apply"]);
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let after = ws.read("daily/source/day.md");
        assert!(
            after.contains("summary_policy_done: \"summary-v1\"")
                || after.contains("summary_policy_done: summary-v1")
        );
        assert!(after.contains("Newly reviewed summary."));
        let completed_retry = ws.run(&args);
        assert!(!completed_retry.status.success());
        assert!(String::from_utf8_lossy(&completed_retry.stderr).contains("lore queue prune"));
        assert_eq!(ws.read("daily/source/day.md"), after);
        assert!(ws.run(&["queue", "prune"]).status.success());
        assert!(ws.run(&["queue", "check"]).status.success());
        assert!(
            ws.run(&args).status.success(),
            "completed work may be deliberately regenerated again"
        );
    }
}

#[test]
fn pending_regeneration_preserves_exact_siblings_and_rejects_corrupt_runs() {
    for corruption in [
        None,
        Some(b"{truncated".as_slice()),
        Some(b"\xff".as_slice()),
    ] {
        let ws = Workspace::new();
        let input = serde_json::json!({"text":"Retained source.","max_sentences":3,"locale":"en"});
        let hash = lk_queue::cache_hash(input.as_object().unwrap());
        let page = format!(
            "---\ntype: daily\nllm_inputs:\n  summary: {hash}\n---\n## Summary\nAuthored body.\n"
        );
        ws.write("daily/source/old.md", &page);
        ws.write("daily/source/sibling.md", &page);
        let mut old = serde_json::json!({"task_id":"retained","kind":"summarize","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":{"vault_path":"daily/source/old.md","kind":"daily-summary","anchor":"## Summary"},"historical_note":"preserve exact bytes"});
        let mut sibling = old.clone();
        sibling["task_id"] = "sibling".into();
        sibling["target"]["vault_path"] = "daily/source/sibling.md".into();
        old["input"]["generation_policy"] = "previous-policy-v0".into();
        let sibling_lines = format!("\t{} \r\n\r\n", sibling);
        let old_line = old.to_string();
        let mut original = format!("{sibling_lines}{old_line}").into_bytes();
        if let Some(corruption) = corruption {
            original.push(b'\n');
            original.extend_from_slice(corruption);
        }
        let pending = ".lorekeeper/queue/retained.jsonl";
        ws.write(pending, "");
        std::fs::write(ws.vault().join(pending), &original).unwrap();
        let preview = ws.run(&["queue", "regenerate", "--task-id", "retained", "--dry-run"]);
        assert_eq!(
            preview.status.success(),
            corruption.is_none(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(std::fs::read(ws.vault().join(pending)).unwrap(), original);
        assert!(!ws.vault().join(".lorekeeper/queue/processed").exists());
        let output = ws.run(&["queue", "regenerate", "--task-id", "retained"]);
        assert_eq!(
            output.status.success(),
            corruption.is_none(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if corruption.is_some() {
            assert_eq!(std::fs::read(ws.vault().join(pending)).unwrap(), original);
            assert_eq!(ws.read("daily/source/old.md"), page);
            assert!(!ws.vault().join(".lorekeeper/queue/processed").exists());
        } else {
            assert_eq!(ws.read(pending), sibling_lines);
            let archived = std::fs::read_dir(ws.vault().join(".lorekeeper/queue/processed"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(std::fs::read_to_string(archived).unwrap(), old_line);
            let status = work_json(&ws, &["queue", "status", "--json"]);
            assert_eq!(status["data"]["current"], 2);
            assert_eq!(status["data"]["unreadable"], 0);
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|task| task["task_id"] == "sibling")
            );
        }
    }
}

#[test]
fn regeneration_replaces_pending_attempts_without_admitting_a_matching_ancestor_again() {
    let ws = Workspace::new();
    let input = serde_json::json!({"text":"Retained source.","max_sentences":3,"locale":"en"});
    let hash = lk_queue::cache_hash(input.as_object().unwrap());
    let page = format!(
        "---\ntype: daily\nllm_inputs:\n  summary: {hash}\n---\n## Summary\nAuthored body.\n"
    );
    ws.write("daily/source/page.md", &page);
    let task = serde_json::json!({"task_id":"ancestor","kind":"summarize","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":{"vault_path":"daily/source/page.md","kind":"daily-summary","anchor":"## Summary"}});
    ws.write(
        ".lorekeeper/queue/processed/original.jsonl",
        &task.to_string(),
    );
    let first = work_json(&ws, &["queue", "regenerate", "--task-id", "ancestor"]);
    let first_id = first["data"]["task_id"].as_str().unwrap();
    let replacement = work_json(&ws, &["queue", "regenerate", "--task-id", first_id]);
    let replacement_id = replacement["data"]["task_id"].as_str().unwrap();
    let before = ws.read("daily/source/page.md");
    let repeated = ws.run(&["queue", "regenerate", "--task-id", "ancestor"]);
    assert!(!repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("regeneration already pending"));
    assert_eq!(ws.read("daily/source/page.md"), before);
    let status = work_json(&ws, &["queue", "status", "--json"]);
    assert_eq!(status["data"]["current"], 1);
    assert_eq!(status["data"]["unreadable"], 0);
    assert_eq!(status["data"]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(status["data"]["tasks"][0]["task_id"], replacement_id);
}

#[test]
fn regeneration_of_answered_synthesis_still_checks_current_source_revisions() {
    use lk_queue::TaskRequest;
    for changed in [false, true] {
        let ws = Workspace::new();
        let source = "---\ntype: document\n---\nOriginal evidence.\n";
        ws.write("wiki/documents/source.md", source);
        let citation = lk_core::concept::Citation::new(
            Path::new("wiki/documents/source.md"),
            &lk_core::frontmatter::parse_page(source).unwrap(),
        );
        let req = lk_queue::ConceptSynthesisRequest {
            citations: vec![citation],
            locale: lk_core::i18n::Locale::En,
            related_anchor: Some("## Related".into()),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/concepts/example.md".into(),
                kind: lk_queue::TargetKind::ConceptSynthesis,
                anchor: "## Synthesis".into(),
            },
        };
        let hash = req.cache_hash();
        let mut input = req.task_input();
        input.insert("locale".into(), "en".into());
        let task = serde_json::json!({"task_id":"retained-synthesis","kind":"synthesize-concept","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":req.target});
        ws.write(
            ".lorekeeper/queue/processed/retained.jsonl",
            &task.to_string(),
        );
        let page = format!(
            "---\ntype: concept\nllm_inputs:\n  synthesis: \"{hash}\"\n  synthesis_done: \"{hash}\"\n---\n## Synthesis\nPreserve authored knowledge.\n\n## Related\n\n"
        );
        ws.write("wiki/concepts/example.md", &page);
        if changed {
            ws.write(
                "wiki/documents/source.md",
                &source.replace("Original", "Changed"),
            );
        }
        let result = ws.run(&["queue", "regenerate", "--task-id", "retained-synthesis"]);
        assert_eq!(
            result.status.success(),
            !changed,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            ws.read("wiki/concepts/example.md")
                .contains("Preserve authored knowledge.")
        );
        if changed {
            assert_eq!(ws.read("wiki/concepts/example.md"), page);
        }
    }
}

#[cfg(unix)]
#[test]
fn staged_section_links_must_resolve_inside_the_vault() {
    for external in [false, true] {
        for directory in [false, true] {
            let ws = Workspace::new();
            let outside = tempfile::tempdir().unwrap();
            let evidence = "---\ntype: document\n---\nOriginal evidence.\n";
            ws.write("wiki/documents/evidence.md", evidence);
            std::fs::write(outside.path().join("evidence.md"), evidence).unwrap();
            let owner = if external {
                outside.path().to_owned()
            } else {
                ws.vault().join("wiki/documents")
            };
            let (alias, destination) = if directory {
                ("alias", "../../alias/evidence.md")
            } else {
                ("alias.md", "../../alias.md")
            };
            let source = if directory {
                owner
            } else {
                owner.join("evidence.md")
            };
            std::os::unix::fs::symlink(source, ws.vault().join(alias)).unwrap();
            let request = lk_queue::SummarizeRequest {
                text: "Original evidence".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "daily/source/page.md".into(),
                    kind: lk_queue::TargetKind::DailySummary,
                    anchor: "## Summary".into(),
                },
            };
            let task = lk_queue::QueueLlmClient::new(ws.vault().join(".lorekeeper/queue"))
                .build_task("fixture", lk_queue::TaskKind::Summarize, request);
            let page = format!(
                "---\ntype: daily\nllm_inputs:\n  summary: {}\n---\n## Summary\nOriginal.\n",
                task.cache_hash
            );
            ws.write(&task.target.vault_path, &page);
            let pending_path = ".lorekeeper/queue/tasks.jsonl";
            let pending = serde_json::to_string(&task).unwrap();
            ws.write(pending_path, &pending);
            let body = format!("A [linked document]({destination}).");
            let result = lk_queue::SectionResult {
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: lk_core::concept::file_revision(&page),
                body: body.clone(),
            };
            let result_path = ".lorekeeper/queue/results/links.section.json";
            let result = serde_json::to_string(&result).unwrap();
            ws.write(result_path, &result);

            for args in [vec!["queue", "apply", "--dry-run"], vec!["queue", "apply"]] {
                let output = ws.run(&args);
                assert_eq!(
                    output.status.success(),
                    !external,
                    "external={external}, directory={directory}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if external || args.contains(&"--dry-run") {
                    assert_eq!(ws.read(&task.target.vault_path), page);
                    assert_eq!(ws.read(pending_path), pending);
                    assert_eq!(ws.read(result_path), result);
                } else {
                    let published = ws.read(&task.target.vault_path);
                    assert!(published.contains(&body));
                    let page = lk_vault::parse_page(&published).unwrap();
                    assert_eq!(
                        page.frontmatter.get("llm_inputs").unwrap()["summary_done"],
                        task.cache_hash
                    );
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn a_section_target_outside_the_vault_is_unreadable_and_never_published() {
    let ws = Workspace::new();
    let external = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(external.path(), ws.vault().join("alias")).unwrap();
    let req = lk_queue::SummarizeRequest {
        text: "Original evidence".into(),
        max_sentences: 3,
        focus: None,
        locale: lk_core::i18n::Locale::En,
        source_type: None,
        target: lk_queue::TaskTarget {
            vault_path: "alias/page.md".into(),
            kind: lk_queue::TargetKind::DailySummary,
            anchor: "## Summary".into(),
        },
    };
    let hash = req.cache_hash();
    let page =
        format!("---\ntype: daily\nllm_inputs:\n  summary: {hash}\n---\n## Summary\nOriginal.\n");
    std::fs::write(external.path().join("page.md"), &page).unwrap();
    let task = serde_json::json!({"task_id":"outside", "kind":"summarize", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash, "input":req.task_input(), "target":req.target});
    ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
    let status: serde_json::Value =
        serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
    assert_eq!(status["data"]["tasks"][0]["status"], "unreadable");
    assert!(status["data"]["tasks"][0]["target_revision"].is_null());
    let result = serde_json::json!({"task_id":"outside", "cache_hash":hash, "target_revision":lk_core::concept::file_revision(&page), "body":"Overwrite outside."});
    let path = ".lorekeeper/queue/results/outside.section.json";
    ws.write(path, &result.to_string());
    assert!(!ws.run(&["queue", "apply", "--dry-run"]).status.success());
    assert!(!ws.run(&["queue", "apply"]).status.success());
    assert_eq!(
        std::fs::read_to_string(external.path().join("page.md")).unwrap(),
        page
    );
    assert!(ws.vault().join(path).exists());
    let pruned = ws.run(&["queue", "prune"]);
    assert!(!pruned.status.success());
    assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
}

#[test]
fn source_questions_can_publish_but_processing_blockers_cannot_bypass_explicit_work() {
    for kind in ["source_questions", "unresolved"] {
        let ws = Workspace::new();
        bounded_config(&ws);
        let source_path = "wiki/documents/source.md";
        let source = "---\ntype: document\n---\nPolicy drafts disagree on retention; the approving authority is not recorded.\n";
        ws.write(source_path, source);
        let citation = lk_core::concept::Citation::new(
            Path::new(source_path),
            &lk_vault::parse_page(source).unwrap(),
        );
        let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
        let target = "wiki/concepts/topic.md";
        let body = format!(
            "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\n\n## Related\n"
        );
        ws.write(target, &body);
        let task = serde_json::json!({"task_id":"syn-question", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
            "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":target,"kind":"concept-synthesis","anchor":"## Synthesis"}});
        ws.write(".lorekeeper/queue/syn-question.jsonl", &task.to_string());
        let planned = work_json(&ws, &["work", "plan", "--task-id", "syn-question"]);
        assert_eq!(planned["parts"], 1);
        let work_id = planned["work_id"].as_str().unwrap();
        let progress = work_json(&ws, &["work", "status", work_id]);
        let node_id = progress["ready"][0].as_str().unwrap();
        let input = work_json(&ws, &["work", "read", work_id, node_id]);
        let mut observation = serde_json::json!({"work_id":work_id, "node_id":node_id, "input_revision":input["input_revision"],
            "summary":"The exact source leaves the approving authority unknown.", "findings":[], "source_questions":[], "unresolved":[], "resolutions":[]});
        observation[kind] = serde_json::json!([{"id":"authority", "question":"Who approves the policy?", "evidence":input["input"]["ranges"].as_array().unwrap().iter().map(|range| range["range"].clone()).collect::<Vec<_>>()}]);
        let observation_file = ws.vault().join(".lorekeeper/observation.json");
        ws.write(".lorekeeper/observation.json", &observation.to_string());
        work_json(
            &ws,
            &[
                "work",
                "record",
                "--file",
                observation_file.to_str().unwrap(),
            ],
        );
        let result = serde_json::json!({"task_id":"syn-question", "cache_hash":hash,"context_revision":planned["context_revision"],
            "target_revision":blake3::hash(body.as_bytes()).to_hex().to_string(), "synthesis":"The approving authority is unknown; the policy drafts remain unresolved.", "related":""});
        let final_file = ws.vault().join(".lorekeeper/final.json");
        ws.write(".lorekeeper/final.json", &result.to_string());
        let finished = ws.run(&[
            "work",
            "finish",
            work_id,
            "--file",
            final_file.to_str().unwrap(),
        ]);
        if kind == "source_questions" {
            assert!(
                finished.status.success(),
                "{}",
                String::from_utf8_lossy(&finished.stderr)
            );
            let applied = ws.run(&["queue", "apply"]);
            assert!(
                applied.status.success(),
                "{}",
                String::from_utf8_lossy(&applied.stderr)
            );
            assert!(ws.read(target).contains("approving authority is unknown"));
        } else {
            assert!(!finished.status.success());
            assert!(
                String::from_utf8_lossy(&finished.stderr)
                    .contains("unresolved work concerns remain")
            );
            ws.write(
                ".lorekeeper/queue/results/blocked.synthesis.json",
                &result.to_string(),
            );
            let applied = ws.run(&["queue", "apply"]);
            assert!(!applied.status.success());
            assert_eq!(ws.read(target), body);
        }
    }
}
