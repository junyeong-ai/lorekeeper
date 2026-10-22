use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Stdio;

use lk_core::captured_file::CapturedFile;
use lk_core::content::{
    InputFormat, InputLocation, InputProcessingConfig, InputUnit, PdfProjectionPolicy,
    ProjectionMethod, UnitRepresentation, VisionConfig, VisionRuntime, VisualStructure,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use super::{ProcessingError, ProcessingFailure, ProjectionCache};

pub const REQUIREMENTS: &str = include_str!("vision-requirements.txt");
pub const MLX_REQUIREMENTS: &str = include_str!("mlx-requirements.txt");
const COMMON: &str = include_str!("vision_common.py");
const WORKER: &str = include_str!("paddle.py");
const RECOGNIZER: &str = include_str!("mlx.py");

#[derive(Clone, Copy)]
enum WorkerRole {
    Layout,
    Recognition,
}

pub(super) struct VisionOutput {
    pub runtime: VisionRuntime,
    pub units: Vec<InputUnit>,
}

#[derive(Serialize)]
struct Options<'a> {
    operation: &'a str,
    models_dir: &'a Path,
    input_path: Option<&'a Path>,
    format: InputFormat,
    max_input_bytes: usize,
    max_decoded_bytes: usize,
    max_raster_bytes: usize,
    max_units: usize,
    timeout_seconds: u64,
    max_output_bytes: usize,
    scratch_dir: &'a Path,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
enum Record {
    Inventory {
        runtime: VisionRuntime,
        units: Vec<InventoryUnit>,
    },
    Unit {
        unit: InputUnit,
    },
    Complete {},
    Ready {
        runtime: VisionRuntime,
    },
    Failed {
        code: ProcessingFailure,
    },
    Recognize {
        image: String,
        query: String,
    },
    Recognized {
        content: String,
        finish_reason: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryUnit {
    location: InputLocation,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextBlock {
    label: String,
    bbox: [f64; 4],
    order: Option<u32>,
    content: String,
}

struct UnitBudget {
    decoded: usize,
    serialized: usize,
}

impl UnitBudget {
    fn consume(&mut self, unit: &InputUnit) -> Result<(), ProcessingError> {
        let serialized = serde_json::to_vec(unit)
            .map_err(|_| ProcessingError::Protocol)?
            .len();
        let structure = serde_json::to_vec(&unit.visual_structure)
            .map_err(|_| ProcessingError::Protocol)?
            .len();
        let decoded = unit
            .text
            .len()
            .checked_add(structure)
            .ok_or(ProcessingError::Protocol)?;
        self.decoded = self
            .decoded
            .checked_sub(decoded)
            .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
        self.serialized = self
            .serialized
            .checked_sub(serialized)
            .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
        Ok(())
    }
}

fn scratch_directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("lorekeeper-vision-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

fn worker_revision() -> String {
    let mut hash = blake3::Hasher::new();
    for source in [COMMON, WORKER, RECOGNIZER] {
        hash.update(source.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}

fn merge_runtime(
    layout: VisionRuntime,
    recognition: VisionRuntime,
) -> Result<VisionRuntime, ProcessingError> {
    validate_runtime(&layout)?;
    validate_runtime(&recognition)?;
    if layout.models != recognition.models || layout.model_revisions != recognition.model_revisions
    {
        return Err(ProcessingError::Failed(
            ProcessingFailure::VisionModelsChanged,
        ));
    }
    let mut packages = BTreeMap::new();
    for (prefix, versions) in [
        ("layout", layout.packages),
        ("recognition", recognition.packages),
    ] {
        packages.extend(
            versions
                .into_iter()
                .map(|(name, version)| (format!("{prefix}/{name}"), version)),
        );
    }
    let mut models = layout.models;
    models.insert("worker".into(), worker_revision());
    Ok(VisionRuntime {
        engine: layout.engine,
        packages,
        models,
        model_revisions: layout.model_revisions,
    })
}

pub(super) async fn check(
    engine: &VisionConfig,
    config: &InputProcessingConfig,
    deadline: Instant,
) -> Result<VisionRuntime, ProcessingError> {
    let (layout, recognition) = tokio::join!(
        control(engine, config, WorkerRole::Layout, "check", deadline),
        control(engine, config, WorkerRole::Recognition, "check", deadline),
    );
    merge_runtime(layout?, recognition?)
}

pub async fn install(
    python: &Path,
    vlm_python: &Path,
    models_dir: &Path,
) -> Result<VisionRuntime, ProcessingError> {
    let deadline = super::processing_deadline(1800)?;
    let config: InputProcessingConfig =
        serde_json::from_value(serde_json::json!({"python": python, "timeout_seconds": 1800}))
            .map_err(|_| ProcessingError::Protocol)?;
    let engine = VisionConfig::PaddleOcrVl {
        python: python.into(),
        vlm_python: vlm_python.into(),
        models_dir: models_dir.into(),
    };
    let layout = control(&engine, &config, WorkerRole::Layout, "install", deadline).await?;
    let recognition = control(&engine, &config, WorkerRole::Recognition, "check", deadline).await?;
    merge_runtime(layout, recognition)
}

async fn control(
    engine: &VisionConfig,
    config: &InputProcessingConfig,
    role: WorkerRole,
    operation: &'static str,
    deadline: Instant,
) -> Result<VisionRuntime, ProcessingError> {
    super::ensure_deadline(deadline)?;
    let scratch = scratch_directory()?;
    let mut child = command(
        engine,
        config,
        None,
        InputFormat::Pdf,
        operation,
        scratch.path(),
        role,
    )?
    .spawn()?;
    let output = child.stdout.take().ok_or(ProcessingError::Protocol)?;
    let diagnostics = child.stderr.take().ok_or(ProcessingError::Protocol)?;
    let operation = async {
        let (output, (), status) = tokio::try_join!(
            super::read_limited(output, config.max_output_bytes.get()),
            super::drain(diagnostics),
            child.wait()
        )?;
        if !status.success() {
            return Err(ProcessingError::Exited);
        }
        match serde_json::from_slice(&output).map_err(|_| ProcessingError::Protocol)? {
            Record::Ready { runtime } => {
                validate_runtime(&runtime)?;
                super::ensure_deadline(deadline)?;
                Ok(runtime)
            }
            Record::Failed { code } => Err(ProcessingError::Failed(code)),
            _ => Err(ProcessingError::Protocol),
        }
    };
    let result = tokio::time::timeout_at(deadline, operation)
        .await
        .unwrap_or(Err(ProcessingError::Timeout));
    if result.is_err() {
        super::terminate(&mut child).await?;
    }
    result
}

fn command(
    engine: &VisionConfig,
    config: &InputProcessingConfig,
    captured: Option<&CapturedFile>,
    format: InputFormat,
    operation: &'static str,
    scratch_dir: &Path,
    role: WorkerRole,
) -> Result<tokio::process::Command, ProcessingError> {
    let VisionConfig::PaddleOcrVl {
        python,
        vlm_python,
        models_dir,
    } = engine;
    let (python, worker) = match role {
        WorkerRole::Layout => (python, WORKER),
        WorkerRole::Recognition => (vlm_python, RECOGNIZER),
    };
    let script = format!("{COMMON}\n{worker}");
    let options = Options {
        operation,
        models_dir,
        input_path: captured.map(CapturedFile::path),
        format,
        max_input_bytes: captured.map_or(0, CapturedFile::byte_len),
        max_decoded_bytes: config.max_decoded_bytes.get(),
        max_units: config.max_units.get(),
        max_raster_bytes: config.max_raster_bytes.get(),
        timeout_seconds: config.timeout_seconds.get(),
        max_output_bytes: config.max_output_bytes.get(),
        scratch_dir,
    };
    let mut command = tokio::process::Command::new(python);
    command.env_clear();
    for name in ["SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("PYTHONIOENCODING", "utf-8")
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args(["-I", "-X", "utf8", "-c", &script])
        .arg(serde_json::to_string(&options).map_err(|_| ProcessingError::Protocol)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

pub(super) async fn project(
    captured: &CapturedFile,
    format: InputFormat,
    config: &InputProcessingConfig,
    cache: Option<&ProjectionCache>,
    deadline: Instant,
) -> Result<VisionOutput, ProcessingError> {
    super::ensure_deadline(deadline)?;
    let engine = config.vision.as_ref().ok_or(ProcessingError::Protocol)?;
    let layout_scratch = scratch_directory()?;
    let recognition_scratch = scratch_directory()?;
    let mut layout_command = command(
        engine,
        config,
        Some(captured),
        format,
        "project",
        layout_scratch.path(),
        WorkerRole::Layout,
    )?;
    let mut recognition_command = command(
        engine,
        config,
        Some(captured),
        format,
        "project",
        recognition_scratch.path(),
        WorkerRole::Recognition,
    )?;
    let mut layout = layout_command.spawn()?;
    let mut recognition = match recognition_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            super::terminate(&mut layout).await?;
            return Err(ProcessingError::Io(error));
        }
    };
    let mut layout_output = BufReader::new(layout.stdout.take().ok_or(ProcessingError::Protocol)?);
    let layout_diagnostics = layout.stderr.take().ok_or(ProcessingError::Protocol)?;
    let mut layout_input = layout.stdin.take().ok_or(ProcessingError::Protocol)?;
    let mut recognition_output =
        BufReader::new(recognition.stdout.take().ok_or(ProcessingError::Protocol)?);
    let recognition_diagnostics = recognition.stderr.take().ok_or(ProcessingError::Protocol)?;
    let mut recognition_input = recognition.stdin.take().ok_or(ProcessingError::Protocol)?;
    let operation = async {
        let consume = async {
            let mut remaining = config.max_output_bytes.get();
            let recognition_runtime =
                match read_record(&mut recognition_output, &mut remaining).await? {
                    Some(Record::Ready { runtime }) => runtime,
                    Some(Record::Failed { code }) => return Err(ProcessingError::Failed(code)),
                    _ => return Err(ProcessingError::Protocol),
                };
            let (layout_runtime, inventory) =
                match read_record(&mut layout_output, &mut remaining).await? {
                    Some(Record::Inventory { runtime, units }) => (runtime, units),
                    Some(Record::Failed { code }) => return Err(ProcessingError::Failed(code)),
                    _ => return Err(ProcessingError::Protocol),
                };
            let runtime = merge_runtime(layout_runtime, recognition_runtime)?;
            validate_inventory(&inventory, format, config)?;
            let selected = if format == InputFormat::Pdf {
                selected_pages(config.pdf_policy(), inventory.len())?
            } else {
                (1..=inventory.len()).collect()
            };
            let selected_set: BTreeSet<_> = selected.iter().copied().collect();
            if selected.len()
                + if format == InputFormat::Pdf {
                    inventory.len()
                } else {
                    0
                }
                > config.max_units.get()
            {
                return Err(ProcessingError::Failed(ProcessingFailure::UnitLimit));
            }
            let identity = blake3::hash(
                &serde_json::to_vec(&(
                    captured.revision(),
                    format,
                    lk_core::content::PROJECTION_POLICY,
                    &runtime,
                ))
                .map_err(|_| ProcessingError::Protocol)?,
            )
            .to_hex()
            .to_string();
            let claim = cache
                .map(|cache| {
                    cache.claim(
                        &identity,
                        config.max_output_bytes.get().saturating_add(1024),
                    )
                })
                .transpose()?;
            let mut units = BTreeMap::new();
            let mut budget = UnitBudget {
                decoded: config.max_decoded_bytes.get(),
                serialized: config.max_output_bytes.get(),
            };
            let mut skip: BTreeSet<_> = (1..=inventory.len())
                .filter(|number| !selected_set.contains(number))
                .collect();
            if let Some(claim) = &claim {
                for &number in &selected {
                    if let Some(unit) = claim.load_unit(number)? {
                        validate_unit(&unit, &inventory[number - 1], format)?;
                        budget.consume(&unit)?;
                        claim.touch_unit(number)?;
                        units.insert(number, unit);
                        skip.insert(number);
                    }
                }
            }
            let pending: Vec<_> = selected
                .iter()
                .filter(|number| !skip.contains(number))
                .copied()
                .collect();
            for &number in &pending {
                let page = &inventory[number - 1];
                if page.width > 65536
                    || page.height > 65536
                    || u64::from(page.width) * u64::from(page.height) * 4
                        > config.max_raster_bytes.get() as u64
                {
                    return Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit));
                }
            }
            write_message(&mut layout_input, &serde_json::json!({"skip": skip})).await?;
            let mut next = pending.into_iter().peekable();
            let transport_limit = config
                .max_raster_bytes
                .get()
                .checked_mul(2)
                .and_then(|limit| limit.checked_add(1024 * 1024))
                .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?
                .max(config.max_output_bytes.get());
            let mut recognized_bytes = 0usize;
            let mut complete = false;
            loop {
                let mut message_remaining = transport_limit;
                let Some(record) = read_record(&mut layout_output, &mut message_remaining).await?
                else {
                    break;
                };
                if complete {
                    return Err(ProcessingError::Protocol);
                }
                if let Record::Recognize { image, query } = record {
                    if next.peek().is_none() {
                        return Err(ProcessingError::Protocol);
                    }
                    write_message(
                        &mut recognition_input,
                        &serde_json::json!({"image": image, "query": query}),
                    )
                    .await?;
                    let mut response_limit = transport_limit;
                    let content = match read_record(&mut recognition_output, &mut response_limit)
                        .await?
                    {
                        Some(Record::Recognized {
                            content,
                            finish_reason,
                        }) if finish_reason == "stop" => content,
                        Some(Record::Recognized { .. }) => {
                            return Err(ProcessingError::Failed(
                                ProcessingFailure::IncompleteVision,
                            ));
                        }
                        Some(Record::Failed { code }) => return Err(ProcessingError::Failed(code)),
                        _ => return Err(ProcessingError::Protocol),
                    };
                    recognized_bytes = recognized_bytes
                        .checked_add(content.len())
                        .filter(|bytes| *bytes <= config.max_decoded_bytes.get())
                        .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
                    write_message(
                        &mut layout_input,
                        &serde_json::json!({"content": content, "finish_reason": "stop"}),
                    )
                    .await?;
                    continue;
                }
                remaining = remaining
                    .checked_sub(transport_limit - message_remaining)
                    .ok_or(ProcessingError::Failed(ProcessingFailure::DecodedLimit))?;
                match record {
                    Record::Unit { unit } => {
                        let number = next.next().ok_or(ProcessingError::Protocol)?;
                        validate_unit(&unit, &inventory[number - 1], format)?;
                        budget.consume(&unit)?;
                        if cache.is_some_and(ProjectionCache::is_writable) {
                            captured.verify()?;
                            claim
                                .as_ref()
                                .ok_or(ProcessingError::Protocol)?
                                .store_unit(number, &unit)?;
                        }
                        units.insert(number, unit);
                    }
                    Record::Complete {} if next.peek().is_none() => complete = true,
                    Record::Failed { code } => return Err(ProcessingError::Failed(code)),
                    _ => return Err(ProcessingError::Protocol),
                }
            }
            if !complete || units.len() != selected.len() {
                return Err(ProcessingError::Protocol);
            }
            drop(layout_input);
            drop(recognition_input);
            let mut eof_limit = transport_limit;
            if read_record(&mut recognition_output, &mut eof_limit)
                .await?
                .is_some()
            {
                return Err(ProcessingError::Protocol);
            }
            Ok(VisionOutput {
                runtime,
                units: units.into_values().collect(),
            })
        };
        let (value, (), (), layout_status, recognition_status) = tokio::try_join!(
            consume,
            async {
                super::drain(layout_diagnostics)
                    .await
                    .map_err(ProcessingError::Io)
            },
            async {
                super::drain(recognition_diagnostics)
                    .await
                    .map_err(ProcessingError::Io)
            },
            async { layout.wait().await.map_err(ProcessingError::Io) },
            async { recognition.wait().await.map_err(ProcessingError::Io) },
        )?;
        if !layout_status.success() || !recognition_status.success() {
            return Err(ProcessingError::Exited);
        }
        super::ensure_deadline(deadline)?;
        Ok(value)
    };
    let result = tokio::time::timeout_at(deadline, operation)
        .await
        .unwrap_or(Err(ProcessingError::Timeout));
    if result.is_err() {
        let (layout_cleanup, recognition_cleanup) = tokio::join!(
            super::terminate(&mut layout),
            super::terminate(&mut recognition),
        );
        layout_cleanup?;
        recognition_cleanup?;
    }
    result
}

async fn write_message(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<(), ProcessingError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| ProcessingError::Protocol)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_record(
    reader: &mut (impl AsyncBufRead + Unpin),
    remaining: &mut usize,
) -> Result<Option<Record>, ProcessingError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(ProcessingError::Protocol)
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|at| at + 1);
        let length = end.unwrap_or(available.len());
        if length > *remaining {
            return Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit));
        }
        *remaining -= length;
        bytes.extend_from_slice(&available[..length]);
        reader.consume(length);
        if end.is_some() {
            return serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| ProcessingError::Protocol);
        }
    }
}

pub(super) fn selected_pages(
    policy: &PdfProjectionPolicy,
    count: usize,
) -> Result<Vec<usize>, ProcessingError> {
    let pages = match policy {
        PdfProjectionPolicy::Native {} => Vec::new(),
        PdfProjectionPolicy::Visual { pages: None } => (1..=count).collect(),
        PdfProjectionPolicy::Visual { pages: Some(pages) } => {
            pages.iter().map(|number| number.get()).collect()
        }
    };
    if pages.iter().any(|number| *number > count) || pages.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(ProcessingError::Protocol);
    }
    Ok(pages)
}

pub(super) fn validate_runtime(runtime: &VisionRuntime) -> Result<(), ProcessingError> {
    if runtime.engine != "paddle-ocr-vl"
        || runtime.packages.is_empty()
        || runtime
            .packages
            .iter()
            .any(|(name, version)| name.is_empty() || version.is_empty())
        || ["vlm", "layout", "orientation", "recipe"]
            .iter()
            .any(|name| !runtime.models.contains_key(*name))
        || ["vlm", "layout", "orientation"].iter().any(|name| {
            !runtime.model_revisions.get(*name).is_some_and(|revision| {
                !revision.is_empty() && !revision.chars().any(char::is_control)
            })
        })
        || runtime.models.values().any(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(ProcessingError::Protocol);
    }
    Ok(())
}

fn validate_inventory(
    inventory: &[InventoryUnit],
    format: InputFormat,
    config: &InputProcessingConfig,
) -> Result<(), ProcessingError> {
    if inventory.is_empty() || inventory.len() > config.max_units.get() {
        return Err(ProcessingError::Protocol);
    }
    for (at, unit) in inventory.iter().enumerate() {
        let valid = match unit.location {
            InputLocation::Page { number } => format == InputFormat::Pdf && number.get() == at + 1,
            InputLocation::ImageFrame { number } => format.is_image() && number.get() == at + 1,
            _ => false,
        };
        if !valid || unit.width == 0 || unit.height == 0 {
            return Err(ProcessingError::Protocol);
        }
    }
    Ok(())
}

fn validate_unit(
    unit: &InputUnit,
    expected: &InventoryUnit,
    format: InputFormat,
) -> Result<(), ProcessingError> {
    if unit.location != expected.location
        || unit.method != ProjectionMethod::Visual
        || unit.representation != UnitRepresentation::JsonLines
        || unit.sheet_structure.is_some()
    {
        return Err(ProcessingError::Protocol);
    }
    let structure = unit
        .visual_structure
        .as_ref()
        .ok_or(ProcessingError::Protocol)?;
    validate_structure(structure)?;
    let (width, height) = if matches!(structure.rotation, 90 | 270) {
        (expected.height, expected.width)
    } else {
        (expected.width, expected.height)
    };
    let dimensions = (structure.width, structure.height);
    // Image EXIF transforms can transpose the encoded canvas before document rotation.
    let valid_dimensions = if format.is_image() {
        dimensions == (width, height) || dimensions == (height, width)
    } else {
        format == InputFormat::Pdf && dimensions == (width, height)
    };
    if !valid_dimensions {
        return Err(ProcessingError::Protocol);
    }
    let blocks: Vec<TextBlock> = unit
        .text
        .lines()
        .map(|line| serde_json::from_str(line).map_err(|_| ProcessingError::Protocol))
        .collect::<Result<_, _>>()?;
    if blocks.len() != structure.blocks.len()
        || blocks
            .iter()
            .zip(&structure.blocks)
            .any(|(text, metadata)| {
                text.label != metadata.label
                    || text.bbox != metadata.bbox
                    || text.order != metadata.order
            })
    {
        return Err(ProcessingError::Protocol);
    }
    Ok(())
}

pub(super) fn has_content(unit: &InputUnit) -> bool {
    if unit.method != ProjectionMethod::Visual {
        return !unit.text.trim().is_empty();
    }
    unit.text.lines().any(|line| {
        serde_json::from_str::<TextBlock>(line).is_ok_and(|block| !block.content.trim().is_empty())
    })
}

pub(super) fn validate_structure(structure: &VisualStructure) -> Result<(), ProcessingError> {
    if structure.width == 0
        || structure.height == 0
        || !matches!(structure.rotation, 0 | 90 | 180 | 270)
        || structure.blocks.iter().any(|block| {
            let [left, top, right, bottom] = block.bbox;
            block.label.is_empty()
                || !block.bbox.iter().all(|number| number.is_finite())
                || left < 0.0
                || top < 0.0
                || left > right
                || top > bottom
                || right > f64::from(structure.width)
                || bottom > f64::from(structure.height)
        })
    {
        return Err(ProcessingError::Protocol);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> serde_json::Value {
        serde_json::json!({"engine":"paddle-ocr-vl", "packages":{"fixture":"1"}, "model_revisions":{"vlm":"fixture/vlm@1","layout":"fixture/layout@1","orientation":"fixture/orientation@1"}, "models": {
            "vlm":"a".repeat(64), "layout":"b".repeat(64), "orientation":"c".repeat(64), "recipe":"d".repeat(64)
        }})
    }

    fn unit(number: usize, content: &str) -> InputUnit {
        let block = serde_json::json!({"label":"text","bbox":[0.0,0.0,8.0,8.0],"order":1,"content":content});
        serde_json::from_value(serde_json::json!({"location":{"kind":"image-frame","number":number},"method":"visual","representation":"json-lines","text":block.to_string(),
            "visual_structure":{"width":8,"height":8,"rotation":0,"blocks":[{"label":"text","bbox":[0.0,0.0,8.0,8.0],"order":1}]}})).unwrap()
    }

    #[cfg(unix)]
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    fn emit(value: serde_json::Value) -> String {
        format!("printf '%s\\n' {}\n", quote(&value.to_string()))
    }

    #[cfg(unix)]
    fn executable(root: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    fn configuration(root: &Path) -> InputProcessingConfig {
        serde_json::from_value(serde_json::json!({"python":root.join("layout"), "vision":{"engine":"paddle-ocr-vl","python":root.join("layout"),"vlm_python":root.join("recognition"),"models_dir":root.join("models")}})).unwrap()
    }

    fn deadline(config: &InputProcessingConfig) -> Instant {
        super::super::processing_deadline(config.timeout_seconds.get()).unwrap()
    }

    #[cfg(unix)]
    fn inventory(count: usize) -> String {
        emit(
            serde_json::json!({"status":"inventory", "runtime":runtime(), "units":(1..=count).map(|number|serde_json::json!({"location":{"kind":"image-frame","number":number},"width":8,"height":8})).collect::<Vec<_>>() }),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_later_page_resumes_verified_units_and_rechecks_current_limits() {
        let root = tempfile::tempdir().unwrap();
        let captured = CapturedFile::capture(&b"captured fixture"[..], 100).unwrap();
        let cache = ProjectionCache::for_vault(
            &root.path().join("vault"),
            super::super::CacheAccess::ReadWrite,
        );
        executable(
            root.path(),
            "recognition",
            &(emit(serde_json::json!({"status":"ready","runtime":runtime()}))
                + "exec /bin/cat >/dev/null\n"),
        );
        let first = inventory(2)
            + "IFS= read -r control\n"
            + &emit(serde_json::json!({"status":"unit","unit":unit(1,"first")}))
            + &emit(serde_json::json!({"status":"failed","code":"incomplete-vision"}));
        executable(root.path(), "layout", &first);
        let mut config = configuration(root.path());
        assert!(matches!(
            project(
                &captured,
                InputFormat::Tiff,
                &config,
                Some(&cache),
                deadline(&config)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::IncompleteVision))
        ));
        let second = inventory(2)
            + "IFS= read -r control\n"
            + &format!("test \"$control\" = {}\n", quote("{\"skip\":[1]}"))
            + &emit(serde_json::json!({"status":"unit","unit":unit(2,"second")}))
            + &emit(serde_json::json!({"status":"complete"}));
        executable(root.path(), "layout", &second);
        let resumed = project(
            &captured,
            InputFormat::Tiff,
            &config,
            Some(&cache),
            deadline(&config),
        )
        .await
        .unwrap();
        assert_eq!(resumed.units.len(), 2);
        assert!(resumed.units[0].text.contains("first"));
        let cached = inventory(2)
            + "IFS= read -r control\n"
            + &format!("test \"$control\" = {}\n", quote("{\"skip\":[1,2]}"))
            + &emit(serde_json::json!({"status":"complete"}));
        executable(root.path(), "layout", &cached);
        config.max_raster_bytes = std::num::NonZeroUsize::new(1).unwrap();
        assert_eq!(
            project(
                &captured,
                InputFormat::Tiff,
                &config,
                Some(&cache),
                deadline(&config)
            )
            .await
            .unwrap()
            .units
            .len(),
            2
        );
        let paths: Vec<_> = std::fs::read_dir(
            root.path()
                .join("vault")
                .join(lk_core::vault_path::PROJECTION_CACHE_DIR),
        )
        .unwrap()
        .map(|entry| entry.unwrap().path().join("1.json"))
        .filter(|path| path.is_file())
        .collect();
        assert_eq!(paths.len(), 1);
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&paths[0])
            .unwrap()
            .set_modified(old)
            .unwrap();
        config.max_decoded_bytes = std::num::NonZeroUsize::new(10).unwrap();
        assert!(matches!(
            project(
                &captured,
                InputFormat::Tiff,
                &config,
                Some(&cache),
                deadline(&config)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::DecodedLimit))
        ));
        assert_eq!(paths[0].metadata().unwrap().modified().unwrap(), old);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn raw_length_termination_refuses_a_nonempty_recognition() {
        let root = tempfile::tempdir().unwrap();
        let captured = CapturedFile::capture(&b"captured fixture"[..], 100).unwrap();
        executable(
            root.path(),
            "layout",
            &(inventory(1)
                + "IFS= read -r control\n"
                + &emit(
                    serde_json::json!({"status":"recognize","image":"fixture","query":"OCR:"}),
                )
                + "IFS= read -r response\n"),
        );
        executable(
            root.path(),
            "recognition",
            &(emit(serde_json::json!({"status":"ready","runtime":runtime()}))
                + "IFS= read -r request\n"
                + &emit(
                    serde_json::json!({"status":"recognized","content":"nonempty partial output","finish_reason":"length"}),
                )
                + "exec /bin/cat >/dev/null\n"),
        );
        let config = configuration(root.path());
        assert!(matches!(
            project(
                &captured,
                InputFormat::Png,
                &config,
                None,
                deadline(&config)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::IncompleteVision))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_projection_timeout_reaps_both_workers_and_resumes_the_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let captured = CapturedFile::capture(&b"captured fixture"[..], 100).unwrap();
        let cache = ProjectionCache::for_vault(
            &root.path().join("vault"),
            super::super::CacheAccess::ReadWrite,
        );
        let mut config = configuration(root.path());
        let normal_timeout = config.timeout_seconds;
        executable(
            root.path(),
            "layout",
            &(inventory(2)
                + "IFS= read -r control\n"
                + &emit(serde_json::json!({"status":"unit","unit":unit(1,"first")}))
                + &emit(serde_json::json!({"status":"failed","code":"incomplete-vision"}))),
        );
        executable(
            root.path(),
            "recognition",
            &(emit(serde_json::json!({"status":"ready","runtime":runtime()}))
                + "exec /bin/cat >/dev/null\n"),
        );
        assert!(matches!(
            super::super::project_file_with_cache(
                &captured,
                InputFormat::Tiff,
                Some(&config),
                Some(&cache)
            )
            .await,
            Err(ProcessingError::Failed(ProcessingFailure::IncompleteVision))
        ));
        let checkpoints: Vec<_> = std::fs::read_dir(
            root.path()
                .join("vault")
                .join(lk_core::vault_path::PROJECTION_CACHE_DIR),
        )
        .unwrap()
        .map(|entry| entry.unwrap().path().join("1.json"))
        .filter(|path| path.is_file())
        .collect();
        assert_eq!(checkpoints.len(), 1);
        let checkpoint = std::fs::read(&checkpoints[0]).unwrap();
        let record_process =
            "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' \"$6\" > \"$0.options\"\n";
        executable(
            root.path(),
            "layout",
            &(record_process.to_string()
                + &inventory(2)
                + "IFS= read -r control\n"
                + &format!("test \"$control\" = {}\n", quote("{\"skip\":[1]}"))
                + "exec /bin/sleep 30\n"),
        );
        executable(
            root.path(),
            "recognition",
            &(record_process.to_string()
                + &emit(serde_json::json!({"status":"ready","runtime":runtime()}))
                + "exec /bin/sleep 30\n"),
        );
        config.timeout_seconds = std::num::NonZeroU64::new(1).unwrap();
        assert!(matches!(
            super::super::project_file_with_cache(
                &captured,
                InputFormat::Tiff,
                Some(&config),
                Some(&cache)
            )
            .await,
            Err(ProcessingError::Timeout)
        ));
        for name in ["layout", "recognition"] {
            let pid = std::fs::read_to_string(root.path().join(format!("{name}.pid")))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            let pid = rustix::process::Pid::from_raw(pid).unwrap();
            assert_eq!(
                rustix::process::getpgid(Some(pid)),
                Err(rustix::io::Errno::SRCH)
            );
            let options: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.path().join(format!("{name}.options"))).unwrap(),
            )
            .unwrap();
            assert!(!Path::new(options["scratch_dir"].as_str().unwrap()).exists());
        }
        assert_eq!(std::fs::read(&checkpoints[0]).unwrap(), checkpoint);
        config.timeout_seconds = normal_timeout;
        executable(
            root.path(),
            "layout",
            &(inventory(2)
                + "IFS= read -r control\n"
                + &format!("test \"$control\" = {}\n", quote("{\"skip\":[1]}"))
                + &emit(serde_json::json!({"status":"unit","unit":unit(2,"second")}))
                + &emit(serde_json::json!({"status":"complete"}))),
        );
        executable(
            root.path(),
            "recognition",
            &(emit(serde_json::json!({"status":"ready","runtime":runtime()}))
                + "exec /bin/cat >/dev/null\n"),
        );
        let resumed = super::super::project_file_with_cache(
            &captured,
            InputFormat::Tiff,
            Some(&config),
            Some(&cache),
        )
        .await
        .unwrap();
        assert_eq!(resumed.units.len(), 2);
        assert!(resumed.units[0].text.contains("first"));
        assert!(resumed.units[1].text.contains("second"));
    }

    #[tokio::test]
    #[ignore = "requires installed visual runtimes and models; set LORE_TEST_VISION_PYTHON, LORE_TEST_VISION_VLM_PYTHON and LORE_TEST_VISION_MODELS"]
    async fn installed_adapter_preserves_visual_provenance_and_readonly_cache() {
        fn snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, (Vec<u8>, std::time::SystemTime)> {
            let mut entries = BTreeMap::new();
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    entries.extend(snapshot(&path));
                } else {
                    entries.insert(
                        path.clone(),
                        (
                            std::fs::read(&path).unwrap(),
                            std::fs::metadata(path).unwrap().modified().unwrap(),
                        ),
                    );
                }
            }
            entries
        }

        let root = tempfile::tempdir().unwrap();
        let python = std::path::PathBuf::from(
            std::env::var_os("LORE_TEST_VISION_PYTHON").expect("LORE_TEST_VISION_PYTHON"),
        );
        let recognition = std::path::PathBuf::from(
            std::env::var_os("LORE_TEST_VISION_VLM_PYTHON").expect("LORE_TEST_VISION_VLM_PYTHON"),
        );
        let models = std::path::PathBuf::from(
            std::env::var_os("LORE_TEST_VISION_MODELS").expect("LORE_TEST_VISION_MODELS"),
        );
        let config: InputProcessingConfig = serde_json::from_value(serde_json::json!({"python":python,"vision":{"engine":"paddle-ocr-vl","python":python,"vlm_python":recognition,"models_dir":models}})).unwrap();
        let original = include_bytes!("fixtures/policy.png");
        let captured = CapturedFile::capture(original.as_slice(), original.len()).unwrap();
        let cache = ProjectionCache::for_vault(root.path(), super::super::CacheAccess::ReadWrite);
        let first = super::super::project_file_with_cache(
            &captured,
            InputFormat::Png,
            Some(&config),
            Some(&cache),
        )
        .await
        .unwrap();
        assert_eq!(first.units.len(), 1);
        assert!(matches!(
            first.units[0].location,
            InputLocation::ImageFrame { number } if number.get() == 1
        ));
        assert_eq!(first.units[0].method, ProjectionMethod::Visual);
        assert!(has_content(&first.units[0]));
        assert!(first.units[0].visual_structure.is_some());
        let runtime = first.vision.as_ref().unwrap();
        for key in ["vlm", "layout", "orientation", "recipe", "worker"] {
            assert!(runtime.models.contains_key(key), "{key}");
        }
        let expected = serde_json::to_vec(&first).unwrap();
        let cache_root = root.path().join(lk_core::vault_path::PROJECTION_CACHE_DIR);
        let first_cache = snapshot(&cache_root);
        let second = super::super::project_file_with_cache(
            &captured,
            InputFormat::Png,
            Some(&config),
            Some(&cache),
        )
        .await
        .unwrap();
        assert_eq!(serde_json::to_vec(&second).unwrap(), expected);
        let warm_cache = snapshot(&cache_root);
        assert_eq!(
            first_cache
                .iter()
                .map(|(path, (bytes, _))| (path, bytes))
                .collect::<Vec<_>>(),
            warm_cache
                .iter()
                .map(|(path, (bytes, _))| (path, bytes))
                .collect::<Vec<_>>()
        );
        let readonly = ProjectionCache::for_vault(root.path(), super::super::CacheAccess::ReadOnly);
        let inspected = super::super::project_file_with_cache(
            &captured,
            InputFormat::Png,
            Some(&config),
            Some(&readonly),
        )
        .await
        .unwrap();
        assert_eq!(serde_json::to_vec(&inspected).unwrap(), expected);
        assert_eq!(snapshot(&cache_root), warm_cache);
        captured.verify().unwrap();
    }

    #[test]
    fn image_canvas_allows_exif_transposition_while_pdf_requires_declared_rotation() {
        let mut image = unit(1, "text");
        image.visual_structure.as_mut().unwrap().height = 16;
        let mut expected = InventoryUnit {
            location: image.location.clone(),
            width: 16,
            height: 8,
        };
        validate_unit(&image, &expected, InputFormat::Png).unwrap();
        image.visual_structure.as_mut().unwrap().height = 17;
        assert!(validate_unit(&image, &expected, InputFormat::Png).is_err());
        image.visual_structure.as_mut().unwrap().height = 16;
        let page = InputLocation::Page {
            number: std::num::NonZeroUsize::new(1).unwrap(),
        };
        image.location = page.clone();
        expected.location = page;
        assert!(validate_unit(&image, &expected, InputFormat::Pdf).is_err());
        image.visual_structure.as_mut().unwrap().rotation = 90;
        validate_unit(&image, &expected, InputFormat::Pdf).unwrap();
    }

    #[test]
    fn blank_block_records_do_not_become_knowledge_and_unit_budget_is_aggregate() {
        assert!(!has_content(&unit(1, "  ")));
        assert!(has_content(&unit(1, "0")));
        let first = unit(1, "first");
        let size = serde_json::to_vec(&first).unwrap().len();
        let mut budget = UnitBudget {
            decoded: usize::MAX,
            serialized: size + 1,
        };
        budget.consume(&first).unwrap();
        assert!(budget.consume(&first).is_err());
    }

    #[tokio::test]
    async fn progress_requires_bounded_complete_json_lines() {
        let mut limit = 100;
        let mut input = BufReader::new(&b"{\"status\":\"complete\"}\n"[..]);
        assert!(matches!(
            read_record(&mut input, &mut limit).await.unwrap(),
            Some(Record::Complete {})
        ));
        assert!(read_record(&mut input, &mut limit).await.unwrap().is_none());
        for bytes in [
            &b"{\"status\":\"complete\"}"[..],
            &b"{\"status\":\"complete\",\"extra\":1}\n"[..],
        ] {
            assert!(
                read_record(&mut BufReader::new(bytes), &mut 100)
                    .await
                    .is_err()
            );
        }
        assert!(
            read_record(
                &mut BufReader::new(&b"{\"status\":\"complete\"}\n"[..]),
                &mut 5
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn explicit_page_selection_rejects_absent_or_reordered_pages() {
        for values in [vec![1, 3], vec![2, 1], vec![1, 1]] {
            let policy: PdfProjectionPolicy =
                serde_json::from_value(serde_json::json!({"mode":"visual","pages":values}))
                    .unwrap();
            assert!(selected_pages(&policy, 2).is_err());
        }
        assert_eq!(
            selected_pages(&PdfProjectionPolicy::Visual { pages: None }, 2).unwrap(),
            [1, 2]
        );
    }
}
