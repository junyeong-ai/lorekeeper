use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

pub const PROJECTION_POLICY: &str = "document-projection-v1";

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
    #[serde(default)]
    pub ocr: Option<OcrConfig>,
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
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let mut paths = vec![("ingest.documents.python", &self.python)];
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
        self.python = crate::config::resolve_vault_root(&self.python.to_string_lossy(), parent);
        if let Some(ocr) = &mut self.ocr {
            ocr.tessdata_dir =
                crate::config::resolve_vault_root(&ocr.tessdata_dir.to_string_lossy(), parent);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InputLocation {
    Document,
    Page { number: NonZeroUsize },
    Sheet { name: String, state: SheetState },
    Image,
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
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputProjection {
    pub policy: String,
    pub format: InputFormat,
    pub packages: BTreeMap<String, String>,
    pub ocr: Option<OcrProjection>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
