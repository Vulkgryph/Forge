# Forge updater (Windows / PowerShell 5.1+)
#
# Usage:
#   .\update.ps1
#   .\update.ps1 -NoPull
#   .\update.ps1 -Branch stable

param(
    [switch]$NoPull,
    [string]$Branch
)

$ErrorActionPreference = 'Stop'

function Write-Info { param([string]$msg) Write-Host "==> $msg" }
function Write-Ok   { param([string]$msg) Write-Host "[OK] $msg" -ForegroundColor Green }
function Write-Warn { param([string]$msg) Write-Host "warning: $msg" -ForegroundColor Yellow }
function Write-Err  { param([string]$msg) Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

$REPO_ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $REPO_ROOT

if (-not (Test-Path "install.ps1")) { Write-Err "install.ps1 not found. Run this from the Forge source checkout." }
if (-not (Test-Path ".git"))        { Write-Err "This updater expects a git checkout of Forge." }

Write-Info "Updating Forge in $REPO_ROOT"

if (-not $NoPull) {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { Write-Err "git is required to update Forge" }

    if ($Branch) {
        Write-Info "Fetching branch $Branch..."
        git fetch origin $Branch
        $dirty = git status --porcelain
        if ($dirty) { Write-Err "Local changes are present. Commit/stash them or run .\update.ps1 -NoPull." }
        Write-Info "Checking out $Branch..."
        git checkout $Branch
    }

    $dirty = git status --porcelain
    if ($dirty) {
        Write-Warn "Local changes detected; skipping git pull. Run 'git status' to inspect, or use a clean checkout."
    } else {
        $currentBranch = git rev-parse --abbrev-ref HEAD
        if ($currentBranch -eq "HEAD") {
            Write-Warn "Detached HEAD; skipping git pull. Use -Branch <name> to switch branches."
        } else {
            Write-Info "Pulling latest changes for $currentBranch..."
            git pull --ff-only
            Write-Ok "Source updated"
        }
    }
} else {
    Write-Warn "Skipping git pull; rebuilding current checkout"
}

Write-Info "Building and reinstalling Forge..."
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $REPO_ROOT "install.ps1")

Write-Ok "Forge update complete"
Write-Host ""
Write-Host "Run: forge"
