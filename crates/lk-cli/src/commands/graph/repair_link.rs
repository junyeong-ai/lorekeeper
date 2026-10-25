use std::path::{Component, Path};

use lk_core::link;
use lk_vault::{MutationPlan, MutationSession};

use super::ResolvedConfig;

pub(super) struct Request<'a> {
    pub origin: &'a Path,
    pub from: &'a Path,
    pub to: &'a Path,
    pub revision: &'a str,
    pub to_revision: &'a str,
}

fn concept(root: &Path, path: &Path, revision: &str) -> Result<String, String> {
    if lk_vault::resolve_page_owner(root, path).map_err(|error| error.to_string())? != path {
        return Err("link repair requires the exact canonical page address, not an alias".into());
    }
    let raw = std::fs::read_to_string(
        lk_vault::page_path(root, path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if lk_core::concept::file_revision(&raw) != revision {
        return Err(
            "link repair file revision changed; read and reconsider the exact mapping".into(),
        );
    }
    let page = lk_vault::parse_page(&raw).map_err(|error| error.to_string())?;
    if !page.frontmatter.is_declared_concept() {
        return Err("link repair requires declared concept pages".into());
    }
    if !lk_core::rss_evidence::bindings(&page)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err("typed RSS evidence requires its provenance-aware repair path".into());
    }
    Ok(raw)
}

fn prepare(
    config: &ResolvedConfig,
    request: &Request<'_>,
) -> Result<(MutationPlan, serde_json::Value), String> {
    let Request {
        origin,
        from,
        to,
        revision,
        to_revision,
    } = request;
    for path in [origin, from, to] {
        if path.to_str().is_none()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !lk_graph::scan::is_concept_page(path, &config.vault_dirs)
        {
            return Err("link repair requires exact vault-relative concept .md addresses".into());
        }
    }
    let before = concept(&config.root, origin, revision)?;
    let target = concept(&config.root, to, to_revision)?;
    let missing = lk_vault::page_path(&config.root, from).map_err(|error| error.to_string())?;
    match std::fs::symlink_metadata(missing) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
        Ok(_) => return Err(
            "the replaced address exists; link repair cannot redirect a valid or occupied address"
                .into(),
        ),
    }
    let offset =
        lk_core::frontmatter::body_offset(&before).ok_or("cannot repair unclosed frontmatter")?;
    let destination = link::relative_dest(origin, to);
    let mut replacements = 0;
    let body = link::rewrite_links_outside_code(&before[offset..], |inline| {
        let (dest, anchor) = link::split_dest_anchor(inline.destination);
        if link::is_external(dest)
            || link::resolve_dest(origin, &link::decode_dest(dest)).as_deref() != Some(*from)
        {
            return None;
        }
        replacements += 1;
        Some(inline.render_with_destination(&format!("{destination}{anchor}")))
    });
    if replacements == 0 {
        return Err(
            "no supported inline link resolves to the explicitly selected missing address".into(),
        );
    }
    let after = format!("{}{body}", &before[..offset]);
    let report = serde_json::json!({"origin":origin,"from":from,"to":to,"replacements":replacements,
        "before_revision":lk_core::concept::file_revision(&before),"after_revision":lk_core::concept::file_revision(&after),
        "to_revision":to_revision});
    let mut plan = MutationPlan::default();
    plan.edit(origin.to_path_buf(), before, after);
    if origin != to {
        plan.require_unchanged(to.to_path_buf(), target);
    }
    plan.require_absent(from.to_path_buf());
    Ok((plan, report))
}

pub(super) fn request(
    config: &ResolvedConfig,
    request: Request<'_>,
    dry_run: bool,
    session: Option<&MutationSession>,
) -> Result<serde_json::Value, String> {
    if dry_run
        && config
            .root
            .join(".lorekeeper/mutation/pending.json")
            .try_exists()
            .map_err(|error| error.to_string())?
    {
        return Err("pending mutation must be recovered before link repair preview".into());
    }
    let (plan, report) = prepare(config, &request)?;
    if dry_run {
        plan.validate_addresses(&config.root)
            .map_err(|error| error.to_string())?;
    } else {
        session
            .ok_or("link repair requires a vault mutation session")?
            .apply(plan)
            .map_err(|error| error.to_string())?;
    }
    Ok(serde_json::json!({"ok":true,"data":report,"dry_run":dry_run}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "wiki/concepts/stripe.md";
    const FROM: &str = "wiki/concepts/wiki/concepts/openrouter.md";
    const TO: &str = "wiki/concepts/openrouter.md";

    struct Fixture {
        _directory: tempfile::TempDir,
        config: ResolvedConfig,
        before: String,
        target: String,
        revision: String,
        to_revision: String,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
            let before = "\u{feff}---\r\ntype: concept\r\ntitle: Stripe\r\nllm_inputs: {synthesis: paid, synthesis_done: paid}\r\n---\r\n# Stripe\r\n\r\nAuthored prose.\r\n\r\n## Related\r\n[OpenRouter](wiki/concepts/openrouter.md)\r\n\r\n`[OpenRouter](wiki/concepts/openrouter.md)`\r\n![OpenRouter](wiki/concepts/openrouter.md)\r\n\r\n```md\r\n[OpenRouter](wiki/concepts/openrouter.md)\r\n```\r\n".to_owned();
            let target =
                "---\ntype: concept\ntitle: OpenRouter\n---\n# OpenRouter\n\nExisting owner.\n"
                    .to_owned();
            std::fs::write(root.join(ORIGIN), &before).unwrap();
            std::fs::write(root.join(TO), &target).unwrap();
            Self {
                revision: lk_core::concept::file_revision(&before),
                to_revision: lk_core::concept::file_revision(&target),
                before,
                target,
                config: ResolvedConfig {
                    root,
                    pin: crate::commands::ConfigurationPin::Absent,
                    graph: Default::default(),
                    locale: lk_core::i18n::Locale::En,
                    vault_dirs: Default::default(),
                    concept_categories: vec![],
                    llm: Default::default(),
                },
                _directory: directory,
            }
        }

        fn request(&self) -> Request<'_> {
            Request {
                origin: Path::new(ORIGIN),
                from: Path::new(FROM),
                to: Path::new(TO),
                revision: &self.revision,
                to_revision: &self.to_revision,
            }
        }
    }

    #[test]
    fn preview_and_publication_preserve_prose_markers_and_nonlink_regions() {
        let fixture = Fixture::new();
        let root = &fixture.config.root;
        let expected = fixture.before.replacen(
            "[OpenRouter](wiki/concepts/openrouter.md)",
            "[OpenRouter](openrouter.md)",
            1,
        );
        let preview = request(&fixture.config, fixture.request(), true, None).unwrap();
        assert_eq!(preview["data"]["replacements"], 1);
        assert_eq!(
            std::fs::read_to_string(root.join(ORIGIN)).unwrap(),
            fixture.before
        );
        assert!(!root.join(".lorekeeper").exists());
        std::fs::create_dir_all(root.join(".lorekeeper/work/retained")).unwrap();
        std::fs::write(
            root.join(".lorekeeper/work/retained/plan.json"),
            "Preserved paid state.",
        )
        .unwrap();
        let session = MutationSession::acquire(root).unwrap();
        let actual = request(&fixture.config, fixture.request(), false, Some(&session)).unwrap();
        assert_eq!(actual["data"], preview["data"]);
        assert_eq!(
            std::fs::read_to_string(root.join(ORIGIN)).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read_to_string(root.join(TO)).unwrap(),
            fixture.target
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".lorekeeper/work/retained/plan.json")).unwrap(),
            "Preserved paid state."
        );
        assert!(request(&fixture.config, fixture.request(), false, Some(&session)).is_err());
    }

    #[test]
    fn stale_revisions_occupied_addresses_and_nonconcept_inputs_refuse() {
        for scenario in [
            "origin",
            "target",
            "occupied",
            "format",
            "missing-link",
            "outside",
            "traversal",
            "alias",
        ] {
            let fixture = Fixture::new();
            let root = &fixture.config.root;
            let mut selected = fixture.request();
            let document = "---\ntype: document\n---\nExisting document.\n";
            let document_revision = lk_core::concept::file_revision(document);
            match scenario {
                "origin" => selected.revision = "old",
                "target" => selected.to_revision = "old",
                "occupied" => {
                    std::fs::create_dir_all(root.join(FROM).parent().unwrap()).unwrap();
                    std::fs::write(root.join(FROM), "Occupied.").unwrap();
                }
                "format" => {
                    std::fs::write(root.join(TO), document).unwrap();
                    selected.to_revision = &document_revision;
                }
                "missing-link" => selected.from = Path::new("wiki/concepts/another-missing.md"),
                "outside" => selected.origin = Path::new(".lorekeeper/originals/page.md"),
                "traversal" => selected.from = Path::new("wiki/concepts/../../outside.md"),
                "alias" => {
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(
                            "openrouter.md",
                            root.join("wiki/concepts/alias.md"),
                        )
                        .unwrap();
                        selected.to = Path::new("wiki/concepts/alias.md");
                    }
                    #[cfg(not(unix))]
                    {
                        continue;
                    }
                }
                _ => unreachable!(),
            }
            assert!(
                request(&fixture.config, selected, true, None).is_err(),
                "{scenario}"
            );
            assert_eq!(
                std::fs::read_to_string(root.join(ORIGIN)).unwrap(),
                fixture.before
            );
            assert!(!root.join(".lorekeeper").exists());
        }
    }

    #[test]
    fn publication_guards_target_and_missing_address_after_preparation() {
        for changed_target in [false, true] {
            let fixture = Fixture::new();
            let root = &fixture.config.root;
            let session = MutationSession::acquire(root).unwrap();
            let (plan, _) = prepare(&fixture.config, &fixture.request()).unwrap();
            if changed_target {
                std::fs::write(root.join(TO), "Changed target.").unwrap();
            } else {
                std::fs::create_dir_all(root.join(FROM).parent().unwrap()).unwrap();
                std::fs::write(root.join(FROM), "New owner.").unwrap();
            }
            assert!(session.apply(plan).is_err());
            assert_eq!(
                std::fs::read_to_string(root.join(ORIGIN)).unwrap(),
                fixture.before
            );
        }
    }

    #[test]
    fn pending_journal_refuses_preview_and_guarded_recovery_retains_original_revisions() {
        let fixture = Fixture::new();
        let root = &fixture.config.root;
        let (plan, _) = prepare(&fixture.config, &fixture.request()).unwrap();
        let encoded = serde_json::to_string(&plan).unwrap();
        let plan_revision = lk_core::concept::file_revision(&encoded);
        let plan: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let after = plan["steps"][0]["after"].as_str().unwrap();
        let directory = root.join(".lorekeeper/mutation");
        std::fs::create_dir_all(&directory).unwrap();
        let pending = serde_json::to_vec(&serde_json::json!({"version":1,"plan":plan})).unwrap();
        std::fs::write(directory.join("pending.json"), &pending).unwrap();
        std::fs::write(
            directory.join("progress.json"),
            serde_json::to_vec(
                &serde_json::json!({"plan_revision":plan_revision,"completed":1,
            "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:1"))}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(request(&fixture.config, fixture.request(), true, None).is_err());
        assert_eq!(
            std::fs::read(directory.join("pending.json")).unwrap(),
            pending
        );
        std::fs::write(root.join(ORIGIN), after).unwrap();
        std::fs::write(
            root.join(TO),
            "Changed target after interrupted publication.",
        )
        .unwrap();
        let refused = MutationSession::acquire(root)
            .err()
            .expect("changed target must block recovery");
        assert!(refused.to_string().contains(TO), "{refused}");
        std::fs::write(root.join(TO), &fixture.target).unwrap();
        let _recovered = MutationSession::acquire(root).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(ORIGIN)).unwrap(), after);
        assert!(!directory.join("pending.json").exists());
    }
}
