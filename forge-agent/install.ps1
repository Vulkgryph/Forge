# Compatibility entry point for the monorepo installer.
& (Join-Path (Split-Path -Parent $PSScriptRoot) 'install.ps1') @args
exit $LASTEXITCODE
