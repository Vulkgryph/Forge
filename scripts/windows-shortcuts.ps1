function Resolve-ForgeShortcutChoice {
    param([string]$Choice, [bool]$Default, [string]$Prompt, [switch]$NonInteractive)
    if ($Choice -eq 'Yes') { return $true }
    if ($Choice -eq 'No') { return $false }
    if ($NonInteractive -or -not [Environment]::UserInteractive -or [Console]::IsInputRedirected) { return $Default }
    $hint = if ($Default) { '[Y/n]' } else { '[y/N]' }
    while ($true) {
        $answer = Read-Host "$Prompt $hint"
        if ([string]::IsNullOrWhiteSpace($answer)) { return $Default }
        if ($answer -match '^(y|yes)$') { return $true }
        if ($answer -match '^(n|no)$') { return $false }
        Write-Host 'Please enter yes or no.'
    }
}

function Set-ForgeShortcuts {
    param(
        [string]$InstallRoot, [string]$Generation, [string]$Component,
        [string]$StartMenuDirectory, [string]$DesktopDirectory,
        [bool]$StartMenu, [bool]$Desktop
    )
    $shell = New-Object -ComObject WScript.Shell
    $apps = @()
    if ($Component -in @('Both', 'Ide')) { $apps += @{ Name = 'Forge IDE'; Binary = 'forge-ide.exe' } }
    if ($Component -in @('Both', 'Terminal')) { $apps += @{ Name = 'Forge Terminal'; Binary = 'forge.exe' } }
    $ownedRoot = [IO.Path]::GetFullPath((Join-Path $InstallRoot 'releases')) + [IO.Path]::DirectorySeparatorChar
    foreach ($place in @(@{ Directory = $StartMenuDirectory; Enabled = $StartMenu }, @{ Directory = $DesktopDirectory; Enabled = $Desktop })) {
        if ([string]::IsNullOrWhiteSpace($place.Directory)) {
            if ($place.Enabled) { throw 'Windows did not provide a shortcut folder.' }
            continue
        }
        foreach ($app in $apps) {
            $path = Join-Path $place.Directory ($app.Name + '.lnk')
            if (-not $place.Enabled) {
                # A changed preference removes our link, never a different
                # installation's shortcut with the same display name.
                if (Test-Path -LiteralPath $path) {
                    $old = $shell.CreateShortcut($path)
                    if ($old.TargetPath -and [IO.Path]::GetFullPath($old.TargetPath).StartsWith($ownedRoot, [StringComparison]::OrdinalIgnoreCase)) {
                        Remove-Item -LiteralPath $path
                    }
                }
                continue
            }
            New-Item -ItemType Directory -Path $place.Directory -Force | Out-Null
            $shortcut = $shell.CreateShortcut($path)
            $shortcut.TargetPath = Join-Path $Generation $app.Binary
            $shortcut.IconLocation = (Join-Path $Generation 'forge.ico') + ',0'
            $shortcut.WorkingDirectory = $env:USERPROFILE
            $shortcut.Description = $app.Name
            $shortcut.Save()
        }
    }
}
