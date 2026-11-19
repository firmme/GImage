param(
    [string]$Exe = "E:\RustBuild\gimage\debug\gimage.exe",
    [string[]]$AppArgs = @(),
    [string]$Out = "E:\code\GImage\docs\images\shot.png",
    [int]$WaitSeconds = 8,
    [string]$Renderer = "winit-software"
)
# Captures a GImage window through PrintWindow. The desktop session used by
# automation cannot use GDI screen capture, but the software renderer presents
# a GDI-compatible surface that PrintWindow can read back.

$ErrorActionPreference = 'Stop'
Get-Process gimage -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
$env:SLINT_BACKEND = $Renderer
$stderr = [System.IO.Path]::ChangeExtension($Out, ".stderr.txt")
Remove-Item $stderr -ErrorAction SilentlyContinue
if ($AppArgs.Count -gt 0) {
    $proc = Start-Process $Exe -ArgumentList $AppArgs -RedirectStandardError $stderr
} else {
    $proc = Start-Process $Exe -RedirectStandardError $stderr
}
Start-Sleep -Seconds $WaitSeconds

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class GImageCapture {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
}
"@
$p = Get-Process gimage -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { throw "GImage did not start. stderr: $(Get-Content $stderr -Raw -ErrorAction SilentlyContinue)" }
$handle = $p.MainWindowHandle
if ($handle -eq 0) { throw "GImage window handle not available" }
$rect = New-Object GImageCapture+RECT
[GImageCapture]::GetWindowRect($handle, [ref]$rect) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$hdc = $graphics.GetHdc()
$ok = [GImageCapture]::PrintWindow($handle, $hdc, 2)
$graphics.ReleaseHdc($hdc)
$graphics.Dispose()
$bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()
"captured: $Out ($width x $height, printwindow=$ok)"
if (Test-Path $stderr) {
    $err = Get-Content $stderr -Raw -ErrorAction SilentlyContinue
    if ($null -ne $err -and $err.Trim().Length -gt 0) { "--- stderr ---"; $err }
}