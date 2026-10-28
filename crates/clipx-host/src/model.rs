//! 窄腰数据模型（Step 0 骨架）：插件与底座之间只用四样东西对话——
//! Item（结果）、Action（动作）、Query（查询上下文）、Trigger（触发器）。
//! 纯数据、无行为；三平台一致。Step 1~4 迁移时逐项接线，形状变更走 ADR。

use clipx_core::EntryKind;

/// 条目 ID：命名空间化，替代 `logic.rs` 的负 ID hack（`id < 0` = 短语，
/// 加一个 vault 源就撞车）。`EntryMeta` 本体不动，Step 4 外包
/// `ListItem { id, meta }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    /// SQLite 历史行。
    Db(i64),
    /// 外部源：`Ext("phrases", idx)`、`Ext("vault", n)`……
    Ext(&'static str, u64),
}

/// 前台上下文：一次查询/填充决策所需的全部环境（host 一次取齐，插件只读）。
#[derive(Debug, Clone, Default)]
pub struct ForegroundCtx {
    /// 前台进程基名（无 .exe，已小写），空串 = 未知。
    pub exe_stem: String,
    /// 前台窗口类名（Windows）。
    pub window_class: String,
    /// 是否密码框 / Secure Input（真 = 全插件静音，底座强制）。
    pub password_focus: bool,
    /// 是否终端目标（Shift+Insert + 去 CR）。
    pub console: bool,
    /// 是否提权/受限目标（UAC、RDP、Secure Desktop：注入与模拟可能失效）。
    pub restricted: bool,
}

/// 统一查询上下文（含模式前缀剥离后的 tokens，空格交集语义保留）。
#[derive(Debug, Clone, Default)]
pub struct QueryCtx {
    pub raw: String,
    pub tokens: Vec<String>,
    pub filter: Option<EntryKind>,
    pub source: Option<String>,
    pub deep: bool,
    pub limit: i64,
    pub foreground: ForegroundCtx,
}

/// 动作：动词与目标解耦，目标感知（终端/提权/Wayland 降级）由 host 裁定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Paste,
    Copy,
    /// 逐字打出（RDP/终端/UAC 下粘贴被吞时的降级）。
    Type,
    Open,
    Reveal,
    /// 秘密填充（备份剪贴板→写→粘贴→恢复→清零，全走 ephemeral 通道）。
    FillSecret,
    RunCommand,
    Pin,
    Delete,
    Edit,
    /// 文本变换（翻译/改写/AI 预留；秘密与密码框内容永不进模型，host 强制）。
    Transform,
}

/// 触发器：热键/对话框/打字/托盘/启动/CLI/URI/Timer 全是触发器，
/// 三平台入口不同但归一到同一个 Trigger（见 runtime-design §7.3）。
#[derive(Debug, Clone)]
pub enum Trigger {
    Hotkey(&'static str),
    DialogForeground(isize),
    ExplorerTyping(char),
    AnyTyping,
    TrayItem(&'static str),
    Startup,
    Cli(String),
    Uri(String),
    Timer(u64),
}
