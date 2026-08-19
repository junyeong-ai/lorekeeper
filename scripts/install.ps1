# Lorekeeper installer for Windows (PowerShell 5.1+)
# Usage: irm https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.ps1 | iex
[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir,
    [string]$DataDir,
    [ValidateSet('user', 'project', 'none')] [string]$Skill = 'user',
    [switch]$FromSource,
    [switch]$Yes,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'junyeong-ai/lorekeeper'
$BinaryName = 'lore'
$ReleaseBase = "https://github.com/$Repo/releases/download"
$LatestUrl = "https://github.com/$Repo/releases/latest"
$ApiLatestUrl = "https://api.github.com/repos/$Repo/releases/latest"

if (-not $InstallDir) {
    $InstallDir = if ($env:LORE_INSTALL_DIR) { $env:LORE_INSTALL_DIR }
                  else { Join-Path $env:USERPROFILE '.local\bin' }
}
# The same order `lk_dist::layout::data_dir` reads, so `lore self status` looks where this
# installer wrote. Checking only LOCALAPPDATA left an XDG_DATA_HOME install invisible to it.
if (-not $DataDir) {
    $DataDir = if ($env:LORE_INSTALL_DATA_DIR) { $env:LORE_INSTALL_DATA_DIR }
               elseif ($env:XDG_DATA_HOME) { Join-Path $env:XDG_DATA_HOME 'lorekeeper' }
               else { Join-Path $env:LOCALAPPDATA 'lorekeeper' }
}

function Write-Step($msg)  { Write-Host "▸  $msg" -ForegroundColor Blue }
function Write-Ok($msg)    { Write-Host "✓  $msg" -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "!  $msg" -ForegroundColor Yellow }
function Die($msg)         { Write-Host "✗ $msg" -ForegroundColor Red; exit 1 }

# The latest published release.
#
# Two sources answer this and they disagree for minutes at a time: the release page is a cached
# view that trails the API right after a release is published, which is exactly when someone
# runs this. Read in that window it names the release before, and the install lands on it while
# reporting success. So the API settles it, and the page answers only where the API could not.
function Get-LatestVersion {
    try {
        $tag = (Invoke-RestMethod -Uri $ApiLatestUrl -Headers @{ Accept = 'application/vnd.github+json' }).tag_name
        if ($tag) { return ($tag -replace '^v', '') }
    } catch { }

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
    # Every URL below is built as `v$version`, so the version here is the bare number. The tag
    # on the releases page is `v0.21.0` and that is what a user copies, which made
    # `-Version v0.21.0` request `.../vv0.21.0/lore-vv0.21.0-…` and 404.
    if ($Version) { return ($Version -replace '^v', '') }
    if ($env:LORE_INSTALL_VERSION) { return ($env:LORE_INSTALL_VERSION -replace '^v', '') }
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

# Drop config.example.yaml into the config dir `lore` auto-discovers on Windows
# (`%XDG_CONFIG_HOME%\lorekeeper` else `%USERPROFILE%\.config\lorekeeper`), matching
# install.sh — so a binary-only install gives the user a starting point to copy to config.yaml.
function Get-ConfigDir {
    if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME 'lorekeeper' }
    else { Join-Path $env:USERPROFILE '.config\lorekeeper' }
}

# Everything besides the binary is written by the binary.
#
# The skills, the rendering templates and the config example are compiled into `lore`, so the
# version that deploys them is the version that carries them — there is no second artifact to
# fetch, verify, or find a stale copy of. The pipeline scripts are POSIX shell fired by a
# system scheduler, so `self deploy` writes them and Windows simply has no scheduler to point
# at them. This is also what a later `lore self update` runs.
function Deploy-Artifacts($bin, $level) {
    Write-Step 'Deploying skills, templates and the config example'
    # `lore` reports progress on stderr, and under `$ErrorActionPreference = 'Stop'` Windows
    # PowerShell turns a native command's stderr into a terminating NativeCommandError the
    # moment the stream is redirected — throwing on a SUCCESSFUL deploy, before its exit code
    # is ever read. The exit code is what decides here, so the preference is relaxed for
    # exactly this call.
    #
    # `$exit` is seeded pessimistically because `Set-StrictMode` raises a TERMINATING error for
    # an unset variable and the relaxed preference governs only non-terminating ones — so a
    # binary that never ran at all still reaches the message naming the repair, instead of
    # "$LASTEXITCODE cannot be retrieved because it has not been set".
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $exit = 1
    try {
        & $bin self deploy --skills $level --data-dir $DataDir
        $exit = $LASTEXITCODE
    } catch {
        Write-Warn $_.Exception.Message
    } finally { $ErrorActionPreference = $previous }
    if ($exit -ne 0) { Die "Deploy failed; the binary is installed - run: $bin self deploy" }
}

# Scheduled-task definitions installed by versions up to 0.10. They drove `lore ingest`
# through Claude Desktop, so a day was silently skipped whenever the app was not running,
# and their drain contract no longer matches the code. The replacement is a pair of POSIX
# pipeline scripts fired by a system scheduler (launchd or cron) — Unix only, so this
# installer removes the superseded definitions without installing a substitute. On Windows,
# schedule the equivalent stages yourself with Task Scheduler: `lore ingest`, then
# `claude -p /lore-process`, then `lore queue apply` and `lore graph backlinks-sync`.
$LegacyScheduledTasks = @('lore-daily-ingest', 'lore-weekly-ingest')

function Remove-LegacyScheduledTasks {
    $removed = 0
    foreach ($name in $LegacyScheduledTasks) {
        $dir = Join-Path $env:USERPROFILE ".claude\scheduled-tasks\$name"
        if (Test-Path $dir) {
            Remove-Item -Path $dir -Recurse -Force
            $removed++
        }
    }
    if ($removed -gt 0) {
        Write-Ok "Removed $removed superseded scheduled task(s)"
        Write-Host "  Also drop their entries from .claude\scheduled-tasks\registry.json if present" -ForegroundColor DarkGray
    }
}

# The version a checkout declares, read from `[workspace.package]` — single-sourced there, so
# every crate inherits it and `crates\*\Cargo.toml` no longer carries a literal. Leading
# whitespace and either quote style are accepted because TOML allows both, and a reader that
# required column zero and double quotes returned nothing for a manifest that is still valid.
function Get-RepoVersion($repo) {
    $inSection = $false
    foreach ($line in Get-Content (Join-Path $repo 'Cargo.toml')) {
        if ($line -match '^\s*\[workspace\.package\]\s*$') { $inSection = $true; continue }
        if ($line -match '^\s*\[') { $inSection = $false; continue }
        if ($inSection -and $line -match '^\s*version\s*=\s*["'']([^"'']+)["'']') {
            return $Matches[1]
        }
    }
    return 'dev'
}

# ── main ─────────────────────────────────────────────────────────────────

$repoDir = $null
# When run via `irm ... | iex`, $PSCommandPath is empty; guard so the one-liner
# install path doesn't error before downloading anything.
$scriptParent = if ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { $null }
if ($scriptParent -and (Test-Path (Join-Path $scriptParent '..\Cargo.toml'))) {
    $repoDir = (Resolve-Path (Join-Path $scriptParent '..')).Path
}

if ($FromSource -or $env:LORE_INSTALL_FROM_SOURCE -eq '1') {
    $method = 'source'
    $target = 'windows-x86_64'
    $version = if ($repoDir) { Get-RepoVersion $repoDir } else { 'dev' }
} else {
    $method = 'prebuilt'
    $target = Detect-Target
    $version = Resolve-Version
}

Write-Host ''
Write-Host '╭──────────────────────────────────────────╮' -ForegroundColor Cyan
Write-Host '  Lorekeeper installer' -ForegroundColor Cyan
Write-Host "  v$version • $target" -ForegroundColor DarkGray
Write-Host '╰──────────────────────────────────────────╯' -ForegroundColor Cyan
Write-Host ''
Write-Host 'Review' -ForegroundColor White
Write-Host "  binary    $(Join-Path $InstallDir "$BinaryName.exe") (v$version, $method)"
Write-Host "  templates $(Join-Path $DataDir 'templates')"
switch ($Skill) {
    'user'    { Write-Host "  skills    $env:USERPROFILE\.claude\skills\lore-*" }
    'project' { Write-Host "  skills    .\.claude\skills\lore-*" }
    'none'    { Write-Host '  skills    (skipped)' }
}
if ($Skill -ne 'none') {
    Write-Host "  schedule  (Windows: register the stages with Task Scheduler)" -ForegroundColor DarkGray
}

if ($DryRun) { Write-Host ''; Write-Warn '(dry-run) Not executing'; exit 0 }

if (-not $Yes -and $env:LORE_INSTALL_YES -ne '1') {
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
} else {
    if (-not $repoDir) { Die '--from-source requires running from a cloned repo' }
    Write-Step 'Building from source (cargo build --release --locked)'
    Push-Location $repoDir
    try { cargo build --release --locked --quiet -p lore }
    finally { Pop-Location }
    $binSrc = Join-Path $repoDir 'target\release\lore.exe'
}

Install-Binary $binSrc $InstallDir
Deploy-Artifacts (Join-Path $InstallDir "$BinaryName.exe") $Skill
Remove-LegacyScheduledTasks

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
$cfgDir = Get-ConfigDir
Write-Host 'Next steps:'
Write-Host "  1. Create your config (auto-discovered, no repo needed):"
Write-Host "       Copy-Item '$cfgDir\config.example.yaml' '$cfgDir\config.yaml'; notepad '$cfgDir\config.yaml'"
Write-Host "  2. $BinaryName init credentials   Enter API tokens interactively"
Write-Host "  3. $BinaryName validate           Verify config + credentials"
Write-Host "  4. $BinaryName ingest --dry-run   Preview ingest without writing"
Write-Host "  5. $BinaryName schedule           Print the cron cadences from your config"
