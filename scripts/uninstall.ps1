# wiki-ingest uninstaller for Windows
[CmdletBinding()]
param(
    [switch]$Yes,
    [switch]$KeepData
)

$ErrorActionPreference = 'Stop'

$BinaryName = 'wi'
$SkillName = 'wiki-ingest'
$InstallDir = if ($env:WI_INSTALL_DIR) { $env:WI_INSTALL_DIR }
              else { Join-Path $env:USERPROFILE '.local\bin' }
$DataDir = if ($env:WI_INSTALL_DATA_DIR) { $env:WI_INSTALL_DATA_DIR }
           else { Join-Path $env:LOCALAPPDATA 'wi-ingest' }
$SkillUser = Join-Path $env:USERPROFILE ".claude\skills\$SkillName"
$SkillProject = Join-Path (Get-Location) ".claude\skills\$SkillName"

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

# User skill
if (Test-Path $SkillUser) {
    if (Prompt-YesNo "Remove user-level skill $SkillUser?") {
        Write-Step "Removing $SkillUser"
        Remove-Item -Recurse -Force $SkillUser
        Write-Ok 'User skill removed'
        $removed++
    }
}

# Project skill
if ((Test-Path $SkillProject) -and ($SkillProject -ne $SkillUser)) {
    if (Prompt-YesNo "Remove project-level skill $SkillProject?") {
        Write-Step "Removing $SkillProject"
        Remove-Item -Recurse -Force $SkillProject
        Write-Ok 'Project skill removed'
        $removed++
    }
}

Write-Host ''
if ($removed -eq 0) {
    Write-Host 'Nothing to uninstall.' -ForegroundColor DarkGray
} else {
    Write-Host "✅ Removed $removed item(s)" -ForegroundColor Green
    Write-Host 'Vault data (.wiki-ingest\) and config.yaml are untouched.' -ForegroundColor DarkGray
}
