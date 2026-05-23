# wiki-ingest uninstaller for Windows
[CmdletBinding()]
param(
    [switch]$Yes,
    [switch]$KeepData
)

$ErrorActionPreference = 'Stop'

$BinaryName = 'wi'
$SkillNames = @('wiki-ingest', 'wi-process')
$InstallDir = if ($env:WI_INSTALL_DIR) { $env:WI_INSTALL_DIR }
              else { Join-Path $env:USERPROFILE '.local\bin' }
$DataDir = if ($env:WI_INSTALL_DATA_DIR) { $env:WI_INSTALL_DATA_DIR }
           else { Join-Path $env:LOCALAPPDATA 'wi-ingest' }

function Write-Step($msg) { Write-Host "▸  $msg" -ForegroundColor Yellow }
function Write-Ok($msg)   { Write-Host "✓  $msg" -ForegroundColor Green }

function Prompt-YesNo($q) {
    if ($Yes) { return $true }
    $r = Read-Host -Prompt "$q [y/N]"
    return $r -match '^[Yy]'
}

Write-Host ''
Write-Host 'wiki-ingest uninstaller' -ForegroundColor White
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
        if (Prompt-YesNo "Remove project-level skill $skillProject?") {
            Write-Step "Removing $skillProject"
            Remove-Item -Recurse -Force $skillProject
            Write-Ok "Project skill removed: $skillName"
            $removed++
        }
    }
}

Write-Host ''
if ($removed -eq 0) {
    Write-Host 'Nothing to uninstall.' -ForegroundColor DarkGray
} else {
    Write-Host "✅ Removed $removed item(s)" -ForegroundColor Green
    Write-Host 'Vault data (.wiki-ingest\) and config.yaml are untouched.' -ForegroundColor DarkGray
}
