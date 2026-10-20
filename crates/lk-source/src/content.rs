use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use lk_core::content::{
    InputFormat, InputLocation, InputProcessingConfig, InputProjection, OcrConfig,
    PROJECTION_POLICY, ProjectionMethod, UnitRepresentation,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub const REQUIREMENTS: &str = include_str!("content/requirements.txt");
const PROCESSOR: &str = include_str!("content/processor.py");
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ProcessingError {
    #[error("binary document processing is not configured (ingest.documents)")]
    NotConfigured,
    #[error("document processor IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("document processor timed out")]
    Timeout,
    #[error("document processor exited without a valid result")]
    Exited,
    #[error("document processor response is invalid")]
    Protocol,
    #[error("document processing: {0}")]
    Failed(ProcessingFailure),
}

#[derive(Clone, Copy, Debug, Deserialize, Error)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessingFailure {
    #[error("input exceeds 32 MiB")]
    InputLimit,
    #[error("decoded content exceeds configured limits")]
    DecodedLimit,
    #[error("document exceeds the configured unit limit")]
    UnitLimit,
    #[error("image bytes do not match the selected format")]
    FormatMismatch,
    #[error("OCR model data changed during projection")]
    OcrDataChanged,
    #[error("pinned decoder dependencies are missing")]
    Dependency,
    #[error("decoder dependency versions differ from the embedded lock")]
    DependencyVersion,
    #[error("OCR is not configured")]
    OcrNotConfigured,
    #[error("configured OCR traineddata is missing")]
    OcrDataMissing,
    #[error("no text was extracted; retain the original for inspection")]
    NoText,
    #[error("unsupported input format")]
    UnsupportedFormat,
    #[error("unsupported content encountered; the original was retained")]
    UnsupportedContent,
    #[error("decoder returned an invalid projection")]
    InvalidProjection,
    #[error("decoder could not read the input; the original was retained")]
    Decoder,
}

#[derive(Serialize)]
struct Options<'a> {
    check: bool,
    format: InputFormat,
    max_input_bytes: usize,
    max_decoded_bytes: usize,
    max_units: usize,
    timeout_seconds: u64,
    ocr: Option<OcrOptions<'a>>,
}

#[derive(Serialize)]
struct OcrOptions<'a> {
    tessdata_dir: &'a std::path::Path,
    languages: &'a [String],
    layout: lk_core::content::OcrLayout,
    pdf: bool,
}

impl<'a> From<&'a OcrConfig> for OcrOptions<'a> {
    fn from(value: &'a OcrConfig) -> Self {
        Self {
            tessdata_dir: &value.tessdata_dir,
            languages: &value.languages,
            layout: value.layout,
            pdf: value.pdf,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
enum Response {
    Projected { projection: InputProjection },
    Failed { code: ProcessingFailure },
    Ready { runtime: ProcessingRuntime },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessingRuntime {
    pub packages: BTreeMap<String, String>,
    pub formats: Vec<InputFormat>,
    pub ocr_models: BTreeMap<String, String>,
}

pub async fn check(config: &InputProcessingConfig) -> Result<ProcessingRuntime, ProcessingError> {
    match run_processor(&[], InputFormat::Pdf, config, true).await? {
        Response::Ready { runtime } => Ok(runtime),
        Response::Failed { code } => Err(ProcessingError::Failed(code)),
        Response::Projected { .. } => Err(ProcessingError::Protocol),
    }
}

/// Converts captured bytes without writing source, vault or cache files. The selected
/// interpreter is a trusted local installation, never a document-provided executable.
pub async fn project(
    bytes: &[u8],
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
) -> Result<InputProjection, ProcessingError> {
    if !format.requires_processor() {
        return Err(ProcessingError::Failed(
            ProcessingFailure::UnsupportedFormat,
        ));
    }
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ProcessingError::Failed(ProcessingFailure::InputLimit));
    }
    let config = config.ok_or(ProcessingError::NotConfigured)?;
    match run_processor(bytes, format, config, false).await? {
        Response::Failed { code } => Err(ProcessingError::Failed(code)),
        Response::Projected { projection } => {
            validate_projection(&projection, format, config)?;
            Ok(projection)
        }
        Response::Ready { .. } => Err(ProcessingError::Protocol),
    }
}

async fn run_processor(
    bytes: &[u8],
    format: InputFormat,
    config: &InputProcessingConfig,
    check: bool,
) -> Result<Response, ProcessingError> {
    let options = Options {
        check,
        format,
        max_input_bytes: MAX_INPUT_BYTES,
        max_decoded_bytes: config.max_decoded_bytes.get(),
        max_units: config.max_units.get(),
        timeout_seconds: config.timeout_seconds.get(),
        ocr: config.ocr.as_ref().map(OcrOptions::from),
    };
    let mut command = tokio::process::Command::new(&config.python);
    command.env_clear();
    for name in ["SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("PYTHONIOENCODING", "utf-8")
        .args(["-I", "-X", "utf8", "-c", PROCESSOR])
        .arg(serde_json::to_string(&options).map_err(|_| ProcessingError::Protocol)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or(ProcessingError::Protocol)?;
    let stdout = child.stdout.take().ok_or(ProcessingError::Protocol)?;
    let stderr = child.stderr.take().ok_or(ProcessingError::Protocol)?;
    let send = async move {
        stdin.write_all(bytes).await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let operation = async {
        tokio::try_join!(
            send,
            read_limited(stdout, config.max_output_bytes.get()),
            drain(stderr),
            child.wait()
        )
    };
    let result =
        tokio::time::timeout(Duration::from_secs(config.timeout_seconds.get()), operation).await;
    let ((), output, (), status) = match result {
        Ok(Ok(value)) => value,
        failure => {
            let _ = child.kill().await;
            return Err(match failure {
                Err(_) => ProcessingError::Timeout,
                Ok(Err(error)) => ProcessingError::Io(error),
                Ok(Ok(_)) => unreachable!(),
            });
        }
    };
    if !status.success() {
        return Err(ProcessingError::Exited);
    }
    serde_json::from_slice::<Response>(&output).map_err(|_| ProcessingError::Protocol)
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            return Ok(output);
        }
        if size > limit.saturating_sub(output.len()) {
            return Err(std::io::Error::other(
                "document processor output exceeds configured limit",
            ));
        }
        output.extend_from_slice(&buffer[..size]);
    }
}

async fn drain(mut reader: impl AsyncRead + Unpin) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    while reader.read(&mut buffer).await? != 0 {}
    Ok(())
}

fn validate_projection(
    value: &InputProjection,
    format: InputFormat,
    config: &InputProcessingConfig,
) -> Result<(), ProcessingError> {
    let expected: BTreeMap<_, _> = [
        ("kreuzberg".into(), "4.10.4".into()),
        ("openpyxl".into(), "3.1.5".into()),
        ("defusedxml".into(), "0.7.1".into()),
        ("et-xmlfile".into(), "2.0.0".into()),
        ("pillow".into(), "13.0.0".into()),
    ]
    .into();
    if value.policy != PROJECTION_POLICY || value.format != format || value.packages != expected {
        return Err(ProcessingError::Protocol);
    }
    let uses_ocr = format.is_image()
        || (format == InputFormat::Pdf && config.ocr.as_ref().is_some_and(|ocr| ocr.pdf));
    match (&value.ocr, config.ocr.as_ref()) {
        (Some(projection), Some(configured)) if uses_ocr => {
            if projection.languages != configured.languages
                || projection.layout != configured.layout
                || projection.models.len() != configured.languages.len()
                || configured.languages.iter().any(|language| {
                    !projection.models.get(language).is_some_and(|digest| {
                        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                })
            {
                return Err(ProcessingError::Protocol);
            }
        }
        (None, _) if !uses_ocr => {}
        _ => return Err(ProcessingError::Protocol),
    }
    let mut bytes = 0usize;
    let mut locations = std::collections::BTreeSet::new();
    let mut frames = Vec::new();
    let mut native_pages = Vec::new();
    let mut ocr_pages = Vec::new();
    let mut sheets = std::collections::BTreeSet::new();
    for unit in &value.units {
        bytes = bytes
            .checked_add(unit.text.len())
            .ok_or(ProcessingError::Protocol)?;
        let valid = match (&unit.location, unit.method, unit.representation) {
            (InputLocation::Document, ProjectionMethod::Native, UnitRepresentation::Markdown) => {
                format == InputFormat::Docx
            }
            (
                InputLocation::Sheet { name, .. },
                ProjectionMethod::Native,
                UnitRepresentation::JsonLines,
            ) => format == InputFormat::Xlsx && !name.is_empty(),
            (InputLocation::Page { .. }, method, UnitRepresentation::Markdown) => {
                format == InputFormat::Pdf
                    && (method == ProjectionMethod::Native
                        || config.ocr.as_ref().is_some_and(|ocr| ocr.pdf))
            }
            (
                InputLocation::ImageFrame { .. },
                ProjectionMethod::Ocr,
                UnitRepresentation::Markdown,
            ) => format.is_image() && config.ocr.is_some(),
            _ => false,
        };
        let key = serde_json::to_string(&(&unit.location, unit.method))
            .map_err(|_| ProcessingError::Protocol)?;
        if !valid || !locations.insert(key) {
            return Err(ProcessingError::Protocol);
        }
        match &unit.location {
            InputLocation::ImageFrame { number } => frames.push(number.get()),
            InputLocation::Page { number } if unit.method == ProjectionMethod::Native => {
                native_pages.push(number.get())
            }
            InputLocation::Page { number } => ocr_pages.push(number.get()),
            InputLocation::Sheet { name, .. } if !sheets.insert(name) => {
                return Err(ProcessingError::Protocol);
            }
            InputLocation::Document | InputLocation::Sheet { .. } => {}
        }
    }
    if frames.iter().copied().ne(1..=frames.len())
        || native_pages.iter().copied().ne(1..=native_pages.len())
        || (format == InputFormat::Pdf && uses_ocr && ocr_pages != native_pages)
    {
        return Err(ProcessingError::Protocol);
    }
    if value.units.len() > config.max_units.get()
        || bytes > config.max_decoded_bytes.get()
        || !value.units.iter().any(|unit| !unit.text.trim().is_empty())
    {
        return Err(ProcessingError::Protocol);
    }
    Ok(())
}

impl From<ProcessingError> for crate::SourceError {
    fn from(value: ProcessingError) -> Self {
        Self::Parse(value.to_string())
    }
}

/// Retains document structure while preventing original-relative links from being
/// mistaken for canonical wiki relationships.
pub fn render_markdown(value: &InputProjection) -> Result<String, crate::SourceError> {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};
    let mut output = String::new();
    for unit in &value.units {
        let label = serde_json::to_string(&(&unit.location, unit.method))
            .map_err(|error| crate::SourceError::Parse(error.to_string()))?;
        output.push_str(&format!(
            "\n\n{}\n\n",
            crate::fetch::literal_markdown(&label)?
        ));
        if unit.representation != UnitRepresentation::Markdown {
            output.push_str(&crate::fetch::literal_markdown(&unit.text)?);
            continue;
        }
        let mut links = Vec::new();
        let events = Parser::new_ext(
            &unit.text,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
        )
        .flat_map(|event| match event {
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push(dest_url.into_string());
                vec![Event::Start(Tag::Emphasis)]
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                let target = links.pop().unwrap_or_default();
                vec![
                    Event::End(TagEnd::Emphasis),
                    Event::Text(CowStr::from(format!(" ({target})"))),
                ]
            }
            Event::Html(text) | Event::InlineHtml(text) => vec![Event::Text(text)],
            event => vec![event],
        });
        pulldown_cmark_to_cmark::cmark(
            events,
            &mut output,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
        )
        .map_err(|error| crate::SourceError::Parse(error.to_string()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(python: &std::path::Path) -> InputProcessingConfig {
        serde_json::from_value(serde_json::json!({"python": python})).unwrap()
    }

    #[tokio::test]
    async fn refuses_output_overflow_instead_of_truncating_json() {
        let error = read_limited(&b"12345"[..], 4).await.unwrap_err();
        assert!(error.to_string().contains("exceeds configured limit"));
        assert_eq!(read_limited(&b"1234"[..], 4).await.unwrap(), b"1234");
    }

    #[test]
    fn projection_links_remain_original_text_without_wiki_edges() {
        let value = InputProjection {
            policy: PROJECTION_POLICY.into(), format: InputFormat::Docx, packages: BTreeMap::new(), ocr: None, warnings: vec![],
            units: vec![lk_core::content::InputUnit {
                location: InputLocation::Document, method: ProjectionMethod::Native, representation: UnitRepresentation::Markdown,
                text: "# Policy\n\n[local](../concepts/policy.md) ![figure](image.png) <script>source</script>\n\n| Scope | Days |\n| --- | --- |\n| prod | 7 |".into(),
            }],
        };
        let body = render_markdown(&value).unwrap();
        assert!(body.contains("concepts/policy.md"));
        assert!(body.contains("Days"));
        let events: Vec<_> =
            pulldown_cmark::Parser::new_ext(&body, pulldown_cmark::Options::ENABLE_TABLES)
                .collect();
        assert!(!events.iter().any(|event| matches!(
            event,
            pulldown_cmark::Event::Start(
                pulldown_cmark::Tag::Link { .. } | pulldown_cmark::Tag::Image { .. }
            ) | pulldown_cmark::Event::Html(_)
                | pulldown_cmark::Event::InlineHtml(_)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Table(_))
        )));
    }

    #[cfg(unix)]
    fn executable(root: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("processor");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn processor_failures_are_typed_and_diagnostics_are_private() {
        let root = tempfile::tempdir().unwrap();
        let python = executable(
            root.path(),
            "/bin/cat >/dev/null\nprintf '%s' 'private input text' >&2\nprintf '%s' '{\"status\":\"failed\",\"code\":\"no-text\"}'",
        );
        let mut config = configuration(&python);
        config.timeout_seconds = std::num::NonZeroU64::new(1).unwrap();
        let result = project(b"captured bytes", InputFormat::Pdf, Some(&config)).await;
        assert!(matches!(
            result,
            Err(ProcessingError::Failed(ProcessingFailure::NoText))
        ));
        assert!(project(b"", InputFormat::Pdf, None).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn processor_inherits_workflow_group_and_timeout_reaps_it() {
        let root = tempfile::tempdir().unwrap();
        let python = executable(
            root.path(),
            "printf '%s' \"$$\" > \"$0.pid\"\nexec /bin/sleep 60",
        );
        let pid_path = python.with_extension("pid");
        let mut config = configuration(&python);
        config.timeout_seconds = std::num::NonZeroU64::new(2).unwrap();
        let task = tokio::spawn(async move { project(b"", InputFormat::Pdf, Some(&config)).await });
        let pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&pid_path)
                    && let Ok(value) = value.parse::<i32>()
                    && let Some(pid) = rustix::process::Pid::from_raw(value)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let group = rustix::process::getpgid(Some(pid)).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ProcessingError::Timeout)));
        assert_eq!(group, rustix::process::getpgrp());
        assert_eq!(
            rustix::process::getpgid(Some(pid)),
            Err(rustix::io::Errno::SRCH)
        );
    }

    #[tokio::test]
    #[ignore = "requires the pinned optional document runtime; set LORE_TEST_DOCUMENT_PYTHON and LORE_TEST_TESSDATA"]
    async fn binary_formats_preserve_scope_and_formula_evidence() {
        let python =
            std::env::var_os("LORE_TEST_DOCUMENT_PYTHON").expect("LORE_TEST_DOCUMENT_PYTHON");
        let mut config = configuration(std::path::Path::new(&python));
        let cases = [
            (
                InputFormat::Docx,
                include_bytes!("content/fixtures/policy.docx").as_slice(),
            ),
            (
                InputFormat::Xlsx,
                include_bytes!("content/fixtures/policy.xlsx").as_slice(),
            ),
            (
                InputFormat::Pdf,
                include_bytes!("content/fixtures/policy.pdf").as_slice(),
            ),
        ];
        for (format, bytes) in cases {
            let value = project(bytes, format, Some(&config)).await.unwrap();
            match format {
                InputFormat::Pdf => {
                    assert_eq!(value.units.len(), 2);
                    assert!(value.units[0].text.contains("30 days"));
                    assert!(value.units[1].text.contains("7 days"));
                }
                InputFormat::Xlsx => {
                    assert_eq!(value.units.len(), 2);
                    assert!(value.units[0].text.contains("\"formula\": \"=B1+1\""));
                    assert!(value.units[0].text.contains("\"cached_value\": 31"));
                    assert!(value.units[1].text.contains("\"cached_value\": 8"));
                }
                InputFormat::Docx => assert!(value.units[0].text.contains("Production")),
                _ => unreachable!(),
            }
        }
        assert!(matches!(
            project(b"not a pdf", InputFormat::Pdf, Some(&config)).await,
            Err(ProcessingError::Failed(ProcessingFailure::Decoder))
        ));
        config.max_decoded_bytes = std::num::NonZeroUsize::new(10).unwrap();
        assert!(matches!(
            project(
                include_bytes!("content/fixtures/policy.xlsx"),
                InputFormat::Xlsx,
                Some(&config)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit))
        ));
        config.max_decoded_bytes = std::num::NonZeroUsize::new(128 * 1024 * 1024).unwrap();
        config.ocr = Some(serde_json::from_value(serde_json::json!({"tessdata_dir": std::path::PathBuf::from(std::env::var_os("LORE_TEST_TESSDATA").expect("LORE_TEST_TESSDATA")), "languages": ["kor", "eng"]})).unwrap());
        let image = project(
            include_bytes!("content/fixtures/policy.png"),
            InputFormat::Png,
            Some(&config),
        )
        .await
        .unwrap();
        assert_eq!(image.units[0].method, ProjectionMethod::Ocr);
        assert!(image.units[0].text.contains("30 days"));
        config.ocr.as_mut().unwrap().layout = lk_core::content::OcrLayout::SingleBlock;
        let korean = project(
            include_bytes!("content/fixtures/policy-ko.png"),
            InputFormat::Png,
            Some(&config),
        )
        .await
        .unwrap();
        assert!(korean.units[0].text.contains("운영 환경"));
        assert!(korean.units[0].text.contains("30 일"));
        let temporary = tempfile::tempdir().unwrap();
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/content/fixtures");
        let generated = std::process::Command::new(&config.python)
            .args([
                "-I",
                "-c",
                r#"
from PIL import Image
import pathlib, sys
source, target = map(pathlib.Path, sys.argv[1:])
first = Image.open(source/'policy.png').convert('RGB')
second = Image.open(source/'policy-ko.png').convert('RGB')
first.save(target/'pages.tiff', save_all=True, append_images=[second], compression='tiff_deflate')
first.save(target/'page.jpeg', quality=95)
first.save(target/'page.webp', lossless=True)
"#,
            ])
            .arg(fixtures)
            .arg(temporary.path())
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        for (name, format, count) in [
            ("pages.tiff", InputFormat::Tiff, 2),
            ("page.jpeg", InputFormat::Jpeg, 1),
            ("page.webp", InputFormat::Webp, 1),
        ] {
            let bytes = std::fs::read(temporary.path().join(name)).unwrap();
            let projection = project(&bytes, format, Some(&config)).await.unwrap();
            assert_eq!(projection.units.len(), count);
            assert!(projection.units[0].text.contains("30 days"));
            if count == 2 {
                assert!(projection.units[1].text.contains("운영 환경"));
                let mut omitted = serde_json::from_value::<InputProjection>(
                    serde_json::to_value(&projection).unwrap(),
                )
                .unwrap();
                omitted.units.remove(0);
                assert!(validate_projection(&omitted, format, &config).is_err());
            }
        }
        let tiff = std::fs::read(temporary.path().join("pages.tiff")).unwrap();
        config.max_units = std::num::NonZeroUsize::new(1).unwrap();
        assert!(matches!(
            project(&tiff, InputFormat::Tiff, Some(&config)).await,
            Err(ProcessingError::Failed(ProcessingFailure::UnitLimit))
        ));
        config.max_units = std::num::NonZeroUsize::new(10000).unwrap();
        config.max_decoded_bytes = std::num::NonZeroUsize::new(1024).unwrap();
        assert!(matches!(
            project(&tiff, InputFormat::Tiff, Some(&config)).await,
            Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit))
        ));
        config.max_decoded_bytes = std::num::NonZeroUsize::new(128 * 1024 * 1024).unwrap();
        assert!(matches!(
            project(&tiff, InputFormat::Png, Some(&config)).await,
            Err(ProcessingError::Failed(ProcessingFailure::FormatMismatch))
        ));
        config.ocr.as_mut().unwrap().pdf = true;
        let pdf = project(
            include_bytes!("content/fixtures/policy.pdf"),
            InputFormat::Pdf,
            Some(&config),
        )
        .await
        .unwrap();
        assert_eq!(pdf.units.len(), 4);
        assert!(pdf.units[2].text.contains("30 days"));
        assert!(pdf.units[3].text.contains("7 days"));
    }
}
