# Forge installer for Windows (PowerShell 5.1+)
#
# Usage:
#   .\install.ps1
#
# Builds forge-agent.exe and the UI bundle, installs symlink-style wrappers
# into %USERPROFILE%\.local\bin, and runs the same 5-way config wizard as
# install.sh on Unix.

$ErrorActionPreference = 'Stop'

# ANSI / colored output (Windows 10+ Terminal supports this out of the box)
function Write-Info  { param([string]$msg) Write-Host "==> $msg" -ForegroundColor White -NoNewline; Write-Host "" }
function Write-Ok    { param([string]$msg) Write-Host "[OK] $msg" -ForegroundColor Green }
function Write-Warn  { param([string]$msg) Write-Host "warning: $msg" -ForegroundColor Yellow }
function Write-Err   { param([string]$msg) Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

$INSTALL_BIN   = Join-Path $env:USERPROFILE ".local\bin"
$INSTALL_SHARE = Join-Path $env:USERPROFILE ".local\share\forge"
$CONFIG_DIR    = Join-Path $env:USERPROFILE ".config\forge"
$CONFIG_FILE   = Join-Path $CONFIG_DIR "config.toml"

# -------------------------------------------------------------------
# 1. Detect arch
# -------------------------------------------------------------------
$arch = $env:PROCESSOR_ARCHITECTURE
Write-Info "Detected Windows $arch"

# -------------------------------------------------------------------
# 2. Check / install preflight tooling via winget
# -------------------------------------------------------------------
function Ensure-Tool {
    param([string]$cmdName, [string]$wingetId, [string]$friendlyName)
    if (Get-Command $cmdName -ErrorAction SilentlyContinue) {
        # Don't probe --version: rustup's cargo proxy symlink can fail to
        # execute even when Get-Command resolves it. Presence is enough.
        Write-Ok "$friendlyName found"
    } else {
        if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
            Write-Err "$friendlyName not installed and winget unavailable. Install $friendlyName manually (https://winget.run/), then re-run."
        }
        Write-Warn "$friendlyName not found - installing via winget..."
        winget install --id $wingetId --silent --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { Write-Err "winget failed to install $friendlyName" }
        # winget may not refresh PATH in the current shell - pull from registry
        $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH", "User")
        Write-Ok "$friendlyName installed"
    }
}

Ensure-Tool -cmdName "git"    -wingetId "Git.Git"          -friendlyName "Git"
Ensure-Tool -cmdName "rustup" -wingetId "Rustlang.Rustup"  -friendlyName "Rust (rustup)"
Ensure-Tool -cmdName "bun"    -wingetId "Oven-sh.Bun"      -friendlyName "Bun"

# Make sure a stable toolchain is installed and active. Idempotent.
& rustup default stable | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Err "rustup default stable failed - rustup may be broken." }

# -------------------------------------------------------------------
# 3. Build
# -------------------------------------------------------------------
$REPO_ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $REPO_ROOT

Write-Info "Building forge-agent (Rust)..."
# rustup's proxy shims in ~\.cargo\bin can be broken on some Windows installs
# (0-byte symlinks that Windows refuses to traverse, "untrusted mount point"
# error 448). Find the active toolchain's real binaries via `rustup which` and
# prepend that directory to PATH so cargo finds rustc through real .exe files.
$rustcPath = & rustup which rustc 2>$null
if ($LASTEXITCODE -ne 0 -or -not $rustcPath) { Write-Err "rustup which rustc failed" }
$toolchainBin = Split-Path -Parent $rustcPath
$env:PATH = "$toolchainBin;$env:PATH"
$cargoExe = Join-Path $toolchainBin "cargo.exe"

& $cargoExe build --release
if ($LASTEXITCODE -ne 0) { Write-Err "cargo build failed" }
Write-Ok "forge-agent built"

Write-Info "Building UI (Bun)..."
Push-Location ui
bun install
if ($LASTEXITCODE -ne 0) { Pop-Location; Write-Err "bun install failed" }
bun run build
if ($LASTEXITCODE -ne 0) { Pop-Location; Write-Err "bun run build failed" }
Pop-Location
Write-Ok "UI built"

# -------------------------------------------------------------------
# 4. Install
# -------------------------------------------------------------------
Write-Info "Installing to $INSTALL_BIN and $INSTALL_SHARE..."

New-Item -ItemType Directory -Force -Path $INSTALL_BIN, (Join-Path $INSTALL_SHARE "ui\dist") | Out-Null

# Copy (not symlink - Windows symlinks require admin or developer mode)
Copy-Item -Force "$REPO_ROOT\target\release\forge-agent.exe" "$INSTALL_BIN\forge-agent.exe"
Copy-Item -Force "$REPO_ROOT\ui\dist\forge.js"               "$INSTALL_SHARE\ui\dist\forge.js"

# Bundle ui/node_modules and package.json so bun can resolve dependencies at runtime
if (Test-Path "$REPO_ROOT\ui\node_modules") {
    if (Test-Path "$INSTALL_SHARE\ui\node_modules") { Remove-Item -Recurse -Force "$INSTALL_SHARE\ui\node_modules" }
    Copy-Item -Recurse "$REPO_ROOT\ui\node_modules" "$INSTALL_SHARE\ui\node_modules"
}
Copy-Item -Force "$REPO_ROOT\ui\package.json" "$INSTALL_SHARE\ui\package.json"

# Version stamp - graceful when not in a git checkout
try {
    $sha = git rev-parse HEAD 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $sha) { $sha = "unknown" }
} catch { $sha = "unknown" }
Set-Content -Path (Join-Path $INSTALL_SHARE "version") -Value $sha

# Wrapper: forge.cmd
$wrapper = @"
@echo off
bun run "%USERPROFILE%\.local\share\forge\ui\dist\forge.js" %*
"@
Set-Content -Path "$INSTALL_BIN\forge.cmd" -Value $wrapper -Encoding ASCII

# Updater wrapper: forge-update.cmd
$updater = @"
@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "$REPO_ROOT\update.ps1" %*
"@
Set-Content -Path "$INSTALL_BIN\forge-update.cmd" -Value $updater -Encoding ASCII

Write-Ok "Installed forge       -> $INSTALL_BIN\forge.cmd"
Write-Ok "Installed forge-agent -> $INSTALL_BIN\forge-agent.exe"
Write-Ok "Installed forge-update-> $INSTALL_BIN\forge-update.cmd"

# -------------------------------------------------------------------
# 5. PATH
# -------------------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if (-not ($userPath -split ';' | Where-Object { $_ -ieq $INSTALL_BIN })) {
    $newPath = if ($userPath) { "$userPath;$INSTALL_BIN" } else { $INSTALL_BIN }
    [Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
    Write-Warn "Added $INSTALL_BIN to your user PATH. Open a new PowerShell window to pick it up."
}

# -------------------------------------------------------------------
# 6. Config
# -------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $CONFIG_DIR | Out-Null

if (Test-Path $CONFIG_FILE) {
    Write-Info "Existing config at $CONFIG_FILE - leaving as-is. Edit by hand or delete and re-run to use the wizard."
} else {
    # Read-Host in PowerShell 5.1 only reads from the interactive host, not piped stdin.
    # Read-Line uses [Console]::In so the wizard works under both terminal and pipe.
    function Read-Line {
        param([string]$prompt)
        Write-Host -NoNewline $prompt
        return [Console]::In.ReadLine()
    }

    function Read-Required {
        param([string]$prompt)
        $v = ""
        while ([string]::IsNullOrWhiteSpace($v)) {
            $v = Read-Line $prompt
            if ([string]::IsNullOrWhiteSpace($v)) { Write-Host "    Required." }
        }
        return $v
    }

    Write-Host ""
    Write-Host "Configure Forge" -ForegroundColor White
    Write-Host "Choose how you want Forge to reach an LLM."
    Write-Host ""
    Write-Host "  1) Local LLM server   (LM Studio, Ollama, llama.cpp, vLLM, etc.)"
    Write-Host "  2) Claude subscription   (claude.ai / Pro / Max - OAuth login)"
    Write-Host "  3) ChatGPT Codex subscription   (OAuth login)"
    Write-Host "  4) Direct API key   (Anthropic API, OpenAI API, OpenRouter, custom OpenAI-compatible)"
    Write-Host "  5) Skip - I'll edit the config file myself"
    Write-Host ""

    $choice = ""
    while ($choice -notmatch '^[1-5]$') {
        $choice = Read-Line "  Choice [1-5]: "
    }

    $POST_INSTALL_HINT = ""
    $LOGIN_CMD = ""; $LOGIN_LABEL = ""

    switch ($choice) {
        '1' {
            Write-Host ""
            Write-Host "  Examples:"
            Write-Host "    LM Studio default base URL:  http://127.0.0.1:1234/v1"
            Write-Host "    Ollama default base URL:     http://127.0.0.1:11434/v1"
            Write-Host "    llama.cpp server default:    http://127.0.0.1:8080/v1"
            Write-Host ""

            $epName    = Read-Required "    Endpoint name (any label you'll recognize): "
            $epUrl     = Read-Required "    Base URL: "
            Write-Host "    Model ID - the exact identifier your server expects."
            Write-Host "    Tip: enter `"auto`" to have Forge query /v1/models on startup."
            $epModel   = Read-Required "    Model ID: "
            $epContext = (Read-Required "    Context window in tokens (e.g. 32768, 131072): ") -replace ',', ''

@"
[models]
default = "$epName"

[[models.endpoints]]
name = "$epName"
base_url = "$epUrl"
model_id = "$epModel"
max_context_tokens = $epContext
max_output_tokens = 8192
endpoint_type = "open_ai"
"@ | Set-Content -Path $CONFIG_FILE
            $defaultModel = $epName
        }

        '2' {
@'
[models]
default = "claude"

[[models.endpoints]]
name = "claude"
base_url = "https://api.anthropic.com"
model_id = "auto"
max_context_tokens = 200000
max_output_tokens = 8192
endpoint_type = "anthropic"
'@ | Set-Content -Path $CONFIG_FILE
            $defaultModel = "claude"
            $LOGIN_CMD = "--login"
            $LOGIN_LABEL = "Claude"
        }

        '3' {
@'
[models]
default = "chatgpt-codex"

[[models.endpoints]]
name = "chatgpt-codex"
base_url = "https://chatgpt.com/backend-api/codex"
model_id = "auto"
max_context_tokens = 200000
max_output_tokens = 16384
endpoint_type = "chatgpt_codex"
'@ | Set-Content -Path $CONFIG_FILE
            $defaultModel = "chatgpt-codex"
            $LOGIN_CMD = "--login-chatgpt"
            $LOGIN_LABEL = "ChatGPT Codex"
        }

        '4' {
            Write-Host ""
            Write-Host "  Which provider?"
            Write-Host "    1) Anthropic API     (api.anthropic.com - keys start with sk-ant-)"
            Write-Host "    2) OpenAI API        (api.openai.com - keys start with sk-)"
            Write-Host "    3) OpenRouter        (openrouter.ai/api)"
            Write-Host "    4) Custom OpenAI-compatible endpoint"
            Write-Host ""
            $prov = ""
            while ($prov -notmatch '^[1-4]$') { $prov = Read-Line "    Provider [1-4]: " }

            switch ($prov) {
                '1' { $epUrl="https://api.anthropic.com";    $epType="anthropic"; $epName="anthropic" }
                '2' { $epUrl="https://api.openai.com/v1";    $epType="open_ai";   $epName="openai" }
                '3' { $epUrl="https://openrouter.ai/api/v1"; $epType="open_ai";   $epName="openrouter" }
                '4' {
                    $epName = Read-Required "    Endpoint name (any label): "
                    $epUrl  = Read-Required "    Base URL: "
                    $epType = "open_ai"
                }
            }

            $epKey     = Read-Required "    API key: "
            $epModel   = Read-Required "    Model ID (e.g. claude-sonnet-4-6, gpt-4o, anthropic/claude-opus-4): "
            $epContext = (Read-Required "    Context window in tokens (e.g. 200000): ") -replace ',', ''

@"
[models]
default = "$epName"

[[models.endpoints]]
name = "$epName"
base_url = "$epUrl"
model_id = "$epModel"
api_key = "$epKey"
max_context_tokens = $epContext
max_output_tokens = 8192
endpoint_type = "$epType"
"@ | Set-Content -Path $CONFIG_FILE
            $defaultModel = $epName
        }

        '5' {
@'
# Forge config - edit this file to point at your LLM, then run "forge".
#
# Examples:
#
# Local OpenAI-compatible server (LM Studio, Ollama, llama.cpp, vLLM):
#
#   [models]
#   default = "local"
#
#   [[models.endpoints]]
#   name = "local"
#   base_url = "http://127.0.0.1:1234/v1"
#   model_id = "auto"
#   max_context_tokens = 32768
#   max_output_tokens = 8192
#   endpoint_type = "open_ai"
#
# Claude / ChatGPT subscription - set endpoint_type to "anthropic" or
# "chatgpt_codex" with no api_key, then run "forge-agent --login" or
# "forge-agent --login-chatgpt" to authenticate via OAuth.
#
# Direct API key - add api_key = "..." to the endpoint block.
#
# See ARCHITECTURE.md for the full config reference.

[models]
default = "placeholder"

[[models.endpoints]]
name = "placeholder"
base_url = "http://127.0.0.1:1234/v1"
model_id = "auto"
max_context_tokens = 32768
max_output_tokens = 8192
endpoint_type = "open_ai"
'@ | Set-Content -Path $CONFIG_FILE
            $defaultModel = "placeholder"
            $POST_INSTALL_HINT = "Edit the config file before running forge:`n    $CONFIG_FILE`n  The file is annotated with examples for local servers, OAuth subscriptions, and direct API keys.`n  Or re-run .\install.ps1 to use the interactive wizard."
        }
    }

    # Append the common [agent] block
    $agentBlock = @"

[agent]
auto_approve_reads = true
auto_approve_writes = false
permission_mode = "default"
max_history_messages = 200
compaction_threshold = 150

[agent.subagents]
enabled = true
max_depth = 2
max_concurrent = 4
default_model = "$defaultModel"
"@
    Add-Content -Path $CONFIG_FILE -Value $agentBlock

    Write-Ok "Config written to $CONFIG_FILE"

    # OAuth login for choices 2 and 3
    if ($choice -in @('2','3')) {
        Write-Host ""
        $loginNow = Read-Line "Run $LOGIN_LABEL OAuth login now? [Y/n]: "
        if ($loginNow -notmatch '^[Nn]') {
            Write-Info "Launching $LOGIN_LABEL OAuth login..."
            & "$INSTALL_BIN\forge-agent.exe" $LOGIN_CMD
            if ($LASTEXITCODE -ne 0) {
                Write-Warn "Login did not complete. Retry later with:  forge-agent $LOGIN_CMD"
                $POST_INSTALL_HINT = "To authenticate later, run:  forge-agent $LOGIN_CMD"
            } else {
                Write-Ok "Logged in to $LOGIN_LABEL"
            }
        } else {
            $POST_INSTALL_HINT = "To authenticate later, run:  forge-agent $LOGIN_CMD"
        }
    }

    # Connection test for choices 1 and 4
    if ($choice -in @('1','4')) {
        Write-Host ""
        $testConn = Read-Line "Test the endpoint now? [y/N]: "
        if ($testConn -match '^[Yy]') {
            try {
                Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 "$epUrl/models" -ErrorAction Stop | Out-Null
                Write-Ok "Endpoint reachable: $epUrl"
            } catch {
                try {
                    Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 "$($epUrl -replace '/v1$','')/v1/models" -ErrorAction Stop | Out-Null
                    Write-Ok "Endpoint reachable: $epUrl"
                } catch {
                    Write-Warn "Could not reach $epUrl - start your server, then run forge."
                }
            }
        }
    }
}

# -------------------------------------------------------------------
# 7. Done
# -------------------------------------------------------------------
Write-Host ""
Write-Host "Forge installed successfully!" -ForegroundColor Green
Write-Host ""
Write-Host "  forge          Launch Forge"
Write-Host "  forge-agent    Launch in headless mode (for scripting)"
Write-Host "  forge-update   Update, rebuild, and reinstall Forge"
Write-Host ""
Write-Host "  Config: $CONFIG_FILE"
if ($POST_INSTALL_HINT) {
    Write-Host ""
    Write-Host "  Next step:" -ForegroundColor White
    Write-Host "  $POST_INSTALL_HINT"
}
Write-Host ""
