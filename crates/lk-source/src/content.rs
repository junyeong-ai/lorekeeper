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
use tokio::time::Instant;

mod cache;
mod vision;

pub use cache::{CacheAccess, CacheError, ProjectionCache, prune as prune_projection_cache};
pub use vision::{
    MLX_REQUIREMENTS, REQUIREMENTS as VISION_REQUIREMENTS, install as install_vision,
};

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
    #[error("document processing timeout exceeds the platform clock range")]
    InvalidTimeout,
    #[error("document processor exited without a valid result")]
    Exited,
    #[error("document processor response is invalid")]
    Protocol,
    #[error("document processing: {0}")]
    Failed(ProcessingFailure),
    #[error("document projection cache: {0}")]
    Cache(#[from] cache::CacheError),
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
    #[error("vision runtime is unavailable on this platform")]
    UnsupportedPlatform,
    #[error("vision model artifacts are missing or invalid")]
    VisionModels,
    #[error("vision model artifacts changed during processing")]
    VisionModelsChanged,
    #[error("vision output did not terminate normally")]
    IncompleteVision,
    #[error("vision model installation failed")]
    ModelDownload,
    #[error("document processing exceeded its time budget")]
    Timeout,
}

#[derive(Serialize)]
struct Options<'a> {
    check: bool,
    format: InputFormat,
    max_input_bytes: usize,
    input_path: Option<&'a std::path::Path>,
    max_decoded_bytes: usize,
    max_raster_bytes: usize,
    max_units: usize,
    timeout_seconds: u64,
    ocr: Option<OcrOptions<'a>>,
    allow_empty: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<lk_core::content::VisionRuntime>,
}

pub async fn check(config: &InputProcessingConfig) -> Result<ProcessingRuntime, ProcessingError> {
    let deadline = processing_deadline(config.timeout_seconds.get())?;
    match run_processor(None, InputFormat::Pdf, config, true, deadline).await? {
        Response::Ready { mut runtime } => {
            if let Some(vision) = &config.vision {
                runtime.vision = Some(vision::check(vision, config, deadline).await?);
            }
            ensure_deadline(deadline)?;
            Ok(runtime)
        }
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
    project_file_with_cache(captured, format, config, None).await
}

pub async fn project_file_with_cache(
    captured: &lk_core::captured_file::CapturedFile,
    format: InputFormat,
    config: Option<&InputProcessingConfig>,
    cache: Option<&ProjectionCache>,
) -> Result<InputProjection, ProcessingError> {
    let config = config.ok_or(ProcessingError::NotConfigured)?;
    let deadline = processing_deadline(config.timeout_seconds.get())?;
    project_components(captured, format, config, cache, deadline).await
}

async fn project_components(
    captured: &lk_core::captured_file::CapturedFile,
    format: InputFormat,
    config: &InputProcessingConfig,
    cache: Option<&ProjectionCache>,
    deadline: Instant,
) -> Result<InputProjection, ProcessingError> {
    if !format.requires_processor() {
        return Err(ProcessingError::Failed(
            ProcessingFailure::UnsupportedFormat,
        ));
    }
    captured.verify()?;
    let uses_vision = config.vision.is_some()
        && (format.is_image()
            || (format == InputFormat::Pdf
                && matches!(
                    config.pdf_policy(),
                    lk_core::content::PdfProjectionPolicy::Visual { .. }
                )));
    let mut projection = if format.is_image() && uses_vision {
        InputProjection {
            policy: PROJECTION_POLICY.into(),
            format,
            packages: BTreeMap::new(),
            ocr: None,
            vision: None,
            pdf: None,
            units: Vec::new(),
            warnings: Vec::new(),
        }
    } else {
        project_native(captured, format, config, cache, deadline).await?
    };
    if uses_vision {
        let native_serialized = serde_json::to_vec(&projection)
            .map_err(|_| ProcessingError::Protocol)?
            .len();
        let native_decoded = projection
            .units
            .iter()
            .try_fold(0usize, |size, unit| size.checked_add(unit.text.len()))
            .ok_or(ProcessingError::Protocol)?;
        let mut available = config.clone();
        available.max_output_bytes = std::num::NonZeroUsize::new(
            config
                .max_output_bytes
                .get()
                .checked_sub(native_serialized)
                .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?,
        )
        .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
        available.max_decoded_bytes = std::num::NonZeroUsize::new(
            config
                .max_decoded_bytes
                .get()
                .checked_sub(native_decoded)
                .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?,
        )
        .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
        let output = vision::project(captured, format, &available, cache, deadline).await?;
        projection.vision = Some(output.runtime);
        projection.units.extend(output.units);
        projection.warnings.push("Visual recognition is a derived interpretation, not an accuracy guarantee. Native text remains separate; inspect the preserved original for consequential identifiers, quantities and operators.".into());
    }
    if format == InputFormat::Pdf {
        projection.pdf = if config.is_pdf_ocr_enabled() {
            None
        } else {
            Some(config.pdf_policy().clone())
        };
        if let lk_core::content::PdfProjectionPolicy::Visual { pages: Some(_) } =
            config.pdf_policy()
        {
            projection.warnings.push("Visual recognition covers only the explicitly selected physical pages; other pages contain native text only.".into());
        }
    }
    captured.verify()?;
    validate_projection(&projection, format, config)?;
    if serde_json::to_vec(&projection)
        .map_err(|_| ProcessingError::Protocol)?
        .len()
        > config.max_output_bytes.get()
    {
        return Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit));
    }
    ensure_deadline(deadline)?;
    Ok(projection)
}

fn processing_deadline(seconds: u64) -> Result<Instant, ProcessingError> {
    Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or(ProcessingError::InvalidTimeout)
}

fn ensure_deadline(deadline: Instant) -> Result<(), ProcessingError> {
    if Instant::now() >= deadline {
        Err(ProcessingError::Timeout)
    } else {
        Ok(())
    }
}

async fn terminate(child: &mut tokio::process::Child) -> std::io::Result<()> {
    let _ = child.start_kill();
    child.wait().await?;
    Ok(())
}

async fn project_native(
    captured: &lk_core::captured_file::CapturedFile,
    format: InputFormat,
    config: &InputProcessingConfig,
    cache: Option<&ProjectionCache>,
    deadline: Instant,
) -> Result<InputProjection, ProcessingError> {
    let pdf_ocr = config.is_pdf_ocr_enabled();
    let mut native_config = config.clone();
    native_config.vision = None;
    native_config.pdf = (!pdf_ocr).then_some(lk_core::content::PdfProjectionPolicy::Native {});
    if !format.is_image() && !(format == InputFormat::Pdf && pdf_ocr) {
        native_config.ocr = None;
    }
    let mut expected_models = None;
    let claim = if let Some(cache) = cache {
        let runtime = match run_processor(None, format, &native_config, true, deadline).await? {
            Response::Ready { runtime } => runtime,
            Response::Failed { code } => return Err(ProcessingError::Failed(code)),
            _ => return Err(ProcessingError::Protocol),
        };
        let ocr = native_config
            .ocr
            .as_ref()
            .map(|ocr| (&ocr.languages, ocr.layout, ocr.pdf));
        let identity = blake3::hash(
            &serde_json::to_vec(&(
                captured.revision(),
                format,
                PROJECTION_POLICY,
                blake3::hash(PROCESSOR.as_bytes()).to_hex().as_str(),
                &runtime.packages,
                &runtime.ocr_models,
                ocr,
            ))
            .map_err(|_| ProcessingError::Protocol)?,
        )
        .to_hex()
        .to_string();
        expected_models = Some(runtime.ocr_models);
        Some(cache.claim(
            &identity,
            config.max_output_bytes.get().saturating_add(1024),
        )?)
    } else {
        None
    };
    if let Some(claim) = &claim
        && let Some(projection) = claim.load_projection()?
    {
        validate_projection_parts(
            &projection,
            format,
            &native_config,
            format == InputFormat::Pdf,
        )?;
        validate_native_models(&projection, expected_models.as_ref())?;
        if serde_json::to_vec(&projection)
            .map_err(|_| ProcessingError::Protocol)?
            .len()
            > config.max_output_bytes.get()
        {
            return Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit));
        }
        captured.verify()?;
        ensure_deadline(deadline)?;
        claim.touch_projection()?;
        return Ok(projection);
    }
    let projection =
        match run_processor(Some(captured), format, &native_config, false, deadline).await? {
            Response::Projected { projection } => projection,
            Response::Failed { code } => return Err(ProcessingError::Failed(code)),
            _ => return Err(ProcessingError::Protocol),
        };
    validate_projection_parts(
        &projection,
        format,
        &native_config,
        format == InputFormat::Pdf,
    )?;
    validate_native_models(&projection, expected_models.as_ref())?;
    if cache.is_some_and(ProjectionCache::is_writable) {
        captured.verify()?;
        claim
            .as_ref()
            .ok_or(ProcessingError::Protocol)?
            .store_projection(&projection)?;
    }
    Ok(projection)
}

fn validate_native_models(
    projection: &InputProjection,
    expected: Option<&BTreeMap<String, String>>,
) -> Result<(), ProcessingError> {
    if let Some(expected) = expected
        && projection
            .ocr
            .as_ref()
            .map_or(!expected.is_empty(), |ocr| &ocr.models != expected)
    {
        return Err(ProcessingError::Failed(ProcessingFailure::OcrDataChanged));
    }
    Ok(())
}

async fn run_processor(
    captured: Option<&lk_core::captured_file::CapturedFile>,
    format: InputFormat,
    config: &InputProcessingConfig,
    check: bool,
    deadline: Instant,
) -> Result<Response, ProcessingError> {
    ensure_deadline(deadline)?;
    let options = Options {
        check,
        format,
        max_input_bytes: captured.map_or(0, |file| file.byte_len()),
        input_path: captured.map(|file| file.path()),
        max_decoded_bytes: config.max_decoded_bytes.get(),
        max_raster_bytes: config.max_raster_bytes.get(),
        max_units: config.max_units.get(),
        timeout_seconds: config.timeout_seconds.get(),
        ocr: config.ocr.as_ref().map(|ocr| OcrOptions {
            pdf: config.is_pdf_ocr_enabled(),
            ..OcrOptions::from(ocr)
        }),
        allow_empty: format == InputFormat::Pdf,
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
    let result = tokio::time::timeout_at(deadline, operation).await;
    let (output, (), status) = match result {
        Ok(Ok(value)) => value,
        failure => {
            terminate(&mut child).await?;
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
    ensure_deadline(deadline)?;
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
    validate_projection_parts(value, format, config, false)
}

fn validate_projection_parts(
    value: &InputProjection,
    format: InputFormat,
    config: &InputProcessingConfig,
    allow_empty: bool,
) -> Result<(), ProcessingError> {
    let expected: BTreeMap<_, _> = [
        ("xberg".into(), "1.3.9".into()),
        ("pypdfium2".into(), "5.14.0".into()),
        ("openpyxl".into(), "3.1.5".into()),
        ("defusedxml".into(), "0.7.1".into()),
        ("et-xmlfile".into(), "2.0.0".into()),
        ("pillow".into(), "13.0.0".into()),
    ]
    .into();
    let uses_vision = config.vision.is_some()
        && (format.is_image()
            || (format == InputFormat::Pdf
                && matches!(
                    config.pdf_policy(),
                    lk_core::content::PdfProjectionPolicy::Visual { .. }
                )));
    if value.policy != PROJECTION_POLICY
        || value.format != format
        || (if format.is_image() && uses_vision {
            !value.packages.is_empty()
        } else {
            value.packages != expected
        })
    {
        return Err(ProcessingError::Protocol);
    }
    let uses_ocr = (format.is_image() && !uses_vision)
        || (format == InputFormat::Pdf && config.is_pdf_ocr_enabled());
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
    let mut visual_pages = Vec::new();
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
        match (&unit.visual_structure, unit.method) {
            (Some(structure), ProjectionMethod::Visual) => {
                vision::validate_structure(structure)?;
                bytes = bytes
                    .checked_add(
                        serde_json::to_vec(structure)
                            .map_err(|_| ProcessingError::Protocol)?
                            .len(),
                    )
                    .ok_or(ProcessingError::Protocol)?;
            }
            (None, ProjectionMethod::Visual) | (Some(_), _) => {
                return Err(ProcessingError::Protocol);
            }
            (None, _) => {}
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
                format == InputFormat::Pdf && config.is_pdf_ocr_enabled()
            }
            (InputLocation::ImageFrame { .. }, ProjectionMethod::Ocr, UnitRepresentation::Text) => {
                format.is_image() && config.ocr.is_some()
            }
            (
                InputLocation::Page { .. },
                ProjectionMethod::Visual,
                UnitRepresentation::JsonLines,
            ) => format == InputFormat::Pdf && uses_vision,
            (
                InputLocation::ImageFrame { .. },
                ProjectionMethod::Visual,
                UnitRepresentation::JsonLines,
            ) => format.is_image() && uses_vision,
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
            InputLocation::Page { number } if unit.method == ProjectionMethod::Visual => {
                visual_pages.push(number.get())
            }
            InputLocation::Page { number } => ocr_pages.push(number.get()),
            InputLocation::Sheet { name, .. } if !sheets.insert(name) => {
                return Err(ProcessingError::Protocol);
            }
            InputLocation::Document | InputLocation::Sheet { .. } => {}
        }
    }
    if uses_vision {
        vision::validate_runtime(value.vision.as_ref().ok_or(ProcessingError::Protocol)?)?;
        if format == InputFormat::Pdf
            && visual_pages != vision::selected_pages(config.pdf_policy(), native_pages.len())?
        {
            return Err(ProcessingError::Protocol);
        }
    } else if value.vision.is_some() {
        return Err(ProcessingError::Protocol);
    }
    if value
        .pdf
        .as_ref()
        .is_some_and(|policy| policy != config.pdf_policy())
    {
        return Err(ProcessingError::Protocol);
    }
    if frames.iter().copied().ne(1..=frames.len())
        || native_pages.iter().copied().ne(1..=native_pages.len())
        || (format == InputFormat::Pdf && uses_ocr && ocr_pages != native_pages)
    {
        return Err(ProcessingError::Protocol);
    }
    if value.units.len() > config.max_units.get() || bytes > config.max_decoded_bytes.get() {
        return Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit));
    }
    if !allow_empty && !value.units.iter().any(vision::has_content) {
        return Err(ProcessingError::Failed(ProcessingFailure::NoText));
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
    use pulldown_cmark::{CowStr, Event, Options, Tag, TagEnd};
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
        if let Some(structure) = &unit.visual_structure {
            let metadata =
                serde_json::to_string(&serde_json::json!({"visual_structure": structure}))
                    .map_err(|error| crate::SourceError::Parse(error.to_string()))?;
            output.push_str(&crate::fetch::literal_markdown(&metadata)?);
            output.push_str("\n\n");
        }
        if unit.representation != UnitRepresentation::Markdown {
            output.push_str(&crate::fetch::literal_markdown(&unit.text)?);
            continue;
        }
        let mut links = Vec::new();
        let input = lk_core::markdown::MarkdownInput::from_text(&unit.text);
        let events = input
            .events(Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH)
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
    async fn rejected_native_cache_reuse_does_not_renew_retention() {
        let python =
            std::env::var_os("LORE_TEST_DOCUMENT_PYTHON").expect("LORE_TEST_DOCUMENT_PYTHON");
        let config = configuration(std::path::Path::new(&python));
        let temporary = tempfile::tempdir().unwrap();
        let cache = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadWrite);
        let bytes = include_bytes!("content/fixtures/policy.pdf");
        let captured =
            lk_core::captured_file::CapturedFile::capture(bytes.as_slice(), bytes.len()).unwrap();
        project_file_with_cache(&captured, InputFormat::Pdf, Some(&config), Some(&cache))
            .await
            .unwrap();
        let payload = std::fs::read_dir(
            temporary
                .path()
                .join(lk_core::vault_path::PROJECTION_CACHE_DIR),
        )
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().path().join("projection.json"))
                .find(|path| path.is_file())
                .unwrap()
        })
        .unwrap();
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1);
        let envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&payload).unwrap()).unwrap();
        let native_bytes = serde_json::to_vec(&envelope["payload"]).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&payload)
            .unwrap()
            .set_modified(old)
            .unwrap();
        for output_limited in [false, true] {
            let mut limited = config.clone();
            if output_limited {
                limited.max_output_bytes = std::num::NonZeroUsize::new(native_bytes - 1).unwrap();
            } else {
                limited.max_decoded_bytes = std::num::NonZeroUsize::new(1).unwrap();
            }
            assert!(matches!(
                project_file_with_cache(&captured, InputFormat::Pdf, Some(&limited), Some(&cache))
                    .await,
                Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit))
            ));
            assert_eq!(payload.metadata().unwrap().modified().unwrap(), old);
        }
        let reader = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadOnly);
        project_file_with_cache(&captured, InputFormat::Pdf, Some(&config), Some(&reader))
            .await
            .unwrap();
        assert_eq!(payload.metadata().unwrap().modified().unwrap(), old);
        project_file_with_cache(&captured, InputFormat::Pdf, Some(&config), Some(&cache))
            .await
            .unwrap();
        assert!(payload.metadata().unwrap().modified().unwrap() > old);
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
            policy: PROJECTION_POLICY.into(), format: InputFormat::Docx, packages: BTreeMap::new(), ocr: None, vision: None, pdf: None, warnings: vec![],
            units: vec![lk_core::content::InputUnit {
                location: InputLocation::Document, method: ProjectionMethod::Native, representation: UnitRepresentation::Markdown,
                text: "# Policy\n\n[local](../concepts/policy.md) ![figure](image.png) <script>source</script>\n\n| Scope | Days |\n| --- | --- |\n| prod | 7 |".into(),
                sheet_structure: None,
                visual_structure: None,
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

    #[test]
    fn projection_cr_code_keeps_literal_content_while_live_links_are_sanitized() {
        use pulldown_cmark::{Event, Tag, TagEnd};
        for text in [
            "```python\rif ready:\r    print('first')\r\r[ghost](ghost.md)\r```\r\r[live](live.md)\r",
            "```python\r\nif ready:\r    print('first')\r\r[ghost](ghost.md)\r```\n\r[live](live.md)\n",
        ] {
            let value = InputProjection {
                policy: PROJECTION_POLICY.into(),
                format: InputFormat::Docx,
                packages: BTreeMap::new(),
                ocr: None,
                vision: None,
                pdf: None,
                warnings: vec![],
                units: vec![lk_core::content::InputUnit {
                    location: InputLocation::Document,
                    method: ProjectionMethod::Native,
                    representation: UnitRepresentation::Markdown,
                    text: text.into(),
                    sheet_structure: None,
                    visual_structure: None,
                }],
            };
            let rendered = render_markdown(&value).unwrap();
            let input = lk_core::markdown::MarkdownInput::from_text(&rendered);
            let mut in_code = false;
            let mut code = String::new();
            for event in input.events(pulldown_cmark::Options::empty()) {
                match event {
                    Event::Start(Tag::CodeBlock(_)) => in_code = true,
                    Event::End(TagEnd::CodeBlock) => in_code = false,
                    Event::Text(text) if in_code => code.push_str(&text),
                    Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                        panic!("projection emitted a wiki edge: {rendered:?}")
                    }
                    _ => {}
                }
            }
            assert_eq!(
                code, "if ready:\n    print('first')\n\n[ghost](ghost.md)\n",
                "{rendered:?}"
            );
            assert!(rendered.contains("live.md"));
            assert_eq!(value.units[0].text, text);
        }
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
    async fn explicit_native_pdf_policy_overrides_legacy_ocr_in_worker_and_projection() {
        let root = tempfile::tempdir().unwrap();
        let native_unit = serde_json::json!({"location":{"kind":"page","number":1},"method":"native","representation":"markdown","text":"native source"});
        for legacy in [true, false] {
            let mut projection = serde_json::json!({
                "policy":PROJECTION_POLICY,"format":"pdf",
                "packages":{"xberg":"1.3.9","pypdfium2":"5.14.0","openpyxl":"3.1.5","defusedxml":"0.7.1","et-xmlfile":"2.0.0","pillow":"13.0.0"},
                "units":[native_unit.clone()],"warnings":[]
            });
            if legacy {
                projection["ocr"] = serde_json::json!({"languages":["eng"],"layout":"auto","models":{"eng":"a".repeat(64)}});
                projection["units"].as_array_mut().unwrap().push(serde_json::json!({"location":{"kind":"page","number":1},"method":"ocr","representation":"text","text":"recognized source"}));
            }
            let response = serde_json::json!({"status":"projected","projection":projection});
            let python = executable(
                root.path(),
                &format!(
                    "printf '%s' \"$6\" > \"$0.options\"\nprintf '%s' '{}'",
                    response
                ),
            );
            let mut config = configuration(&python);
            config.ocr = Some(
                serde_json::from_value(
                    serde_json::json!({"tessdata_dir":root.path(),"languages":["eng"],"pdf":true}),
                )
                .unwrap(),
            );
            config.pdf = (!legacy).then_some(lk_core::content::PdfProjectionPolicy::Native {});
            let captured =
                lk_core::captured_file::CapturedFile::capture(&b"captured PDF"[..], 100).unwrap();
            let result = project_file(&captured, InputFormat::Pdf, Some(&config))
                .await
                .unwrap();
            let options: serde_json::Value =
                serde_json::from_slice(&std::fs::read(python.with_extension("options")).unwrap())
                    .unwrap();
            assert_eq!(
                options.get("ocr").is_some_and(|ocr| ocr["pdf"] == true),
                legacy
            );
            assert_eq!(result.ocr.is_some(), legacy);
            assert_eq!(result.pdf.is_none(), legacy);
            assert_eq!(result.units.len(), if legacy { 2 } else { 1 });
        }
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

    #[test]
    #[ignore = "requires Python; set LORE_TEST_DOCUMENT_PYTHON"]
    fn visual_selection_control_uses_the_configured_output_budget() {
        let python =
            std::env::var_os("LORE_TEST_DOCUMENT_PYTHON").expect("LORE_TEST_DOCUMENT_PYTHON");
        let script = [
            "__name__ = 'selection_test'\n",
            include_str!("content/vision_common.py"),
            "\n",
            include_str!("content/paddle.py"),
            r#"
units = [{"location": {"kind": "page", "number": number}, "width": 8, "height": 8} for number in range(1, 50001)]
control = canonical({"skip": list(range(1, 50001))}) + b"\n"
assert len(control) > 256 * 1024
inventory = lambda *args: units
class Sink:
    def __init__(self):
        self.records = []
    def emit(self, record):
        self.records.append(record["status"])
sink = Sink()
prepare = lambda options: (None, {}, {}, sink)
with tempfile.NamedTemporaryFile() as captured:
    options = {"operation": "project", "input_path": captured.name, "format": "pdf", "max_input_bytes": 100, "max_output_bytes": 32 * 1024 * 1024, "max_decoded_bytes": 1}
    sys.stdin = io.TextIOWrapper(io.BytesIO(control))
    main(options)
    assert sink.records == ["inventory", "complete"]
    options["max_output_bytes"] = 256 * 1024
    sys.stdin = io.TextIOWrapper(io.BytesIO(control))
    try:
        main(options)
    except ProjectionError as error:
        assert error.code == "decoded-limit"
    else:
        raise AssertionError("over-budget control was accepted")
"#,
        ].concat();
        let output = std::process::Command::new(python)
            .args(["-I", "-X", "utf8", "-c", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "requires PDFium in the visual layout runtime; set LORE_TEST_VISION_PYTHON"]
    fn visual_inventory_preflights_frames_and_matches_pdfium_geometry() {
        let python = std::env::var_os("LORE_TEST_VISION_PYTHON").expect("LORE_TEST_VISION_PYTHON");
        let script = [
            "__name__ = 'geometry_test'\n",
            include_str!("content/vision_common.py"),
            "\n",
            include_str!("content/paddle.py"),
            r#"
import tempfile
import pypdfium2 as pdfium
from PIL import Image, ImageFile

def inspect_canvas(path, format_name, expected):
    loads = []
    def observe(frame, event, arg):
        if event == "call" and frame.f_code is ImageFile.ImageFile.load.__code__:
            loads.append(frame.f_code)
    sys.setprofile(observe)
    try:
        units = inventory(path, format_name, {"max_units": 3, "max_raster_bytes": 1})
    finally:
        sys.setprofile(None)
    assert not loads, format_name
    assert [(unit["width"], unit["height"]) for unit in units] == expected

with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    path = root / "geometry.pdf"
    with pdfium.PdfDocument.new() as document:
        for width, height in [(612, 792), (480, 408), (100000, 100000)]:
            document.new_page(width, height).close()
        document.save(path)
    units = inventory(path, "pdf", {"max_units": 3})
    assert len(units) == 3
    assert [(unit["width"], unit["height"]) for unit in units[:2]] == [(1275, 1651), (1001, 851)]
    assert units[2]["width"] * units[2]["height"] * 4 > 64 * 1024 * 1024
    for number in [1, 2]:
        with render(path, "pdf", number) as image:
            assert image.size == (units[number - 1]["width"], units[number - 1]["height"])
    with Image.new("RGB", (100, 100), "red") as first, Image.new("RGB", (100, 100), "blue") as second:
        path = root / "frames.png"
        first.save(path, save_all=True, append_images=[second])
    inspect_canvas(path, "png", [(100, 100), (100, 100)])
    with Image.new("RGB", (100, 50)) as first, Image.new("RGB", (80, 120)) as second:
        path = root / "variable.tiff"
        first.save(path, save_all=True, append_images=[second])
    inspect_canvas(path, "tiff", [(100, 50), (80, 120)])
    path = root / "late-exif.png"
    with Image.new("RGB", (2, 3), "white") as image:
        exif = Image.Exif()
        exif[274] = 6
        image.save(path, exif=exif)
    data = path.read_bytes()
    chunks, position = [], 8
    while position < len(data):
        length = int.from_bytes(data[position:position + 4], "big")
        chunks.append(data[position:position + length + 12])
        position += length + 12
    exif_chunks = [chunk for chunk in chunks if chunk[4:8] == b"eXIf"]
    assert len(exif_chunks) == 1 and chunks[-1][4:8] == b"IEND"
    path.write_bytes(data[:8] + b"".join(chunk for chunk in chunks if chunk[4:8] not in {b"eXIf", b"IEND"}) + exif_chunks[0] + chunks[-1])
    inspect_canvas(path, "png", [(2, 3)])
    with render(path, "png", 1) as image:
        assert image.size == (3, 2)
"#,
        ]
        .concat();
        let output = std::process::Command::new(python)
            .args(["-I", "-X", "utf8", "-c", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
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
import pathlib, sys, zipfile
source, target = map(pathlib.Path, sys.argv[1:])
first = Image.open(source/'policy.png').convert('RGB')
second = Image.open(source/'policy-ko.png').convert('RGB')
first.save(target/'pages.tiff', save_all=True, append_images=[second], compression='tiff_deflate')
first.save(target/'page.jpeg', quality=95)
first.save(target/'page.webp', lossless=True)
first.save(target/'scanned.pdf', 'PDF', save_all=True, append_images=[second])
with zipfile.ZipFile(source/'policy.docx') as archive:
    members = {item.filename: archive.read(item) for item in archive.infolist()}
members['word/document.xml'] = b'<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Keep records for 7 days.</w:t></w:r></w:p><w:p><w:r><w:t'
with zipfile.ZipFile(target/'malformed.docx', 'w', compression=zipfile.ZIP_DEFLATED) as archive:
    for name, data in members.items():
        archive.writestr(name, data)
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
        assert!(matches!(
            project(
                &std::fs::read(temporary.path().join("malformed.docx")).unwrap(),
                InputFormat::Docx,
                Some(&config)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::Decoder))
        ));
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
        config.pdf = Some(lk_core::content::PdfProjectionPolicy::Native {});
        for (name, format) in [
            ("policy.pdf", InputFormat::Pdf),
            ("policy.docx", InputFormat::Docx),
            ("policy.xlsx", InputFormat::Xlsx),
        ] {
            let projection = project(
                &std::fs::read(fixtures.join(name)).unwrap(),
                format,
                Some(&config),
            )
            .await
            .unwrap();
            assert!(projection.ocr.is_none());
            assert!(
                projection
                    .units
                    .iter()
                    .all(|unit| unit.method == ProjectionMethod::Native)
            );
        }
    }
}
