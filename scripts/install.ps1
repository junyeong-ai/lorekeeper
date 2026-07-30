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
# Pinned to `.claude/skills` by `install_scripts_list_every_skill`, because a skill added to the
# repo passed every gate and was then never packaged, published or installed.
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

# Content hash of a whole skill DIRECTORY: every file's relative path and bytes, in a stable
# order. Hashing SKILL.md alone read three skills' `references/` as unchanged forever — a
# references-only edit leaves SKILL.md byte-identical, so the installer reported "already
# current" and the stale reference files survived. Paths are included so a rename is a change,
# and taken relative to the skill dir so the answer does not depend on where the copy lives.
function Get-SkillHash($path) {
    if (-not (Test-Path $path -PathType Container)) { return $null }
    $root = (Resolve-Path $path).Path
    # `-Force` so hidden files count: `find` includes them, and a hash that skipped them would
    # answer "already current" for a change the other installer sees.
    $lines = Get-ChildItem -Path $root -Recurse -File -Force | Sort-Object FullName | ForEach-Object {
        $rel = $_.FullName.Substring($root.Length).TrimStart('\', '/')
        "$rel`n$((Get-FileHash -Algorithm SHA256 $_.FullName).Hash)"
    }
    $stream = [System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes(($lines -join "`n")))
    (Get-FileHash -Algorithm SHA256 -InputStream $stream).Hash
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
        # hash is the change signal — it catches every edit, stamped or not, including one
        # confined to `references/`.
        $existing = Get-SkillHash $target
        if ($existing -and $existing -eq (Get-SkillHash $src) -and -not $Force) {
            Write-Host "  Skill '$skillName' already current; kept" -ForegroundColor DarkGray
            return
        }
        Remove-Item -Path $target -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
    Copy-Item -Path $src -Destination $target -Recurse -Force
    Write-Ok 'Skill installed'
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
    'user'    { Write-Host "  skills    $env:USERPROFILE\.claude\skills\{lore-ingest,lore-process,lore-setup,lore-wiki,lore-capture,lore-extract}" }
    'project' { Write-Host "  skills    .\.claude\skills\{lore-ingest,lore-process,lore-setup,lore-wiki,lore-capture,lore-extract}" }
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
    # ONE provenance rule for every asset: a source install takes them all from the checkout it
    # built from, a prebuilt install takes them all from the release it downloaded. The binary,
    # templates and config example already worked this way; the skills had their own rule, so a
    # clone parked on an old commit installed a downloaded binary beside the working tree's
    # skills, with nothing saying so.
    $repoSkills = if ($method -eq 'source' -and $repoDir) {
        Join-Path $repoDir '.claude\skills'
    } else { $null }
    foreach ($skillName in $SkillNames) {
        $skillSrc = if ($repoSkills -and (Test-Path (Join-Path $repoSkills $skillName))) {
            Join-Path $repoSkills $skillName
        } else {
            Download-Skill $version $skillName
        }
        if ($skillSrc) { Install-Skill $Skill $skillSrc $skillName }
        else { Write-Warn "Skill '$skillName' unavailable; skipping" }
    }
    Remove-LegacyScheduledTasks
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
Write-Host "  5. $BinaryName schedule           Print the cron cadences from your config"
Write-Host "  /lore-setup  /lore-ingest  /lore-process  /lore-wiki  /lore-capture  /lore-extract"
