@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set "forge_install_result=%errorlevel%"
echo.
pause
exit /b %forge_install_result%
