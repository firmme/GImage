$ErrorActionPreference = 'Stop'
$target = $env:GIMAGE_UPDATE_TARGET
$directory = $env:GIMAGE_UPDATE_DIRECTORY
$replacement = Join-Path $directory 'replacement.exe'
$backup = Join-Path $directory 'previous.exe'
$failed = Join-Path $directory 'failed.exe'
$parentExited = $false
$swapped = $false

function Start-GImage {
    Start-Process -FilePath $target -WorkingDirectory (Split-Path -Parent $target) -PassThru
}

try {
    $parent = Get-Process -Id ([int]$env:GIMAGE_UPDATE_PARENT) -ErrorAction SilentlyContinue
    if ($parent -and !$parent.WaitForExit(45000)) {
        throw 'GImage did not exit before the update timeout.'
    }
    $parentExited = $true
    if ((Get-FileHash -LiteralPath $replacement -Algorithm SHA256).Hash -ine $env:GIMAGE_UPDATE_SHA256) {
        throw 'Update SHA-256 verification failed.'
    }
    for ($attempt = 1; ; $attempt++) {
        try {
            [IO.File]::Replace($replacement, $target, $backup)
            $swapped = $true
            break
        } catch {
            if ($attempt -eq 12 -or !(Test-Path -LiteralPath $replacement)) { throw }
            Start-Sleep -Milliseconds 250
        }
    }
    $next = Start-GImage
    if ($next.WaitForExit(1800)) {
        throw "GImage exited during startup ($($next.ExitCode))."
    }
} catch {
    $reason = $_.Exception.Message
    if ($swapped) {
        try {
            [IO.File]::Replace($backup, $target, $failed)
            $swapped = $false
        } catch {
            $reason += [Environment]::NewLine + "Previous executable: $backup"
        }
    }
    if ($parentExited -and !$swapped -and (Test-Path -LiteralPath $target -PathType Leaf)) {
        try { Start-GImage | Out-Null } catch { $reason += [Environment]::NewLine + $_.Exception.Message }
    }
    Add-Type -AssemblyName System.Windows.Forms
    [void][Windows.Forms.MessageBox]::Show($reason, 'GImage update')
    exit 1
} finally {
    foreach ($file in @($replacement, $failed)) {
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
        }
    }
    if (!(Test-Path -LiteralPath $backup)) {
        try { [IO.Directory]::Delete($directory) } catch { }
    }
}

Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
try { [IO.Directory]::Delete($directory) } catch { }
