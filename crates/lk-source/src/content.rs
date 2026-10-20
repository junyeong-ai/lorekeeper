use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use lk_core::content::{
    InputFormat, InputLocation, InputProcessingConfig, InputProjection, OcrConfig,
    PROJECTION_POLICY, ProjectionMethod, SheetStructure, UnitRepresentation,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

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
    #[error("input exceeds the configured capture limit")]
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
    input_path: Option<&'a std::path::Path>,
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
    match run_processor(None, InputFormat::Pdf, config, true).await? {
        Response::Ready { runtime } => Ok(runtime),
        Response::Failed { code } => Err(ProcessingError::Failed(code)),
        Response::Projected { .. } => Err(ProcessingError::Protocol),
    }
}

/// Converts bounded bytes through a private captured file. The selected
/// interpreter is a trusted local installation, never a document-provided executable.
pub async fn project(
    bytes: &[u8],
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
) -> Result<InputProjection, ProcessingError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ProcessingError::Failed(ProcessingFailure::InputLimit));
    }
    let captured = lk_core::captured_file::CapturedFile::capture(
        std::io::Cursor::new(bytes),
        MAX_INPUT_BYTES,
    )?;
    project_file(&captured, format, config).await
}

/// Convert the exact owned acquisition without retaining or piping the full raw file.
pub async fn project_file(
    captured: &lk_core::captured_file::CapturedFile,
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
) -> Result<InputProjection, ProcessingError> {
    if !format.requires_processor() {
        return Err(ProcessingError::Failed(
            ProcessingFailure::UnsupportedFormat,
        ));
    }
    let config = config.ok_or(ProcessingError::NotConfigured)?;
    captured.verify()?;
    let response = run_processor(Some(captured), format, config, false).await?;
    captured.verify()?;
    match response {
        Response::Failed { code } => Err(ProcessingError::Failed(code)),
        Response::Projected { projection } => {
            validate_projection(&projection, format, config)?;
            Ok(projection)
        }
        Response::Ready { .. } => Err(ProcessingError::Protocol),
    }
}

async fn run_processor(
    captured: Option<&lk_core::captured_file::CapturedFile>,
    format: InputFormat,
    config: &InputProcessingConfig,
    check: bool,
) -> Result<Response, ProcessingError> {
    let options = Options {
        check,
        format,
        max_input_bytes: captured.map_or(0, |file| file.byte_len()),
        input_path: captured.map(|file| file.path()),
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().ok_or(ProcessingError::Protocol)?;
    let stderr = child.stderr.take().ok_or(ProcessingError::Protocol)?;
    let operation = async {
        tokio::try_join!(
            read_limited(stdout, config.max_output_bytes.get()),
            drain(stderr),
            child.wait()
        )
    };
    let result =
        tokio::time::timeout(Duration::from_secs(config.timeout_seconds.get()), operation).await;
    let (output, (), status) = match result {
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
        match (&unit.location, &unit.sheet_structure) {
            (InputLocation::Sheet { .. }, Some(structure)) => {
                validate_sheet_structure(structure)?;
                bytes = bytes
                    .checked_add(
                        serde_json::to_vec(structure)
                            .map_err(|_| ProcessingError::Protocol)?
                            .len(),
                    )
                    .ok_or(ProcessingError::Protocol)?;
            }
            (InputLocation::Sheet { .. }, None) | (_, Some(_)) => {
                return Err(ProcessingError::Protocol);
            }
            (_, None) => {}
        }
        let valid = match (&unit.location, unit.method, unit.representation) {
            (InputLocation::Document, ProjectionMethod::Native, UnitRepresentation::Markdown) => {
                format == InputFormat::Docx
            }
            (
                InputLocation::Sheet { name, .. },
                ProjectionMethod::Native,
                UnitRepresentation::JsonLines,
            ) => format == InputFormat::Xlsx && !name.is_empty(),
            (
                InputLocation::Page { .. },
                ProjectionMethod::Native,
                UnitRepresentation::Markdown,
            ) => format == InputFormat::Pdf,
            (InputLocation::Page { .. }, ProjectionMethod::Ocr, UnitRepresentation::Text) => {
                format == InputFormat::Pdf && config.ocr.as_ref().is_some_and(|ocr| ocr.pdf)
            }
            (InputLocation::ImageFrame { .. }, ProjectionMethod::Ocr, UnitRepresentation::Text) => {
                format.is_image() && config.ocr.is_some()
            }
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

fn validate_sheet_structure(value: &SheetStructure) -> Result<(), ProcessingError> {
    const MAX_ROWS: u32 = 1_048_576;
    const MAX_COLUMNS: u32 = 16_384;
    let is_range = |start, end, maximum| 1 <= start && start <= end && end <= maximum;
    if value.merged_ranges.iter().any(|range| {
        !is_range(range.start_row, range.end_row, MAX_ROWS)
            || !is_range(range.start_column, range.end_column, MAX_COLUMNS)
    }) || value
        .row_visibility
        .iter()
        .any(|range| !is_range(range.start, range.end, MAX_ROWS))
        || value
            .row_visibility
            .windows(2)
            .any(|pair| pair[0].end >= pair[1].start)
        || value
            .column_visibility
            .iter()
            .any(|range| !is_range(range.start, range.end, MAX_COLUMNS))
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
        if let Some(structure) = &unit.sheet_structure {
            let metadata = serde_json::to_string(&serde_json::json!({
                "sheet_structure": structure,
            }))
            .map_err(|error| crate::SourceError::Parse(error.to_string()))?;
            output.push_str(&crate::fetch::literal_markdown(&metadata)?);
            output.push_str("\n\n");
        }
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
    fn sheet_structure_validates_coordinates_and_keeps_ordered_column_declarations() {
        let valid = serde_json::json!({
            "default_row_hidden": true,
            "merged_ranges": [{"start_row": 1, "end_row": 1048576, "start_column": 1, "end_column": 16384}],
            "row_visibility": [{"start": 1, "end": 2, "hidden": false}, {"start": 4, "end": 4, "hidden": true}],
            "column_visibility": [{"start": 2, "end": 4, "hidden": true}, {"start": 2, "end": 2, "hidden": false}]
        });
        let structure: SheetStructure = serde_json::from_value(valid.clone()).unwrap();
        validate_sheet_structure(&structure).unwrap();
        for (pointer, value) in [
            ("/merged_ranges/0/start_row", 0),
            ("/merged_ranges/0/end_row", 1048577),
            ("/merged_ranges/0/end_column", 16385),
            ("/row_visibility/1/start", 2),
            ("/column_visibility/0/end", 1),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = serde_json::json!(value);
            let structure = serde_json::from_value(invalid).unwrap();
            assert!(validate_sheet_structure(&structure).is_err(), "{pointer}");
        }
    }

    #[tokio::test]
    #[ignore = "requires the pinned optional document runtime; set LORE_TEST_DOCUMENT_PYTHON"]
    async fn spreadsheet_structure_preserves_hidden_values_and_sparse_geometry() {
        let python =
            std::env::var_os("LORE_TEST_DOCUMENT_PYTHON").expect("LORE_TEST_DOCUMENT_PYTHON");
        let config = configuration(std::path::Path::new(&python));
        let temporary = tempfile::tempdir().unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/content/fixtures/policy.xlsx");
        let generated = std::process::Command::new(&config.python)
            .args([
                "-I",
                "-c",
                r#"
import pathlib, sys, zipfile, xml.etree.ElementTree as E
source, output = map(pathlib.Path, sys.argv[1:])
ns = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
def tag(name): return '{' + ns + '}' + name
with zipfile.ZipFile(source) as archive:
    members = {name: archive.read(name) for name in archive.namelist()}
sheet = E.fromstring(members['xl/worksheets/sheet1.xml'])
sheet.insert(0, E.Element(tag('dimension'), ref='A1:A1'))
sheet.insert(1, E.Element(tag('sheetFormatPr'), zeroHeight='1', defaultRowHeight='15'))
columns = E.Element(tag('cols'))
E.SubElement(columns, tag('col'), min='2', max='4', hidden='1')
E.SubElement(columns, tag('col'), min='2', max='2', hidden='0')
sheet.insert(2, columns)
sheet_data = sheet.find(tag('sheetData'))
sheet_data[0].set('hidden', '1')
sheet_data[1].set('hidden', '0')
row = E.SubElement(sheet_data, tag('row'), r='1048576')
cell = E.SubElement(row, tag('c'), r='XFD1048576', t='inlineStr')
E.SubElement(E.SubElement(cell, tag('is')), tag('t')).text = 'sparse end'
merged = E.SubElement(sheet, tag('mergeCells'), count='1')
E.SubElement(merged, tag('mergeCell'), ref='C3:XFD1048575')
members['xl/worksheets/sheet1.xml'] = E.tostring(sheet)
workbook = E.fromstring(members['xl/workbook.xml'])
workbook.find(tag('sheets'))[1].set('state', 'veryHidden')
members['xl/workbook.xml'] = E.tostring(workbook)
relations = E.fromstring(members['xl/_rels/workbook.xml.rels'])
relations[0].set('Target', 'worksheets/nested/data.xml')
members['xl/_rels/workbook.xml.rels'] = E.tostring(relations)
members['xl/worksheets/nested/data.xml'] = members.pop('xl/worksheets/sheet1.xml')
def save(name, contents):
    with zipfile.ZipFile(output / name, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for path, data in contents.items(): archive.writestr(path, data)
save('structured.xlsx', members)
invalid = dict(members)
invalid.pop('xl/worksheets/nested/data.xml')
save('missing-sheet.xlsx', invalid)
invalid = dict(members)
broken = E.fromstring(members['xl/worksheets/nested/data.xml'])
broken.find(tag('cols'))[0].set('max', '16385')
invalid['xl/worksheets/nested/data.xml'] = E.tostring(broken)
save('invalid-range.xlsx', invalid)
invalid = dict(members)
relations[0].set('TargetMode', 'External')
invalid['xl/_rels/workbook.xml.rels'] = E.tostring(relations)
save('external-sheet.xlsx', invalid)
"#,
            ])
            .arg(&fixture)
            .arg(temporary.path())
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let bytes = std::fs::read(temporary.path().join("structured.xlsx")).unwrap();
        let value = project(&bytes, InputFormat::Xlsx, Some(&config))
            .await
            .unwrap();
        assert_eq!(value.policy, PROJECTION_POLICY);
        assert_eq!(value.units.len(), 2);
        let first = &value.units[0];
        assert_eq!(first.text.lines().count(), 5);
        assert!(first.text.contains("\"value\": 30"));
        assert!(first.text.contains("\"formula\": \"=B1+1\""));
        assert!(first.text.contains("\"cached_value\": 31"));
        assert!(first.text.contains("XFD1048576"));
        let structure = first.sheet_structure.as_ref().unwrap();
        assert!(structure.default_row_hidden);
        assert_eq!(structure.merged_ranges[0].end_row, 1048575);
        assert_eq!(structure.merged_ranges[0].end_column, 16384);
        assert!(structure.row_visibility[0].hidden);
        assert!(!structure.row_visibility[1].hidden);
        assert_eq!(structure.row_visibility[2].start, 1048576);
        assert_eq!(structure.column_visibility.len(), 2);
        assert!(structure.column_visibility[0].hidden);
        assert!(!structure.column_visibility[1].hidden);
        assert!(matches!(
            value.units[1].location,
            InputLocation::Sheet {
                state: lk_core::content::SheetState::VeryHidden,
                ..
            }
        ));
        let rendered = render_markdown(&value).unwrap();
        assert!(rendered.contains("sheet_structure"));
        assert!(rendered.contains("merged_ranges"));
        assert!(rendered.contains("sparse end"));
        let mut bounded = config.clone();
        bounded.max_decoded_bytes =
            std::num::NonZeroUsize::new(value.units.iter().map(|unit| unit.text.len()).sum())
                .unwrap();
        assert!(validate_projection(&value, InputFormat::Xlsx, &bounded).is_err());
        let plain = project(
            &std::fs::read(&fixture).unwrap(),
            InputFormat::Xlsx,
            Some(&config),
        )
        .await
        .unwrap();
        assert_ne!(first.sheet_structure, plain.units[0].sheet_structure);
        for name in [
            "missing-sheet.xlsx",
            "invalid-range.xlsx",
            "external-sheet.xlsx",
        ] {
            let invalid = std::fs::read(temporary.path().join(name)).unwrap();
            assert!(
                project(&invalid, InputFormat::Xlsx, Some(&config))
                    .await
                    .is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn projection_links_remain_original_text_without_wiki_edges() {
        let value = InputProjection {
            policy: PROJECTION_POLICY.into(), format: InputFormat::Docx, packages: BTreeMap::new(), ocr: None, warnings: vec![],
            units: vec![lk_core::content::InputUnit {
                location: InputLocation::Document, method: ProjectionMethod::Native, representation: UnitRepresentation::Markdown,
                text: "# Policy\n\n[local](../concepts/policy.md) ![figure](image.png) <script>source</script>\n\n| Scope | Days |\n| --- | --- |\n| prod | 7 |".into(),
                sheet_structure: None,
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
    async fn file_projection_accepts_explicit_large_capture_without_piping_raw_bytes() {
        let root = tempfile::tempdir().unwrap();
        let python = executable(
            root.path(),
            "/bin/cat > \"$0.stdin\"\nprintf '%s' \"$6\" > \"$0.options\"\nprintf '%s' '{\"status\":\"failed\",\"code\":\"no-text\"}'",
        );
        let bytes = MAX_INPUT_BYTES + 1;
        let captured = lk_core::captured_file::CapturedFile::capture(
            std::io::Read::take(std::io::repeat(0), bytes as u64),
            bytes,
        )
        .unwrap();
        let error = project_file(&captured, InputFormat::Pdf, Some(&configuration(&python)))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProcessingError::Failed(ProcessingFailure::NoText)
        ));
        let options: serde_json::Value =
            serde_json::from_slice(&std::fs::read(python.with_extension("options")).unwrap())
                .unwrap();
        assert_eq!(
            options["input_path"],
            captured.path().to_string_lossy().as_ref()
        );
        assert_eq!(options["max_input_bytes"], bytes);
        assert_eq!(
            std::fs::metadata(python.with_extension("stdin"))
                .unwrap()
                .len(),
            0
        );
        captured.verify().unwrap();
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
            let captured =
                lk_core::captured_file::CapturedFile::capture(bytes, bytes.len()).unwrap();
            assert!(captured.path().extension().is_none());
            let value = project_file(&captured, format, Some(&config))
                .await
                .unwrap();
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
        assert_eq!(image.units[0].representation, UnitRepresentation::Text);
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
        assert!(korean.units[0].text.contains("30일"));
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
first.save(target/'scanned.pdf', 'PDF', save_all=True, append_images=[second])
"#,
            ])
            .arg(&fixtures)
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
        let mut raster_limited = config.clone();
        raster_limited.max_decoded_bytes = std::num::NonZeroUsize::new(10 * 1024 * 1024).unwrap();
        assert!(matches!(
            project(
                include_bytes!("content/fixtures/policy.pdf"),
                InputFormat::Pdf,
                Some(&raster_limited)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit))
        ));
        let scanned = project(
            &std::fs::read(temporary.path().join("scanned.pdf")).unwrap(),
            InputFormat::Pdf,
            Some(&config),
        )
        .await
        .unwrap();
        assert_eq!(scanned.units.len(), 4);
        assert!(scanned.units[..2].iter().all(|unit| unit.text.is_empty()));
        assert!(scanned.units[2].text.contains("30 days"));
        assert!(scanned.units[3].text.contains("운영 환경"));
        for number in 1..=2 {
            let native = &scanned.units[number - 1];
            let recognized = &scanned.units[number + 1];
            assert_eq!(native.location, recognized.location);
            assert_eq!(native.method, ProjectionMethod::Native);
            assert_eq!(native.representation, UnitRepresentation::Markdown);
            assert_eq!(recognized.method, ProjectionMethod::Ocr);
            assert_eq!(recognized.representation, UnitRepresentation::Text);
        }
        let mut invalid = scanned;
        invalid.units[2].representation = UnitRepresentation::Markdown;
        assert!(validate_projection(&invalid, InputFormat::Pdf, &config).is_err());
        invalid.units[2].representation = UnitRepresentation::Text;
        invalid.units.pop();
        assert!(validate_projection(&invalid, InputFormat::Pdf, &config).is_err());

        let invalid_models = temporary.path().join("invalid-tessdata");
        std::fs::create_dir(&invalid_models).unwrap();
        let ocr = config.ocr.as_mut().unwrap();
        for language in &ocr.languages {
            let filename = format!("{language}.traineddata");
            let target = invalid_models.join(&filename);
            std::fs::copy(ocr.tessdata_dir.join(&filename), &target).unwrap();
            std::fs::write(target, b"invalid OCR model fixture").unwrap();
        }
        ocr.tessdata_dir = invalid_models;
        for (path, format) in [
            (fixtures.join("policy.png"), InputFormat::Png),
            (fixtures.join("policy.pdf"), InputFormat::Pdf),
            (temporary.path().join("scanned.pdf"), InputFormat::Pdf),
        ] {
            let bytes = std::fs::read(path).unwrap();
            assert!(matches!(
                project(&bytes, format, Some(&config)).await,
                Err(ProcessingError::Failed(ProcessingFailure::Decoder))
            ));
        }
    }
}
