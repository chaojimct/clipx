//! 登录 / 解锁弹窗（密码库入口重设计，Windows 与非 Windows 同一份 UI）。
//!
//! - 模型照抄 `plugin_win`：按需重建实例（躲软件渲染器 ReusedBuffer 残缺坑）、
//!   回调 → `AppEvt`、快照无状态重推；实例强引用只存事件循环线程。
//! - 字段值只在 `open` 时下发（预填邮箱/服务器）；之后**永不回推**——
//!   双向绑定显示用户键入，回推会让光标跳（旧统一窗主密码框的教训）。
//! - 主密码不进本模块任何结构：暂存在 logic 层 `VaultDialogState`（内存态，
//!   关窗即清）；这里只转发键入事件。
//! - X / 取消 / `PluginClose`（统一窗关闭）都走 `AppEvt::VaultDialogClose`，
//!   逻辑层清字段后调 [`hide`]。

use std::cell::RefCell;
use std::sync::mpsc;

use slint::ComponentHandle;

use crate::logic::AppEvt;

/// 开窗参数（首帧字段预填：邮箱/服务器来自设置；2FA 重试后重开保留已填值，
/// 由 logic 层传入）。
pub struct DialogInit {
    /// "login" | "unlock"。
    pub mode: &'static str,
    /// 登录步骤：1 = 账密，2 = 二次验证（解锁恒 1）。
    pub step: u8,
    pub email: String,
    pub server: String,
    pub show_server: bool,
    pub hello_available: bool,
}

/// 状态重推（忙位/步骤/状态行/方式切换；不含字段值，见模块注释）。
pub struct DialogSnapshot {
    pub mode: &'static str,
    pub step: u8,
    pub busy: bool,
    pub status: String,
    pub status_warn: bool,
    pub suggest_login: bool,
    pub show_server: bool,
    pub method: u8,
    pub hello_available: bool,
}

/// 打开（销毁重建；`open` 之后字段只由用户键入驱动，不回推）。
pub fn open(init: DialogInit, evt_tx: mpsc::Sender<AppEvt>) {
    let _ = slint::invoke_from_event_loop(move || {
        close_current();
        let Ok(ui) = crate::VaultDialogWindow::new() else {
            return;
        };
        bind(&ui, evt_tx.clone());
        apply_init(&ui, &init);
        crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        // 原生 X 与「取消」同一条关闭路径（清字段由逻辑层做）。
        {
            let tx = evt_tx.clone();
            ui.window().on_close_requested(move || {
                let _ = tx.send(AppEvt::VaultDialogClose);
                slint::CloseRequestResponse::KeepWindowShown
            });
        }
        let _ = ui.window().show();
        // 文档型窗口收编（同设置窗）：摘浮窗 owner（否则不进任务栏 / 落 (0,0)
        // / 不抢前台）+ 光标屏居中 + 后台一拍后找窗激活（adopt_document_window）。
        let logical_h = match (init.mode, init.step) {
            ("unlock", _) => 310.0,
            ("login", 2) => 420.0,
            ("login", _) if init.show_server => 460.0,
            _ => 400.0,
        };
        crate::win_popup::center_on_cursor_monitor(ui.window(), 430.0, logical_h);
        crate::win_popup::adopt_document_window("clipx 密码库");
        CURRENT.with(|c| *c.borrow_mut() = Some(ui));
    });
}

/// 关闭并销毁实例（X/取消/成功收场，逻辑层调）。
pub fn hide() {
    let _ = slint::invoke_from_event_loop(|| {
        close_current();
    });
}

fn close_current() {
    CURRENT.with(|c| {
        if let Some(ui) = c.borrow_mut().take() {
            let _ = ui.window().hide();
        }
    });
}

/// UI 自检用：把当前弹窗渲染成 PNG（`--vault-dialog-demo` 快照路径；
/// 必须在事件循环线程调用；没开返回 false）。
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

// 实例强引用只放事件循环线程（创建与 drop 都必须在该线程，同 plugin_win）。
thread_local! {
    static CURRENT: RefCell<Option<crate::VaultDialogWindow>> = const { RefCell::new(None) };
}

/// UI 回调 → 逻辑线程事件（每次重建重绑；`submit` 只发动作名，
/// 步骤分流由逻辑层按 State 完成——UI 不猜状态）。
fn bind(ui: &crate::VaultDialogWindow, tx: mpsc::Sender<AppEvt>) {
    ui.on_close_req({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogClose); } });
    ui.on_field_edited({ let tx = tx.clone(); move |k, v| { let _ = tx.send(AppEvt::VaultDialogField(k.into(), v.into())); } });
    ui.on_submit({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("submit".into())); } });
    ui.on_send_email({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("send-email".into())); } });
    ui.on_hello({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("hello".into())); } });
    ui.on_back({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("back".into())); } });
    ui.on_switch_mode({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("switch".into())); } });
    ui.on_toggle_server({ let tx = tx.clone(); move || { let _ = tx.send(AppEvt::VaultDialogAction("server".into())); } });
}

fn apply_init(ui: &crate::VaultDialogWindow, init: &DialogInit) {
    ui.set_mode(init.mode.into());
    ui.set_step(init.step as i32);
    ui.set_busy(false);
    ui.set_status("".into());
    ui.set_status_warn(false);
    ui.set_suggest_login(false);
    ui.set_show_server(init.show_server);
    ui.set_hello_available(init.hello_available);
    ui.set_method(0);
    ui.set_field_email(init.email.as_str().into());
    ui.set_field_pw("".into());
    ui.set_field_server(init.server.as_str().into());
    ui.set_field_code("".into());
}

/// 状态重推（字段不动）。窗口没开是空操作（逻辑层早退，双保险）。
pub fn push(snap: DialogSnapshot) {
    let _ = slint::invoke_from_event_loop(move || {
        CURRENT.with(|c| {
            let borrow = c.borrow_mut();
            let Some(ui) = borrow.as_ref() else {
                return;
            };
            ui.set_mode(snap.mode.into());
            ui.set_step(snap.step as i32);
            ui.set_busy(snap.busy);
            ui.set_status(snap.status.as_str().into());
            ui.set_status_warn(snap.status_warn);
            ui.set_suggest_login(snap.suggest_login);
            ui.set_show_server(snap.show_server);
            ui.set_method(snap.method as i32);
            ui.set_hello_available(snap.hello_available);
        });
    });
}
