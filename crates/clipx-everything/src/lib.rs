//! Everything IPC 查询（WM_COPYDATA 直连，免 Everything64.dll SDK 依赖）。
//!
//! 协议照 voidtools SDK everything_ipc.h：Everything 常驻一个
//! `EVERYTHING_TASKBAR_NOTIFICATION` 窗口；向它投递
//! `EVERYTHING_IPC_COPYDATAQUERYW(2)` 的 COPYDATASTRUCT，包体为
//! EVERYTHING_IPC_QUERYW（pack(1)）。Everything 1.4 头字段全是 DWORD；
//! 1.5 起 HWND/ULONG_PTR 为指针宽；findx2-service 兼容层字段顺序又不同。
//! `ipc` 按 1.4 → 1.5 → findx 空串探测并缓存。
//!
//! 行为对齐 WPF 版（EverythingIpc.cs 的源码注记）：
//! - 搜索串用 `parent:` / `path:` / `folder:` 限定；不设 MATCHPATH；
//!   「盘符:\ 关键词」形式 IPC 实测恒 0 条，勿用
//! - max_results 夹紧 [1, 10000]；搜索串截断 2048 字符
//! - 不依赖 Everything_IsDBLoaded（部分版本该标志长期为 0，会误报未运行）

pub mod search;

#[cfg(windows)]
mod ipc;

use std::time::Duration;

/// 单次查询默认超时（WPF 实测单查 <5ms；给 Everything 首查/冷缓存留余量）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(3000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    /// Everything 未运行（找不到 IPC 窗口）。
    NotRunning,
    /// 查询消息投递失败（Everything 挂起或中途退出）。
    SendFailed,
    /// 等待结果超时。
    Timeout,
    /// 应答包格式非法。
    InvalidReply,
    /// IPC 回执窗口创建失败（资源枯竭等，极罕见）。
    ReplyWindowFailed,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::NotRunning => "Everything 未运行",
            Self::SendFailed => "查询投递失败（Everything 无响应或已退出）",
            Self::Timeout => "Everything 查询超时",
            Self::InvalidReply => "Everything 应答数据损坏",
            Self::ReplyWindowFailed => "IPC 回执窗口创建失败",
        };
        f.write_str(s)
    }
}

impl std::error::Error for QueryError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultItem {
    /// 完整路径（path + "\\" + filename；根目录项即盘符本身）。
    pub full_path: String,
    /// 文件名部分。
    pub file_name: String,
    pub is_folder: bool,
    pub is_drive: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryResults {
    /// 命中总数（可能远大于返回条数）。
    pub total_items: u32,
    pub total_folders: u32,
    pub total_files: u32,
    pub items: Vec<ResultItem>,
}

/// Everything 是否在运行（IPC 窗口存在）。
pub fn is_running() -> bool {
    #[cfg(windows)]
    {
        ipc::find_everything_hwnd().is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 当前缓存的 IPC 包布局（调试/验收用）。
pub fn debug_layout() -> &'static str {
    #[cfg(windows)]
    {
        ipc::debug_layout()
    }
    #[cfg(not(windows))]
    {
        "n/a"
    }
}

/// 启动时预热：探测 IPC 窗口并缓存查询包布局，避免首次快查卡在协商。
pub fn warmup() {
    #[cfg(windows)]
    {
        ipc::warmup();
    }
}

/// 查询并返回结构化结果。
pub fn query(search: &str, max_results: u32, timeout: Duration) -> Result<QueryResults, QueryError> {
    #[cfg(windows)]
    {
        ipc::query(search, max_results, timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = (search, max_results, timeout);
        Err(QueryError::NotRunning)
    }
}

/// 查询并仅取全路径列表（对齐 WPF `EverythingIpc.TryQueryFullPaths` 语义）。
pub fn query_full_paths(
    search: &str,
    max_results: u32,
    timeout: Duration,
) -> Result<Vec<String>, QueryError> {
    Ok(query(search, max_results, timeout)?
        .items
        .into_iter()
        .map(|i| i.full_path)
        .collect())
}
