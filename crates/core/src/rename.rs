use regex::Regex;
use serde::{Deserialize, Serialize};

/// Batch-rename template.
///
/// Output name is built from the non-empty parts joined with `_`:
/// `[prefix] [edited stem] [suffix] [zero-padded index]`, followed by the chosen extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RenameOptions {
    pub prefix: String,
    pub suffix: String,
    pub extension: String,
    pub start_index: u32,
    pub padding: u8,
    /// Literal substring replaced in the stem; empty disables replacement.
    pub find: String,
    pub replace: String,
    pub regex: bool,
    pub numbered: bool,
}

impl Default for RenameOptions {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            suffix: String::new(),
            extension: String::new(),
            start_index: 1,
            padding: 3,
            find: String::new(),
            replace: String::new(),
            regex: false,
            numbered: true,
        }
    }
}

impl RenameOptions {
    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.padding = out.padding.clamp(1, 9);
        out.start_index = out.start_index.max(1);
        out
    }

    pub fn is_noop(&self) -> bool {
        self.prefix.is_empty()
            && self.suffix.is_empty()
            && self.extension.trim().is_empty()
            && self.find.is_empty()
            && self.start_index == 1
    }
}

/// Invalid Windows file-name characters are replaced with `_`.
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

pub struct RenameRule {
    options: RenameOptions,
    pattern: Option<Regex>,
}

impl RenameRule {
    pub fn new(options: &RenameOptions) -> Result<Self, String> {
        let options = options.normalized();
        let extension = options.extension.trim();
        let ext = extension.strip_prefix('.').unwrap_or(extension);
        if !extension.is_empty() && (ext.is_empty() || ext.contains('.') || sanitize(ext) != ext) {
            return Err("请输入有效的扩展名，例如 png 或 .png".into());
        }
        let pattern = if options.regex && !options.find.is_empty() {
            if options.find == "*" {
                return Err("匹配整个原名请用 ^.*$。* 必须跟在要重复的表达式后面。".into());
            }
            Some(Regex::new(&options.find).map_err(|error| format!("正则表达式无效：{error}"))?)
        } else {
            None
        };
        Ok(Self { options, pattern })
    }

    pub fn target(&self, stem: &str, extension: &str, index: u32) -> Option<String> {
        let opts = &self.options;
        let edited_stem = if let Some(pattern) = &self.pattern {
            if !pattern.is_match(stem) {
                return None;
            }
            pattern
                .replace_all(stem, opts.replace.as_str())
                .into_owned()
        } else if opts.find.is_empty() {
            stem.to_owned()
        } else {
            stem.replacen(&opts.find, &opts.replace, 1)
        };

        let mut parts = Vec::with_capacity(4);
        for part in [opts.prefix.trim(), edited_stem.trim(), opts.suffix.trim()] {
            if !part.is_empty() {
                parts.push(part.to_owned());
            }
        }
        if opts.numbered {
            let number = opts.start_index.saturating_add(index);
            parts.push(format!("{number:0width$}", width = opts.padding as usize));
        }
        let sanitized = sanitize(&parts.join("_"));
        let stem_out = sanitized.trim_end_matches(['.', ' ']);
        if stem_out.is_empty() {
            return None;
        }
        let ext = opts.extension.trim();
        let ext = if ext.is_empty() { extension } else { ext };
        let ext = sanitize(ext.trim_start_matches('.'));
        let device = stem_out
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (device.len() == 4
                && (device.starts_with("COM") || device.starts_with("LPT"))
                && matches!(device.as_bytes()[3], b'1'..=b'9'));
        let stem_out = if reserved {
            format!("_{stem_out}")
        } else {
            stem_out.to_owned()
        };
        Some(if ext.is_empty() {
            stem_out
        } else {
            format!("{stem_out}.{ext}")
        })
    }
}

/// Build the new file name (with extension) for item `index` (0-based).
pub fn build_rename_target(
    stem: &str,
    extension: &str,
    index: u32,
    options: &RenameOptions,
) -> String {
    RenameRule::new(options)
        .ok()
        .and_then(|rule| rule.target(stem, extension, index))
        .unwrap_or_else(|| {
            if extension.is_empty() {
                stem.to_owned()
            } else {
                format!("{stem}.{extension}")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_preserves_extension() {
        let opts = RenameOptions {
            prefix: "trip".into(),
            suffix: "raw".into(),
            start_index: 5,
            padding: 2,
            ..Default::default()
        };
        assert_eq!(
            build_rename_target("DSC_001", "JPG", 0, &opts),
            "trip_DSC_001_raw_05.JPG"
        );
        assert_eq!(
            build_rename_target("DSC_001", "JPG", 2, &opts),
            "trip_DSC_001_raw_07.JPG"
        );
    }

    #[test]
    fn find_replace_only_first_occurrence() {
        let opts = RenameOptions {
            find: "IMG".into(),
            replace: "PIC".into(),
            padding: 1,
            ..Default::default()
        };
        assert_eq!(
            build_rename_target("IMG_IMG_1", "png", 0, &opts),
            "PIC_IMG_1_1.png"
        );
    }

    #[test]
    fn invalid_characters_are_replaced() {
        let opts = RenameOptions {
            prefix: "a:b".into(),
            padding: 1,
            ..Default::default()
        };
        assert_eq!(build_rename_target("x", "jpg", 0, &opts), "a_b_x_1.jpg");
    }
}
