#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod keyboard_hook;
mod logic;
mod mouse_hook;
mod paste;
mod settings;
mod win_popup;

use std::sync::mpsc;

use anyhow::{Context, Result};

use clipx_core::event::ClipEvent;
use clipx_core::{ClipboardGate, EntryKind, NewEntry};
use clipx_ocr::OcrQueue;
use clipx_store::{InsertOutcome, Store, StoreLimits};

use logic::{AppEvt, MenuAction};

slint::include_modules!();

fn main() -> Result<()> {
    let seed = parse_seed_arg();
    let db_path = default_db_path()?;
    let settings_path = default_settings_path()?;
    let settings = settings::load(&settings_path);

    if std::env::args().any(|a| a == "--bench") {
        return bench();
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

    // OCR 队列：启动回填（迁移图片补做）由 worker 自驱批量拉取；
    // 新图片实时入队，完成/失败一个条目即通知刷新
    let ocr_queue = spawn_ocr(store.clone(), evt_tx.clone(), settings.image_ocr_enabled)?;

    // 剪贴板监听（事件直发处理器；Gate 防自环）
    let (clip_tx, clip_rx) = mpsc::channel::<ClipEvent>();
    clipx_monitor::spawn(clip_tx, gate.clone()).context("启动剪贴板监听失败")?;

    // 热键 Ctrl+Alt+V → Toggle
    let _hotkey_manager = init_hotkey(evt_tx.clone())?;

    // 处理线程：入库成功后通知逻辑线程刷新列表；新图片入 OCR 队列
    spawn_processor(clip_rx, store.clone(), evt_tx.clone(), ocr_queue.clone())?;

    // 钩子事件通道先行建立，再安装钩子（避免早期事件丢失）
    let key_rx = keyboard_hook::evt_channel();
    let mouse_rx = mouse_hook::hide_channel();
    spawn_hook_forwarder("clipx-key-fwd", evt_tx.clone(), key_rx, AppEvt::Key)?;
    spawn_hook_forwarder("clipx-mouse-fwd", evt_tx.clone(), mouse_rx, |_| {
        AppEvt::Hide
    })?;

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
    if let Some(tray) = tray.as_ref() {
        let tx = evt_tx.clone();
        tray.on_tray_toggle(move || {
            let _ = tx.send(AppEvt::Toggle);
        });
        tray.set_autostart_label(
            if autostart::is_enabled() {
                "开机自启：开"
            } else {
                "开机自启：关"
            }
            .into(),
        );
        let tray_weak = tray.as_weak();
        tray.on_tray_autostart(move || {
            if let Some(enabled) = autostart::toggle() {
                if let Some(t) = tray_weak.upgrade() {
                    t.set_autostart_label(
                        if enabled {
                            "开机自启：开"
                        } else {
                            "开机自启：关"
                        }
                        .into(),
                    );
                }
            }
        });
        tray.on_tray_quit(move || {
            let _ = slint::quit_event_loop();
        });
    }

    logic::spawn(
        logic::LogicDeps {
            store,
            settings,
            gate,
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
            while let Ok(ev) = receiver.recv() {
                if ev.state() == global_hotkey::HotKeyState::Pressed {
                    let _ = evt_tx.send(AppEvt::Toggle);
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
    ocr: Option<OcrQueue>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("clipx-processor".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let entry = match event {
                    ClipEvent::Text(text) => NewEntry::from_text(text),
                    ClipEvent::Image {
                        blob,
                        width,
                        height,
                        mime,
                    } => NewEntry::from_image(blob, width, height, mime),
                    // 采集次序对齐 WPF 版：文件 > 富文本 > 纯文本 > 图片
                    ClipEvent::Files(paths) => NewEntry::from_files(paths),
                    ClipEvent::RichText { text, html } => NewEntry::from_rich_text(text, html),
                };
                let is_image = entry.kind == EntryKind::Image;
                match store.insert(entry) {
                    Ok(InsertOutcome::Inserted(id)) => {
                        if is_image {
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
