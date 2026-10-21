use std::path::{Path, PathBuf};
#[cfg(not(unix))]
use std::process::Stdio;
use std::time::Duration;

use lk_core::content::{InputFormat, InputProcessingConfig, PdfProjectionPolicy};

use super::{GlobalOptions, find_config, load_config};

#[derive(clap::Subcommand)]
pub enum InputCommand {
    /// Install the pinned optional document runtime into a new dedicated directory
    Install {
        /// New dedicated directory whose parent already exists
        directory: PathBuf,
        /// Python 3.11+ interpreter used only to create the isolated environment
        #[arg(long, default_value = "python3")]
        python: PathBuf,
    },
    /// Install PaddleOCR-VL 1.6 and verified model artifacts on Apple Silicon
    InstallVision {
        /// New dedicated directory for the isolated runtimes and model artifacts
        directory: PathBuf,
        /// Python 3.12 interpreter used to create both isolated environments
        #[arg(long, default_value = "python3.12")]
        python: PathBuf,
    },
    /// Check configured decoder versions and OCR model availability without reading documents
    Check,
    /// Project one captured file to bounded JSON without publishing or retiring it
    Inspect {
        /// Regular DOCX, XLSX, PDF, PNG, JPEG, TIFF or WebP input file
        path: PathBuf,
        /// Override the configured PDF extraction scope
        #[arg(long, value_enum)]
        pdf_mode: Option<PdfMode>,
        /// Physical PDF pages to recognize; requires --pdf-mode visual
        #[arg(long, value_delimiter = ',', requires = "pdf_mode")]
        visual_pages: Option<Vec<std::num::NonZeroUsize>>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum PdfMode {
    Native,
    Visual,
}

#[derive(Clone, Copy)]
enum RuntimeKind {
    Native,
    PaddleOcrVl,
}

pub async fn run(opts: &GlobalOptions, command: InputCommand) -> miette::Result<()> {
    match command {
        InputCommand::Install { directory, python } => {
            install(&directory, &python, RuntimeKind::Native).await
        }
        InputCommand::InstallVision { directory, python } => {
            install(&directory, &python, RuntimeKind::PaddleOcrVl).await
        }
        InputCommand::Check => {
            let config = load_config(&find_config(opts)?)?;
            let processing = config.ingest.documents.as_ref().ok_or_else(|| miette::miette!("configure ingest.documents first; use `lore input install DIRECTORY` for the optional runtime"))?;
            let runtime = lk_source::content::check(processing)
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&runtime)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        InputCommand::Inspect {
            path,
            pdf_mode,
            visual_pages,
        } => {
            let mut config = load_config(&find_config(opts)?)?;
            let format = path
                .extension()
                .and_then(|value| value.to_str())
                .and_then(InputFormat::from_extension)
                .filter(|format| format.requires_processor())
                .ok_or_else(|| {
                    miette::miette!(
                        "input inspect supports DOCX, XLSX, PDF, PNG, JPEG, TIFF and WebP"
                    )
                })?;
            if pdf_mode.is_some() || visual_pages.is_some() {
                if format != InputFormat::Pdf {
                    return Err(miette::miette!("PDF scope options require a PDF input"));
                }
                let policy = match pdf_mode {
                    Some(PdfMode::Visual) => PdfProjectionPolicy::Visual {
                        pages: visual_pages,
                    },
                    Some(PdfMode::Native) if visual_pages.is_none() => {
                        PdfProjectionPolicy::Native {}
                    }
                    _ => return Err(miette::miette!("--visual-pages requires --pdf-mode visual")),
                };
                policy
                    .validate()
                    .map_err(|error| miette::miette!("{error}"))?;
                let processing = config
                    .ingest
                    .documents
                    .as_mut()
                    .ok_or_else(|| miette::miette!("configure ingest.documents first"))?;
                if matches!(policy, PdfProjectionPolicy::Visual { .. })
                    && processing.vision.is_none()
                {
                    return Err(miette::miette!(
                        "PDF visual projection requires ingest.documents.vision"
                    ));
                }
                processing.pdf = Some(policy);
                if let Some(ocr) = processing.ocr.as_mut() {
                    ocr.pdf = false;
                }
            }
            let captured = read_input(&path, config.ingest.originals.max_entry_bytes.get())?;
            let cache = lk_source::content::ProjectionCache::for_vault(
                &config.vault.root_path(),
                lk_source::content::CacheAccess::ReadOnly,
            );
            let projection = lk_source::content::project_file_with_cache(
                &captured,
                format,
                config.ingest.documents.as_ref(),
                Some(&cache),
            )
            .await
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&projection)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
    }
}

fn read_input(path: &Path, limit: usize) -> miette::Result<lk_core::captured_file::CapturedFile> {
    let file = std::fs::File::open(path).map_err(|error| miette::miette!("open input: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| miette::miette!("input metadata: {error}"))?
        .is_file()
    {
        return Err(miette::miette!("input must be a regular file"));
    }
    lk_core::captured_file::CapturedFile::capture(file, limit)
        .map_err(|error| miette::miette!("capture input: {error}"))
}

async fn install(directory: &Path, python: &Path, kind: RuntimeKind) -> miette::Result<()> {
    if matches!(kind, RuntimeKind::PaddleOcrVl)
        && !cfg!(all(target_os = "macos", target_arch = "aarch64"))
    {
        return Err(miette::miette!(
            "the bundled PaddleOCR-VL runtime requires Apple Silicon macOS"
        ));
    }
    let directory = if directory.is_absolute() {
        directory.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| miette::miette!("current directory: {error}"))?
            .join(directory)
    };
    // Never overwrite or adopt a directory the installer did not create.
    std::fs::create_dir(&directory).map_err(|error| {
        miette::miette!(
            "create new runtime directory {}: {error}",
            directory.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| miette::miette!("runtime permissions: {error}"))?;
    }
    if matches!(kind, RuntimeKind::PaddleOcrVl) {
        let layout = directory.join("layout");
        let recognition = directory.join("vlm");
        std::fs::create_dir(&layout).map_err(|error| miette::miette!("{error}"))?;
        std::fs::create_dir(&recognition).map_err(|error| miette::miette!("{error}"))?;
        let executable = install_environment(
            &layout,
            python,
            lk_source::content::VISION_REQUIREMENTS,
            Duration::from_secs(1800),
        )
        .await?;
        let vlm_executable = install_environment(
            &recognition,
            python,
            lk_source::content::MLX_REQUIREMENTS,
            Duration::from_secs(1800),
        )
        .await?;
        let models = directory.join("models");
        lk_source::content::install_vision(&executable, &vlm_executable, &models)
            .await
            .map_err(|error| {
                miette::miette!(
                    "{error}; incomplete runtime retained at {}",
                    directory.display()
                )
            })?;
        println!(
            "ingest:\n  documents:\n    vision:\n      engine: paddle-ocr-vl\n      python: {}\n      vlm_python: {}\n      models_dir: {}",
            serde_json::to_string(&executable).map_err(|error| miette::miette!("{error}"))?,
            serde_json::to_string(&vlm_executable).map_err(|error| miette::miette!("{error}"))?,
            serde_json::to_string(&models).map_err(|error| miette::miette!("{error}"))?
        );
        eprintln!(
            "Vision runtimes and pinned model artifacts verified. Merge this block into ingest.documents; PDF scope is configured separately."
        );
        return Ok(());
    }
    let executable = install_environment(
        &directory,
        python,
        lk_source::content::REQUIREMENTS,
        Duration::from_secs(300),
    )
    .await?;
    let processing: InputProcessingConfig =
        serde_json::from_value(serde_json::json!({"python": executable}))
            .map_err(|error| miette::miette!("runtime config: {error}"))?;
    lk_source::content::check(&processing)
        .await
        .map_err(|error| miette::miette!("runtime check: {error}"))?;
    println!(
        "ingest:\n  documents:\n    python: {}",
        serde_json::to_string(&executable)
            .map_err(|error| miette::miette!("runtime path: {error}"))?
    );
    eprintln!(
        "Runtime installed and dependency versions verified. Add this block to your selected config. OCR requires explicit traineddata and language configuration; no models were downloaded."
    );
    Ok(())
}

async fn install_environment(
    directory: &Path,
    python: &Path,
    lock: &str,
    timeout: Duration,
) -> miette::Result<PathBuf> {
    let requirements = directory.join("requirements.txt");
    std::fs::write(&requirements, lock)
        .map_err(|error| miette::miette!("write dependency lock: {error}"))?;
    let mut create = tokio::process::Command::new(python);
    create
        .args(["-I", "-m", "venv"])
        .arg(directory.join("venv"));
    install_step(create, Duration::from_secs(300)).await?;
    let executable = directory.join(if cfg!(windows) {
        "venv/Scripts/python.exe"
    } else {
        "venv/bin/python"
    });
    let mut packages = tokio::process::Command::new(&executable);
    packages
        .args([
            "-I",
            "-m",
            "pip",
            "--isolated",
            "install",
            "--disable-pip-version-check",
            "--no-cache-dir",
            "--require-hashes",
            "--only-binary=:all:",
            "--index-url",
            "https://pypi.org/simple",
            "-r",
        ])
        .arg(&requirements);
    install_step(packages, timeout).await?;
    Ok(executable)
}

async fn install_step(
    mut command: tokio::process::Command,
    timeout: Duration,
) -> miette::Result<()> {
    command.env_clear();
    for name in [
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "TMPDIR",
        "HOME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(unix)]
    {
        let result = super::execution::Supervisor::new()?
            .run(command, Some(timeout), false)
            .await?;
        if !result.success {
            return Err(miette::miette!(
                "runtime installation failed: {}; the incomplete directory was retained for inspection",
                result
                    .detail
                    .unwrap_or_else(|| "unknown child failure".into())
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| miette::miette!("start runtime installation: {error}"))?;
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(miette::miette!(
                "runtime installation {status}; the directory is incomplete and was retained for inspection"
            )),
            Ok(Err(error)) => Err(miette::miette!("runtime installation: {error}")),
            Err(_) => {
                let _ = child.kill().await;
                Err(miette::miette!(
                    "runtime installation timed out; the incomplete directory was retained for inspection"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_reads_only_regular_bounded_inputs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.pdf");
        std::fs::write(&path, b"fixture").unwrap();
        assert_eq!(
            read_input(&path, 32 * 1024 * 1024)
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            b"fixture"
        );
        assert!(read_input(root.path(), 32 * 1024 * 1024).is_err());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_input(&path, 32 * 1024 * 1024).is_err());
    }

    #[tokio::test]
    async fn installation_refuses_existing_directories_before_executing_anything() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("authored"), b"keep").unwrap();
        for kind in [RuntimeKind::Native, RuntimeKind::PaddleOcrVl] {
            assert!(
                install(root.path(), Path::new("does-not-exist"), kind)
                    .await
                    .is_err()
            );
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
