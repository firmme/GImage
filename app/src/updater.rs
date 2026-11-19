use base64::{Engine as _, engine::general_purpose::STANDARD};
use semver::Version;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const MAX_UPDATE_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Debug)]
pub struct Update {
    pub version: Version,
    url: String,
    size: u64,
    sha256: String,
}

#[derive(Debug)]
pub struct StagedUpdate {
    directory: TempDir,
    executable: PathBuf,
    sha256: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

fn powershell(script: &str) -> Result<Command, String> {
    if !cfg!(all(windows, target_arch = "x86_64")) {
        return Err("自动更新仅支持 Windows x64".into());
    }
    let system = std::env::var_os("SystemRoot").ok_or("找不到 Windows 系统目录")?;
    let mut command =
        Command::new(Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(bytes),
        ])
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    Ok(command)
}

fn release_from_json(
    json: &[u8],
    repo: &str,
    installed: &Version,
) -> Result<Option<Update>, String> {
    let release: Release =
        serde_json::from_slice(json).map_err(|_| "GitHub 返回的版本信息无法读取")?;
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .ok_or("Release 标签格式不正确")?,
    )
    .map_err(|_| "Release 标签格式不正确")?;
    if release.draft
        || release.prerelease
        || !version.pre.is_empty()
        || !version.build.is_empty()
        || version <= *installed
    {
        return Ok(None);
    }
    let expected = format!("GImage-{version}-win64.exe");
    let url = format!(
        "https://github.com/{repo}/releases/download/{}/{expected}",
        release.tag_name
    );
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == expected && asset.browser_download_url == url)
        .ok_or("新版尚未提供 Windows x64 更新文件")?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("新版缺少有效的 SHA-256 校验码")?;
    if asset.size == 0 || asset.size > MAX_UPDATE_SIZE {
        return Err("新版更新文件的大小不符合预期".into());
    }
    Ok(Some(Update {
        version,
        url,
        size: asset.size,
        sha256: digest.to_ascii_lowercase(),
    }))
}

pub fn check() -> Result<Option<Update>, String> {
    let repo = env!("CARGO_PKG_REPOSITORY")
        .strip_prefix("https://github.com/")
        .ok_or("程序未配置 GitHub 发布仓库")?;
    let installed = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| "当前版本号无法识别")?;
    let output = powershell(r#"
        $ErrorActionPreference = 'Stop'
        [Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
        $response = Invoke-WebRequest -UseBasicParsing -TimeoutSec 20 -Uri $env:GIMAGE_UPDATE_API -UserAgent 'GImage Updater' -Headers @{ Accept = 'application/vnd.github+json' }
        [Console]::Write($response.Content)
    "#)?.env("GIMAGE_UPDATE_API", format!("https://api.github.com/repos/{repo}/releases/latest"))
        .output().map_err(|error| format!("无法启动更新检查：{error}"))?;
    if !output.status.success() {
        tracing::warn!(error = %String::from_utf8_lossy(&output.stderr), "GitHub update request failed");
        return Err("无法读取 GitHub Release，请检查网络连接后重试".into());
    }
    release_from_json(&output.stdout, repo, &installed)
}

pub fn download(update: Update) -> Result<StagedUpdate, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let parent = executable.parent().ok_or("找不到程序目录")?;
    let directory = tempfile::Builder::new()
        .prefix(".gimage-update-")
        .tempdir_in(parent)
        .map_err(|error| format!("程序目录不可写：{error}"))?;
    let output = powershell(r#"
        $ErrorActionPreference = 'Stop'
        Invoke-WebRequest -UseBasicParsing -TimeoutSec 90 -MaximumRedirection 5 -Uri $env:GIMAGE_UPDATE_URL -OutFile $env:GIMAGE_UPDATE_FILE
        $file = Get-Item -LiteralPath $env:GIMAGE_UPDATE_FILE
        if ($file.Length -ne [long]$env:GIMAGE_UPDATE_SIZE) { throw 'Unexpected update file size' }
        if ((Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash -ine $env:GIMAGE_UPDATE_SHA256) {
            throw 'Update SHA-256 verification failed'
        }
    "#)?.env("GIMAGE_UPDATE_URL", update.url)
        .env("GIMAGE_UPDATE_FILE", directory.path().join("replacement.exe"))
        .env("GIMAGE_UPDATE_SIZE", update.size.to_string())
        .env("GIMAGE_UPDATE_SHA256", &update.sha256)
        .output().map_err(|error| format!("无法启动更新下载：{error}"))?;
    if !output.status.success() {
        tracing::warn!(error = %String::from_utf8_lossy(&output.stderr), "update download or validation failed");
        return Err("更新下载或 SHA-256 校验失败，当前版本可继续使用".into());
    }
    Ok(StagedUpdate {
        directory,
        executable,
        sha256: update.sha256,
    })
}

impl StagedUpdate {
    pub fn launch(self) -> Result<(), String> {
        powershell(include_str!("../../scripts/apply-update.ps1"))?
            .env("GIMAGE_UPDATE_PARENT", std::process::id().to_string())
            .env("GIMAGE_UPDATE_DIRECTORY", self.directory.path())
            .env("GIMAGE_UPDATE_TARGET", &self.executable)
            .env("GIMAGE_UPDATE_SHA256", &self.sha256)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动更新程序：{error}"))?;
        let _ = self.directory.keep();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_newer_release_and_exact_verified_asset() {
        let installed = Version::parse("0.1.1").unwrap();
        let repo = "firmme/GImage";
        let asset =
            format!("https://github.com/{repo}/releases/download/v0.1.2/GImage-0.1.2-win64.exe");
        let mut release = serde_json::json!({
            "tag_name": "v0.1.2", "draft": false, "prerelease": false,
            "assets": [{
                "name": "GImage-0.1.2-win64.exe", "browser_download_url": asset, "size": 48_000_000,
                "digest": format!("sha256:{}", "a".repeat(64))
            }]
        });
        let parse = |json: &serde_json::Value| {
            release_from_json(&serde_json::to_vec(json).unwrap(), repo, &installed)
        };
        assert_eq!(
            parse(&release).unwrap().unwrap().version,
            Version::parse("0.1.2").unwrap()
        );
        release["assets"][0]["browser_download_url"] =
            "https://example.org/GImage-0.1.2-win64.exe".into();
        assert!(parse(&release).is_err());
        release["assets"][0]["browser_download_url"] = asset.into();
        release["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(parse(&release).is_err());
        release["tag_name"] = "v0.1.1".into();
        assert!(parse(&release).unwrap().is_none());
    }
}
