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
// findx 客户端三平台可用（Windows 命名管道 / macOS+Linux UDS，同一 JSON 行协议）
mod findx_pipe;

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
    /// FindX `name_highlight`：Unicode 标量下标 [start, end)。
    pub name_hl: Vec<(u32, u32)>,
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
        // 非 Windows 没有窗口探测，以 findx 端点缓存为准
        findx_pipe::has_cached_pipe()
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

/// 启动时预热：优先打通 FindX 端点，再协商 Everything IPC 布局（Windows）。
pub fn warmup() {
    let _ = findx_pipe::warmup();
    #[cfg(windows)]
    {
        ipc::warmup();
    }
}

/// 查询并返回结构化结果。
///
/// 三平台先走 FindX 端点（Windows 命名管道 / macOS+Linux UDS，默认拼音），
/// Windows 上 FindX 不可用再回退 Everything IPC。
pub fn query(search: &str, max_results: u32, timeout: Duration) -> Result<QueryResults, QueryError> {
    match findx_pipe::query(search, max_results, timeout) {
        Ok(r) => return Ok(r),
        Err(QueryError::NotRunning) => {}
        Err(e) => {
            // FindX 曾登记过端点、这次却超时/协议坏 → 冷却期内跳过，
            // 但**必须继续回退 Everything IPC**。
            // 旧实现 `Err(e) if has_cached_pipe() => return Err(e)` 让 FindX 一掉线
            // 就把整个全盘检索判死（实证：Everything 明明 0ms 应答，列表里却一条
            // 全盘结果都没有，还连带熔断 30s）。
            let _ = e;
        }
    }
    #[cfg(windows)]
    {
        ipc::query(search, max_results, timeout)
    }
    #[cfg(not(windows))]
    {
        Err(QueryError::NotRunning)
    }
}

/// 全盘检索的索引引擎。二者都是**外部服务**，不是本进程内建索引。
///
/// - `Everything`：voidtools 官方（1.4 / 1.5a），也含「复用同名窗口与消息号」
///   的第三方兼容实现，走 `WM_COPYDATA`。
/// - `FindX`：`findx2-service` 的私有通道 `\\.\pipe\findx2`，JSON 行协议，
///   带 `pinyin: true` 与 `name_highlight` 命中区间。
///
/// 选哪个由设置决定（默认 Everything，因为官方版装拼音插件后中文场景已够用）；
/// 两者都不可用时上层降级为内置目录递归，不需要在这里兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Everything,
    FindX,
}

impl Engine {
    /// 设置里的字符串 → 引擎。未知值一律回 Everything（与 `Settings::normalize` 一致）。
    pub fn parse(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("findx") {
            Self::FindX
        } else {
            Self::Everything
        }
    }

    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Everything => "Everything",
            Self::FindX => "FindX",
        }
    }
}

/// 该引擎此刻可用吗（服务在跑 / 端点能连）。
pub fn engine_available(engine: Engine) -> bool {
    match engine {
        Engine::Everything => is_running(),
        Engine::FindX => findx_pipe::endpoint_present(),
    }
}

/// 按指定引擎查询。
///
/// `Everything` 走官方 IPC（跳过 FindX 探测，用户已显式选它）；
/// `FindX` 优先自家管道，端点不在/协议坏时回退官方 IPC —— 宁可少拼音，
/// 也不能让「选了 FindX 但服务没起」变成全盘零结果。
pub fn query_engine(
    engine: Engine,
    search: &str,
    max_results: u32,
    timeout: Duration,
) -> Result<QueryResults, QueryError> {
    match engine {
        Engine::Everything => {
            #[cfg(windows)]
            {
                ipc::query_official(search, max_results, timeout)
            }
            #[cfg(not(windows))]
            {
                let _ = (search, max_results, timeout);
                Err(QueryError::NotRunning)
            }
        }
        Engine::FindX => match findx_pipe::query(search, max_results, timeout) {
            Ok(r) => Ok(r),
            Err(_) => {
                #[cfg(windows)]
                {
                    ipc::query_official(search, max_results, timeout)
                }
                #[cfg(not(windows))]
                {
                    Err(QueryError::NotRunning)
                }
            }
        },
    }
}

/// 按引擎预热：把布局协商（可能要一次空串全库探测）挪到启动时做掉，
/// 别让用户敲下第一个字符时再等。
pub fn warmup_engine(engine: Engine) {
    match engine {
        Engine::Everything => {
            #[cfg(windows)]
            {
                let _ = ipc::query_official("windows", 8, Duration::from_millis(1500));
            }
        }
        Engine::FindX => findx_pipe::warmup(),
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

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive_and_defaults_to_everything() {
        assert_eq!(Engine::parse("findx"), Engine::FindX);
        assert_eq!(Engine::parse("  FindX "), Engine::FindX);
        assert_eq!(Engine::parse("Everything"), Engine::Everything);
        assert_eq!(Engine::parse("everything"), Engine::Everything);
        // 空串/手改配置的垃圾值一律回默认引擎，与 Settings::normalize 同口径。
        assert_eq!(Engine::parse(""), Engine::Everything);
        assert_eq!(Engine::parse("voidtools"), Engine::Everything);
    }

    #[test]
    fn setting_roundtrip() {
        for e in [Engine::Everything, Engine::FindX] {
            assert_eq!(Engine::parse(e.as_setting()), e);
        }
    }
}
