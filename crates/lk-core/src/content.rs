use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

pub const PROJECTION_POLICY: &str = "document-projection-v4";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, strum::EnumIter)]
#[serde(rename_all = "kebab-case")]
pub enum InputFormat {
    Markdown,
    Text,
    Html,
    Docx,
    Xlsx,
    Pdf,
    Png,
    Jpeg,
    Tiff,
    Webp,
}

impl InputFormat {
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(Self::Markdown),
            "txt" => Some(Self::Text),
            "html" | "htm" => Some(Self::Html),
            "docx" => Some(Self::Docx),
            "xlsx" => Some(Self::Xlsx),
            "pdf" => Some(Self::Pdf),
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "tif" | "tiff" => Some(Self::Tiff),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Markdown => "text/markdown",
            Self::Text => "text/plain",
            Self::Html => "text/html",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Pdf => "application/pdf",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Tiff => "image/tiff",
            Self::Webp => "image/webp",
        }
    }

    pub fn from_mime_type(value: &str) -> Option<Self> {
        use strum::IntoEnumIterator;
        Self::iter().find(|format| format.mime_type().eq_ignore_ascii_case(value))
    }

    pub fn is_image(self) -> bool {
        match self {
            Self::Png | Self::Jpeg | Self::Tiff | Self::Webp => true,
            Self::Markdown | Self::Text | Self::Html | Self::Docx | Self::Xlsx | Self::Pdf => false,
        }
    }

    pub fn requires_processor(self) -> bool {
        match self {
            Self::Markdown | Self::Text | Self::Html => false,
            Self::Docx
            | Self::Xlsx
            | Self::Pdf
            | Self::Png
            | Self::Jpeg
            | Self::Tiff
            | Self::Webp => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputProcessingConfig {
    pub python: PathBuf,
    #[serde(default = "processing_timeout")]
    pub timeout_seconds: NonZeroU64,
    #[serde(default = "output_limit")]
    pub max_output_bytes: NonZeroUsize,
    #[serde(default = "decoded_limit")]
    pub max_decoded_bytes: NonZeroUsize,
    #[serde(default = "raster_limit")]
    pub max_raster_bytes: NonZeroUsize,
    #[serde(default = "unit_limit")]
    pub max_units: NonZeroUsize,
    #[serde(default)]
    pub ocr: Option<OcrConfig>,
    #[serde(default)]
    pub vision: Option<VisionConfig>,
    #[serde(default)]
    pub pdf: Option<PdfProjectionPolicy>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PdfProjectionPolicy {
    Native {},
    Visual {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pages: Option<Vec<NonZeroUsize>>,
    },
}

impl Default for PdfProjectionPolicy {
    fn default() -> Self {
        Self::Native {}
    }
}

impl PdfProjectionPolicy {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Self::Visual { pages: Some(pages) } = self
            && (pages.is_empty() || pages.windows(2).any(|pair| pair[0] >= pair[1]))
        {
            return Err(ConfigError::Validation(
                "PDF visual pages must be a nonempty, strictly increasing list of physical page numbers".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "engine", rename_all = "kebab-case", deny_unknown_fields)]
pub enum VisionConfig {
    PaddleOcrVl {
        python: PathBuf,
        vlm_python: PathBuf,
        models_dir: PathBuf,
    },
}

fn processing_timeout() -> NonZeroU64 {
    NonZeroU64::new(120).unwrap()
}
fn output_limit() -> NonZeroUsize {
    NonZeroUsize::new(32 * 1024 * 1024).unwrap()
}
fn decoded_limit() -> NonZeroUsize {
    NonZeroUsize::new(128 * 1024 * 1024).unwrap()
}
fn raster_limit() -> NonZeroUsize {
    NonZeroUsize::new(64 * 1024 * 1024).unwrap()
}
fn unit_limit() -> NonZeroUsize {
    NonZeroUsize::new(10000).unwrap()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrConfig {
    pub tessdata_dir: PathBuf,
    pub languages: Vec<String>,
    #[serde(default)]
    pub layout: OcrLayout,
    /// Explicitly OCR every PDF page as an additional projection. Native text remains distinct.
    #[serde(default)]
    pub pdf: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OcrLayout {
    #[default]
    Auto,
    SingleBlock,
    SparseText,
}

impl InputProcessingConfig {
    pub fn is_pdf_ocr_enabled(&self) -> bool {
        self.pdf.is_none() && self.ocr.as_ref().is_some_and(|ocr| ocr.pdf)
    }

    pub fn pdf_policy(&self) -> &PdfProjectionPolicy {
        const NATIVE: PdfProjectionPolicy = PdfProjectionPolicy::Native {};
        const VISUAL: PdfProjectionPolicy = PdfProjectionPolicy::Visual { pages: None };
        self.pdf.as_ref().unwrap_or(if self.vision.is_some() {
            &VISUAL
        } else {
            &NATIVE
        })
    }
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let mut paths = vec![("ingest.documents.python", &self.python)];
        self.pdf_policy().validate()?;
        if self.vision.is_some() && self.ocr.is_some() {
            return Err(ConfigError::Validation(
                "select either ingest.documents.vision or ingest.documents.ocr".into(),
            ));
        }
        if matches!(self.pdf_policy(), PdfProjectionPolicy::Visual { .. }) && self.vision.is_none()
        {
            return Err(ConfigError::Validation(
                "PDF visual projection requires ingest.documents.vision".into(),
            ));
        }
        if let Some(VisionConfig::PaddleOcrVl {
            python,
            vlm_python,
            models_dir,
        }) = &self.vision
        {
            paths.push(("ingest.documents.vision.python", python));
            paths.push(("ingest.documents.vision.vlm_python", vlm_python));
            paths.push(("ingest.documents.vision.models_dir", models_dir));
        }
        if let Some(ocr) = &self.ocr {
            paths.push(("ingest.documents.ocr.tessdata_dir", &ocr.tessdata_dir));
            let mut seen = std::collections::BTreeSet::new();
            if ocr.languages.is_empty()
                || ocr.languages.iter().any(|language| {
                    language.is_empty()
                        || !language
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                        || !seen.insert(language)
                })
            {
                return Err(ConfigError::Validation("OCR languages must be distinct nonempty traineddata names containing ASCII letters, digits or underscore; order is preserved".into()));
            }
        }
        for (name, path) in paths {
            let value = path.to_string_lossy();
            if value.trim().is_empty()
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err(ConfigError::Validation(format!(
                    "{name} must be a nonblank path without surrounding whitespace or control characters"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_relative_to(&mut self, parent: &Path) {
        self.python = crate::config::resolve_vault_root(&self.python, parent);
        if let Some(ocr) = &mut self.ocr {
            ocr.tessdata_dir = crate::config::resolve_vault_root(&ocr.tessdata_dir, parent);
        }
        if let Some(VisionConfig::PaddleOcrVl {
            python,
            vlm_python,
            models_dir,
        }) = &mut self.vision
        {
            *python = crate::config::resolve_vault_root(&*python, parent);
            *vlm_python = crate::config::resolve_vault_root(&*vlm_python, parent);
            *models_dir = crate::config::resolve_vault_root(&*models_dir, parent);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InputLocation {
    Document,
    Page { number: NonZeroUsize },
    Sheet { name: String, state: SheetState },
    ImageFrame { number: NonZeroUsize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SheetState {
    Visible,
    Hidden,
    VeryHidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionMethod {
    Native,
    Ocr,
    Visual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitRepresentation {
    Text,
    Markdown,
    JsonLines,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputUnit {
    pub location: InputLocation,
    pub method: ProjectionMethod,
    pub representation: UnitRepresentation,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet_structure: Option<SheetStructure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_structure: Option<VisualStructure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VisualStructure {
    pub width: u32,
    pub height: u32,
    /// Counterclockwise orientation correction; block coordinates use the corrected image.
    pub rotation: u16,
    pub blocks: Vec<VisualBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VisualBlock {
    pub label: String,
    pub bbox: [f64; 4],
    pub order: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SheetStructure {
    pub default_row_hidden: bool,
    pub merged_ranges: Vec<SheetRange>,
    /// Inclusive intervals for declared rows; adjacent equal declarations may coalesce.
    pub row_visibility: Vec<SheetVisibility>,
    /// Inclusive intervals in source order, including overlapping declarations.
    pub column_visibility: Vec<SheetVisibility>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SheetRange {
    pub start_row: u32,
    pub end_row: u32,
    pub start_column: u32,
    pub end_column: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SheetVisibility {
    pub start: u32,
    pub end: u32,
    pub hidden: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputProjection {
    pub policy: String,
    pub format: InputFormat,
    pub packages: BTreeMap<String, String>,
    pub ocr: Option<OcrProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<VisionRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfProjectionPolicy>,
    pub units: Vec<InputUnit>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OcrProjection {
    pub languages: Vec<String>,
    pub layout: OcrLayout,
    pub models: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VisionRuntime {
    pub engine: String,
    pub packages: BTreeMap<String, String>,
    pub models: BTreeMap<String, String>,
    pub model_revisions: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_projection_roundtrips_without_claiming_sheet_structure() {
        let original = serde_json::json!({
            "policy": "document-projection-v1", "format": "xlsx", "packages": {},
            "ocr": null, "warnings": [], "units": [{
                "location": {"kind": "sheet", "name": "Data", "state": "visible"},
                "method": "native", "representation": "json-lines", "text": "retained input"
            }]
        });
        for policy in [
            "document-projection-v1",
            "document-projection-v2",
            "document-projection-v3",
        ] {
            let mut original = original.clone();
            original["policy"] = policy.into();
            if policy != "document-projection-v1" {
                original["units"][0]["sheet_structure"] = serde_json::json!({
                    "default_row_hidden": false, "merged_ranges": [],
                    "row_visibility": [], "column_visibility": []
                });
            }
            let projection: InputProjection = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(
                projection.units[0].sheet_structure.is_some(),
                policy != "document-projection-v1"
            );
            assert_eq!(serde_json::to_value(projection).unwrap(), original);
        }
    }

    #[test]
    fn format_selection_does_not_treat_an_extension_as_a_decoder() {
        for extension in ["DOCX", "xlsx", "PDF", "png", "jpg", "tif", "webp"] {
            let format = InputFormat::from_extension(extension).unwrap();
            assert_eq!(
                InputFormat::from_mime_type(format.mime_type()),
                Some(format)
            );
        }
        for extension in ["doc", "xls", "zip", "exe", "mp4"] {
            assert_eq!(InputFormat::from_extension(extension), None);
        }
    }

    #[test]
    fn processing_configuration_rejects_invalid_paths_limits_and_languages() {
        let valid: InputProcessingConfig = crate::yaml::from_str(
            "python: runtime/bin/python\nocr: {tessdata_dir: models, languages: [kor, eng]}\n",
        )
        .unwrap();
        valid.validate().unwrap();
        assert_eq!(valid.ocr.unwrap().languages, ["kor", "eng"]);
        for fields in [
            "python: ''",
            "python: ' bad '",
            "python: bin/python\nocr: {tessdata_dir: models, languages: []}",
            "python: bin/python\nocr: {tessdata_dir: models, languages: [eng, eng]}",
            "python: bin/python\nocr: {tessdata_dir: models, languages: ['../secret']}",
        ] {
            assert!(
                crate::yaml::from_str::<InputProcessingConfig>(fields)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{fields}"
            );
        }
        for fields in [
            "timeout_seconds: 0",
            "max_output_bytes: 0",
            "max_decoded_bytes: 0",
            "max_raster_bytes: 0",
            "max_units: 0",
            "unknown: true",
        ] {
            assert!(
                crate::yaml::from_str::<InputProcessingConfig>(&format!(
                    "python: bin/python\n{fields}"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_pdf_scope_overrides_legacy_ocr_and_vision_defaults_to_all_pages() {
        let mut config: InputProcessingConfig = crate::yaml::from_str(
            "python: native\nocr: {tessdata_dir: models, languages: [eng], pdf: true}",
        )
        .unwrap();
        assert!(config.is_pdf_ocr_enabled());
        config.pdf = Some(PdfProjectionPolicy::Native {});
        assert!(!config.is_pdf_ocr_enabled());
        let mut config: InputProcessingConfig = crate::yaml::from_str(
            "python: native\nvision: {engine: paddle-ocr-vl, python: layout, vlm_python: vlm, models_dir: models}",
        )
        .unwrap();
        assert_eq!(
            config.pdf_policy(),
            &PdfProjectionPolicy::Visual { pages: None }
        );
        config.pdf = Some(PdfProjectionPolicy::Native {});
        assert_eq!(config.pdf_policy(), &PdfProjectionPolicy::Native {});
    }

    #[test]
    fn vision_configuration_keeps_engine_and_pdf_scope_separate() {
        let yaml = "python: native/bin/python\nvision: {engine: paddle-ocr-vl, python: vision/bin/python, vlm_python: vlm/bin/python, models_dir: models}\npdf: {mode: visual, pages: [1, 3]}\n";
        let mut config: InputProcessingConfig = crate::yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        config.resolve_relative_to(Path::new("/runtime"));
        let Some(VisionConfig::PaddleOcrVl {
            python,
            vlm_python,
            models_dir,
        }) = config.vision
        else {
            panic!("vision config")
        };
        assert_eq!(python, Path::new("/runtime/vision/bin/python"));
        assert_eq!(models_dir, Path::new("/runtime/models"));
        assert_eq!(vlm_python, Path::new("/runtime/vlm/bin/python"));
        for yaml in [
            "python: native\npdf: {mode: visual}",
            "python: native\npdf: {mode: native, pages: [1]}",
            "python: native\nvision: {engine: unknown, python: vision, vlm_python: vlm, models_dir: models}",
            "python: native\nvision: {engine: paddle-ocr-vl, python: ' ', vlm_python: vlm, models_dir: models}",
            "python: native\nvision: {engine: paddle-ocr-vl, python: vision, vlm_python: vlm, models_dir: models, fallback: true}",
            "python: native\nvision: {engine: paddle-ocr-vl, python: vision, vlm_python: vlm, models_dir: models}\nocr: {tessdata_dir: models, languages: [eng]}",
        ] {
            if let Ok(value) = crate::yaml::from_str::<InputProcessingConfig>(yaml) {
                assert!(value.validate().is_err(), "{yaml}");
            }
        }
        for pages in ["[]", "[0]", "[2, 1]", "[1, 1]"] {
            let parsed = crate::yaml::from_str::<PdfProjectionPolicy>(&format!(
                "mode: visual\npages: {pages}"
            ));
            assert!(parsed.is_err() || parsed.unwrap().validate().is_err());
        }
    }
}
