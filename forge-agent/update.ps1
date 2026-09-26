# Reinstall the current local checkout; source synchronization is explicit.
& (Join-Path (Split-Path -Parent $PSScriptRoot) 'install.ps1') @args
exit $LASTEXITCODE
