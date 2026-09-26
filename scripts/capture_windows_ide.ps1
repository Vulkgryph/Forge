param(
    [int]$ProcessId,
    [string]$OutputPath = (Join-Path $env:TEMP 'forge-window.png')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ForgeCapture {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
}
'@
$null = [ForgeCapture]::SetProcessDpiAwarenessContext([IntPtr](-4))
$app = if ($ProcessId) { Get-Process -Id $ProcessId } else {
    Get-Process forge-ide | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1
}
if (-not $app -or $app.MainWindowHandle -eq 0) { throw 'No Forge window found.' }
$rect = New-Object ForgeCapture+Rect
$null = [ForgeCapture]::GetWindowRect($app.MainWindowHandle, [ref]$rect)
$bitmap = New-Object Drawing.Bitmap ($rect.Right-$rect.Left), ($rect.Bottom-$rect.Top)
$graphics = [Drawing.Graphics]::FromImage($bitmap)
try {
    $dc = $graphics.GetHdc()
    try {
        if (-not [ForgeCapture]::PrintWindow($app.MainWindowHandle, $dc, 2)) { throw 'Window capture failed.' }
    } finally { $graphics.ReleaseHdc($dc) }
    $bitmap.Save($OutputPath, [Drawing.Imaging.ImageFormat]::Png)
    [pscustomobject]@{ Path = $OutputPath; DPI = [ForgeCapture]::GetDpiForWindow($app.MainWindowHandle); Width = $bitmap.Width; Height = $bitmap.Height }
} finally { $graphics.Dispose(); $bitmap.Dispose() }
