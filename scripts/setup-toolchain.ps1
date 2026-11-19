#Requires -Version 7.0
# Changes only this PowerShell process. Dot-source before direct Cargo commands.
[CmdletBinding()]
param(
    [string]$MinGWBin = $env:GIMAGE_MINGW_BIN,
    [switch]$Skia
)

$ErrorActionPreference = 'Stop'
if ($Skia) {
    if (-not $env:VCToolsVersion -or [version]$env:VCToolsVersion -lt [version]'14.40') {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $install = if (Test-Path -LiteralPath $vswhere) {
            & $vswhere -latest -version '[17.0,)' -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        }
        if (-not $install) {
            throw 'Skia 构建需要 Visual Studio 2022 或更新版本的 C++ Build Tools。'
        }
        $vcvars = Join-Path $install 'VC/Auxiliary/Build/vcvars64.bat'
        $environment = & cmd.exe /d /c "call `"$vcvars`" >nul && set"
        if ($LASTEXITCODE -ne 0) {
            throw 'Visual Studio C++ 工具链初始化失败。'
        }
        foreach ($entry in $environment) {
            if ($entry -match '^([^=]+)=(.*)$') {
                [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
            }
        }
    }
    return
}
$requiredTools = @('gcc.exe', 'dlltool.exe', 'ar.exe', 'windres.exe')
if (-not $MinGWBin -and @($requiredTools | Where-Object { -not (Get-Command $_ -ErrorAction SilentlyContinue) }).Count -eq 0) {
    return
}
if (-not $MinGWBin) {
    $MinGWBin = 'E:\tools\mingw64\bin'
}
foreach ($tool in $requiredTools) {
    if (-not (Test-Path -LiteralPath (Join-Path $MinGWBin $tool) -PathType Leaf)) {
        throw "缺少 GNU 工具 $tool。请安装 MinGW-w64，并设置 GIMAGE_MINGW_BIN 或使用 -MinGWBin 指定 bin 目录。"
    }
}
$env:PATH = "$MinGWBin;$env:PATH"
