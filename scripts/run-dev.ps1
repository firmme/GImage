#Requires -Version 7.0
<#
.SYNOPSIS
    Build and launch GImage from the workspace.

.DESCRIPTION
    Builds `gimage-app` (debug by default) and starts the resulting gimage.exe,
    waiting for it to exit. The target directory is read from cargo metadata, so
    a custom `target-dir` in .cargo/config.toml is respected.

.PARAMETER Release
    Build and run the release profile instead of debug.

.PARAMETER SkipBuild
    Launch the existing binary without rebuilding.

.PARAMETER Offline
    Pass --offline to cargo (useful when the dependency cache is already warm).

.PARAMETER Skia
    Use the Windows MSVC toolchain and Skia renderer.

.EXAMPLE
    pwsh -ExecutionPolicy Bypass -File scripts/run-dev.ps1

.EXAMPLE
    pwsh -ExecutionPolicy Bypass -File scripts/run-dev.ps1 -Release
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$SkipBuild,
    [switch]$Offline,
    [switch]$Skia
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "找不到 cargo。请先安装 Rust（rustup）并确保 PATH 中有 cargo。"
    }

    $profileName = if ($Release) { 'release' } else { 'debug' }
    $rustupArgs = if ($Skia) { @('+stable-x86_64-pc-windows-msvc') } else { @() }

    if (-not $SkipBuild) {
        . (Join-Path $PSScriptRoot 'setup-toolchain.ps1') -Skia:$Skia
        $buildArgs = @('build', '-p', 'gimage-app')
        if ($Skia) { $buildArgs += @('--target', 'x86_64-pc-windows-msvc') }
        if ($Release) { $buildArgs += '--release' }
        if ($Offline) { $buildArgs += '--offline' }

        Write-Host "==> cargo $($buildArgs -join ' ')" -ForegroundColor Cyan
        & cargo @rustupArgs @buildArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build 失败（退出码 $LASTEXITCODE），未启动应用。"
        }
    }

    $metadataArgs = @('metadata', '--no-deps', '--format-version', '1')
    if ($Offline) { $metadataArgs += '--offline' }
    $json = & cargo @metadataArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata 失败（退出码 $LASTEXITCODE）。"
    }
    $targetDir = [string](($json | ConvertFrom-Json).target_directory)
    if ([string]::IsNullOrWhiteSpace($targetDir)) {
        throw "cargo metadata 未返回 target_directory。"
    }

    $targetProfile = if ($Skia) { "x86_64-pc-windows-msvc\$profileName" } else { $profileName }
    $exe = Join-Path $targetDir "$targetProfile\gimage.exe"
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        throw "找不到可执行文件：$exe"
    }

    Write-Host "==> 启动 $exe" -ForegroundColor Cyan
    & $exe
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "GImage 退出码：$LASTEXITCODE"
    }
}
finally {
    Pop-Location
}
