//! 候选路径模型（对齐 WPF `FileJumpCandidate` / `FileJumpPickerRow`）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    /// Explorer / TC / XY / DOpus 等管理器当前路径。
    Manager,
    /// 收藏路径（⭐）。
    Favorite,
    /// 最近确认路径（`RecentFileDialogFolders`）。
    Recent,
    /// 上次路径记忆（`LastFileDialogFolder`）。
    Last,
    /// Everything 补充的文件夹。
    Everything,
    /// 对话框当前文件夹及其匹配的子文件夹。
    Current,
}

/// 跳转候选：一行 = 别名行 + 路径行（Picker 显示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub alias: Option<String>,
    pub source: CandidateSource,
}

impl Candidate {
    pub fn manager(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            alias: None,
            source: CandidateSource::Manager,
        }
    }

    /// 显示用主行：别名优先，否则路径。
    pub fn display_line(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.path)
    }
}
