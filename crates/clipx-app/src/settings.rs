use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Enter 粘贴后是否向目标应用模拟 Ctrl+V
    #[serde(default = "default_true")]
    pub paste_simulate: bool,
    /// 历史条数上限（含置顶）
    #[serde(default = "default_max_items")]
    pub max_items: i64,
    /// 图片条数独立上限（WPF 版 MaxImageItems）
    #[serde(default = "default_max_image_items")]
    pub max_image_items: i64,
    /// 图片 OCR 开关（WPF 版 ImageOcrEnabled）
    #[serde(default = "default_true")]
    pub image_ocr_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_max_items() -> i64 {
    2000
}

fn default_max_image_items() -> i64 {
    150
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            paste_simulate: true,
            max_items: 2000,
            max_image_items: 150,
            image_ocr_enabled: true,
        }
    }
}

pub fn load(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// M2 设置界面接入前暂未调用
#[allow(dead_code)]
pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, json)?;
    Ok(())
}
