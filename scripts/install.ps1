# wiki-ingest installer for Windows (PowerShell 5.1+)
# Usage: irm https://raw.githubusercontent.com/junyeong-ai/wiki-ingest/main/scripts/install.ps1 | iex
[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir,
    [string]$DataDir,
    [ValidateSet('user', 'project', 'none')] [string]$Skill = 'user',
    [switch]$FromSource,
    [switch]$Force,
    [switch]$Yes,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'junyeong-ai/wiki-ingest'
$BinaryName = 'wi'
$SkillNames = @('wiki-ingest', 'wi-process')
$ReleaseBase = "https://github.com/$Repo/releases/download"
$LatestUrl = "https://github.com/$Repo/releases/latest"

if (-not $InstallDir) {
    $InstallDir = if ($env:WI_INSTALL_DIR) { $env:WI_INSTALL_DIR }
                  else { Join-Path $env:USERPROFILE '.local\bin' }
}
if (-not $DataDir) {
    $DataDir = if ($env:WI_INSTALL_DATA_DIR) { $env:WI_INSTALL_DATA_DIR }
               else { Join-Path $env:LOCALAPPDATA 'wi-ingest' }
}

function Write-Step($msg)  { Write-Host "▸  $msg" -ForegroundColor Blue }
function Write-Ok($msg)    { Write-Host "✓  $msg" -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "!  $msg" -ForegroundColor Yellow }
function Die($msg)         { Write-Host "✗ $msg" -ForegroundColor Red; exit 1 }

function Get-LatestVersion {
    $resp = Invoke-WebRequest -Uri $LatestUrl -MaximumRedirection 0 -ErrorAction SilentlyContinue
    $location = $resp.Headers.Location
    if (-not $location) {
        $resp = [System.Net.HttpWebRequest]::Create($LatestUrl)
        $resp.AllowAutoRedirect = $false
        try { $r = $resp.GetResponse(); $location = $r.Headers['Location'] } catch { }
    }
    if ($location -match '/tag/v(.+)$') { return $Matches[1] }
    return $null
}

function Resolve-Version {
    if ($Version) { return $Version }
    if ($env:WI_INSTALL_VERSION) { return $env:WI_INSTALL_VERSION }
    $v = Get-LatestVersion
    if (-not $v) { Die 'Cannot fetch latest version (network issue or no release exists yet)' }
    return $v
}

function Detect-Target {
    $arch = (Get-CimInstance Win32_Processor | Select-Object -First 1).Architecture
    if ($arch -eq 9) { return 'x86_64-pc-windows-msvc' }
    Die "Unsupported Windows architecture: $arch"
}

function Download-Archive($version, $target, $archive) {
    $url = "$ReleaseBase/v$version/$archive"
    $tmp = New-TemporaryFile
    Remove-Item $tmp
    $tmpDir = New-Item -ItemType Directory -Path $tmp.FullName -Force
    Write-Step "Downloading $archive"
    Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmpDir $archive)
    Invoke-WebRequest -Uri "$url.sha256" -OutFile (Join-Path $tmpDir "$archive.sha256")
    Write-Ok 'Downloaded'
    return $tmpDir
}

function Verify-Checksum($tmpDir, $archive) {
    Write-Step 'Verifying SHA256'
    $expected = (Get-Content (Join-Path $tmpDir "$archive.sha256")).Split(' ')[0]
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmpDir $archive)).Hash.ToLower()
    if ($expected -ne $actual) { Die "Checksum mismatch for $archive" }
    Write-Ok 'Checksum match'
}

function Extract-Archive($tmpDir, $archive) {
    Write-Step 'Extracting'
    Expand-Archive -Path (Join-Path $tmpDir $archive) -DestinationPath $tmpDir -Force
    Write-Ok 'Extracted'
}

function Install-Binary($src, $destDir) {
    $dest = Join-Path $destDir "$BinaryName.exe"
    Write-Step "Installing binary to $dest"
    New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    Copy-Item -Path $src -Destination $dest -Force
    Write-Ok $dest
}

function Install-Templates($srcDir, $destBase) {
    if (-not (Test-Path $srcDir)) { Write-Warn "Templates not found at $srcDir; skipping"; return }
    $dest = Join-Path $destBase 'templates'
    Write-Step "Installing templates to $dest"
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Copy-Item -Path (Join-Path $srcDir '*.md.jinja') -Destination $dest -Force
    Write-Ok 'Templates installed'
}

function Download-Skill($version, $skillName) {
    # Mirrors scripts/install.sh download_skill_tarball: fetch and verify
    # `{skill}-skill-v{version}.tar.gz`, extract, and return the skill dir path.
    $archive = "$skillName-skill-v$version.tar.gz"
    $url = "$ReleaseBase/v$version/$archive"
    $tmp = New-TemporaryFile
    Remove-Item $tmp
    $tmpDir = New-Item -ItemType Directory -Path $tmp.FullName -Force
    try {
        Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmpDir $archive)
        Invoke-WebRequest -Uri "$url.sha256" -OutFile (Join-Path $tmpDir "$archive.sha256")
    } catch {
        Write-Warn "Skill archive unavailable for '$skillName'; skipping"
        return $null
    }
    $expected = (Get-Content (Join-Path $tmpDir "$archive.sha256")).Split(' ')[0]
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmpDir $archive)).Hash.ToLower()
    if ($expected -ne $actual) { Write-Warn "Skill checksum mismatch for '$skillName'; skipping"; return $null }
    tar -xzf (Join-Path $tmpDir $archive) -C $tmpDir
    return (Join-Path $tmpDir $skillName)
}

function Install-Skill($level, $src, $skillName) {
    if ($level -eq 'none') { Write-Host '  Skill install skipped' -ForegroundColor DarkGray; return }
    if (-not (Test-Path $src)) { Write-Warn "Skill source not found: $src (skipping)"; return }
    $target = switch ($level) {
        'user'    { Join-Path $env:USERPROFILE ".claude\skills\$skillName" }
        'project' { Join-Path (Get-Location) ".claude\skills\$skillName" }
    }
    Write-Step "Installing skill -> $target"
    if (Test-Path $target) {
        $backup = "${target}.backup_$(Get-Date -Format yyyyMMdd_HHmmss)"
        Copy-Item -Path $target -Destination $backup -Recurse -Force
        Write-Host "  Backup: $backup" -ForegroundColor DarkGray
        Remove-Item -Path $target -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
    Copy-Item -Path $src -Destination $target -Recurse -Force
    Write-Ok 'Skill installed'
}

# ── main ─────────────────────────────────────────────────────────────────

$repoDir = $null
# When run via `irm ... | iex`, $PSCommandPath is empty; guard so the one-liner
# install path doesn't error before downloading anything.
$scriptParent = if ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { $null }
if ($scriptParent -and (Test-Path (Join-Path $scriptParent '..\Cargo.toml'))) {
    $repoDir = (Resolve-Path (Join-Path $scriptParent '..')).Path
}

if ($FromSource -or $env:WI_INSTALL_FROM_SOURCE -eq '1') {
    $method = 'source'
    $target = 'windows-x86_64'
    $version = if ($repoDir) {
        (Select-String -Path (Join-Path $repoDir 'crates\wi-cli\Cargo.toml') -Pattern '^version' | Select-Object -First 1).Line -replace '.*"(.+)".*', '$1'
    } else { 'dev' }
} else {
    $method = 'prebuilt'
    $target = Detect-Target
    $version = Resolve-Version
}

Write-Host ''
Write-Host '╭──────────────────────────────────────────╮' -ForegroundColor Cyan
Write-Host '  wiki-ingest installer' -ForegroundColor Cyan
Write-Host "  v$version • $target" -ForegroundColor DarkGray
Write-Host '╰──────────────────────────────────────────╯' -ForegroundColor Cyan
Write-Host ''
Write-Host 'Review' -ForegroundColor White
Write-Host "  binary    $(Join-Path $InstallDir "$BinaryName.exe") (v$version, $method)"
Write-Host "  templates $(Join-Path $DataDir 'templates')"
switch ($Skill) {
    'user'    { Write-Host "  skills    $env:USERPROFILE\.claude\skills\{wiki-ingest,wi-process}" }
    'project' { Write-Host "  skills    .\.claude\skills\{wiki-ingest,wi-process}" }
    'none'    { Write-Host '  skills    (skipped)' }
}

if ($DryRun) { Write-Host ''; Write-Warn '(dry-run) Not executing'; exit 0 }

if (-not $Yes -and $env:WI_INSTALL_YES -ne '1') {
    $resp = Read-Host -Prompt 'Proceed? [Y/n]'
    if ($resp -match '^[Nn]') { Write-Host '  Aborted by user' -ForegroundColor DarkGray; exit 0 }
}

Write-Host ''

if ($method -eq 'prebuilt') {
    $archive = "$BinaryName-v$version-$target.zip"
    $tmpDir = Download-Archive $version $target $archive
    Verify-Checksum $tmpDir $archive
    Extract-Archive $tmpDir $archive
    $stage = Join-Path $tmpDir "$BinaryName-v$version-$target"
    $binSrc = Join-Path $stage "$BinaryName.exe"
    $templatesSrc = Join-Path $stage 'templates'
} else {
    if (-not $repoDir) { Die '--from-source requires running from a cloned repo' }
    Write-Step 'Building from source (cargo build --release --locked)'
    Push-Location $repoDir
    try { cargo build --release --locked --quiet -p wi }
    finally { Pop-Location }
    $binSrc = Join-Path $repoDir 'target\release\wi.exe'
    $templatesSrc = Join-Path $repoDir 'templates'
}

Install-Binary $binSrc $InstallDir
Install-Templates $templatesSrc $DataDir

if ($Skill -ne 'none') {
    foreach ($skillName in $SkillNames) {
        $skillSrc = if ($repoDir -and (Test-Path (Join-Path $repoDir ".claude\skills\$skillName"))) {
            Join-Path $repoDir ".claude\skills\$skillName"
        } else {
            Download-Skill $version $skillName
        }
        if ($skillSrc) { Install-Skill $Skill $skillSrc $skillName }
        else { Write-Warn "Skill '$skillName' unavailable; skipping" }
    }
}

Write-Host ''
$pathEnv = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($pathEnv -notlike "*$InstallDir*") {
    Write-Warn "$InstallDir is not in your User PATH"
    Write-Host "   Add via: [Environment]::SetEnvironmentVariable('Path', '$InstallDir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
} else {
    Write-Ok "$InstallDir is in PATH"
}

Write-Host ''
Write-Host '✅ Installation complete' -ForegroundColor Green
Write-Host ''
Write-Host 'Next steps:'
Write-Host "  $BinaryName validate              Verify config.yaml in current directory"
Write-Host "  $BinaryName ingest --dry-run      Preview ingest without writing"
Write-Host "  $BinaryName schedule              Generate scheduled task entries"
Write-Host "  /wiki-ingest                  Use as Claude Code skill"
