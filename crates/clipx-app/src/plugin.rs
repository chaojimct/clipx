//! 内部插件注册表（Step 2b）：事件收敛 + trait 落地。
//!
//! - 事件包络：`AppEvt::Qf*` 四变体收成一个 `AppEvt::PluginEvt`（plugin id +
//!   [`PluginPayload`]）。FJ / inline 后续照此加分支，不再加顶层变体。
//! - trait：[`AppPlugin`]（`id()`）是注册表的锚点；`QfPlugin` 是第一个实现。
//!   注册表结构体（`Vec<Box<dyn AppPlugin>>` + 统一 `dispatch`）等第二个插件
//!   迁移（FileJump 3c）时再立——一个实现撑不起注册表，硬立就是伪通用。
//! - keyboard_hook 的 `KeyEvt::Qf*` 生产侧不动：钩子继续发键事件，
//!   `main.rs` 转发器包成包络，逻辑层按 id 路由。

/// 插件事件载荷。QF 四事件原样搬运（字段不动），只换包络。
#[derive(Debug)]
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
}

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
