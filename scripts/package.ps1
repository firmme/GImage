#Requires -Version 7.0
<#
.SYNOPSIS
    Build the GImage release binary and package it as a zip.

.DESCRIPTION
    - Builds `gimage-app` in release mode with the lockfile enforced.
    - Copies gimage.exe, README/LICENSE (when present) and any runtime DLLs
      found next to the executable into dist/GImage-<version>-win64/.
    - Zips that folder into dist/GImage-<version>-win64.zip.
    - Fails loudly (non-zero exit) if the build fails or the exe is missing.

    Cargo uses the local dependency cache in offline mode. Prime it with a
    normal build before packaging. The first Skia build may fetch native binaries.

.PARAMETER SkipBuild
    Package the existing release binary without rebuilding. It must already
    exist in the target directory, otherwise the script fails.

.PARAMETER Skia
    Package the Windows MSVC build with the Skia renderer.

.EXAMPLE
    pwsh -ExecutionPolicy Bypass -File scripts/package.ps1

.EXAMPLE
    pwsh -ExecutionPolicy Bypass -File scripts/package.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$Skia
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path -LiteralPath (Join-Path $repoRoot 'Cargo.toml') -PathType Leaf)) {
    throw "未找到工作区根目录（$repoRoot 下没有 Cargo.toml）。"
}

function Get-WorkspaceVersion {
    param([Parameter(Mandatory)][string]$ManifestPath)

    $section = ''
    foreach ($line in Get-Content -LiteralPath $ManifestPath -Encoding utf8) {
        if ($line -match '^\s*\[(?<name>[^\]]+)\]\s*$') {
            $section = $Matches['name']
            continue
        }
        if ($section -eq 'workspace.package' -and $line -match '^\s*version\s*=\s*"(?<version>[^"]+)"\s*$') {
            $version = $Matches['version']
            if ($version -notmatch '^[0-9A-Za-z][0-9A-Za-z.+-]*$') {
                throw "Cargo.toml 中的版本号 '$version' 不能安全地用于文件名。"
            }
            return $version
        }
    }
    throw "无法从 $ManifestPath 的 [workspace.package] 读取 version。"
}

function Get-TargetDirectory {
    $metadataArgs = @('metadata', '--no-deps', '--format-version', '1', '--offline')

    $json = & cargo @metadataArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata 失败（退出码 $LASTEXITCODE），无法确定 target 目录。"
    }
    try {
        $metadata = $json | ConvertFrom-Json
    } catch {
        throw "cargo metadata 输出无法解析：$($_.Exception.Message)"
    }
    if ([string]::IsNullOrWhiteSpace($metadata.target_directory)) {
        throw "cargo metadata 未返回 target_directory。"
    }
    return [string]$metadata.target_directory
}

Push-Location $repoRoot
try {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        throw "找不到 cargo。请先安装 Rust（rustup）并确保 PATH 中有 cargo。"
    }

    $version = Get-WorkspaceVersion -ManifestPath (Join-Path $repoRoot 'Cargo.toml')
    $stageName = "GImage-$version-win64"
    $distRoot = Join-Path $repoRoot 'dist'
    $stageDir = Join-Path $distRoot $stageName
    $zipPath = Join-Path $distRoot "$stageName.zip"

    if (-not $SkipBuild) {
        . (Join-Path $PSScriptRoot 'setup-toolchain.ps1') -Skia:$Skia
        $buildArgs = @('build', '--release', '--locked', '--offline', '-p', 'gimage-app')
        $rustupArgs = if ($Skia) { @('+stable-x86_64-pc-windows-msvc') } else { @() }
        if ($Skia) { $buildArgs += @('--target', 'x86_64-pc-windows-msvc') }
        Write-Host "==> cargo $($buildArgs -join ' ')" -ForegroundColor Cyan
        & cargo @rustupArgs @buildArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build 失败（退出码 $LASTEXITCODE）。打包中止，未生成 zip。"
        }
    }

    $targetDir = Get-TargetDirectory
    $targetProfile = if ($Skia) { 'x86_64-pc-windows-msvc\release' } else { 'release' }
    $exe = Join-Path $targetDir "$targetProfile\gimage.exe"
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        throw "找不到 release 可执行文件：$exe。请先成功执行 cargo build --release -p gimage-app。"
    }

    Write-Host "==> 准备 $stageDir" -ForegroundColor Cyan
    if (Test-Path -LiteralPath $stageDir) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stageDir -Force | Out-Null

    Copy-Item -LiteralPath $exe -Destination (Join-Path $stageDir 'gimage.exe') -Force

    # Runtime DLLs (MinGW runtime or plugin DLLs) placed next to the exe by cargo/rustc.
    $releaseDir = Split-Path -Parent $exe
    $runtimeDlls = @(Get-ChildItem -LiteralPath $releaseDir -Filter '*.dll' -File -ErrorAction SilentlyContinue)
    foreach ($dll in $runtimeDlls) {
        Copy-Item -LiteralPath $dll.FullName -Destination $stageDir -Force
    }

    foreach ($extra in @('README.md', 'LICENSE', 'LICENSE.md', 'THIRD_PARTY_LICENSES.md')) {
        $source = Join-Path $repoRoot $extra
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination $stageDir -Force
        }
    }

    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }
    Write-Host "==> 压缩 $zipPath" -ForegroundColor Cyan
    Compress-Archive -LiteralPath $stageDir -DestinationPath $zipPath -CompressionLevel Optimal

    if (-not (Test-Path -LiteralPath $zipPath -PathType Leaf)) {
        throw "压缩失败，未生成 $zipPath。"
    }

    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash
    $zipSize = (Get-Item -LiteralPath $zipPath).Length
    Write-Host ''
    Write-Host '打包完成：' -ForegroundColor Green
    Write-Host "  目录 : $stageDir"
    Write-Host "  ZIP  : $zipPath"
    Write-Host "  大小 : $([math]::Round($zipSize / 1MB, 2)) MB"
    Write-Host "  SHA256: $hash"
    Write-Host "  运行时 DLL: $($runtimeDlls.Count) 个"
}
finally {
    Pop-Location
}
