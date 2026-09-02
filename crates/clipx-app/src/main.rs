#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod keyboard_hook;
mod win_popup;

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, VecModel};

use clipx_core::event::ClipEvent;
use clipx_core::{EntryMeta, NewEntry};
use clipx_store::Store;

slint::include_modules!();

const LIST_LIMIT: i64 = 2000;

fn main() -> Result<()> {
    let seed = parse_seed_arg();
    let db_path = default_db_path()?;
    let store = Store::open(&db_path).context("打开数据库失败")?;
    if let Some(n) = seed {
        store.seed(n).context("写入压测数据失败")?;
        println!("已写入 {n} 条压测数据");
        return Ok(());
    }

    let ui = PopupWindow::new().context("创建窗口失败")?;
    win_popup::apply_style(ui.window());
    ui.window().hide().ok();
    ui.set_rows(ModelRc::new(VecModel::from(rows_from_metas(
        store.list_recent(LIST_LIMIT),
    ))));

    let (event_tx, event_rx) = std::sync::mpsc::channel::<ClipEvent>();
    clipx_monitor::spawn(event_tx).context("启动剪贴板监听失败")?;

    let _hotkey_manager = init_hotkey(&ui)?;
    spawn_processor(event_rx, store, ui.as_weak())?;

    // 等价 WPF 版 ShutdownMode="OnExplicitShutdown"：窗口全部隐藏也不退出
    slint::run_event_loop_until_quit().map_err(|e| anyhow::anyhow!("事件循环异常退出: {e}"))?;
    Ok(())
}

fn init_hotkey(ui: &PopupWindow) -> Result<global_hotkey::GlobalHotKeyManager> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};

    let manager = global_hotkey::GlobalHotKeyManager::new().context("创建热键管理器失败")?;
    manager
        .register(HotKey::new(
            Some(Modifiers::CONTROL | Modifiers::ALT),
            Code::KeyV,
        ))
        .context("注册 Ctrl+Alt+V 全局热键失败")?;

    let weak = ui.as_weak();
    std::thread::Builder::new()
        .name("clipx-hotkey".into())
        .spawn(move || {
            let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
            loop {
                match receiver.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok(ev) => {
                        if ev.state() != global_hotkey::HotKeyState::Pressed {
                            continue;
                        }
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak.upgrade() else { return };
                            if ui.window().is_visible() {
                                let _ = ui.window().hide();
                                keyboard_hook::uninstall();
                            } else {
                                let _ = ui.window().show();
                                win_popup::apply_style(ui.window());
                                if !keyboard_hook::install(ui.as_weak()) {
                                    // 钩子失败时 Esc 不可用，热键仍可关闭
                                }
                            }
                        });
                    }
                    Err(_) => {}
                }
            }
        })
        .context("启动热键线程失败")?;
    Ok(manager)
}

fn spawn_processor(
    rx: std::sync::mpsc::Receiver<ClipEvent>,
    store: Store,
    weak: slint::Weak<PopupWindow>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("clipx-processor".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let entry = match event {
                    ClipEvent::Text(text) => NewEntry::from_text(text),
                    ClipEvent::Image { .. } | ClipEvent::Files(_) => continue,
                };
                if store.insert(entry).is_err() {
                    continue;
                }
                let metas = store.list_recent(LIST_LIMIT);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_rows(ModelRc::new(VecModel::from(rows_from_metas(metas))));
                    }
                });
            }
        })
        .context("启动剪贴板处理线程失败")?;
    Ok(())
}

fn rows_from_metas(metas: Vec<EntryMeta>) -> Vec<RowData> {
    metas
        .into_iter()
        .map(|m| RowData {
            id: m.id as i32,
            kind: m.kind.as_i64() as i32,
            preview: m.preview.into(),
        })
        .collect()
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
