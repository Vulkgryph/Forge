$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('forge installer test ' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
Copy-Item -LiteralPath (Join-Path $repo 'install.ps1'), (Join-Path $repo 'LICENSE'), (Join-Path $repo 'NOTICE') -Destination $fixture
Copy-Item -LiteralPath (Join-Path $repo 'licenses') -Destination $fixture -Recurse
New-Item -ItemType Directory -Path (Join-Path $fixture 'scripts'), (Join-Path $fixture 'forge-ide\assets') -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $repo 'scripts\windows-shortcuts.ps1') -Destination (Join-Path $fixture 'scripts')
Copy-Item -LiteralPath (Join-Path $repo 'forge-ide\assets\forge.ico') -Destination (Join-Path $fixture 'forge-ide\assets')
$stub = Join-Path $fixture 'stub.exe'
Add-Type -TypeDefinition 'public class InstallerFixture { public static void Main(string[] args) { System.Console.WriteLine("Forge fixture 0.5.1"); } }' -OutputAssembly $stub -OutputType ConsoleApplication
$driver = @'
param([string]$Fixture, [switch]$FailBuild)
$ErrorActionPreference = 'Stop'
function cargo {
    if ($FailBuild) { $global:LASTEXITCODE = 42; return }
    if ('--locked' -notin $args -or $env:RUSTFLAGS -notmatch '-D warnings') { throw 'Build policy missing' }
    foreach ($package in @('forge-agent', 'forge-ide', 'forge-tui-rs')) {
        if ($package -notin $args) { throw "Missing build package: $package" }
    }
    $output = Join-Path $Fixture 'target\windows-installer\x86_64-pc-windows-msvc\release'
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    foreach ($binary in @('forge-agent.exe', 'forge-ide.exe', 'forge-tui-rs.exe')) {
        Copy-Item -LiteralPath (Join-Path $Fixture 'stub.exe') -Destination (Join-Path $output $binary) -Force
    }
    $global:LASTEXITCODE = 0
}
& (Join-Path $Fixture 'install.ps1') -SkipPrerequisites -NoPath -NoShortcut -InstallRoot (Join-Path $Fixture 'installed')
exit $LASTEXITCODE
'@
$driverPath = Join-Path $fixture 'driver.ps1'
Set-Content -LiteralPath $driverPath -Value $driver -Encoding UTF8
$root = Join-Path $fixture 'installed'
$record = Join-Path $root 'install.json'
& powershell -NoProfile -ExecutionPolicy Bypass -File $driverPath -Fixture $fixture -FailBuild
if ($LASTEXITCODE -eq 0 -or (Test-Path -LiteralPath $root)) { throw 'Failed build must not create an installation.' }
& powershell -NoProfile -ExecutionPolicy Bypass -File $driverPath -Fixture $fixture
if ($LASTEXITCODE -ne 0) { throw 'Installation failed.' }
$first = Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
foreach ($name in @('forge', 'forge-agent', 'forge-ide')) {
    & (Join-Path $root "bin\$name.cmd") --version
    if ($LASTEXITCODE -ne 0) { throw "Launcher failed: $name" }
    if (-not (Test-Path -LiteralPath (Join-Path $first.binaries "$name.exe"))) { throw 'Missing executable' }
}
& powershell -NoProfile -ExecutionPolicy Bypass -File $driverPath -Fixture $fixture -FailBuild
if ($LASTEXITCODE -eq 0) { throw 'Failed update reported success.' }
if ((Get-Content -LiteralPath $record -Raw | ConvertFrom-Json).binaries -ne $first.binaries) { throw 'Failed update replaced the working installation.' }
& powershell -NoProfile -ExecutionPolicy Bypass -File $driverPath -Fixture $fixture
if ($LASTEXITCODE -ne 0) { throw 'Reinstall failed.' }
$second = Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
if ($first.binaries -eq $second.binaries -or -not (Test-Path -LiteralPath $first.binaries)) { throw 'Reinstall must preserve the old generation.' }
. (Join-Path $repo 'scripts\windows-shortcuts.ps1')
$shortcutArgs = @{ InstallRoot = $root; Generation = $first.binaries; Component = 'Both'; StartMenuDirectory = (Join-Path $fixture 'Start menu'); DesktopDirectory = (Join-Path $fixture 'Desktop') }
Set-ForgeShortcuts @shortcutArgs -StartMenu $true -Desktop $true
$shell = New-Object -ComObject WScript.Shell
foreach ($directory in @($shortcutArgs.StartMenuDirectory, $shortcutArgs.DesktopDirectory)) {
    foreach ($app in @(@{ Name = 'Forge IDE'; Binary = 'forge-ide.exe' }, @{ Name = 'Forge Terminal'; Binary = 'forge.exe' })) {
        $link = $shell.CreateShortcut((Join-Path $directory ($app.Name + '.lnk')))
        if ($link.TargetPath -ne (Join-Path $first.binaries $app.Binary)) { throw 'Wrong shortcut target.' }
        if ($link.IconLocation -ne ((Join-Path $first.binaries 'forge.ico') + ',0')) { throw 'Forge artwork missing from shortcut.' }
    }
}
$shortcutArgs.Generation = $second.binaries
Set-ForgeShortcuts @shortcutArgs -StartMenu $true -Desktop $true
$desktopLink = Join-Path $shortcutArgs.DesktopDirectory 'Forge IDE.lnk'
if ($shell.CreateShortcut($desktopLink).TargetPath -ne (Join-Path $second.binaries 'forge-ide.exe')) { throw 'Shortcut did not update.' }
# Declining must leave a similarly named shortcut from another installation intact.
$foreign = $shell.CreateShortcut($desktopLink)
$foreign.TargetPath = $stub
$foreign.Save()
Set-ForgeShortcuts @shortcutArgs -StartMenu $false -Desktop $false
if (-not (Test-Path -LiteralPath $desktopLink)) { throw 'Unrelated shortcut was removed.' }
if ((Get-ChildItem -LiteralPath $shortcutArgs.StartMenuDirectory -Filter '*.lnk').Count -ne 0) { throw 'Declined Start menu shortcuts remain.' }
if (Test-Path -LiteralPath (Join-Path $shortcutArgs.DesktopDirectory 'Forge Terminal.lnk')) { throw 'Declined desktop shortcut remains.' }
if (-not (Resolve-ForgeShortcutChoice Ask $true '' -NonInteractive)) { throw 'Saved affirmative preference was lost.' }
if (Resolve-ForgeShortcutChoice Ask $false '' -NonInteractive) { throw 'Saved negative preference was lost.' }
Write-Host "Installer and shortcut checks passed (isolated fixture: $fixture)" -ForegroundColor Green
