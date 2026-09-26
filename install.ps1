# Local Windows source installer. Run from any directory; never pulls source.
[CmdletBinding()]
param(
    [ValidateSet('Both', 'Ide', 'Terminal', 'Agent')]
    [string]$Component = 'Both',
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\Forge'),
    [switch]$SkipPrerequisites,
    [switch]$NoPath,
    [switch]$NoShortcut,
    [ValidateSet('Ask', 'Yes', 'No')]
    [string]$StartMenuShortcut = 'Ask',
    [ValidateSet('Ask', 'Yes', 'No')]
    [string]$DesktopShortcut = 'Ask',
    [switch]$NonInteractive,
    [switch]$PrerequisitesOnly,
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = $PSScriptRoot
. (Join-Path $repo 'scripts\windows-shortcuts.ps1')

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed (exit $LASTEXITCODE). Installation stopped." }
}

function Refresh-Tools {
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $env:PATH = "$cargoHome\bin;" + [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
        [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:PATH
    $tools = Join-Path $env:LOCALAPPDATA 'ForgeBuildTools'
    foreach ($relative in @('nasm-3.02\nasm-3.02', 'perl-5.42.3.1\perl\bin')) {
        $directory = Join-Path $tools $relative
        if (Test-Path -LiteralPath $directory) { $env:PATH = "$directory;$env:PATH" }
    }
    $vs = Find-CppTools
    if ($vs) {
        $cmake = Join-Path $vs 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
        if (Test-Path -LiteralPath $cmake) { $env:PATH = "$cmake;$env:PATH" }
    }
}

function Install-ArchiveTool {
    param([string]$Name, [string]$Url, [string]$Sha256, [string]$Executable)
    $tools = Join-Path $env:LOCALAPPDATA 'ForgeBuildTools'
    $destination = Join-Path $tools $Name
    $binary = Join-Path $destination $Executable
    if (-not (Test-Path -LiteralPath $binary)) {
        New-Item -ItemType Directory -Path $tools -Force | Out-Null
        $archive = Join-Path $tools "$Name.zip"
        if (-not (Test-Path -LiteralPath $archive)) {
            Write-Host "Downloading $Name..."
            Invoke-Checked curl.exe @('--fail', '--location', '--proto', '=https', '--tlsv1.2', '--output', $archive, $Url)
        }
        if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $Sha256) {
            throw "Checksum mismatch for $archive. Remove that download and rerun; it has not been executed."
        }
        Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
        if (-not (Test-Path -LiteralPath $binary)) { throw "$Name archive did not contain $Executable" }
    }
    $env:PATH = (Split-Path -Parent $binary) + ';' + $env:PATH
}

function Install-Tool {
    param([string]$Command, [string]$Package)
    if (Get-Command $Command -ErrorAction SilentlyContinue) { return }
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw "Install $Package first: winget is unavailable."
    }
    Invoke-Checked winget @('install', '--id', $Package, '--exact', '--source', 'winget',
        '--silent', '--accept-package-agreements', '--accept-source-agreements', '--disable-interactivity')
    Refresh-Tools
    if (-not (Get-Command $Command -ErrorAction SilentlyContinue)) { throw "$Command is still unavailable after installing $Package." }
}

function Find-CppTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere) {
        & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    }
}

try {
    if ($env:OS -ne 'Windows_NT') { throw 'This installer requires Windows.' }
    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
        throw 'This installer currently targets x64 Windows. ARM64 installation has not been validated.'
    }
    Refresh-Tools
    if ($Check) {
        foreach ($tool in @('git', 'rustup', 'cargo')) {
            if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "Missing prerequisite: $tool" }
        }
        if (-not (Find-CppTools)) { throw 'Missing prerequisite: Visual Studio C++ Build Tools and Windows SDK.' }
        Write-Host 'Base Windows build prerequisites found. No build or installation performed.'
        return
    }
    if (-not $SkipPrerequisites) {
        Install-Tool git Git.Git
        if (-not (Find-CppTools)) {
            Invoke-Checked winget @('install', '--id', 'Microsoft.VisualStudio.2022.BuildTools', '--exact',
                '--source', 'winget', '--accept-package-agreements', '--accept-source-agreements',
                '--disable-interactivity', '--override',
                '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
            if (-not (Find-CppTools)) { throw 'C++ tools are unavailable. Restart Windows if setup requested it, then rerun.' }
        }
        Install-Tool rustup Rustlang.Rustup
        Invoke-Checked rustup @('toolchain', 'install', 'stable-x86_64-pc-windows-msvc', '--profile', 'minimal')
        if ($Component -in @('Both', 'Ide')) {
            Refresh-Tools
            Install-Tool cmake Kitware.CMake
            Install-ArchiveTool 'perl-5.42.3.1' 'https://github.com/StrawberryPerl/Perl-Dist-Strawberry/releases/download/SP_54231_64bit/strawberry-perl-5.42.3.1-64bit-portable.zip' '6a081a811781c30aca51dbc036afd93092af91e3297901f02c17043795a10690' 'perl\bin\perl.exe'
            Install-ArchiveTool 'nasm-3.02' 'https://www.nasm.us/pub/nasm/releasebuilds/3.02/win64/nasm-3.02-win64.zip' '161d0bfaff53c2f9e9f3e69fd0672323ebabafd1268976a5cec11be92a19aee7' 'nasm-3.02\nasm.exe'
        }
    }
    if ($PrerequisitesOnly) { Write-Host 'Windows build tools are ready.'; return }

    $menuDefault = $true
    $desktopDefault = $false
    $recordPath = Join-Path $InstallRoot 'install.json'
    if (Test-Path -LiteralPath $recordPath) {
        $previous = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
        if ($previous.PSObject.Properties['shortcuts']) {
            $menuDefault = [bool]$previous.shortcuts.startMenu
            $desktopDefault = [bool]$previous.shortcuts.desktop
        }
    }
    $menuChoice = $menuDefault
    $desktopChoice = $desktopDefault
    if (-not $NoShortcut -and $Component -ne 'Agent') {
        $menuChoice = Resolve-ForgeShortcutChoice $StartMenuShortcut $menuDefault 'Add Forge to the Start menu?' -NonInteractive:$NonInteractive
        $desktopChoice = Resolve-ForgeShortcutChoice $DesktopShortcut $desktopDefault 'Add Forge shortcuts to the desktop?' -NonInteractive:$NonInteractive
    }

    $packages = @('forge-agent')
    $binaries = [ordered]@{'forge-agent.exe' = 'forge-agent.exe'}
    if ($Component -in @('Both', 'Terminal')) {
        $packages += 'forge-tui-rs'
        $binaries['forge-tui-rs.exe'] = 'forge.exe'
    }
    if ($Component -in @('Both', 'Ide')) {
        $packages += 'forge-ide'
        $binaries['forge-ide.exe'] = 'forge-ide.exe'
    }
    # An explicit target directory avoids guessing around CARGO_TARGET_DIR or a
    # developer's cross-compilation settings when selecting install artifacts.
    $build = Join-Path $repo 'target\windows-installer'
    $buildArgs = @('+stable-x86_64-pc-windows-msvc', 'build', '--locked', '--release',
        '--manifest-path', (Join-Path $repo 'Cargo.toml'), '--target-dir', $build,
        '--target', 'x86_64-pc-windows-msvc')
    foreach ($package in $packages) { $buildArgs += @('-p', $package) }
    $previousFlags = $env:RUSTFLAGS
    try {
        $env:RUSTFLAGS = (($previousFlags + ' -D warnings').Trim())
        Invoke-Checked cargo $buildArgs
    } finally { $env:RUSTFLAGS = $previousFlags }
    $output = Join-Path $build 'x86_64-pc-windows-msvc\release'
    foreach ($binary in $binaries.Keys) {
        $file = Join-Path $output $binary
        if (-not (Test-Path -LiteralPath $file)) { throw "Build did not produce $file" }
        Invoke-Checked $file @('--version')
    }

    # Install a complete generation before switching launchers. Running copies
    # keep their files, and a failed build never overwrites a working install.
    $generation = Join-Path $InstallRoot ('releases\' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $generation -Force | Out-Null
    foreach ($binary in $binaries.Keys) {
        Copy-Item -LiteralPath (Join-Path $output $binary) -Destination (Join-Path $generation $binaries[$binary])
    }
    Copy-Item -LiteralPath (Join-Path $repo 'LICENSE'), (Join-Path $repo 'NOTICE') -Destination $generation
    Copy-Item -LiteralPath (Join-Path $repo 'licenses') -Destination $generation -Recurse
    Copy-Item -LiteralPath (Join-Path $repo 'forge-ide\assets\forge.ico') -Destination $generation
    $launcherDir = Join-Path $InstallRoot 'bin'
    New-Item -ItemType Directory -Path $launcherDir -Force | Out-Null
    foreach ($binary in $binaries.Values) {
        $name = [IO.Path]::GetFileNameWithoutExtension($binary)
        $content = "@echo off`r`n`"$generation\$binary`" %*`r`nexit /b %errorlevel%`r`n"
        [IO.File]::WriteAllText((Join-Path $launcherDir "$name.cmd"), $content, [Text.Encoding]::Default)
    }
    if (-not $NoPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($launcherDir -notin ($userPath -split ';')) {
            [Environment]::SetEnvironmentVariable('Path', ($launcherDir + ';' + $userPath), 'User')
        }
    }
    if (-not $NoShortcut -and $Component -ne 'Agent') {
        Set-ForgeShortcuts -InstallRoot $InstallRoot -Generation $generation -Component $Component `
            -StartMenuDirectory ([Environment]::GetFolderPath('Programs')) `
            -DesktopDirectory ([Environment]::GetFolderPath('DesktopDirectory')) `
            -StartMenu $menuChoice -Desktop $desktopChoice
    }
    $record = [ordered]@{ source = $repo; component = $Component; binaries = $generation; installed = (Get-Date).ToString('o'); shortcuts = @{startMenu = $menuChoice; desktop = $desktopChoice} }
    [IO.File]::WriteAllText($recordPath, ($record | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
    Write-Host "Installed $Component to $generation" -ForegroundColor Green
    Write-Host "Launchers: $launcherDir. Open a new terminal to use them."
    Write-Host 'Configure your model in the IDE onboarding screen or ~/.config/forge/config.toml.'
    Write-Host 'To install local changes, rerun this script. It does not fetch or pull source.'
} catch {
    Write-Error $_ -ErrorAction Continue
    exit 1
}
