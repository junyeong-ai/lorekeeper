# Lorekeeper uninstaller for Windows
[CmdletBinding()]
param(
    # The two path flags `install.ps1` takes. Without them an install made with -InstallDir was
    # invisible here: this reported nothing to remove and left the binary where it was.
    [string]$InstallDir,
    [string]$DataDir,
    [switch]$Yes,
    [switch]$KeepData
)

$ErrorActionPreference = 'Stop'

$BinaryName = 'lore'
$SkillNames = @('lore-ingest', 'lore-process', 'lore-setup', 'lore-wiki', 'lore-capture', 'lore-extract', 'lore-day')
if (-not $InstallDir) {
    $InstallDir = if ($env:LORE_INSTALL_DIR) { $env:LORE_INSTALL_DIR }
                  else { Join-Path $env:USERPROFILE '.local\bin' }
}
# The same order `install.ps1` and `lk_dist::layout::data_dir` read.
if (-not $DataDir) {
    $DataDir = if ($env:LORE_INSTALL_DATA_DIR) { $env:LORE_INSTALL_DATA_DIR }
               elseif ($env:XDG_DATA_HOME) { Join-Path $env:XDG_DATA_HOME 'lorekeeper' }
               else { Join-Path $env:LOCALAPPDATA 'lorekeeper' }
}

function Write-Step($msg) { Write-Host "▸  $msg" -ForegroundColor Yellow }
function Write-Ok($msg)   { Write-Host "✓  $msg" -ForegroundColor Green }

function Prompt-YesNo($q) {
    if ($Yes) { return $true }
    $r = Read-Host -Prompt "$q [y/N]"
    return $r -match '^[Yy]'
}

Write-Host ''
Write-Host 'Lorekeeper uninstaller' -ForegroundColor White
Write-Host ''

$removed = 0

# Binary
$bin = Join-Path $InstallDir "$BinaryName.exe"
if (Test-Path $bin) {
    if (Prompt-YesNo "Remove binary $bin?") {
        Write-Step "Removing $bin"
        Remove-Item -Force $bin
        Write-Ok 'Binary removed'
        $removed++
    }
}

# Templates
$templates = Join-Path $DataDir 'templates'
if ((-not $KeepData) -and (Test-Path $templates)) {
    if (Prompt-YesNo "Remove templates $templates?") {
        Write-Step "Removing $templates"
        Remove-Item -Recurse -Force $templates
        try { Remove-Item -Force $DataDir -ErrorAction Stop } catch { }
        Write-Ok 'Templates removed'
        $removed++
    }
}

# What `lore self deploy` wrote into the config directory: the example, and the two records it
# keeps beside it. config.yaml itself is user data and is never touched. Each is asked about on
# its own presence — while the records rode on the example's, a user who tidied the example away
# after copying it kept the `data-dir` record, and a later bare deploy resurrected the directory
# this had just removed.
$configDir = if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME 'lorekeeper' }
             else { Join-Path $env:USERPROFILE '.config\lorekeeper' }
if (-not $KeepData) {
    foreach ($artifact in @('config.example.yaml', 'data-dir', 'deployed-skills')) {
        $path = Join-Path $configDir $artifact
        if (-not (Test-Path $path)) { continue }
        if (Prompt-YesNo "Remove $path?") {
            Write-Step "Removing $path"
            Remove-Item -Force $path
            Write-Ok "Removed: $artifact"
            $removed++
        }
    }
}

# Skills (user-level and project-level for each installed skill name)
foreach ($skillName in $SkillNames) {
    $skillUser = Join-Path $env:USERPROFILE ".claude\skills\$skillName"
    $skillProject = Join-Path (Get-Location) ".claude\skills\$skillName"

    if (Test-Path $skillUser) {
        if (Prompt-YesNo "Remove user-level skill $skillUser?") {
            Write-Step "Removing $skillUser"
            Remove-Item -Recurse -Force $skillUser
            Write-Ok "User skill removed: $skillName"
            $removed++
        }
    }

    if ((Test-Path $skillProject) -and ($skillProject -ne $skillUser)) {
        # Don't remove project-level skills inside a git repo — those are source files.
        $gitCheck = git -C (Split-Path $skillProject) rev-parse --is-inside-work-tree 2>$null
        if ($gitCheck -eq 'true') {
            Write-Host "  Skipping $skillProject (inside git repo)" -ForegroundColor DarkGray
        } elseif (Prompt-YesNo "Remove project-level skill $skillProject?") {
            Write-Step "Removing $skillProject"
            Remove-Item -Recurse -Force $skillProject
            Write-Ok "Project skill removed: $skillName"
            $removed++
        }
    }
}

# Scheduled tasks (user-level only — installed alongside the skills).
# Scheduled tasks installed by versions up to 0.10, before the pipelines replaced them.
# Still offered so an upgrade that never re-ran the installer can clean up.
foreach ($schedName in @('lore-daily-ingest', 'lore-weekly-ingest')) {
    $schedTask = Join-Path $env:USERPROFILE ".claude\scheduled-tasks\$schedName"
    if (Test-Path $schedTask) {
        if (Prompt-YesNo "Remove scheduled task $schedTask?") {
            Write-Step "Removing $schedTask"
            Remove-Item -Recurse -Force $schedTask
            Write-Ok "Scheduled task removed: $schedName"
            $removed++
        }
    }
}

Write-Host ''
if ($removed -eq 0) {
    Write-Host 'Nothing to uninstall.' -ForegroundColor DarkGray
} else {
    Write-Host "✅ Removed $removed item(s)" -ForegroundColor Green
    Write-Host 'Vault data (.lorekeeper\) and config.yaml are untouched.' -ForegroundColor DarkGray
}
