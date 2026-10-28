//! 内部插件注册表（Step 2b/3）：事件收敛 + trait 落地。
//!
//! - 事件包络：`AppEvt::Qf*` / `AppEvt::Fj*` 收成一个 `AppEvt::PluginEvt`
//!  （plugin id + [`PluginPayload`]）。inline 后续照此加分支，不再加顶层变体。
//! - trait：[`AppPlugin`]（`id()`）是注册表的锚点；`QfPlugin` 是第一个实现。
//!   注册表结构体（`Vec<Box<dyn AppPlugin>>` + 统一 `dispatch`）等 FjState
//!   所有权随 Step 4 的 State 改工一起迁移时再立——状态机还在 State 手里，
//!   硬立注册表就是伪通用（见 runtime-design §4）。
//! - keyboard_hook 的 `KeyEvt::*` 生产侧不动：钩子继续发键事件，
//!   `main.rs` 转发器/热键表包成包络，逻辑层按 id 路由。

/// 插件事件载荷。QF/FJ 事件原样搬运（字段不动），只换包络。
#[derive(Debug, Clone)]
pub enum PluginPayload {
    QfKey(crate::keyboard_hook::KeyEvt),
    QfRowActivated(i32),
    QfFolderResolved {
        gen: u64,
        folder: Option<String>,
    },
    QfResults {
        gen: u64,
        view: crate::explorer_quickfind::QfView,
    },
    // ===== 文件夹跳转（Step 3a：worker 回投收敛；函数体暂留 logic.rs 同名 helper，
    // 3c 随 fj_show/fj_activate 一起搬进 FjPlugin）=====
    /// 后台采集完成（gen 过期则丢弃）。
    FjCollected {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 打字查询补充分阶段回投（gen 过期则丢弃）。
    FjEvResults {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 后台回读对话框当前目录（poll tick，不进按键路径）。
    FjFolder {
        dialog: isize,
        folder: Option<String>,
    },
    /// 后台导航完成（无条件复位 navigating，gen 过期也复位）。
    FjNavigated {
        gen: u64,
        path: String,
        ok: bool,
        is_file: bool,
    },
    // ===== 文件夹跳转 watcher/follow（Step 3b）=====
    /// 对话框前台自动弹出（watcher 线程，400ms 轮询 + ShowDelay 二次确认）。
    FjAuto(isize),
    /// WinEvent 实时跟随：对话框移动（跟随线程）。
    FjDialogMoved,
    /// WinEvent：对话框销毁（跟随线程）。
    FjDialogGone,
    // ===== 文件夹跳转触发与行动作（Step 3c）=====
    /// 全局 Ctrl+G / 托盘 / 热键表：切换 Picker（ FjToggle 经包络，见分发）。
    FjToggle,
    /// Picker 列表点击（行回调经包络）。
    FjRowActivated(i32),
    /// Picker header 手动拖拽（逻辑像素位移，经包络）。
    FjDragged(f32, f32),
    // ===== 行内补全（Step 1：键事件 + 行动，经包络；状态暂留主 State，
    // InlinePlugin 壳等交互冻结解除后立）=====
    /// 行内补全键（hook 观察到的键入/导航/确认/关闭，经 Key arm 转包络）。
    InlineKey(crate::keyboard_hook::KeyEvt),
    /// 行内小窗行点击补全。
    InlineActivate(i32),
    /// 行内小窗关闭。
    InlineDismissed,
}

/// 载荷类型名（诊断日志用，只记名不记内容——`InlineChar` 装着击键字符，
/// `QfResults` 装着路径，永远不许整包 `Debug` 落盘）。
/// 新增变体必须同步加分支（exhaustive match，编译器强制）。
pub fn payload_name(p: &PluginPayload) -> &'static str {
    match p {
        PluginPayload::QfKey(_) => "QfKey",
        PluginPayload::QfRowActivated(_) => "QfRowActivated",
        PluginPayload::QfFolderResolved { .. } => "QfFolderResolved",
        PluginPayload::QfResults { .. } => "QfResults",
        PluginPayload::FjCollected { .. } => "FjCollected",
        PluginPayload::FjEvResults { .. } => "FjEvResults",
        PluginPayload::FjFolder { .. } => "FjFolder",
        PluginPayload::FjNavigated { .. } => "FjNavigated",
        PluginPayload::FjAuto(_) => "FjAuto",
        PluginPayload::FjDialogMoved => "FjDialogMoved",
        PluginPayload::FjDialogGone => "FjDialogGone",
        PluginPayload::FjToggle => "FjToggle",
        PluginPayload::FjRowActivated(_) => "FjRowActivated",
        PluginPayload::FjDragged(..) => "FjDragged",
        PluginPayload::InlineKey(_) => "InlineKey",
        PluginPayload::InlineActivate(_) => "InlineActivate",
        PluginPayload::InlineDismissed => "InlineDismissed",
    }
}

/// 插件路由键。QF 用 `QfPlugin::ID`；FJ 壳（`FjPlugin`）/行内壳（`InlinePlugin`）
/// 落地前暂用常量，改名只改这一处。
pub const FILEJUMP_ID: &str = "filejump";
pub const INLINE_ID: &str = "inline";

/// 内部插件：`id()` 是路由键（分发按它匹配），也是审计日志的插件名；
/// `on_plugin_event()` 是包络分发唯一入口。`PluginCtx` 一次给齐四个分支
/// 所需（设置透传/回投通道/自家窗口弱引用/主窗可见性），插件不再直连 State。
pub trait AppPlugin {
    fn id(&self) -> &'static str;
    fn on_plugin_event(&mut self, payload: PluginPayload, ctx: &PluginCtx);
}

/// 分发上下文（`logic.rs` 的 `PluginEvt` arm 构造，字段借用可与插件可变借用并存）。
pub struct PluginCtx<'a> {
    pub settings: &'a crate::settings::Settings,
    pub evt_tx: &'a std::sync::mpsc::Sender<crate::logic::AppEvt>,
    pub qf: &'a slint::Weak<crate::QuickFindWindow>,
    pub main_visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_names_are_stable_for_log_parsers() {
        // 日志只认这些名字；改名同步改排障文档。
        assert_eq!(
            payload_name(&PluginPayload::QfKey(crate::keyboard_hook::KeyEvt::Esc)),
            "QfKey"
        );
        assert_eq!(payload_name(&PluginPayload::FjToggle), "FjToggle");
        assert_eq!(
            payload_name(&PluginPayload::FjDialogMoved),
            "FjDialogMoved"
        );
        assert_eq!(
            payload_name(&PluginPayload::InlineDismissed),
            "InlineDismissed"
        );
        assert_eq!(
            payload_name(&PluginPayload::FjFolder {
                dialog: 0,
                folder: None
            }),
            "FjFolder"
        );
    }
}
