//! Output-affecting processing identity, separate from acquisition admission limits.

use std::collections::BTreeMap;

use lk_core::content::{
    InputFormat, InputProcessingConfig, OcrLayout, PROJECTION_POLICY, PdfProjectionPolicy,
    VisionRuntime,
};
use serde::Serialize;

use super::{PROCESSOR, ProcessingError, ProcessingFailure, ProcessingRuntime, Response};

const RECIPE_POLICY: &str = "input-processing-recipe-v1";

fn digest(value: &impl Serialize) -> Result<String, ProcessingError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ProcessingError::Protocol)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub(super) struct NativeRecipe<'a> {
    format: InputFormat,
    processor_revision: String,
    packages: &'a BTreeMap<String, String>,
    ocr_models: &'a BTreeMap<String, String>,
    ocr: Option<(&'a [String], OcrLayout, bool)>,
}

impl<'a> NativeRecipe<'a> {
    pub(super) fn build(
        format: InputFormat,
        config: &'a InputProcessingConfig,
        runtime: &'a ProcessingRuntime,
    ) -> Self {
        Self {
            format,
            processor_revision: blake3::hash(PROCESSOR.as_bytes()).to_hex().to_string(),
            packages: &runtime.packages,
            ocr_models: &runtime.ocr_models,
            ocr: config
                .ocr
                .as_ref()
                .map(|ocr| (ocr.languages.as_slice(), ocr.layout, ocr.pdf)),
        }
    }

    pub(super) fn cache_identity(
        &self,
        captured: &lk_core::manual_original::OriginalDigest,
    ) -> Result<String, ProcessingError> {
        // Preserve the existing native cache tuple, including the legacy image
        // OCR pdf flag. Receipt meaning below omits that unused image setting.
        digest(&(
            captured,
            self.format,
            PROJECTION_POLICY,
            &self.processor_revision,
            self.packages,
            self.ocr_models,
            self.ocr,
        ))
    }

    fn meaning(&self) -> impl Serialize + '_ {
        (
            self.format,
            PROJECTION_POLICY,
            &self.processor_revision,
            self.packages,
            self.ocr_models,
            self.ocr.map(|(languages, layout, _)| (languages, layout)),
        )
    }
}

fn rust_artifacts(format: InputFormat) -> String {
    let mut hash = blake3::Hasher::new();
    for (name, version) in PARSER_IDENTITIES {
        hash.update(name.as_bytes());
        hash.update(&[0]);
        hash.update(version.as_bytes());
        hash.update(&[0]);
    }
    for source in [
        include_str!("../../../lk-core/src/markdown.rs"),
        include_str!("../../../lk-core/src/link.rs"),
    ] {
        hash.update(blake3::hash(source.as_bytes()).as_bytes());
    }
    match format {
        InputFormat::Markdown => {}
        InputFormat::Text => {
            hash.update(blake3::hash(include_str!("../fetch.rs").as_bytes()).as_bytes());
        }
        InputFormat::Html => {
            hash.update(blake3::hash(include_str!("../fetch.rs").as_bytes()).as_bytes());
            hash.update(blake3::hash(include_str!("../markdown.rs").as_bytes()).as_bytes());
        }
        _ => {
            hash.update(blake3::hash(include_str!("../content.rs").as_bytes()).as_bytes());
        }
    }
    hash.finalize().to_hex().to_string()
}

// Dependency updates must advance their meaning identity; the owner test binds
// this list to the resolved lock without hashing workspace release stamps.
const PARSER_IDENTITIES: &[(&str, &str)] = &[
    ("dom_smoothie", "0.18.2"),
    ("encoding_rs", "0.8.42"),
    ("htmd", "0.5.6"),
    ("html5ever", "0.38.0"),
    ("html5ever", "0.39.0"),
    ("markup5ever", "0.38.0"),
    ("markup5ever", "0.39.0"),
    ("markup5ever_rcdom", "0.38.0+unofficial"),
    ("minijinja", "3.0.2"),
    ("percent-encoding", "2.3.2"),
    ("pulldown-cmark", "0.13.4"),
    ("pulldown-cmark-to-cmark", "23.0.0"),
    ("serde", "1.0.229"),
    ("serde-saphyr", "2.0.0"),
    ("serde_json", "1.0.154"),
    ("unicode-normalization", "0.1.26"),
    ("url", "2.5.8"),
];

#[derive(Serialize)]
#[serde(tag = "mode", content = "policy", rename_all = "kebab-case")]
enum PdfMeaning<'a> {
    LiteralOcr,
    Projection(&'a PdfProjectionPolicy),
}

fn build_recipe(
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
    native: Option<&ProcessingRuntime>,
    vision: Option<&VisionRuntime>,
) -> Result<String, ProcessingError> {
    let effective_native = config.map(|config| super::native_config(format, config));
    let native = match (effective_native.as_ref(), native) {
        (Some(config), Some(runtime)) => Some(NativeRecipe::build(format, config, runtime)),
        _ => None,
    };
    let pdf = config.filter(|_| format == InputFormat::Pdf).map(|config| {
        if config.is_pdf_ocr_enabled() {
            PdfMeaning::LiteralOcr
        } else {
            PdfMeaning::Projection(config.pdf_policy())
        }
    });
    digest(&(
        RECIPE_POLICY,
        format,
        rust_artifacts(format),
        native.as_ref().map(NativeRecipe::meaning),
        vision,
        vision.map(|_| {
            blake3::hash(include_str!("vision.rs").as_bytes())
                .to_hex()
                .to_string()
        }),
        pdf,
    ))
}

/// Identify the effective parser, decoder and model recipe without decoding input.
///
/// Text formats use embedded artifacts and start no process. Binary formats run
/// the same bounded dependency/model checks as projection, without inference or
/// installation. Paths, resource ceilings and timeouts do not enter the identity;
/// checks still obey the current limits. Recheck around acquisition rather than
/// retaining this observation across independent processing sessions.
pub async fn processing_recipe(
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
) -> Result<String, ProcessingError> {
    if !format.requires_processor() {
        return build_recipe(format, None, None, None);
    }
    let config = config.ok_or(ProcessingError::NotConfigured)?;
    let deadline = super::processing_deadline(config.timeout_seconds.get())?;
    let uses_vision = super::uses_vision(format, config);
    if format.is_image() && !uses_vision && config.ocr.is_none() {
        return Err(ProcessingError::Failed(ProcessingFailure::OcrNotConfigured));
    }
    let effective_native = super::native_config(format, config);
    let native = if format.is_image() && uses_vision {
        None
    } else {
        Some(
            match super::run_processor(None, format, &effective_native, true, deadline).await? {
                Response::Ready { runtime } => runtime,
                Response::Failed { code } => return Err(ProcessingError::Failed(code)),
                Response::Projected { .. } => return Err(ProcessingError::Protocol),
            },
        )
    };
    let vision = if uses_vision {
        Some(
            super::vision::check(
                config.vision.as_ref().ok_or(ProcessingError::Protocol)?,
                config,
                deadline,
            )
            .await?,
        )
    } else {
        None
    };
    let recipe = build_recipe(format, Some(config), native.as_ref(), vision.as_ref())?;
    super::ensure_deadline(deadline)?;
    Ok(recipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_identities_match_resolved_external_packages_without_workspace_stamps() {
        let packages = include_str!("../../../../Cargo.lock")
            .split("[[package]]")
            .skip(1)
            .map(|package| {
                let field = |key| package.lines().find_map(|line| line.strip_prefix(key));
                (field("name = ").unwrap(), field("version = ").unwrap())
            })
            .collect::<Vec<_>>();
        for (name, version) in PARSER_IDENTITIES {
            let quoted_name = format!("{name:?}");
            let quoted_version = format!("{version:?}");
            assert!(packages.contains(&(quoted_name.as_str(), quoted_version.as_str())));
        }
        for (name, version) in packages {
            if PARSER_IDENTITIES
                .iter()
                .any(|(known, _)| name == format!("{known:?}"))
            {
                assert!(PARSER_IDENTITIES.iter().any(|(known, resolved)| {
                    name == format!("{known:?}") && version == format!("{resolved:?}")
                }));
            }
        }
    }

    fn configuration() -> InputProcessingConfig {
        serde_json::from_value(serde_json::json!({"python":"/missing/runtime"})).unwrap()
    }

    fn native_runtime() -> ProcessingRuntime {
        ProcessingRuntime {
            packages: BTreeMap::from([("fixture".into(), "1".into())]),
            formats: vec![InputFormat::Docx, InputFormat::Xlsx, InputFormat::Pdf],
            ocr_models: BTreeMap::new(),
            vision: None,
        }
    }

    fn with_ocr(config: &mut InputProcessingConfig) {
        config.ocr = Some(
            serde_json::from_value(serde_json::json!({
                "tessdata_dir":"/missing/models", "languages":["eng"], "pdf":true
            }))
            .unwrap(),
        );
    }

    #[tokio::test]
    async fn text_recipes_need_no_runtime_and_bind_the_declared_format() {
        let mut config = configuration();
        with_ocr(&mut config);
        let mut recipes = std::collections::BTreeSet::new();
        for format in [InputFormat::Markdown, InputFormat::Text, InputFormat::Html] {
            let recipe = processing_recipe(format, None).await.unwrap();
            assert_eq!(
                recipe,
                processing_recipe(format, Some(&config)).await.unwrap()
            );
            assert!(recipes.insert(recipe));
        }
    }

    #[test]
    fn native_cache_identity_keeps_the_exact_existing_tuple() {
        let mut config = configuration();
        with_ocr(&mut config);
        let mut runtime = native_runtime();
        runtime.ocr_models.insert("eng".into(), "a".repeat(64));
        let captured = lk_core::manual_original::OriginalDigest::of(b"original");
        let legacy = digest(&(
            &captured,
            InputFormat::Png,
            PROJECTION_POLICY,
            blake3::hash(PROCESSOR.as_bytes()).to_hex().as_str(),
            &runtime.packages,
            &runtime.ocr_models,
            config
                .ocr
                .as_ref()
                .map(|ocr| (&ocr.languages, ocr.layout, ocr.pdf)),
        ))
        .unwrap();
        let recipe = NativeRecipe::build(InputFormat::Png, &config, &runtime);
        assert_eq!(recipe.cache_identity(&captured).unwrap(), legacy);
    }

    #[test]
    fn native_meaning_binds_processor_packages_models_and_effective_ocr() {
        let mut config = configuration();
        with_ocr(&mut config);
        let mut runtime = native_runtime();
        runtime.ocr_models.insert("eng".into(), "a".repeat(64));
        let baseline = build_recipe(InputFormat::Png, Some(&config), Some(&runtime), None).unwrap();
        let mut changed = configuration();
        with_ocr(&mut changed);
        changed.ocr.as_mut().unwrap().pdf = false;
        assert_eq!(
            baseline,
            build_recipe(InputFormat::Png, Some(&changed), Some(&runtime), None).unwrap()
        );
        changed.ocr.as_mut().unwrap().layout = OcrLayout::SparseText;
        assert_ne!(
            baseline,
            build_recipe(InputFormat::Png, Some(&changed), Some(&runtime), None).unwrap()
        );
        for models in [false, true] {
            let mut changed = native_runtime();
            changed.ocr_models.clone_from(&runtime.ocr_models);
            if models {
                changed.ocr_models.insert("eng".into(), "b".repeat(64));
            } else {
                changed.packages.insert("fixture".into(), "2".into());
            }
            assert_ne!(
                baseline,
                build_recipe(InputFormat::Png, Some(&config), Some(&changed), None).unwrap()
            );
        }
        let mut recipe = NativeRecipe::build(InputFormat::Png, &config, &runtime);
        let before = digest(&recipe.meaning()).unwrap();
        recipe.processor_revision = "different-script".into();
        assert_ne!(before, digest(&recipe.meaning()).unwrap());
    }

    #[test]
    fn pdf_scope_is_meaningful_only_for_pdf_and_unused_engines_are_excluded() {
        let config = configuration();
        let runtime = native_runtime();
        let mut changed = config.clone();
        with_ocr(&mut changed);
        for format in [InputFormat::Docx, InputFormat::Xlsx] {
            assert_eq!(
                build_recipe(format, Some(&config), Some(&runtime), None).unwrap(),
                build_recipe(format, Some(&changed), Some(&runtime), None).unwrap()
            );
        }
        assert_ne!(
            build_recipe(InputFormat::Pdf, Some(&config), Some(&runtime), None).unwrap(),
            build_recipe(InputFormat::Pdf, Some(&changed), Some(&runtime), None).unwrap()
        );
    }

    #[cfg(unix)]
    fn executable(
        root: &std::path::Path,
        name: &str,
        response: serde_json::Value,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        let response = response.to_string().replace('\'', "'\"'\"'");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\ncase \"$6\" in *'\"check\":true'*|*'\"operation\":\"check\"'*) ;; *) exit 91;; esac\nprintf '%s\\n' '{response}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_checks_observe_current_constraints_without_hashing_caps_or_paths() {
        let root = tempfile::tempdir().unwrap();
        let response = serde_json::json!({"status":"ready","runtime":native_runtime()});
        let mut config = configuration();
        config.python = executable(root.path(), "first", response.clone());
        let baseline = processing_recipe(InputFormat::Docx, Some(&config))
            .await
            .unwrap();
        config.python = executable(root.path(), "second", response);
        config.timeout_seconds = std::num::NonZeroU64::new(60).unwrap();
        config.max_output_bytes = std::num::NonZeroUsize::new(1024).unwrap();
        config.max_decoded_bytes = std::num::NonZeroUsize::new(1).unwrap();
        config.max_raster_bytes = std::num::NonZeroUsize::new(1).unwrap();
        config.max_units = std::num::NonZeroUsize::new(1).unwrap();
        with_ocr(&mut config);
        assert_eq!(
            baseline,
            processing_recipe(InputFormat::Docx, Some(&config))
                .await
                .unwrap()
        );
        config.max_output_bytes = std::num::NonZeroUsize::new(1).unwrap();
        assert!(matches!(
            processing_recipe(InputFormat::Docx, Some(&config)).await,
            Err(ProcessingError::Io(_))
        ));
        config.python = executable(
            root.path(),
            "failed",
            serde_json::json!({"status":"failed","code":"dependency-version"}),
        );
        config.max_output_bytes = std::num::NonZeroUsize::new(1024).unwrap();
        assert!(matches!(
            processing_recipe(InputFormat::Docx, Some(&config)).await,
            Err(ProcessingError::Failed(
                ProcessingFailure::DependencyVersion
            ))
        ));
    }

    #[cfg(unix)]
    fn vision_runtime(model: &str) -> serde_json::Value {
        serde_json::json!({"engine":"paddle-ocr-vl", "packages":{"fixture":"1"},
            "model_revisions":{"vlm":"fixture/vlm@1","layout":"fixture/layout@1","orientation":"fixture/orientation@1"},
            "models":{"vlm":model.repeat(64),"layout":"b".repeat(64),"orientation":"c".repeat(64),"recipe":"d".repeat(64)}})
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vision_checks_bind_actual_models_and_pdf_scope_without_native_image_checks() {
        let root = tempfile::tempdir().unwrap();
        let mut config = configuration();
        let response = serde_json::json!({"status":"ready","runtime":vision_runtime("a")});
        config.vision = Some(
            serde_json::from_value(serde_json::json!({"engine":"paddle-ocr-vl",
            "python":executable(root.path(), "layout", response.clone()),
            "vlm_python":executable(root.path(), "recognition", response),
            "models_dir":root.path().join("models")}))
            .unwrap(),
        );
        let image = processing_recipe(InputFormat::Png, Some(&config))
            .await
            .unwrap();
        let same = serde_json::json!({"status":"ready","runtime":vision_runtime("a")});
        config.vision = Some(
            serde_json::from_value(serde_json::json!({"engine":"paddle-ocr-vl",
            "python":executable(root.path(), "other-layout", same.clone()),
            "vlm_python":executable(root.path(), "other-recognition", same),
            "models_dir":root.path().join("other-models")}))
            .unwrap(),
        );
        config.timeout_seconds = std::num::NonZeroU64::new(30).unwrap();
        config.max_output_bytes = std::num::NonZeroUsize::new(2048).unwrap();
        config.max_decoded_bytes = std::num::NonZeroUsize::new(1).unwrap();
        config.max_raster_bytes = std::num::NonZeroUsize::new(1).unwrap();
        config.max_units = std::num::NonZeroUsize::new(1).unwrap();
        assert_eq!(
            image,
            processing_recipe(InputFormat::Png, Some(&config))
                .await
                .unwrap()
        );
        config.pdf =
            Some(serde_json::from_value(serde_json::json!({"mode":"visual","pages":[1]})).unwrap());
        assert_eq!(
            image,
            processing_recipe(InputFormat::Png, Some(&config))
                .await
                .unwrap()
        );
        config.python = executable(
            root.path(),
            "native",
            serde_json::json!({"status":"ready","runtime":native_runtime()}),
        );
        let selected = processing_recipe(InputFormat::Pdf, Some(&config))
            .await
            .unwrap();
        config.pdf = None;
        assert_ne!(
            selected,
            processing_recipe(InputFormat::Pdf, Some(&config))
                .await
                .unwrap()
        );
        let changed = serde_json::json!({"status":"ready","runtime":vision_runtime("f")});
        executable(root.path(), "other-layout", changed.clone());
        executable(root.path(), "other-recognition", changed);
        assert_ne!(
            image,
            processing_recipe(InputFormat::Png, Some(&config))
                .await
                .unwrap()
        );
        config.pdf = Some(PdfProjectionPolicy::Native {});
        let native = processing_recipe(InputFormat::Pdf, Some(&config))
            .await
            .unwrap();
        config.vision = None;
        assert_eq!(
            native,
            processing_recipe(InputFormat::Pdf, Some(&config))
                .await
                .unwrap()
        );
    }
}
