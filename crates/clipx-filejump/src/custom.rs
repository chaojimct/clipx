//! 自定义文件对话框规则（WPF `CustomFileDialogStore`，独立 JSON，不进 settings）。
//!
//! 内置识别为无的窗口按“类名 + 进程名 + 可选标题包含”匹配，命中后按
//! `strategy`（pinned 优先策略）跳转。导入合并同键覆盖。

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// 单条规则（WPF `CustomFileDialogRule` 简化：匹配三元组 + 策略）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomRule {
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub process: String,
    #[serde(default)]
    pub title_contains: String,
    #[serde(default)]
    pub strategy: String,
}

impl CustomRule {
    /// 列表展示行（WPF `SummaryLine`）。
    pub fn summary(&self) -> String {
        let mut s = format!("{} + {}", self.class, self.process);
        if !self.title_contains.is_empty() {
            s.push_str(&format!(" [{}]", self.title_contains));
        }
        if !self.strategy.is_empty() {
            s.push_str(&format!(" → {}", self.strategy));
        }
        s
    }

    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.class.to_lowercase(),
            self.process.to_lowercase(),
            self.title_contains.to_lowercase()
        )
    }

    pub fn matches(&self, class: &str, exe_base_lower: &str, title: &str) -> bool {
        if self.class.is_empty() || self.process.is_empty() {
            return false;
        }
        class.eq_ignore_ascii_case(&self.class)
            && exe_base_lower.eq_ignore_ascii_case(&self.process)
            && (self.title_contains.is_empty() || title.contains(&self.title_contains))
    }
}

/// 文件存储（`Data/custom_file_dialogs.json`，与 WPF 路径约定一致）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomStore {
    #[serde(default)]
    pub rules: Vec<CustomRule>,
}

impl CustomStore {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn find(&self, class: &str, exe_base_lower: &str, title: &str) -> Option<&CustomRule> {
        self.rules
            .iter()
            .find(|r| r.matches(class, exe_base_lower, title))
    }

    /// 导入合并：同键覆盖，不同键追加。返回（新增，覆盖）。
    pub fn import_merge(&mut self, other: CustomStore) -> (usize, usize) {
        let mut added = 0;
        let mut replaced = 0;
        for r in other.rules {
            if let Some(slot) = self.rules.iter_mut().find(|e| e.key() == r.key()) {
                *slot = r;
                replaced += 1;
            } else {
                self.rules.push(r);
                added += 1;
            }
        }
        (added, replaced)
    }
}

static RUNTIME: Mutex<Vec<CustomRule>> = Mutex::new(Vec::new());

/// 设置保存后热更新，供 `classify_hwnd` 命中自定义对话框。
pub fn set_runtime_rules(rules: Vec<CustomRule>) {
    *RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = rules;
}

pub fn runtime_hit(class: &str, exe_base_lower: &str, title: &str) -> bool {
    RUNTIME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|r| r.matches(class, exe_base_lower, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_and_merge() {
        let mut s = CustomStore::default();
        s.rules.push(CustomRule {
            class: "#32770".into(),
            process: "myapp".into(),
            title_contains: "打开".into(),
            strategy: "alt_d".into(),
        });
        assert!(s.find("#32770", "myapp", "请选择打开文件").is_some());
        assert!(s.find("#32770", "other", "打开").is_none());
        let (a, r) = s.import_merge(CustomStore {
            rules: vec![CustomRule {
                class: "#32770".into(),
                process: "myapp".into(),
                title_contains: "打开".into(),
                strategy: "ctrl_l".into(),
            }],
        });
        assert_eq!((a, r), (0, 1));
        assert_eq!(s.rules[0].strategy, "ctrl_l");
    }
}
