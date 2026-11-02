//! 插件管理独立窗口（设置页之外的单独窗口）。
//!
//! - 为什么独立：设置页是草稿+保存语义，插件开关要即时生效语义，混一起打架。
//! - 模型照抄 `settings_win`（按需重建实例 + 事件循环线程强引用 + 无状态渲染），
//!   但**无草稿**：每次 push 按 live 设置重算快照（`plugin::builtin_plugins`）。
//! - 开关走 `AppEvt::PluginToggle`（即时落盘），下载走 `AppEvt::PackDownload`，
//!   链接走 `AppEvt::OpenUrl`——全是现成事件，无新逻辑路径。

use std::cell::RefCell;
use std::sync::mpsc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::logic::AppEvt;
use crate::plugin::{PackInfo, PluginInfo};

/// 管理窗口快照（无状态，每次 push 重算）。
pub struct PluginSnapshot {
    pub plugins: Vec<PluginInfo>,
    pub packs: Vec<PackInfo>,
}

/// 打开（销毁重建，躲软件渲染器 ReusedBuffer 的 hide→show 残缺坑——与设置窗同因）。
/// 实例 weak 经 `AppEvt::PluginWindowReady` 回传（与 SettingsWindowReady 同模式）。
pub fn open(snap: PluginSnapshot, evt_tx: mpsc::Sender<AppEvt>) {
    let _ = slint::invoke_from_event_loop(move || {
        CURRENT.with(|c| *c.borrow_mut() = None);
        close_current();
        let Ok(ui) = crate::PluginWindow::new() else {
            return;
        };
        bind(&ui, evt_tx.clone());
        apply(&ui, snap);
        crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        let size = slint::LogicalSize::new(560.0, 620.0);
        ui.window().set_size(slint::WindowSize::Logical(size));
        let _ = ui.window().show();
        #[cfg(windows)]
        {
            if let Some(hwnd) = crate::win_popup::find_window_by_title("clipx 插件") {
                crate::win_popup::activate_window(hwnd);
            }
        }
        let _ = evt_tx.send(AppEvt::PluginWindowReady(crate::logic::PluginWinReady(
            ui.as_weak(),
        )));
        CURRENT.with(|c| *c.borrow_mut() = Some(ui));
    });
}

/// 关闭（叉号语义：只隐藏实例，无草稿可丢）。
pub fn hide() {
    let _ = slint::invoke_from_event_loop(|| {
        close_current();
    });
}

/// 关闭并销毁当前插件窗口实例（与设置窗同因：只 hide 不 drop 会留幽灵窗口）。
fn close_current() {
    CURRENT.with(|c| {
        if let Some(ui) = c.borrow_mut().take() {
            let _ = ui.window().hide();
        }
    });
}

/// UI 自检用：把当前插件窗口渲染成 PNG（`CURRENT` 只在事件循环线程，
/// 故本函数也必须在事件循环线程调用）。返回 false = 当前没有打开的窗口。
pub fn snapshot_current_to(path: &str) -> bool {
    CURRENT.with(|c| {
        let borrow = c.borrow();
        let Some(ui) = borrow.as_ref() else {
            return false;
        };
        crate::save_snapshot(&ui.window(), path);
        true
    })
}

// 设置窗口强引用只存放在事件循环线程（Slint 组件的创建与 drop 都须在该线程）。
thread_local! {
    static CURRENT: RefCell<Option<crate::PluginWindow>> = const { RefCell::new(None) };
}

/// UI 回调 → 逻辑线程事件（窗口每次重建都要重新绑定）。
/// 开关/下载/链接全复用现成 AppEvt：SettingBool 走草稿（此处不用）、
/// PluginToggle 即时生效、PackDownload 下载、OpenUrl 浏览器。
fn bind(ui: &crate::PluginWindow, tx: mpsc::Sender<AppEvt>) {
    ui.on_plugin_toggle({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginToggle(v.into())); } });
    ui.on_pack_download({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PackDownload(v.into())); } });
    ui.on_open_url({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::OpenUrl(v.into())); } });
    ui.on_plugin_close({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::PluginClose); } });
}

/// 快照应用到 UI 实例（open 重建与后续 patch 共用）。
fn apply(ui: &crate::PluginWindow, snap: PluginSnapshot) {
    let plugins: Vec<crate::PluginCard> = snap
        .plugins
        .iter()
        .map(|p| crate::PluginCard {
            name: p.name.into(),
            desc: p.desc.into(),
            status: p.status.clone().into(),
            enabled: p.enabled,
            has_toggle: p.toggle.is_some(),
            toggle: p.toggle.unwrap_or("").into(),
        })
        .collect();
    ui.set_plugin_rows(ModelRc::new(VecModel::from(plugins)));
    let packs: Vec<crate::PackCard> = snap
        .packs
        .iter()
        .map(|p| {
            let (is_url, target) = match p.btn_cmd {
                Some(c) if c.starts_with("url:") => (true, c[4..].to_string()),
                Some(c) if c.starts_with("pack:") => (false, c[5..].to_string()),
                _ => (false, String::new()),
            };
            crate::PackCard {
                name: p.name.into(),
                desc: p.desc.into(),
                state: p.state.clone().into(),
                has_btn: p.btn_label.is_some(),
                btn_label: p.btn_label.unwrap_or("").into(),
                is_url,
                target: target.into(),
            }
        })
        .collect();
    ui.set_pack_rows(ModelRc::new(VecModel::from(packs)));
}

/// 存量推送（开关拨动/下载状态变化后刷新内容，不重建实例）。
pub fn push(weak: &slint::Weak<crate::PluginWindow>, snap: PluginSnapshot) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        apply(&ui, snap);
    });
}
