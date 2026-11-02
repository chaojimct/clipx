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
/// 选中序号不进快照：存在逻辑层 State（`plugin_selected`），随快照一起下发。
/// 右侧详情/配置区的一帧（纯数据，Slint 只读；`select_detail` 纯函数可单测）。
pub struct PluginDetail {
    pub title: String,
    pub desc: String,
    pub state: String,
    pub is_plugin: bool,
    pub has_toggle: bool,
    pub toggle_on: bool,
    pub toggle_target: String,
    pub has_btn: bool,
    pub btn_label: String,
    pub btn_is_url: bool,
    pub btn_target: String,
    pub note: String,
}

/// 导航总行数（左侧两组之和；统一序号 0..n 为插件，其后为拓展包）。
pub fn nav_count(snap: &PluginSnapshot) -> usize {
    snap.plugins.len() + snap.packs.len()
}

/// 选中钳制（行数变化/旧值越界时兜底 0；空表也返回 0）。
pub fn clamp_selected(snap: &PluginSnapshot, selected: usize) -> usize {
    let n = nav_count(snap);
    if n == 0 || selected >= n {
        0
    } else {
        selected
    }
}

/// 统一序号 → 右侧详情。pack 的 `btn_cmd` 沿用旧解析（`pack:<id>` 下载，`url:` 浏览器）。
pub fn select_detail(snap: &PluginSnapshot, selected: usize) -> PluginDetail {
    let sel = clamp_selected(snap, selected);
    if sel < snap.plugins.len() {
        let p = &snap.plugins[sel];
        PluginDetail {
            title: p.name.to_string(),
            desc: p.desc.to_string(),
            state: p.status.clone(),
            is_plugin: true,
            has_toggle: p.toggle.is_some(),
            toggle_on: p.enabled,
            toggle_target: p.toggle.unwrap_or("").to_string(),
            has_btn: false,
            btn_label: String::new(),
            btn_is_url: false,
            btn_target: String::new(),
            note: p.config_note.to_string(),
        }
    } else {
        let p = &snap.packs[sel - snap.plugins.len()];
        let (is_url, target) = match p.btn_cmd {
            Some(c) if c.starts_with("url:") => (true, c[4..].to_string()),
            Some(c) if c.starts_with("pack:") => (false, c[5..].to_string()),
            _ => (false, String::new()),
        };
        PluginDetail {
            title: p.name.to_string(),
            desc: p.desc.to_string(),
            state: p.state.clone(),
            is_plugin: false,
            has_toggle: false,
            toggle_on: false,
            toggle_target: String::new(),
            has_btn: p.btn_label.is_some(),
            btn_label: p.btn_label.unwrap_or("").to_string(),
            btn_is_url: is_url,
            btn_target: target,
            note: p.config_note.to_string(),
        }
    }
}

pub struct PluginSnapshot {
    pub plugins: Vec<PluginInfo>,
    pub packs: Vec<PackInfo>,
}

/// 打开（销毁重建，躲软件渲染器 ReusedBuffer 的 hide→show 残缺坑——与设置窗同因）。
/// 实例 weak 经 `AppEvt::PluginWindowReady` 回传（与 SettingsWindowReady 同模式）。
pub fn open(snap: PluginSnapshot, selected: usize, evt_tx: mpsc::Sender<AppEvt>) {
    let _ = slint::invoke_from_event_loop(move || {
        CURRENT.with(|c| *c.borrow_mut() = None);
        close_current();
        let Ok(ui) = crate::PluginWindow::new() else {
            return;
        };
        bind(&ui, evt_tx.clone());
        apply(&ui, snap, selected);
        crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        let size = slint::LogicalSize::new(760.0, 620.0);
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
    ui.on_plugin_select({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginSelect(v)); } });
    ui.on_plugin_toggle({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PluginToggle(v.into())); } });
    ui.on_pack_download({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::PackDownload(v.into())); } });
    ui.on_open_url({ let tx = tx.clone(); move |v| { let _ = tx.send(AppEvt::OpenUrl(v.into())); } });
    ui.on_plugin_close({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::PluginClose); } });
}

/// 快照应用到 UI 实例（open 重建与后续 patch 共用；selected 为统一序号）。
fn apply(ui: &crate::PluginWindow, snap: PluginSnapshot, selected: usize) {
    let sel = clamp_selected(&snap, selected);
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
    ui.set_plugin_count(snap.plugins.len() as i32);
    ui.set_selected(sel as i32);
    let d = select_detail(&snap, sel);
    ui.set_detail_title(d.title.into());
    ui.set_detail_desc(d.desc.into());
    ui.set_detail_state(d.state.into());
    ui.set_detail_is_plugin(d.is_plugin);
    ui.set_detail_has_toggle(d.has_toggle);
    ui.set_detail_toggle_on(d.toggle_on);
    ui.set_detail_toggle_target(d.toggle_target.into());
    ui.set_detail_has_btn(d.has_btn);
    ui.set_detail_btn_label(d.btn_label.into());
    ui.set_detail_btn_is_url(d.btn_is_url);
    ui.set_detail_btn_target(d.btn_target.into());
    ui.set_detail_note(d.note.into());
}

/// 存量推送（开关拨动/下载状态变化后刷新内容，不重建实例）。
pub fn push(
    weak: &slint::Weak<crate::PluginWindow>,
    snap: PluginSnapshot,
    selected: usize,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        apply(&ui, snap, selected);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{PackInfo, PluginInfo};

    fn snap() -> PluginSnapshot {
        PluginSnapshot {
            plugins: vec![
                PluginInfo {
                    name: "甲",
                    desc: "d",
                    enabled: true,
                    status: "开".to_string(),
                    toggle: Some("qf"),
                    config_note: "n1",
                },
                PluginInfo {
                    name: "乙",
                    desc: "d",
                    enabled: false,
                    status: "纯状态".to_string(),
                    toggle: None,
                    config_note: "n2",
                },
            ],
            packs: vec![PackInfo {
                name: "包",
                desc: "d",
                state: "未安装".to_string(),
                btn_label: Some("下载"),
                btn_cmd: Some("pack:bw"),
                config_note: "n3",
            }],
        }
    }

    #[test]
    fn select_detail_routes_plugin_and_pack() {
        let s = snap();
        assert_eq!(nav_count(&s), 3);
        let d0 = select_detail(&s, 0);
        assert!(d0.is_plugin && d0.has_toggle && d0.toggle_on);
        assert_eq!(d0.toggle_target, "qf");
        assert_eq!(d0.note, "n1");
        let d1 = select_detail(&s, 1);
        assert!(d1.is_plugin && !d1.has_toggle && !d1.toggle_on);
        let d2 = select_detail(&s, 2);
        assert!(!d2.is_plugin && d2.has_btn && !d2.btn_is_url);
        assert_eq!(d2.btn_target, "bw");
        assert_eq!(d2.note, "n3");
    }

    #[test]
    fn select_detail_clamps_out_of_range() {
        let s = snap();
        // 越界兜底 0（行数变化时旧选中不 panic、不悬空）。
        let d = select_detail(&s, 99);
        assert_eq!(d.title, "甲");
        let empty = PluginSnapshot {
            plugins: vec![],
            packs: vec![],
        };
        assert_eq!(clamp_selected(&empty, 5), 0);
        assert_eq!(nav_count(&empty), 0);
    }

    #[test]
    fn select_detail_parses_url_action() {
        let mut s = snap();
        s.packs[0].btn_cmd = Some("url:https://example.com");
        s.packs[0].btn_label = Some("看看");
        let d = select_detail(&s, 2);
        assert!(d.btn_is_url);
        assert_eq!(d.btn_target, "https://example.com");
    }
}
