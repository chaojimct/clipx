#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod keyboard_hook;
mod logic;
mod mouse_hook;
mod paste;
mod settings;
mod win_popup;

use std::sync::mpsc;

use anyhow::{Context, Result};

use clipx_core::event::ClipEvent;
use clipx_core::{ClipboardGate, NewEntry};
use clipx_store::Store;

use logic::AppEvt;

slint::include_modules!();

fn main() -> Result<()> {
    let seed = parse_seed_arg();
    let db_path = default_db_path()?;
    let store = Store::open(&db_path).context("打开数据库失败")?;
    if let Some(n) = seed {
        store.seed(n).context("写入压测数据失败")?;
        println!("已写入 {n} 条压测数据");
        return Ok(());
    }

    if !ensure_single_instance() {
        eprintln!("clipx 已在运行，退出本实例");
        return Ok(());
    }

    let settings_path = default_settings_path()?;
    let settings = settings::load(&settings_path);

    let gate = ClipboardGate::new();

    let ui = PopupWindow::new().context("创建窗口失败")?;
    win_popup::apply_style(ui.window());
    ui.window().hide().ok();

    let tray = match TrayIcon::new() {
        Ok(tray) => Some(tray),
        Err(e) => {
            eprintln!("系统托盘不可用: {e}");
            None
        }
    };

    let (evt_tx, evt_rx) = mpsc::channel::<AppEvt>();

    // 剪贴板监听（事件直发处理器；Gate 防自环）
    let (clip_tx, clip_rx) = mpsc::channel::<ClipEvent>();
    clipx_monitor::spawn(clip_tx, gate.clone()).context("启动剪贴板监听失败")?;

    // 热键 Ctrl+Alt+V → Toggle
    let _hotkey_manager = init_hotkey(evt_tx.clone())?;

    // 处理线程：入库成功后通知逻辑线程刷新列表
    spawn_processor(clip_rx, store.clone(), evt_tx.clone())?;

    // 钩子事件通道先行建立，再安装钩子（避免早期事件丢失）
    let key_rx = keyboard_hook::evt_channel();
    let mouse_rx = mouse_hook::hide_channel();
    spawn_hook_forwarder("clipx-key-fwd", evt_tx.clone(), key_rx, AppEvt::Key)?;
    spawn_hook_forwarder("clipx-mouse-fwd", evt_tx.clone(), mouse_rx, |_| AppEvt::Hide)?;

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
    if let Some(tray) = tray.as_ref() {
        let tx = evt_tx.clone();
        tray.on_tray_toggle(move || {
            let _ = tx.send(AppEvt::Toggle);
        });
        tray.on_tray_quit(move || {
            let _ = slint::quit_event_loop();
        });
    }

    logic::spawn(
        logic::LogicDeps { store, settings, gate },
        evt_rx,
        ui.as_weak(),
    )?;

    // 等价 WPF 版 ShutdownMode="OnExplicitShutdown"：窗口全部隐藏也不退出
    slint::run_event_loop_until_quit().map_err(|e| anyhow::anyhow!("事件循环异常退出: {e}"))?;

    keyboard_hook::uninstall();
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

fn init_hotkey(evt_tx: mpsc::Sender<AppEvt>) -> Result<global_hotkey::GlobalHotKeyManager> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};

    let manager = global_hotkey::GlobalHotKeyManager::new().context("创建热键管理器失败")?;
    manager
        .register(HotKey::new(
            Some(Modifiers::CONTROL | Modifiers::ALT),
            Code::KeyV,
        ))
        .context("注册 Ctrl+Alt+V 全局热键失败")?;

    std::thread::Builder::new()
        .name("clipx-hotkey".into())
        .spawn(move || {
            let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
            loop {
                match receiver.recv() {
                    Ok(ev) => {
                        if ev.state() == global_hotkey::HotKeyState::Pressed {
                            let _ = evt_tx.send(AppEvt::Toggle);
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .context("启动热键线程失败")?;
    Ok(manager)
}

fn spawn_processor(
    rx: mpsc::Receiver<ClipEvent>,
    store: Store,
    evt_tx: mpsc::Sender<AppEvt>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("clipx-processor".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let entry = match event {
                    ClipEvent::Text(text) => NewEntry::from_text(text),
                    ClipEvent::Image { .. } | ClipEvent::Files(_) => continue,
                };
                if store.insert(entry).is_ok() {
                    let _ = evt_tx.send(AppEvt::ListChanged);
                }
            }
        })
        .context("启动剪贴板处理线程失败")?;
    Ok(())
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
