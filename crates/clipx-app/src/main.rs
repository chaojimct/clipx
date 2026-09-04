#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod explorer_quickfind;
mod explorer_shell;
mod filejump;
mod keyboard_hook;
mod logic;
mod mouse_hook;
mod paste;
mod policy;
mod settings;
mod settings_win;
mod update_check;
mod win_popup;

use std::sync::mpsc;

use anyhow::{Context, Result};

use clipx_core::event::ClipEvent;
use clipx_core::{ClipboardGate, EntryKind, NewEntry};
use clipx_ocr::OcrQueue;
use clipx_store::{InsertOutcome, Store, StoreLimits};

use logic::{AppEvt, MenuAction};
use settings::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};

slint::include_modules!();

/// 热键目标（WPF 三组全局热键 + 可选 Win+V 替换 + 批量切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyKind {
    Clip,
    FileJump,
    Batch,
}

/// 一组待注册热键（设置保存后经通道热更新）。
#[derive(Debug, Clone)]
pub struct HotkeySet {
    pub entries: Vec<(Hotkey, HotkeyKind)>,
}

pub fn hotkeys_from_settings(s: &settings::Settings) -> HotkeySet {
    let entries = vec![
        (s.hotkey, HotkeyKind::Clip),
        (s.filejump_hotkey, HotkeyKind::FileJump),
        (s.batch_hotkey, HotkeyKind::Batch),
    ];
    if s.replace_win_v {
        // Win+V 由键盘钩子拦截并注入 Win KeyUp，不再 RegisterHotKey（避免闪开始菜单）
    }
    HotkeySet { entries }
}

/// 虚键码 → global-hotkey Code（常用全集；Caps 修饰/生僻键返回 None 跳过）。
pub fn vk_to_code(vk: u32) -> Option<global_hotkey::hotkey::Code> {
    use global_hotkey::hotkey::Code::*;
    Some(match vk {
        0x08 => Backspace,
        0x09 => Tab,
        0x0D => Enter,
        0x1B => Escape,
        0x20 => Space,
        0x21 => PageUp,
        0x22 => PageDown,
        0x23 => End,
        0x24 => Home,
        0x25 => ArrowLeft,
        0x26 => ArrowUp,
        0x27 => ArrowRight,
        0x28 => ArrowDown,
        0x2C => PrintScreen,
        0x2D => Insert,
        0x2E => Delete,
        0x30 => Digit0,
        0x31 => Digit1,
        0x32 => Digit2,
        0x33 => Digit3,
        0x34 => Digit4,
        0x35 => Digit5,
        0x36 => Digit6,
        0x37 => Digit7,
        0x38 => Digit8,
        0x39 => Digit9,
        0x41 => KeyA,
        0x42 => KeyB,
        0x43 => KeyC,
        0x44 => KeyD,
        0x45 => KeyE,
        0x46 => KeyF,
        0x47 => KeyG,
        0x48 => KeyH,
        0x49 => KeyI,
        0x4A => KeyJ,
        0x4B => KeyK,
        0x4C => KeyL,
        0x4D => KeyM,
        0x4E => KeyN,
        0x4F => KeyO,
        0x50 => KeyP,
        0x51 => KeyQ,
        0x52 => KeyR,
        0x53 => KeyS,
        0x54 => KeyT,
        0x55 => KeyU,
        0x56 => KeyV,
        0x57 => KeyW,
        0x58 => KeyX,
        0x59 => KeyY,
        0x5A => KeyZ,
        0x60 => Numpad0,
        0x61 => Numpad1,
        0x62 => Numpad2,
        0x63 => Numpad3,
        0x64 => Numpad4,
        0x65 => Numpad5,
        0x66 => Numpad6,
        0x67 => Numpad7,
        0x68 => Numpad8,
        0x69 => Numpad9,
        0x70 => F1,
        0x71 => F2,
        0x72 => F3,
        0x73 => F4,
        0x74 => F5,
        0x75 => F6,
        0x76 => F7,
        0x77 => F8,
        0x78 => F9,
        0x79 => F10,
        0x7A => F11,
        0x7B => F12,
        0xBA => Semicolon,
        0xBB => Equal,
        0xBC => Comma,
        0xBD => Minus,
        0xBE => Period,
        0xBF => Slash,
        0xC0 => Backquote,
        0xDB => BracketLeft,
        0xDC => Backslash,
        0xDD => BracketRight,
        0xDE => Quote,
        _ => return None,
    })
}

fn hotkey_modifiers(m: u32) -> Option<global_hotkey::hotkey::Modifiers> {
    use global_hotkey::hotkey::Modifiers;
    // CapsLock 不是 RegisterHotKey 修饰键（WPF 同样只认四键），含则跳过。
    if m & settings::MOD_CAPS != 0 {
        return None;
    }
    let mut out = Modifiers::empty();
    if m & MOD_CONTROL != 0 {
        out |= Modifiers::CONTROL;
    }
    if m & MOD_ALT != 0 {
        out |= Modifiers::ALT;
    }
    if m & MOD_SHIFT != 0 {
        out |= Modifiers::SHIFT;
    }
    if m & MOD_WIN != 0 {
        out |= Modifiers::SUPER;
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn main() -> Result<()> {
    let seed = parse_seed_arg();
    let db_path = default_db_path()?;
    let settings_path = default_settings_path()?;
    let settings = settings::load(&settings_path);

    if std::env::args().any(|a| a == "--bench") {
        return bench();
    }

    if let Some(expr) = parse_everything_query_arg() {
        return everything_query_cli(&expr);
    }

    // 库容上限来自设置（WPF 版 MaxItems / MaxImageItems 语义）
    let limits = StoreLimits {
        max_items: settings.max_items,
        max_image_items: settings.max_image_items,
    };
    let store = Store::open(&db_path, limits).context("打开数据库失败")?;
    if let Some(n) = seed {
        store.seed(n).context("写入压测数据失败")?;
        println!("已写入 {n} 条压测数据");
        return Ok(());
    }
    if let Some(wpf_db) = parse_import_wpf_arg() {
        import_wpf(
            &store,
            std::path::PathBuf::from(wpf_db),
            &settings_path,
            &settings,
        )?;
        return Ok(());
    }

    if !ensure_single_instance() {
        eprintln!("clipx 已在运行，退出本实例");
        return Ok(());
    }

    let gate = ClipboardGate::new();

    let (evt_tx, evt_rx) = mpsc::channel::<AppEvt>();

    let ui = PopupWindow::new().context("创建窗口失败")?;
    win_popup::apply_style(ui.window());
    ui.window().hide().ok();

    // 快速查找浮层（M4）：常驻实例 Hide/Show 复用（对齐 WPF EnsureWindow）
    let qf_ui = QuickFindWindow::new().context("创建快速查找窗口失败")?;
    win_popup::apply_style(qf_ui.window());
    qf_ui.window().hide().ok();
    // 文件夹跳转浮层（M5d）：同上常驻复用
    let fj_ui = FileJumpWindow::new().context("创建跳转窗口失败")?;
    win_popup::apply_style(fj_ui.window());
    fj_ui.window().hide().ok();
    {
        let tx = evt_tx.clone();
        fj_ui.on_row_activated(move |i| {
            let _ = tx.send(AppEvt::FjRowActivated(i));
        });
    }
    {
        let tx = evt_tx.clone();
        fj_ui.on_drag_by(move |dx, dy| {
            let _ = tx.send(AppEvt::FjDragged(dx, dy));
        });
    }
    {
        let tx = evt_tx.clone();
        qf_ui.on_row_activated(move |i| {
            let _ = tx.send(AppEvt::QfRowActivated(i));
        });
    }
    // 设置窗口（Phase A）：常驻隐藏，逻辑线程经事件驱动（暂存模式）。
    let settings_ui = SettingsWindow::new().context("创建设置窗口失败")?;
    settings_ui.window().hide().ok();
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_bool(move |v| {
            let _ = tx.send(AppEvt::SettingBool(v.into()));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_cycle(move |v| {
            let _ = tx.send(AppEvt::SettingCycle(v.into()));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_record(move |v| {
            let _ = tx.send(AppEvt::SettingRecord(v));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_text(move |a, b| {
            let _ = tx.send(AppEvt::SettingText(a.into(), b.into()));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_save(move || {
            let _ = tx.send(AppEvt::SettingSave);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_cancel(move || {
            let _ = tx.send(AppEvt::SettingCancel);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_setting_clear(move || {
            let _ = tx.send(AppEvt::SettingClear);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_excl_add(move || {
            let _ = tx.send(AppEvt::ExclAdd);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_proc_selected(move |v| {
            let _ = tx.send(AppEvt::SettingText("proc".into(), v.into()));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_excl_del(move |i| {
            let _ = tx.send(AppEvt::ExclDel(i));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_rule_del(move |i| {
            let _ = tx.send(AppEvt::RuleDel(i));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_custom_del(move |i| {
            let _ = tx.send(AppEvt::CustomDel(i));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_custom_import(move || {
            let _ = tx.send(AppEvt::CustomImport);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_custom_export(move || {
            let _ = tx.send(AppEvt::CustomExport);
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_phrase_sel(move |i| {
            let _ = tx.send(AppEvt::SettingText("psel".into(), i.to_string()));
        });
    }
    {
        let tx = evt_tx.clone();
        settings_ui.on_page_changed(move |p| {
            let _ = tx.send(AppEvt::SettingPage(p));
        });
    }

    let tray = match TrayIcon::new() {
        Ok(tray) => Some(tray),
        Err(e) => {
            eprintln!("系统托盘不可用: {e}");
            None
        }
    };

    // OCR 队列：启动回填（迁移图片补做）由 worker 自驱批量拉取；
    // 新图片实时入队，完成/失败一个条目即通知刷新
    let ocr_queue = spawn_ocr(store.clone(), evt_tx.clone(), settings.image_ocr_enabled)?;

    // 剪贴板监听（事件直发处理器；Gate 防自环）
    let (clip_tx, clip_rx) = mpsc::channel::<ClipEvent>();
    clipx_monitor::spawn(clip_tx, gate.clone()).context("启动剪贴板监听失败")?;

    // 全局策略快照（热键排除名单 + 面板主键 + watcher 配置）
    policy::set_exclusions(&settings.exclusion_apps);
    policy::set_panel_key(&settings.panel_key);
    policy::set_ocr_enabled(settings.image_ocr_enabled);
    policy::apply_win_v_replace(settings.replace_win_v);
    keyboard_hook::set_replace_win_v(settings.replace_win_v);
    clipx_monitor::set_max_image_bytes(settings.max_image_bytes);
    {
        let custom_path = settings_path
            .parent()
            .unwrap_or(&settings_path)
            .join("custom_file_dialogs.json");
        clipx_filejump::custom::set_runtime_rules(
            clipx_filejump::custom::CustomStore::load(&custom_path).rules,
        );
    }
    keyboard_hook::set_page_hotkeys(settings.page_up, settings.page_down);
    keyboard_hook::set_passthrough(
        settings.passthrough_enabled,
        settings.passthrough_mask,
        settings.passthrough_keep_panel_keys,
        &settings.passthrough_rules,
    );
    filejump::set_follow_mouse(settings.filejump_follow_mode == "Mouse");
    filejump::watch_set(
        settings.filejump_enabled,
        settings.filejump_auto_popup,
        settings.filejump_show_delay_ms,
    );
    settings_win::apply_theme(&settings.theme, &ui.as_weak());
    win_popup::warmup_caret_uia();
    let _ = std::thread::Builder::new()
        .name("clipx-ev-warmup".into())
        .spawn(|| clipx_everything::warmup());

    // 热键：独立线程自泵消息（Slint 主循环不派发他人 HWND 的 WM_HOTKEY）
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<HotkeySet>();
    spawn_hotkey(evt_tx.clone(), hotkeys_from_settings(&settings), hotkey_rx)?;

    // 处理线程：入库成功后通知逻辑线程刷新列表；新图片入 OCR 队列
    spawn_processor(clip_rx, store.clone(), evt_tx.clone(), ocr_queue.clone())?;

    // 钩子事件通道先行建立，再安装钩子（避免早期事件丢失）；
    // Qf 事件单独路由（快速查找会话与主弹窗键路由互斥）
    let key_rx = keyboard_hook::evt_channel();
    let mouse_rx = mouse_hook::hide_channel();
    spawn_hook_forwarder("clipx-key-fwd", evt_tx.clone(), key_rx, |k| {
        if k.is_qf() {
            AppEvt::QfKey(k)
        } else {
            AppEvt::Key(k)
        }
    })?;
    spawn_hook_forwarder("clipx-mouse-fwd", evt_tx.clone(), mouse_rx, |_| {
        AppEvt::Hide
    })?;

    // 快速查找开关（Explorer 内 Everything 检索，M4）
    keyboard_hook::set_qf_enabled(settings.explorer_everything_quickfind_enabled);

    // 钩子在事件循环线程安装（LL 钩子依赖本线程消息循环）
    if !keyboard_hook::install() {
        eprintln!("键盘钩子安装失败：弹窗键盘输入不可用");
    }
    if !mouse_hook::install() {
        eprintln!("鼠标钩子安装失败：点击外部关闭不可用");
    }

    // UI 回调 → 逻辑线程
    {
        let tx = evt_tx.clone();
        ui.on_row_clicked(move |i| {
            let _ = tx.send(AppEvt::RowClicked(i));
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_row_double_clicked(move |i| {
            let _ = tx.send(AppEvt::RowDoubleClicked(i));
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_filter_clicked(move || {
            let _ = tx.send(AppEvt::FilterCycle);
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_batch_clicked(move || {
            let _ = tx.send(AppEvt::BatchCycle);
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_menu_requested(move |i| {
            let _ = tx.send(AppEvt::MenuRequest(i));
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_menu_action(move |action| {
            let a = match action.as_str() {
                "copy" => MenuAction::Copy,
                "pin" => MenuAction::Pin,
                "phrase" => MenuAction::Phrase,
                "edit" => MenuAction::Edit,
                "ocr" => MenuAction::OcrPaste,
                "file" => MenuAction::PasteAsFile,
                "json" => MenuAction::PasteAsJson,
                "saveimg" => MenuAction::SaveImage,
                "copypath" => MenuAction::CopyPath,
                "source" => MenuAction::FilterSource,
                _ => MenuAction::Delete,
            };
            let _ = tx.send(AppEvt::MenuAction(a));
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_menu_close(move || {
            let _ = tx.send(AppEvt::MenuClose);
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_open_settings(move || {
            let _ = tx.send(AppEvt::OpenSettings);
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_pin_window(move || {
            let _ = tx.send(AppEvt::PinWindow);
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_title_drag(move |dx, dy| {
            let _ = tx.send(AppEvt::PopupDragged(dx as f32, dy as f32));
        });
    }
    {
        let tx = evt_tx.clone();
        ui.on_middle_preview(move |i| {
            let _ = tx.send(AppEvt::MiddlePreview(i));
        });
    }
    if let Some(tray) = tray.as_ref() {
        let tx = evt_tx.clone();
        tray.on_tray_toggle(move || {
            let _ = tx.send(AppEvt::Toggle);
        });
        {
            let tx = evt_tx.clone();
            tray.on_tray_pause(move || {
                let _ = tx.send(AppEvt::TrayPause);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_clear(move || {
                let _ = tx.send(AppEvt::TrayClear);
            });
        }
        tray.set_autostart_label(
            if autostart::is_enabled() {
                "开机自启：开"
            } else {
                "开机自启：关"
            }
            .into(),
        );
        {
            let tx = evt_tx.clone();
            tray.on_tray_autostart(move || {
                let _ = tx.send(AppEvt::TrayAutostart);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_filejump(move || {
                let _ = tx.send(AppEvt::FileJumpToggle);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_settings(move || {
                let _ = tx.send(AppEvt::OpenSettings);
            });
        }
        tray.on_tray_quit(move || {
            let _ = slint::quit_event_loop();
        });
        {
            let tx = evt_tx.clone();
            tray.on_tray_probe(move || {
                let _ = tx.send(AppEvt::TrayProbe);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_about(move || {
                let _ = tx.send(AppEvt::TrayAbout);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_update(move || {
                let _ = tx.send(AppEvt::TrayUpdate);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_export(move || {
                let _ = tx.send(AppEvt::TrayExport);
            });
        }
        {
            let tx = evt_tx.clone();
            tray.on_tray_import(move || {
                let _ = tx.send(AppEvt::TrayImport);
            });
        }
    }

    // 文件夹跳转自动弹出 watcher（M5d，对话框前台轮询；配置走 watch_set 热更新）
    filejump::spawn_watcher(evt_tx.clone());

    let store_for_exit = store.clone();
    if settings.check_updates {
        crate::update_check::spawn(evt_tx.clone(), settings.last_update_tag.clone());
    }

    logic::spawn(
        logic::LogicDeps {
            store,
            settings,
            gate,
            evt_tx: evt_tx.clone(),
            qf: qf_ui.as_weak(),
            fj: fj_ui.as_weak(),
            settings_win: settings_ui.as_weak(),
            tray: tray.as_ref().map(|t| t.as_weak()),
            hotkey_tx,
            settings_path: settings_path.clone(),
        },
        evt_rx,
        ui.as_weak(),
    )?;

    // --uitest：启动即显示弹窗（UI 视觉验收/截图用，绕过全局热键依赖）
    if std::env::args().any(|a| a == "--uitest") {
        let tx = evt_tx.clone();
        std::thread::Builder::new()
            .name("clipx-uitest".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let _ = tx.send(AppEvt::Toggle);
            })?;
    }

    // 等价 WPF 版 ShutdownMode="OnExplicitShutdown"：窗口全部隐藏也不退出
    slint::run_event_loop_until_quit().map_err(|e| anyhow::anyhow!("事件循环异常退出: {e}"))?;

    keyboard_hook::uninstall();
    policy::restore_win_v_history();
    let exit_settings = settings::load(&settings_path);
    if exit_settings.clear_history_on_exit {
        let n = store_for_exit.clear_all();
        eprintln!("退出清空历史：{n} 条");
    }
    Ok(())
}

fn spawn_hook_forwarder<T: Send + 'static>(
    name: &str,
    tx: mpsc::Sender<AppEvt>,
    rx: mpsc::Receiver<T>,
    map: impl Fn(T) -> AppEvt + Send + 'static,
) -> Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            while let Ok(evt) = rx.recv() {
                let _ = tx.send(map(evt));
            }
        })
        .context("启动钩子转发线程失败")?;
    Ok(())
}

/// 全局热键线程：注册集来自设置并可热更新（设置保存后重注册）。
/// 排除应用前台时吞掉触发（WPF `ExclusionApps`）；另保留 Ctrl+Alt+V 兼容键。
fn spawn_hotkey(
    evt_tx: mpsc::Sender<AppEvt>,
    initial: HotkeySet,
    update_rx: mpsc::Receiver<HotkeySet>,
) -> Result<()> {
    use global_hotkey::hotkey::HotKey;
    use global_hotkey::GlobalHotKeyEvent;

    std::thread::Builder::new()
        .name("clipx-hotkey".into())
        .spawn(move || {
            let manager = match global_hotkey::GlobalHotKeyManager::new() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("热键管理器失败: {e}");
                    return;
                }
            };
            // id → (目标事件, 注册句柄)。
            let mut table: Vec<(u32, AppEvt, HotKey)> = Vec::new();

            let register_all = |set: &HotkeySet, table: &mut Vec<(u32, AppEvt, HotKey)>| {
                for (_, _, hk) in table.drain(..).collect::<Vec<_>>() {
                    let _ = manager.unregister(hk);
                }
                // 兼容键（M0 起肌肉记忆，非 WPF 项，恒注册）。
                use global_hotkey::hotkey::{Code, Modifiers};
                let compat = HotKey::new(
                    Some(Modifiers::CONTROL | Modifiers::ALT),
                    Code::KeyV,
                );
                if manager.register(compat).is_ok() {
                    table.push((compat.id(), AppEvt::Toggle, compat));
                }
                for (hk, kind) in &set.entries {
                    let (Some(mods), Some(code)) =
                        (hotkey_modifiers(hk.modifiers), vk_to_code(hk.key))
                    else {
                        eprintln!("跳过不可注册热键: {}", hk.display());
                        continue;
                    };
                    let key = HotKey::new(Some(mods), code);
                    let evt = match kind {
                        HotkeyKind::Clip => AppEvt::Toggle,
                        HotkeyKind::FileJump => AppEvt::FileJumpToggle,
                        HotkeyKind::Batch => AppEvt::BatchCycle,
                    };
                    if let Err(e) = manager.register(key) {
                        eprintln!("注册 {} 失败: {e}", hk.display());
                    } else {
                        table.push((key.id(), evt, key));
                    }
                }
            };
            register_all(&initial, &mut table);

            let fire = |id: u32, table: &[(u32, AppEvt, HotKey)], evt_tx: &mpsc::Sender<AppEvt>| {
                // 排除应用前台时全局热键不触发。
                if crate::policy::is_foreground_excluded() {
                    return;
                }
                if let Some((_, evt, _)) = table.iter().find(|(i, _, _)| *i == id) {
                    let _ = evt_tx.send(match evt {
                        AppEvt::Toggle => AppEvt::Toggle,
                        AppEvt::FileJumpToggle => AppEvt::FileJumpToggle,
                        _ => AppEvt::BatchCycle,
                    });
                }
            };

            // RegisterHotKey 的 WM_HOTKEY 只投到本线程窗口，必须自己泵
            #[cfg(windows)]
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
                };
                let receiver = GlobalHotKeyEvent::receiver();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    while let Ok(ev) = receiver.try_recv() {
                        if ev.state() == global_hotkey::HotKeyState::Pressed {
                            fire(ev.id(), &table, &evt_tx);
                        }
                    }
                    // 设置保存后的热更新（drain 取最新）。
                    let mut pending: Option<HotkeySet> = None;
                    while let Ok(s) = update_rx.try_recv() {
                        pending = Some(s);
                    }
                    if let Some(s) = pending {
                        register_all(&s, &mut table);
                    }
                }
            }
            #[cfg(not(windows))]
            {
                use std::time::Duration;
                let receiver = GlobalHotKeyEvent::receiver();
                loop {
                    while let Ok(ev) = receiver.try_recv() {
                        if ev.state() == global_hotkey::HotKeyState::Pressed {
                            fire(ev.id(), &table, &evt_tx);
                        }
                    }
                    let mut pending: Option<HotkeySet> = None;
                    while let Ok(s) = update_rx.try_recv() {
                        pending = Some(s);
                    }
                    if let Some(s) = pending {
                        register_all(&s, &mut table);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            drop(manager);
        })
        .context("启动热键线程失败")?;
    Ok(())
}

fn spawn_processor(
    rx: mpsc::Receiver<ClipEvent>,
    store: Store,
    evt_tx: mpsc::Sender<AppEvt>,
    ocr: Option<OcrQueue>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("clipx-processor".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                if crate::policy::is_capture_paused() || crate::policy::is_foreground_excluded() {
                    continue;
                }
                let src = crate::policy::foreground_app();
                let entry = match event {
                    ClipEvent::Text(text) => NewEntry::from_text(text).with_source(src),
                    ClipEvent::Image {
                        blob,
                        width,
                        height,
                        mime,
                    } => NewEntry::from_image(blob, width, height, mime).with_source(src),
                    ClipEvent::Files(paths) => NewEntry::from_files(paths).with_source(src),
                    ClipEvent::RichText { text, html } => {
                        NewEntry::from_rich_text(text, html).with_source(src)
                    }
                };
                let is_image = entry.kind == EntryKind::Image;
                match store.insert(entry) {
                    Ok(InsertOutcome::Inserted(id)) => {
                        if is_image && crate::policy::is_ocr_enabled() {
                            if let Some(q) = ocr.as_ref() {
                                q.enqueue(id);
                            }
                        }
                        let _ = evt_tx.send(AppEvt::ListChanged);
                    }
                    Ok(InsertOutcome::Bumped(_)) => {
                        let _ = evt_tx.send(AppEvt::ListChanged);
                    }
                    Err(e) => eprintln!("入库失败: {e:#}"),
                }
            }
        })
        .context("启动剪贴板处理线程失败")?;
    Ok(())
}

/// OCR 队列启动：引擎工厂在工作线程上调用（WinRT 引擎与线程绑定）。
/// 非 Windows 平台 M6/M7 接 Vision/Tesseract 前返回 None 等价队列（不启动）。
fn spawn_ocr(
    store: Store,
    evt_tx: mpsc::Sender<AppEvt>,
    enabled: bool,
) -> Result<Option<OcrQueue>> {
    crate::policy::set_ocr_enabled(enabled);
    // Windows 始终拉起队列，入队由 is_ocr_enabled 门控，设置可热切换。
    #[cfg(not(windows))]
    if !enabled {
        return Ok(None);
    }
    #[cfg(windows)]
    let engine_factory =
        || clipx_ocr::MediaOcrEngine::new().map(|e| Box::new(e) as Box<dyn clipx_ocr::OcrEngine>);
    #[cfg(not(windows))]
    let engine_factory = || None;
    let queue = OcrQueue::spawn(store, engine_factory, move || {
        let _ = evt_tx.send(AppEvt::OcrDone);
    })
    .context("启动 OCR 队列失败")?;
    Ok(Some(queue))
}

#[cfg(windows)]
fn ensure_single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::System::Threading::{
        CreateMutexW, OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS,
    };

    // SYNCHRONIZE (0x00100000)：仅需等待权限判断实例是否存在
    const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000);

    unsafe {
        // 已有实例持有命名互斥体 → 直接退出
        if OpenMutexW(SYNCHRONIZE, false, w!("clipx-single-instance")).is_ok() {
            return false;
        }
        match CreateMutexW(None, false, w!("clipx-single-instance")) {
            Ok(handle) => {
                // 句柄泄漏持有到进程退出，保证互斥体存活
                Box::leak(Box::new(handle));
                true
            }
            Err(_) => true,
        }
    }
}

#[cfg(not(windows))]
fn ensure_single_instance() -> bool {
    true
}

fn parse_seed_arg() -> Option<usize> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--seed" {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn parse_import_wpf_arg() -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--import-wpf" {
            return args.next();
        }
    }
    None
}

fn parse_everything_query_arg() -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--everything-query" {
            return args.next();
        }
    }
    None
}

/// Everything IPC 调试入口：`clipx --everything-query <表达式>`，
/// 输出结果数与耗时（验收脚本用它验证 WM_COPYDATA 链路连通性）。
fn everything_query_cli(expr: &str) -> Result<()> {
    // release 是 windows_subsystem，须挂到父控制台，否则 stdout 丢失、看起来像失败
    #[cfg(windows)]
    attach_parent_console();
    let t0 = std::time::Instant::now();
    match clipx_everything::query(expr, 20, clipx_everything::DEFAULT_TIMEOUT) {
        Ok(r) => {
            let mut body = format!(
                "layout={} items={} total_items={} elapsed={:?}\n",
                clipx_everything::debug_layout(),
                r.items.len(),
                r.total_items,
                t0.elapsed()
            );
            for it in r.items.iter().take(10) {
                body.push_str(&format!(
                    "  [{}] {}\n",
                    if it.is_folder { "D" } else { "F" },
                    it.full_path
                ));
            }
            emit_query_output(&body);
            Ok(())
        }
        Err(e) => {
            emit_query_output(&format!("query failed: {e}\n"));
            std::process::exit(2);
        }
    }
}

fn emit_query_output(text: &str) {
    eprint!("{text}");
    if let Ok(p) = std::env::var("CLIPX_QUERY_OUT") {
        if !p.is_empty() {
            let _ = std::fs::write(p, text);
        }
    }
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::AttachConsole;
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// WPF 版历史迁移：clipboard_history.db → clipx.db（保留原时间戳，幂等可重跑）。
/// 容量设置同步跟随 WPF（MaxItems/MaxImageItems 取较大值）——否则迁移后第一条
/// 新采集就会按 clipx 默认 2000 触发裁剪，把历史裁掉。
fn import_wpf(
    store: &Store,
    path: std::path::PathBuf,
    settings_path: &std::path::Path,
    settings: &settings::Settings,
) -> Result<()> {
    let (rows, bad) = clipx_store::wpf::read_rows(&path)?;
    if rows.is_empty() {
        println!("WPF 库中无可迁移条目（坏行 {bad} 条）");
        return Ok(());
    }

    let mut new_settings = settings.clone();
    if let Some(dir) = path.parent() {
        if let Ok(json) = std::fs::read_to_string(dir.join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(max_items) = v.get("MaxItems").and_then(|x| x.as_i64()) {
                    if max_items > new_settings.max_items {
                        new_settings.max_items = max_items;
                    }
                }
                if let Some(max_img) = v.get("MaxImageItems").and_then(|x| x.as_i64()) {
                    if max_img > new_settings.max_image_items {
                        new_settings.max_image_items = max_img;
                    }
                }
            }
        }
    }
    let capacity_raised = new_settings.max_items != settings.max_items
        || new_settings.max_image_items != settings.max_image_items;
    if capacity_raised {
        settings::save(settings_path, &new_settings)
            .context("同步容量设置失败（settings.json 写入）")?;
    }

    let stats = store.import_batch(rows)?;
    println!(
        "迁移完成：新增 {} 条，跳过重复 {} 条，坏行跳过 {bad} 条",
        stats.inserted, stats.skipped_dup
    );
    if capacity_raised {
        println!(
            "容量设置已跟随 WPF：max_items={} max_image_items={}",
            new_settings.max_items, new_settings.max_image_items
        );
    }
    Ok(())
}

/// 万条压测基准（M3 验收：搜索 <100ms）：临时库 seed 10000 条，
/// 测入库吞吐与典型查询路径（空查询 / FTS 词 / 中文子串 / 拼音首字母）。
fn bench() -> Result<()> {
    let tmp = std::env::temp_dir().join("clipx-bench.db");
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));

    let store = Store::open(
        &tmp,
        StoreLimits {
            max_items: 100000,
            max_image_items: 1000,
        },
    )
    .context("打开基准库失败")?;

    const N: usize = 10000;
    let t = std::time::Instant::now();
    store.seed(N).context("写入基准数据失败")?;
    let seed_ms = t.elapsed().as_millis();

    let cases = [
        ("空查询（列表加载）", ""),
        ("FTS 英文词", "quick"),
        ("中文子串", "压测"),
        ("编号子串", "#999"),
    ];
    println!("== clipx bench（{N} 条，seed {seed_ms}ms）==");
    let mut worst: f64 = 0.0;
    for (name, q) in cases {
        let t = std::time::Instant::now();
        let mut hits = 0;
        for _ in 0..50 {
            hits = store.search(q, None, 2000).len();
        }
        let avg_us = t.elapsed().as_micros() as f64 / 50.0;
        let avg_ms = avg_us / 1000.0;
        worst = worst.max(avg_ms);
        println!("{name:<14} avg {avg_ms:7.2}ms  hits {hits}");
    }
    println!("最慢路径 {worst:.2}ms（验收线 100ms）");
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

fn default_db_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("定位可执行文件失败")?;
    let dir = exe.parent().context("无法获取程序目录")?;
    Ok(dir.join("Data").join("clipx.db"))
}

fn default_settings_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("定位可执行文件失败")?;
    let dir = exe.parent().context("无法获取程序目录")?;
    Ok(dir.join("Data").join("settings.json"))
}
