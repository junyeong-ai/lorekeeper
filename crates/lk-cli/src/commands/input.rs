use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(not(unix))]
use std::process::Stdio;
use std::time::Duration;

use lk_core::content::{InputFormat, InputProcessingConfig};

use super::{GlobalOptions, find_config, load_config};

#[derive(clap::Subcommand)]
pub enum InputCommand {
    /// Install the pinned optional document runtime into a new dedicated directory
    Install {
        directory: PathBuf,
        /// Python 3.10+ interpreter used only to create the isolated environment
        #[arg(long, default_value = "python3")]
        python: PathBuf,
    },
    /// Check configured decoder versions and OCR model availability without reading documents
    Check,
    /// Project one captured file to bounded JSON without publishing or retiring it
    Inspect { path: PathBuf },
}

pub async fn run(opts: &GlobalOptions, command: InputCommand) -> miette::Result<()> {
    match command {
        InputCommand::Install { directory, python } => install(&directory, &python).await,
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
        InputCommand::Inspect { path } => {
            let config = load_config(&find_config(opts)?)?;
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
            let bytes = read_input(&path)?;
            let projection =
                lk_source::content::project(&bytes, format, config.ingest.documents.as_ref())
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

fn read_input(path: &Path) -> miette::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|error| miette::miette!("open input: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| miette::miette!("input metadata: {error}"))?
        .is_file()
    {
        return Err(miette::miette!("input must be a regular file"));
    }
    file.take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| miette::miette!("read input: {error}"))?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(miette::miette!("input exceeds 32 MiB"));
    }
    Ok(bytes)
}

async fn install(directory: &Path, python: &Path) -> miette::Result<()> {
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
    let requirements = directory.join("requirements.txt");
    std::fs::write(&requirements, lk_source::content::REQUIREMENTS)
        .map_err(|error| miette::miette!("write dependency lock: {error}"))?;
    let mut create = tokio::process::Command::new(python);
    create
        .args(["-I", "-m", "venv"])
        .arg(directory.join("venv"));
    install_step(create).await?;
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
    install_step(packages).await?;
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

async fn install_step(mut command: tokio::process::Command) -> miette::Result<()> {
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
            .run(command, Some(Duration::from_secs(300)), false)
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
        match tokio::time::timeout(Duration::from_secs(300), child.wait()).await {
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
        assert_eq!(read_input(&path).unwrap(), b"fixture");
        assert!(read_input(root.path()).is_err());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_input(&path).is_err());
    }

    #[tokio::test]
    async fn installation_refuses_existing_directories_before_executing_anything() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("authored"), b"keep").unwrap();
        assert!(
            install(root.path(), Path::new("does-not-exist"))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
