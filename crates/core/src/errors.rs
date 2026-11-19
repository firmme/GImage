use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("不支持的格式：{0}")]
    Unsupported(String),
    #[error("无法读取文件 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("无法解码 {path}：{message}")]
    Decode { path: PathBuf, message: String },
    #[error("无法编码 {path}：{message}")]
    Encode { path: PathBuf, message: String },
    #[error("图片过大：{width}×{height}（{pixels} 像素，上限 {limit}）")]
    TooLarge { width: u32, height: u32, pixels: u64, limit: u64 },
    #[error("操作已取消")]
    Cancelled,
}

impl EngineError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    pub fn decode(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Decode { path: path.into(), message: message.into() }
    }

    pub fn encode(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Encode { path: path.into(), message: message.into() }
    }

    /// A short, user-facing message for task lists and info panels.
    pub fn user_message(&self) -> String {
        match self {
            Self::Io { source, .. } => format!("读取失败：{source}"),
            Self::Decode { message, .. } => format!("解码失败：{message}"),
            Self::Encode { message, .. } => format!("导出失败：{message}"),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("无法读取设置 {path}：{source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("无法写入设置 {path}：{source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("设置文件格式无效：{0}")]
    Invalid(String),
}

impl fmt::Display for EngineErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Coarse error class, useful for grouping failures in the task list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EngineErrorKind(pub &'static str);

impl EngineError {
    pub fn kind(&self) -> EngineErrorKind {
        match self {
            Self::Unsupported(_) => EngineErrorKind("unsupported"),
            Self::Io { .. } => EngineErrorKind("io"),
            Self::Decode { .. } => EngineErrorKind("decode"),
            Self::Encode { .. } => EngineErrorKind("encode"),
            Self::TooLarge { .. } => EngineErrorKind("too-large"),
            Self::Cancelled => EngineErrorKind("cancelled"),
        }
    }
}