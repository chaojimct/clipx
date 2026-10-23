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
    /// Everything / FindX 补充的文件夹（外部索引）。
    Everything,
    /// 内置目录递归（没有任何索引服务时的退路）。
    Local,
    /// 对话框当前文件夹及其匹配的子文件夹。
    Current,
}

/// 跳转候选：一行 = 别名行 + 路径行（Picker 显示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub alias: Option<String>,
    pub source: CandidateSource,
    /// true = 文件（选文件对话框检索项）；false = 文件夹（跳转项）。
    pub is_file: bool,
}

impl Candidate {
    pub fn manager(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            alias: None,
            source: CandidateSource::Manager,
            is_file: false,
        }
    }

    /// 显示用主行：别名优先，否则路径。
    pub fn display_line(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.path)
    }

    /// 是否文件夹（文件检索项为 false）。
    pub fn is_folder(&self) -> bool {
        !self.is_file
    }
}
