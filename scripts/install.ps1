# Lorekeeper installer for Windows (PowerShell 5.1+)
# Usage: irm https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.ps1 | iex
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

$Repo = 'junyeong-ai/lorekeeper'
$BinaryName = 'lore'
$SkillNames = @('lore-ingest', 'lore-process', 'lore-setup', 'lore-wiki', 'lore-capture', 'lore-extract')
$ReleaseBase = "https://github.com/$Repo/releases/download"
$LatestUrl = "https://github.com/$Repo/releases/latest"

if (-not $InstallDir) {
    $InstallDir = if ($env:LORE_INSTALL_DIR) { $env:LORE_INSTALL_DIR }
                  else { Join-Path $env:USERPROFILE '.local\bin' }
}
if (-not $DataDir) {
    $DataDir = if ($env:LORE_INSTALL_DATA_DIR) { $env:LORE_INSTALL_DATA_DIR }
               else { Join-Path $env:LOCALAPPDATA 'lorekeeper' }
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
    if ($env:LORE_INSTALL_VERSION) { return $env:LORE_INSTALL_VERSION }
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

# Drop config.example.yaml into the config dir `lore` auto-discovers on Windows
# (`%XDG_CONFIG_HOME%\lorekeeper` else `%USERPROFILE%\.config\lorekeeper`), matching
# install.sh — so a binary-only install gives the user a starting point to copy to config.yaml.
function Get-ConfigDir {
    if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME 'lorekeeper' }
    else { Join-Path $env:USERPROFILE '.config\lorekeeper' }
}

function Install-ConfigExample($src) {
    if (-not (Test-Path $src)) { return }
    $dir = Get-ConfigDir
    Write-Step "Installing config example to $dir"
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    Copy-Item -Path $src -Destination (Join-Path $dir 'config.example.yaml') -Force
    Write-Ok "Config example -> $dir\config.example.yaml"
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

function Get-SkillHash($path) {
    if (Test-Path $path) { (Get-FileHash -Algorithm SHA256 $path).Hash } else { $null }
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
        # SKILL.md carries a `version:` stamp (release provenance), but the content
        # hash is the change signal — it catches every edit, stamped or not.
        $existing = Get-SkillHash (Join-Path $target 'SKILL.md')
        if ($existing -and $existing -eq (Get-SkillHash (Join-Path $src 'SKILL.md')) -and -not $Force) {
            Write-Host "  Skill '$skillName' already current; kept" -ForegroundColor DarkGray
            return
        }
        Remove-Item -Path $target -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
    Copy-Item -Path $src -Destination $target -Recurse -Force
    Write-Ok 'Skill installed'
}

# Autonomous scheduled tasks — user-level Claude Code agents the user's scheduler
# fires: `lore-daily-ingest` chains `lore ingest` -> /lore-process -> graph reconcile;
# `lore-weekly-ingest` runs every synthesis period + knowledge audit + retention
# janitors on Mondays. Installed only with the skills (they drive them).
$ScheduledTasks = @('lore-daily-ingest', 'lore-weekly-ingest')

function Install-ScheduledTasks($level, $version, $repoDir) {
    if ($level -eq 'none') { return }
    foreach ($name in $ScheduledTasks) {
        Install-OneScheduledTask $name $version $repoDir
    }
}

function Install-OneScheduledTask($name, $version, $repoDir) {
    $src = $null
    if ($repoDir -and (Test-Path (Join-Path $repoDir "scripts\$name.md"))) {
        $src = Join-Path $repoDir "scripts\$name.md"
    } else {
        $url = "$ReleaseBase/v$version/$name.md"
        $tmp = New-TemporaryFile
        try {
            Invoke-WebRequest -Uri $url -OutFile $tmp.FullName -ErrorAction Stop
            $src = $tmp.FullName
        } catch {
            Write-Warn "Scheduled-task template unavailable at $url; skipping"
            return
        }
    }
    $target = Join-Path $env:USERPROFILE ".claude\scheduled-tasks\$name\SKILL.md"
    Write-Step "Installing scheduled task -> $target"
    if (Test-Path $target) {
        if ((Get-SkillHash $target) -eq (Get-SkillHash $src) -and -not $Force) {
            Write-Host "  Scheduled task '$name' already current; kept" -ForegroundColor DarkGray
            return
        }
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
    Copy-Item -Path $src -Destination $target -Force
    Write-Ok "Scheduled task '$name' installed (register it with your scheduler)"
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
    $version = if ($repoDir) {
        (Select-String -Path (Join-Path $repoDir 'crates\lk-cli\Cargo.toml') -Pattern '^version' | Select-Object -First 1).Line -replace '.*"(.+)".*', '$1'
    } else { 'dev' }
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
    'user'    { Write-Host "  skills    $env:USERPROFILE\.claude\skills\{lore-ingest,lore-process,lore-setup,lore-wiki,lore-capture,lore-extract}" }
    'project' { Write-Host "  skills    .\.claude\skills\{lore-ingest,lore-process,lore-setup,lore-wiki,lore-capture,lore-extract}" }
    'none'    { Write-Host '  skills    (skipped)' }
}
if ($Skill -ne 'none') {
    Write-Host "  schedule  $env:USERPROFILE\.claude\scheduled-tasks\lore-{daily,weekly}-ingest"
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
    $templatesSrc = Join-Path $stage 'templates'
    $configExampleSrc = Join-Path $stage 'config.example.yaml'
} else {
    if (-not $repoDir) { Die '--from-source requires running from a cloned repo' }
    Write-Step 'Building from source (cargo build --release --locked)'
    Push-Location $repoDir
    try { cargo build --release --locked --quiet -p lore }
    finally { Pop-Location }
    $binSrc = Join-Path $repoDir 'target\release\lore.exe'
    $templatesSrc = Join-Path $repoDir 'templates'
    $configExampleSrc = Join-Path $repoDir 'config.example.yaml'
}

Install-Binary $binSrc $InstallDir
Install-Templates $templatesSrc $DataDir
Install-ConfigExample $configExampleSrc

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
    Install-ScheduledTasks $Skill $version $repoDir
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
$cfgDir = Get-ConfigDir
Write-Host 'Next steps:'
Write-Host "  1. Create your config (auto-discovered, no repo needed):"
Write-Host "       Copy-Item '$cfgDir\config.example.yaml' '$cfgDir\config.yaml'; notepad '$cfgDir\config.yaml'"
Write-Host "  2. $BinaryName init credentials   Enter API tokens interactively"
Write-Host "  3. $BinaryName validate           Verify config + credentials"
Write-Host "  4. $BinaryName ingest --dry-run   Preview ingest without writing"
Write-Host "  5. $BinaryName schedule           Generate scheduled task entries"
Write-Host "  /lore-setup  /lore-ingest  /lore-process  /lore-wiki  /lore-capture  /lore-extract"
