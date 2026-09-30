# Forge bootstrap installer (Windows / PowerShell 5.1+)
#
# Quick install:
#
#   irm https://raw.githubusercontent.com/Vulkgryph/Forge/main/bootstrap.ps1 | iex
#
# Environment overrides:
#   $env:FORGE_REPO    repository URL (default: github.com/Vulkgryph/Forge)
#   $env:FORGE_DEST    clone destination (default: $HOME\forge)
#   $env:FORGE_BRANCH  branch to check out (default: main)

$ErrorActionPreference = 'Stop'

$repo   = if ($env:FORGE_REPO)   { $env:FORGE_REPO }   else { "https://github.com/Vulkgryph/Forge.git" }
$dest   = if ($env:FORGE_DEST)   { $env:FORGE_DEST }   else { Join-Path $env:USERPROFILE "forge" }
$branch = if ($env:FORGE_BRANCH) { $env:FORGE_BRANCH } else { "main" }

function Write-Info { param([string]$msg) Write-Host "==> $msg" }
function Write-Ok   { param([string]$msg) Write-Host "[OK] $msg" -ForegroundColor Green }
function Write-Warn { param([string]$msg) Write-Host "warning: $msg" -ForegroundColor Yellow }
function Write-Err  { param([string]$msg) Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

# -------------------------------------------------------------------
# 1. Preflight - make sure git is available
# -------------------------------------------------------------------
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Write-Err "git is not installed and winget is unavailable. Install Git for Windows manually (https://git-scm.com/), then re-run."
    }
    Write-Warn "git not found - installing via winget..."
    winget install --id Git.Git --silent --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) { Write-Err "winget failed to install Git" }
    $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH", "User")
    Write-Ok "Git installed"
}

# -------------------------------------------------------------------
# 2. Clone or update
# -------------------------------------------------------------------
if (Test-Path $dest) {
    if (Test-Path (Join-Path $dest ".git")) {
        Write-Info "Existing checkout at $dest - pulling latest"
        Push-Location $dest
        git fetch --quiet origin $branch
        git checkout --quiet $branch
        git pull --ff-only --quiet
        Pop-Location
        Write-Ok "Updated $dest"
    } else {
        Write-Err "$dest exists and is not a git checkout. Move it aside or set `$env:FORGE_DEST to a different path."
    }
} else {
    Write-Info "Cloning $repo -> $dest"
    git clone --quiet --branch $branch $repo $dest
    Write-Ok "Cloned to $dest"
}

# -------------------------------------------------------------------
# 3. Hand off to install.ps1
# -------------------------------------------------------------------
Set-Location $dest
$installScript = Join-Path $dest "install.ps1"
if (-not (Test-Path $installScript)) {
    Write-Err "install.ps1 not found at $installScript - repo may be broken or on a stale branch."
}

Write-Info "Running install.ps1"
Write-Host ""

# Bypass execution policy for this one-shot run since the user invoked us already.
& powershell -NoProfile -ExecutionPolicy Bypass -File $installScript
