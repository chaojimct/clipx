//! 平台能力 trait 骨架（Step 0）：形状先行，后端随后。
//!
//! - Windows 实现 = 现有代码平移（`keyboard_hook` / `paste` / `win_popup` /
//!   `filejump::inject`），行为冻结为基线。
//! - 非 Windows 先 stub（对应能力 `tier() == Unavailable`），M6/M7 只填后端，
//!   插件侧零改动。任何后端要求改插件代码 = 接口错了，改接口。
//!
//! 所有 trait 零平台依赖（不碰 `windows` / objc / X11 crate），后端在
//! `clipx-app` 的平台门控模块里实现。

use super::model::{Action, ItemId, QueryCtx};

/// 能力可用性分级：插件 manifest 按 requires/prefers 声明，后端按平台实现，
/// host 按分级开关插件与隐藏触发器（绝不"点了没反应"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapTier {
    Full,
    Degraded,
    Unavailable,
}

/// 键盘事件来源：Win `WH_KEYBOARD_LL` / mac keyWindow+全局热键 /
/// Linux X11 global-hotkey+取焦面板 / Wayland 无（CLI 入口）。
/// 现有 `keyboard_hook::{install,reinstall,set_visible}` 即 Windows 后端。
pub trait KeySource {
    fn tier(&self) -> CapTier;
    fn install(&mut self) -> bool;
    fn uninstall(&mut self);
    fn set_visible(&mut self, visible: bool);
}

/// 粘贴输出：Win SendInput / mac CGEvent / Linux XTest / Wayland 无（复制模式）。
/// 现有 `paste::{send_paste,write_*,normalize_console_text}` 即 Windows 后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    CtrlV,
    ShiftInsert,
    /// 逐字打出（RDP/终端/UAC 降级）。
    Type,
    /// 只写剪贴板、用户自粘（Wayland）。
    CopyOnly,
}

pub trait PlatformPaste {
    fn tier(&self) -> CapTier;
    fn send_paste(&self, mode: PasteMode);
}

/// 无焦点浮层：Win `WS_EX_NOACTIVATE` / mac `NONACTIVATING_PANEL`（M6a spike，
/// 失败退化获焦模型）/ Linux 取焦+失焦关闭。现有 `win_popup` 即 Windows 后端。
/// 首帧重绘 workaround 与定位锚点收归此处，插件只供内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Caret,
    Cursor,
    Dialog,
    Explorer,
    WorkArea,
}

pub trait PlatformPopup {
    fn tier(&self) -> CapTier;
    fn anchor(&self) -> Anchor;
}

/// 对话框跳转："把文件夹送到前台对话框"，host 按
/// 注入→AX Set→键盘链→复制路径选链（含 WPS 永不注入的门）。
/// 现有 `inject::navigate_to_folder` / `navigate_keyboard` 即 Windows 后端。
pub trait PlatformDialog {
    fn tier(&self) -> CapTier;
    fn current_folder(&self, dialog: isize) -> Option<String>;
    fn navigate(&self, dialog: isize, folder: &str) -> bool;
}

/// 行级数据源（住主弹窗共享列表）：history / phrases / vault / 命令式源。
/// 即 `merge_list_items` 的注册表版；同步 <5ms，异步走次帧回填。
pub trait DataSource: Send {
    fn id(&self) -> &'static str;
    fn search(&self, ctx: &QueryCtx) -> Vec<ItemId>;
}

/// 填充动作：即 `write_entry_clipboard` 的注册表版；Gate 由 host 代持。
pub trait FillAction: Send {
    fn id(&self) -> &'static str;
    fn fill(&self, id: &ItemId, action: Action) -> bool;
}
